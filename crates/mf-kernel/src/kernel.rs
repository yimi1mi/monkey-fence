//! CoreKernel facade:唯一深模块缝隙(canonical spec §2.2,Issue #23)。
//!
//! - 封闭命令族:transport/UI 只能提交 [`KernelCommand`] 枚举,不存在任意
//!   SQL/effect seam;`#21` 冻结的 CommandCoordinator 在 L-CMD 事务内
//!   完成 intent → 业务效果 + target receipt + outbox 的原子链。
//! - T2a tracer 只冻结 `workflow.rename`(presentation 轴,§7.4):
//!   经 Project v7 持久 handle 定位,只推进 presentation revision。
//! - Snapshot 直接投影 Store 权威状态;事件经 publication barrier 只在
//!   commit/receipt/outbox 之后可见(见 `projection.rs`)。
//! - `attach_terminal` 在 T3 接管 mf-terminal 之前显式 fail-closed。
//!
//! GPUI legacy 侧不直接持有 Store 写路径:经 [`LegacyKernelClient`]
//! (in-process adapter)只读定位后走 `dispatch`,是拆进程后
//! `mf.legacy-transport.v1` 的前置形态。

#[cfg(test)]
use crate::command::ReconcileOutcome;
use crate::command::{
    CommandCoordinator, CommandEnvelope, CommandOutcome, CommandPayload, CommandProblem,
    CommandType, EffectOutput, FaultPoint, ProjectionEffect, ServiceIdempotencyKey, TargetDatabase,
};
use crate::handles::{
    AggregateKind, AggregateRef, ClientId, CommandId, CommandTarget, ExpectedRevision, Principal,
    ProjectStoreHandle, SessionHandle, TargetStoreKind, WorkflowHandle,
};
use crate::lease::{CommandAuthorizer, CommandPermit, LeaseCheck};
use crate::project_registry::ServiceStore;
use crate::projection::{
    EventCursor, EventSubscription, ProjectionHub, RevisionVector, SnapshotData, SnapshotEnvelope,
    SnapshotQuery, WorkflowSnapshotData, SNAPSHOT_SCHEMA,
};
use crate::reconcile::reconcile_startup;
use crate::shutdown::{ShutdownAssessment, ShutdownIntent};
use crate::singleton::{CoreOwnerLock, OwnerLockSetup};
use mf_agent::store::Store;
use parking_lot::{RwLock, RwLockReadGuard};
use rusqlite::OptionalExtension;
use rusqlite::{params, Transaction};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// facade DTO:封闭命令族 / outcome / problem(§2.2/§7.4/§7.5)
// ---------------------------------------------------------------------------

/// facade 层封闭命令枚举。新增命令 = 显式扩枚举 + 编译到 T1 effect,
/// transport/UI 不能自造命令或 payload 形状。
#[derive(Debug, Clone, PartialEq)]
pub enum KernelCommand {
    /// `workflow.rename`(§7.4:rename 归入 presentation 轴):
    /// 经 Project v7 持久 handle 定位,只推进 presentation revision;
    /// semantic/collection revision 不动。同名重命名是幂等 no-op
    /// (有 receipt、无事件)。
    WorkflowRename(WorkflowRenameCommand),
    ProjectWorkflow(ProjectWorkflowCommand),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectWorkflowCommand {
    Create {
        project: ProjectStoreHandle,
        draft: mf_agent::ProjectWorkflowDraft,
        expected_collection_revision: u64,
    },
    Delete {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        expected_collection_revision: u64,
        expected_semantic_revision: u64,
        expected_presentation_revision: u64,
    },
    AddNode {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        node: mf_agent::WorkflowNodeDraft,
        expected_semantic_revision: u64,
    },
    UpdateNode {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        node_handle: String,
        title: String,
        instructions: String,
        agent_instance_id: String,
        expected_semantic_revision: u64,
    },
    RemoveNode {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        node_handle: String,
        expected_semantic_revision: u64,
    },
    MoveNode {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        node_handle: String,
        x: f64,
        y: f64,
        expected_presentation_revision: u64,
    },
    Connect {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        upstream_node_handle: String,
        downstream_node_handle: String,
        expected_semantic_revision: u64,
    },
    Disconnect {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        edge_handle: String,
        expected_semantic_revision: u64,
    },
    SetViewport {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        viewport: Value,
        expected_presentation_revision: u64,
    },
    SetUnsafeParallel {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        allow: bool,
        expected_semantic_revision: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRenameCommand {
    project: ProjectStoreHandle,
    workflow: WorkflowHandle,
    name: String,
    expected_presentation_revision: u64,
}

impl KernelCommand {
    pub fn workflow_rename(
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        name: impl Into<String>,
        expected_presentation_revision: u64,
    ) -> Self {
        Self::WorkflowRename(WorkflowRenameCommand {
            project,
            workflow,
            name: name.into(),
            expected_presentation_revision,
        })
    }

    /// 命令类型名(与 T1 `CommandType` 的 wire 名一致)。
    pub const fn command_type(&self) -> CommandType {
        match self {
            Self::WorkflowRename(_) => CommandType::WorkflowRename,
            Self::ProjectWorkflow(command) => command.command_type(),
        }
    }
}

impl ProjectWorkflowCommand {
    pub const fn command_type(&self) -> CommandType {
        match self {
            Self::Create { .. } => CommandType::WorkflowCreate,
            Self::Delete { .. } => CommandType::WorkflowDelete,
            Self::AddNode { .. } => CommandType::WorkflowAddNode,
            Self::UpdateNode { .. } => CommandType::WorkflowUpdateNode,
            Self::RemoveNode { .. } => CommandType::WorkflowRemoveNode,
            Self::MoveNode { .. } => CommandType::WorkflowMoveNode,
            Self::Connect { .. } => CommandType::WorkflowConnect,
            Self::Disconnect { .. } => CommandType::WorkflowDisconnect,
            Self::SetViewport { .. } => CommandType::WorkflowSetViewport,
            Self::SetUnsafeParallel { .. } => CommandType::WorkflowSetUnsafeParallel,
        }
    }
}

/// 一次 dispatch 请求:身份 + 租约 + 封闭命令。
/// canonical digest 只覆盖命令语义(`#21` 口径),凭据/epoch 不参与。
#[derive(Debug, Clone)]
pub struct KernelCommandRequest {
    command_id: CommandId,
    client_id: ClientId,
    principal: Principal,
    controller_epoch: u64,
    command: KernelCommand,
}

impl KernelCommandRequest {
    pub fn new(
        command_id: CommandId,
        client_id: ClientId,
        principal: Principal,
        controller_epoch: u64,
        command: KernelCommand,
    ) -> Self {
        Self {
            command_id,
            client_id,
            principal,
            controller_epoch,
            command,
        }
    }
}

/// dispatch 结果。`202 accepted`(Operation)属后续 ticket;T2a 只有
/// 同步 applied。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelOutcome {
    Applied {
        revisions: RevisionVector,
        /// true = 命中既有 target receipt 的幂等重放,effect 未重放。
        replayed: bool,
    },
}

/// facade problem(§7.5 稳定错误码的内核子集)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KernelProblem {
    #[error("resource_not_found")]
    ResourceNotFound,
    #[error("invalid_envelope:{0}")]
    InvalidEnvelope(String),
    #[error("revision_conflict")]
    RevisionConflict,
    #[error("validation_failed:{0}")]
    ValidationFailed(String),
    #[error("workflow_cycle:{0}")]
    WorkflowCycle(String),
    #[error("unknown_dependency:{0}")]
    UnknownDependency(String),
    #[error("command_id_reused")]
    CommandIdReused,
    #[error("command_in_progress")]
    CommandInProgress,
    #[error("controller_lease_expired")]
    ControllerLeaseExpired,
    #[error("root_epoch_expired")]
    RootEpochExpired,
    #[error("resync_required")]
    ResyncRequired,
    #[error("service_unavailable:{0}")]
    ServiceUnavailable(String),
    #[error("internal_error:{0}")]
    Internal(String),
}

impl KernelProblem {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ResourceNotFound => "resource_not_found",
            Self::InvalidEnvelope(_) => "invalid_envelope",
            Self::RevisionConflict => "revision_conflict",
            Self::ValidationFailed(_) => "validation_failed",
            Self::WorkflowCycle(_) => "workflow_cycle",
            Self::UnknownDependency(_) => "unknown_dependency",
            Self::CommandIdReused => "command_id_reused",
            Self::CommandInProgress => "command_in_progress",
            Self::ControllerLeaseExpired => "controller_lease_expired",
            Self::RootEpochExpired => "root_epoch_expired",
            Self::ResyncRequired => "resync_required",
            Self::ServiceUnavailable(_) => "service_unavailable",
            Self::Internal(_) => "internal_error",
        }
    }
}

impl From<CommandProblem> for KernelProblem {
    fn from(problem: CommandProblem) -> Self {
        match problem {
            CommandProblem::InvalidEnvelope(message) => Self::InvalidEnvelope(message),
            CommandProblem::CommandIdReused => Self::CommandIdReused,
            CommandProblem::CommandInProgress => Self::CommandInProgress,
            CommandProblem::ControllerLeaseExpired => Self::ControllerLeaseExpired,
            CommandProblem::RootEpochExpired => Self::RootEpochExpired,
            CommandProblem::RevisionConflict => Self::RevisionConflict,
            CommandProblem::ValidationFailed(message) => Self::ValidationFailed(message),
            CommandProblem::WorkflowCycle(message) => Self::WorkflowCycle(message),
            CommandProblem::UnknownDependency(message) => Self::UnknownDependency(message),
            CommandProblem::ResourceNotFound => Self::ResourceNotFound,
            other => Self::Internal(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// attach_terminal 占位 DTO(T3 mf-terminal 接管前 fail-closed)
// ---------------------------------------------------------------------------

/// 终端 attach 请求(T3 冻结完整形状前的最小占位)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalAttach {
    pub after_seq: u64,
}

/// 终端通道(T3 由 `mf-terminal::channel` 接管;当前不可构造)。
#[derive(Debug, Clone)]
pub struct TerminalChannel {
    _private: (),
}

// ---------------------------------------------------------------------------
// CoreKernel trait(§2.2:唯一深模块缝隙)
// ---------------------------------------------------------------------------

/// 所有调用方(WebGateway、legacy GPUI adapter、launcher/tray IPC、测试
/// harness)只允许经这五个方法与 Core 交互;字段一律私有。
pub trait CoreKernel: Send + Sync {
    fn dispatch(&self, request: KernelCommandRequest) -> Result<KernelOutcome, KernelProblem>;
    fn snapshot(&self, query: SnapshotQuery) -> Result<SnapshotEnvelope, KernelProblem>;
    fn subscribe_events(&self, cursor: EventCursor) -> Result<EventSubscription, KernelProblem>;
    fn attach_terminal(
        &self,
        session: SessionHandle,
        attach: TerminalAttach,
    ) -> Result<TerminalChannel, KernelProblem>;
    fn shutdown(&self, intent: ShutdownIntent) -> ShutdownAssessment;
}

// ---------------------------------------------------------------------------
// Controller lease(最小真实实现:epoch + client 绑定)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ControllerLeaseState {
    epoch: u64,
    controller: Option<ClientId>,
    principal: Option<Principal>,
}

struct ControllerLeaseShared {
    state: RwLock<ControllerLeaseState>,
}

// ---------------------------------------------------------------------------
// L-CMD 事务内租约/CAS 复验(kernel 自己的 authorizer)
// ---------------------------------------------------------------------------

struct InProcessAuthorizer {
    lease: Arc<ControllerLeaseShared>,
}

impl CommandAuthorizer for InProcessAuthorizer {
    fn acquire<'a>(
        &'a self,
        _tx: &Transaction<'_>,
        check: &LeaseCheck<'_>,
    ) -> Result<Box<dyn CommandPermit + 'a>, CommandProblem> {
        // 读锁覆盖整个目标事务:复验通过后、commit 前 takeover 无法旋转
        // epoch(permit 契约)。
        let guard = self.lease.state.read();
        if check.controller_epoch != guard.epoch {
            return Err(CommandProblem::ControllerLeaseExpired);
        }
        if guard.controller.as_ref() != Some(check.client_id)
            || guard.principal.as_ref() != Some(check.principal)
        {
            return Err(CommandProblem::ControllerLeaseExpired);
        }
        // T2a 命令族都不携带 root epoch;声明了即拒绝(fail-closed)。
        if check.root_epoch.is_some() {
            return Err(CommandProblem::RootEpochExpired);
        }
        Ok(Box::new(InProcessPermit { _guard: guard }))
    }
}

struct InProcessPermit<'a> {
    _guard: RwLockReadGuard<'a, ControllerLeaseState>,
}

impl CommandPermit for InProcessPermit<'_> {
    fn validate_expected(
        &self,
        tx: &Transaction<'_>,
        check: &LeaseCheck<'_>,
    ) -> Result<(), CommandProblem> {
        for expected in check.expected {
            match expected.aggregate.kind {
                AggregateKind::ProjectWorkflow => {
                    let row = tx
                        .query_row(
                            "SELECT semantic_revision, presentation_revision
                             FROM project_workflows WHERE public_handle = ?1",
                            [expected.aggregate.handle.as_str()],
                            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                        )
                        .optional()
                        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                    let Some((semantic, presentation)) = row else {
                        return Err(CommandProblem::ResourceNotFound);
                    };
                    for (axis, expected_revision) in &expected.revisions {
                        let actual = match axis.as_str() {
                            "semantic_revision" => semantic,
                            "presentation_revision" => presentation,
                            other => {
                                return Err(CommandProblem::InvalidEnvelope(format!(
                                    "未知 Workflow revision 轴:{other}"
                                )))
                            }
                        };
                        if actual != *expected_revision as i64 {
                            return Err(CommandProblem::RevisionConflict);
                        }
                    }
                }
                AggregateKind::Project => {
                    if expected.aggregate.handle != check.target.store_handle {
                        return Err(CommandProblem::TargetStoreMismatch);
                    }
                    let collection = tx
                        .query_row(
                            "SELECT workflow_collection_revision FROM project_meta WHERE id=1",
                            [],
                            |row| row.get::<_, i64>(0),
                        )
                        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                    for (axis, expected_revision) in &expected.revisions {
                        if axis != "workflow_collection_revision" {
                            return Err(CommandProblem::InvalidEnvelope(format!(
                                "未知 Project revision 轴:{axis}"
                            )));
                        }
                        if collection != *expected_revision as i64 {
                            return Err(CommandProblem::RevisionConflict);
                        }
                    }
                }
                _ => {
                    return Err(CommandProblem::InvalidEnvelope(
                        "Project Workflow 命令只接受 project/project_workflow expected".into(),
                    ))
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// in-process CoreKernel 实现
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ProjectRegistration {
    store: Arc<Store>,
    target: TargetDatabase,
    closing: bool,
}

pub struct ProjectCloseToken {
    project: ProjectStoreHandle,
    store: Arc<Store>,
}

/// 进程内 CoreKernel(Bridge A 前身):GPUI 与未来 WebGateway 共用的
/// 唯一写路径。持有 service-v1、已登记 Project Store 与事件 journal;
/// 不拥有 Orchestrator/PTY(那些仍在 legacy AppCtx,后续 ticket 迁移)。
pub(crate) struct InProcessCoreKernel {
    coordinator: CommandCoordinator,
    service: Arc<ServiceStore>,
    lease: Arc<ControllerLeaseShared>,
    authorizer: InProcessAuthorizer,
    projects: RwLock<HashMap<String, ProjectRegistration>>,
    projections: ProjectionHub,
}

impl InProcessCoreKernel {
    pub(crate) fn new(service: Arc<ServiceStore>, idempotency_key: ServiceIdempotencyKey) -> Self {
        Self::with_projections(service, idempotency_key, ProjectionHub::new())
    }

    #[cfg(test)]
    pub(crate) fn new_with_journal_limits(
        service: Arc<ServiceStore>,
        idempotency_key: ServiceIdempotencyKey,
        max_events: usize,
        max_bytes: usize,
    ) -> Self {
        Self::with_projections(
            service,
            idempotency_key,
            ProjectionHub::for_test(max_events, max_bytes),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_projection_limits(
        service: Arc<ServiceStore>,
        idempotency_key: ServiceIdempotencyKey,
        limits: crate::limits::JournalLimits,
    ) -> Self {
        Self::with_projections(
            service,
            idempotency_key,
            ProjectionHub::for_test_limits(limits),
        )
    }

    fn with_projections(
        service: Arc<ServiceStore>,
        idempotency_key: ServiceIdempotencyKey,
        projections: ProjectionHub,
    ) -> Self {
        let lease = Arc::new(ControllerLeaseShared {
            state: RwLock::new(ControllerLeaseState::default()),
        });
        Self {
            coordinator: CommandCoordinator::new(service.clone(), idempotency_key),
            authorizer: InProcessAuthorizer {
                lease: lease.clone(),
            },
            lease,
            service,
            projects: RwLock::new(HashMap::new()),
            projections,
        }
    }

    /// 登记一个 Project Store(handle 来自 service-v1 `project_registry`,
    /// 幂等;同 handle 重复登记覆盖为最新 Store 实例)。
    pub(crate) fn register_project_store(
        &self,
        root: &Path,
        store: Arc<Store>,
    ) -> Result<ProjectStoreHandle, KernelProblem> {
        let registered = self
            .service
            .register_project_path(root)
            .map_err(|error| KernelProblem::Internal(format!("project registry:{error:#}")))?;
        let project = ProjectStoreHandle::parse(registered.project_handle)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        let target = TargetDatabase::project(project.as_str(), store.clone())
            .map_err(KernelProblem::from)?;
        let mut recovery_targets = self.registered_targets();
        let live_targets = recovery_targets.clone();
        recovery_targets.retain(|existing| existing.store_key() != target.store_key());
        recovery_targets.push(target.clone());
        self.projections.linearize(&recovery_targets, |hub| {
            if self
                .projects
                .read()
                .get(project.as_str())
                .is_some_and(|registration| registration.closing)
            {
                return Err(KernelProblem::ServiceUnavailable(
                    "project_close_in_progress".into(),
                ));
            }
            // 已在线 target 的 pending 只能属于当前 epoch：先正常发布；
            // publication fault 会 rotate/resync。新 target 尚未可见，其 pending
            // 才由 startup reconcile 标为旧 epoch，二者不可混淆。
            for live in &live_targets {
                hub.publish_pending(live)?;
            }
            // Project 在旧 outbox / intent / Operation 收口前不进入可见
            // registry；悬空 pending 一律按旧投影 reconciled。
            reconcile_startup(&self.service, &recovery_targets, chrono::Utc::now())
                .map_err(KernelProblem::from)?;
            self.projects.write().insert(
                project.as_str().to_string(),
                ProjectRegistration {
                    store,
                    target,
                    closing: false,
                },
            );
            Ok(())
        })?;
        Ok(project)
    }

    pub(crate) fn unregister_project_store(
        &self,
        project: &ProjectStoreHandle,
    ) -> Result<(), KernelProblem> {
        let token = self.prepare_project_close(project)?;
        self.finalize_project_close(token);
        Ok(())
    }

    pub(crate) fn prepare_project_close(
        &self,
        project: &ProjectStoreHandle,
    ) -> Result<ProjectCloseToken, KernelProblem> {
        let targets = self.registered_targets();
        self.projections.linearize(&targets, |_| {
            let mut projects = self.projects.write();
            let registration = projects
                .get_mut(project.as_str())
                .ok_or(KernelProblem::ResourceNotFound)?;
            if registration.closing {
                return Err(KernelProblem::ServiceUnavailable(
                    "project_close_in_progress".into(),
                ));
            }
            registration.closing = true;
            Ok(ProjectCloseToken {
                project: project.clone(),
                store: registration.store.clone(),
            })
        })
    }

    pub(crate) fn finalize_project_close(&self, token: ProjectCloseToken) {
        self.projections.finalize_close(|| {
            let mut projects = self.projects.write();
            let remove = projects.get(token.project.as_str()).is_some_and(|current| {
                current.closing && Arc::ptr_eq(&current.store, &token.store)
            });
            if remove {
                projects.remove(token.project.as_str());
            }
        });
    }

    /// 授予 Controller lease(新 controller 使旧 epoch 立即失效)并返回
    /// 新 epoch。
    pub(crate) fn grant_controller(
        &self,
        client: &ClientId,
        principal: &Principal,
    ) -> Result<u64, KernelProblem> {
        let mut state = self.lease.state.write();
        state.epoch = state.epoch.checked_add(1).ok_or_else(|| {
            KernelProblem::ServiceUnavailable("controller_epoch_exhausted".into())
        })?;
        state.controller = Some(client.clone());
        state.principal = Some(principal.clone());
        Ok(state.epoch)
    }

    /// 当前事件游标(客户端先取 cursor 再 subscribe)。
    #[cfg(test)]
    pub(crate) fn current_event_cursor(&self) -> EventCursor {
        let registered_targets = self.registered_targets();
        self.projections
            .linearize(&registered_targets, |hub| Ok(hub.cursor()))
            .expect("contract cursor requires recovered projection hub")
    }

    #[cfg(test)]
    pub(crate) fn projection_stats(&self) -> crate::journal::JournalStats {
        self.projections.stats()
    }

    #[cfg(test)]
    pub(crate) fn append_projection_probe(&self) -> Result<u64, KernelProblem> {
        let targets = self.registered_targets();
        self.projections
            .linearize(&targets, |hub| hub.append_probe())
    }

    /// 崩溃恢复:以 target receipt 为权威终结 intent 并补发事件;
    /// 绝不重放业务写(`#21/#22` reconcile 语义)。
    #[cfg(test)]
    pub(crate) fn reconcile_command(
        &self,
        project: &ProjectStoreHandle,
        command_id: &CommandId,
    ) -> Result<ReconcileOutcome, KernelProblem> {
        let registered_targets = self.registered_targets();
        self.projections.linearize(&registered_targets, |hub| {
            let registration = self.project_registration(project)?;
            let outcome = self
                .coordinator
                .reconcile(command_id, &registration.target)
                .map_err(KernelProblem::from)?;
            hub.publish_pending(&registration.target)?;
            Ok(outcome)
        })
    }

    #[cfg(test)]
    pub(crate) fn publish_pending_for_test(
        &self,
        project: &ProjectStoreHandle,
    ) -> Result<(), KernelProblem> {
        let registered_targets = self.registered_targets();
        self.projections.linearize(&registered_targets, |hub| {
            let registration = self.project_registration(project)?;
            hub.publish_pending(&registration.target)
        })
    }

    /// 契约测试故障注入缝隙:模拟 dispatch 在指定线性化点崩溃。
    #[cfg(test)]
    pub(crate) fn dispatch_rename_with_fault(
        &self,
        request: KernelCommandRequest,
        fault: Option<FaultPoint>,
    ) -> Result<KernelOutcome, KernelProblem> {
        match &request.command {
            KernelCommand::WorkflowRename(_) => {
                self.dispatch_workflow_rename(request, fault, || {})
            }
            KernelCommand::ProjectWorkflow(_) => Err(KernelProblem::InvalidEnvelope(
                "fault seam 仅支持 workflow.rename".into(),
            )),
        }
    }

    #[cfg(test)]
    pub(crate) fn dispatch_rename_with_barrier_hook(
        &self,
        request: KernelCommandRequest,
        before_barrier: impl FnOnce(),
    ) -> Result<KernelOutcome, KernelProblem> {
        self.dispatch_workflow_rename(request, None, before_barrier)
    }

    fn project_registration(
        &self,
        project: &ProjectStoreHandle,
    ) -> Result<ProjectRegistration, KernelProblem> {
        self.projects
            .read()
            .get(project.as_str())
            .filter(|registration| !registration.closing)
            .cloned()
            .ok_or(KernelProblem::ResourceNotFound)
    }

    fn registered_targets(&self) -> Vec<TargetDatabase> {
        self.projects
            .read()
            .values()
            .map(|registration| registration.target.clone())
            .collect()
    }

    fn legacy_workflow_locator(
        &self,
        project: &ProjectStoreHandle,
        workflow_key: &str,
    ) -> Result<(WorkflowHandle, u64), KernelProblem> {
        let registered_targets = self.registered_targets();
        let row = self.projections.linearize(&registered_targets, |_| {
            let registration = self.project_registration(project)?;
            registration
                .store
                .with_conn(|conn| {
                    conn.query_row(
                        "SELECT public_handle, presentation_revision FROM project_workflows
                         WHERE workflow_key=?1",
                        [workflow_key],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()
                    .map_err(anyhow::Error::from)
                })
                .map_err(|error| KernelProblem::Internal(format!("{error:#}")))?
                .ok_or(KernelProblem::ResourceNotFound)
        })?;
        Ok((
            WorkflowHandle::parse(row.0)
                .map_err(|error| KernelProblem::Internal(error.to_string()))?,
            u64::try_from(row.1)
                .map_err(|_| KernelProblem::Internal("presentation_revision 溢出".into()))?,
        ))
    }

    fn dispatch_workflow_rename(
        &self,
        request: KernelCommandRequest,
        fault: Option<FaultPoint>,
        before_barrier: impl FnOnce(),
    ) -> Result<KernelOutcome, KernelProblem> {
        let KernelCommand::WorkflowRename(command) = &request.command else {
            return Err(KernelProblem::InvalidEnvelope(
                "rename dispatcher 收到其它命令".into(),
            ));
        };
        let project = &command.project;
        let workflow = &command.workflow;
        let name = &command.name;
        let expected_presentation_revision = &command.expected_presentation_revision;
        let name = name.trim();
        if name.is_empty() {
            return Err(KernelProblem::InvalidEnvelope("工作流名称不能为空".into()));
        }
        let registered_targets = self.registered_targets();
        before_barrier();
        self.projections.linearize(&registered_targets, |hub| {
            // unregister 若先线性化，任何 barrier 外预取的 Store clone 都
            // 不得在注销后继续写；所以 registration 必须在这里重新解析。
            let registration = self.project_registration(project)?;
            let aggregate = AggregateRef::new(
                AggregateKind::ProjectWorkflow,
                workflow.as_str().to_string(),
            )
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
            let mut revisions = std::collections::BTreeMap::new();
            revisions.insert(
                "presentation_revision".to_string(),
                *expected_presentation_revision,
            );
            let envelope = CommandEnvelope::new(
                request.command_id.clone(),
                request.client_id.clone(),
                request.principal.clone(),
                request.controller_epoch,
                None,
                CommandTarget {
                    store: TargetStoreKind::Project,
                    store_handle: project.as_str().to_string(),
                    aggregate: aggregate.clone(),
                },
                vec![ExpectedRevision {
                    aggregate,
                    revisions,
                }],
                CommandType::WorkflowRename,
                CommandPayload::Plain(serde_json::json!({ "name": name })),
            )?;
            let workflow_handle = workflow.as_str().to_string();
            let new_name = name.to_string();
            let outcome = self.coordinator.dispatch_internal(
                &envelope,
                &registration.target,
                &self.authorizer,
                |tx| workflow_rename_effect(tx, &workflow_handle, &new_name),
                fault,
                || {},
            );
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    // dispatch 错误既可能发生在 target commit 前，也可能发生在
                    // target receipt/outbox 已提交之后（crash window）。不能靠错误
                    // 类型猜测；以 target receipt 为权威判断。commit 后失败必须
                    // 旋转 epoch + reconciled outbox，避免 Snapshot 暴露新 Store
                    // 状态却仍携带旧 cursor。
                    let target_committed = registration.target.with_conn(|conn| {
                        conn.query_row(
                            "SELECT EXISTS(
                                 SELECT 1 FROM command_receipt
                                 WHERE command_id=?1 AND state='applied'
                                   AND finalized_at IS NOT NULL
                             )",
                            [request.command_id.as_str()],
                            |row| row.get::<_, bool>(0),
                        )
                        .map_err(|query_error| CommandProblem::Internal(query_error.to_string()))
                    });
                    match target_committed {
                        Ok(true) | Err(_) => {
                            hub.abort_publication(&registration.target)?;
                        }
                        Ok(false) => {}
                    }
                    return Err(KernelProblem::from(error));
                }
            };
            // L-PUBLISH:目标事务 commit + receipt + outbox 之后才对外可见。
            hub.publish_pending(&registration.target)?;
            let CommandOutcome::Applied {
                result_revisions,
                replayed,
            } = outcome;
            Ok(KernelOutcome::Applied {
                revisions: revision_vector_of(&result_revisions)?,
                replayed,
            })
        })
    }

    fn workflow_snapshot(&self, query: SnapshotQuery) -> Result<SnapshotEnvelope, KernelProblem> {
        let SnapshotQuery::Workflow { project, workflow } = &query;
        let registered_targets = self.registered_targets();
        // ProjectionHub 同时拥有 cursor 与 Store reader 的 publication
        // barrier，不可能返回新 Store 状态配旧 through_seq。
        let (cursor, row) = self.projections.snapshot(&registered_targets, || {
            let registration = self.project_registration(project)?;
            registration
                .store
                .with_conn(|conn| {
                    let record = Store::project_workflow_by_handle_tx(conn, workflow.as_str())?;
                    let nodes = Store::workflow_node_identities_tx(conn, workflow.as_str())?;
                    let edges = Store::workflow_edge_identities_tx(conn, workflow.as_str())?;
                    let collection: i64 = conn.query_row(
                        "SELECT workflow_collection_revision FROM project_meta WHERE id=1",
                        [],
                        |row| row.get(0),
                    )?;
                    let mut positions_stmt=conn.prepare("SELECT node_handle,x,y FROM node_position")?;
                    let positions=positions_stmt.query_map([],|row|Ok((row.get::<_,String>(0)?,(row.get::<_,f64>(1)?,row.get::<_,f64>(2)?))))?.collect::<Result<std::collections::HashMap<_,_>,_>>()?;
                    let presentation=conn.query_row("SELECT viewport_json,collapse_json,layout_json FROM workflow_presentation WHERE workflow_handle=?1",[workflow.as_str()],|row|Ok((row.get::<_,Option<String>>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,Option<String>>(2)?))).optional()?;
                    Ok((record, nodes, edges, collection,positions,presentation))
                })
                .map_err(|error| KernelProblem::Internal(format!("{error:#}")))
        })?;
        let (record, nodes, edges, collection, positions, presentation) = row;
        let Some(record) = record else {
            return Err(KernelProblem::ResourceNotFound);
        };
        let revisions = RevisionVector {
            semantic_revision: u64::try_from(record.semantic_revision)
                .map_err(|_| KernelProblem::Internal("semantic_revision 溢出".into()))?,
            presentation_revision: u64::try_from(record.presentation_revision)
                .map_err(|_| KernelProblem::Internal("presentation_revision 溢出".into()))?,
        };
        let node_handle_map: std::collections::HashMap<_, _> = nodes
            .iter()
            .map(|row| (row.node_key.clone(), row.node_handle.clone()))
            .collect();
        let snapshot_nodes = nodes
            .into_iter()
            .filter_map(|identity| {
                record
                    .nodes
                    .iter()
                    .find(|node| node.key == identity.node_key)
                    .map(|node| crate::projection::WorkflowSnapshotNode {
                        handle: identity.node_handle.clone(),
                        key: node.key.clone(),
                        title: node.title.clone(),
                        instructions: node.instructions.clone(),
                        agent_instance_id: node.agent_instance_id.clone(),
                        deps: node.deps.clone(),
                        position: positions.get(&identity.node_handle).copied(),
                    })
            })
            .collect();
        let snapshot_edges = edges
            .into_iter()
            .filter_map(|edge| {
                Some(crate::projection::WorkflowSnapshotEdge {
                    handle: edge.edge_handle,
                    upstream_node_handle: node_handle_map.get(&edge.upstream_node_key)?.clone(),
                    downstream_node_handle: node_handle_map.get(&edge.downstream_node_key)?.clone(),
                })
            })
            .collect();
        Ok(SnapshotEnvelope {
            schema: SNAPSHOT_SCHEMA,
            server_instance_id: self.projections.server_instance_id().clone(),
            cursor,
            data: SnapshotData::Workflow(WorkflowSnapshotData {
                workflow: WorkflowHandle::parse(record.public_handle.clone())
                    .map_err(|error| KernelProblem::Internal(error.to_string()))?,
                name: record.name.clone(),
                allow_unsafe_parallel: record.allow_unsafe_parallel,
                revisions,
                nodes: snapshot_nodes,
                edges: snapshot_edges,
                workflow_collection_revision: u64::try_from(collection)
                    .map_err(|_| KernelProblem::Internal("collection revision 溢出".into()))?,
                viewport: presentation
                    .as_ref()
                    .and_then(|value| value.0.as_ref())
                    .and_then(|raw| serde_json::from_str(raw).ok()),
                collapse: presentation
                    .as_ref()
                    .and_then(|value| value.1.as_ref())
                    .and_then(|raw| serde_json::from_str(raw).ok()),
                layout: presentation
                    .as_ref()
                    .and_then(|value| value.2.as_ref())
                    .and_then(|raw| serde_json::from_str(raw).ok()),
            }),
        })
    }

    fn assess_shutdown(&self, _intent: ShutdownIntent) -> ShutdownAssessment {
        let mut assessment = ShutdownAssessment::default();
        let journal = self.projections.stats();
        log::debug!(
            "workflow journal events={} bytes={} first_seq={} clients={} queue_events={} queue_bytes={} rotations={} capacity_rotations={} publication_rotations={} protocol_rotations={} evicted={} resyncs={}",
            journal.events,
            journal.bytes,
            journal.first_available_seq,
            journal.clients,
            journal.max_client_queue_events,
            journal.max_client_queue_bytes,
            journal.rotations,
            journal.capacity_rotations,
            journal.publication_rotations,
            journal.protocol_rotations,
            journal.evicted,
            journal.resyncs,
        );
        let registrations: Vec<(String, ProjectRegistration)> = self
            .projects
            .read()
            .iter()
            .map(|(handle, registration)| (handle.clone(), registration.clone()))
            .collect();
        for (handle, registration) in registrations {
            match registration.store.running_runs() {
                Ok(runs) if !runs.is_empty() => {
                    assessment.active_workflow_runs += runs.len();
                    assessment
                        .blockers
                        .push(format!("项目 {handle}:{} 个活动 Workflow Run", runs.len()));
                }
                Ok(_) => {}
                Err(error) => assessment
                    .blockers
                    .push(format!("项目 {handle}:活动运行读取失败:{error:#}")),
            }
            match registration.target.with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM projection_outbox WHERE published_at IS NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|error| CommandProblem::Internal(error.to_string()))
            }) {
                Ok(pending) if pending > 0 => {
                    let pending = pending as usize;
                    assessment.pending_outbox_events += pending;
                    assessment
                        .blockers
                        .push(format!("项目 {handle}:{pending} 个事件待发布"));
                }
                Ok(_) => {}
                Err(error) => assessment
                    .blockers
                    .push(format!("项目 {handle}:待发布事件读取失败:{error}")),
            }
        }
        match self.service.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM command_intent WHERE state = 'reserved'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(anyhow::Error::from)
        }) {
            Ok(reserved) if reserved > 0 => {
                assessment.unfinished_intents = reserved as usize;
                assessment.blockers.push(format!(
                    "{} 个未终结 command intent",
                    assessment.unfinished_intents
                ));
            }
            Ok(_) => {}
            Err(error) => assessment
                .blockers
                .push(format!("command intent 读取失败:{error:#}")),
        }
        assessment.safe_to_proceed = assessment.blockers.is_empty();
        assessment
    }
}

impl CoreKernel for InProcessCoreKernel {
    fn dispatch(&self, request: KernelCommandRequest) -> Result<KernelOutcome, KernelProblem> {
        match &request.command {
            KernelCommand::WorkflowRename(_) => self.dispatch_workflow_rename(request, None, || {}),
            KernelCommand::ProjectWorkflow(_) => self.dispatch_project_workflow(request),
        }
    }

    fn snapshot(&self, query: SnapshotQuery) -> Result<SnapshotEnvelope, KernelProblem> {
        self.workflow_snapshot(query)
    }

    fn subscribe_events(&self, cursor: EventCursor) -> Result<EventSubscription, KernelProblem> {
        let registered_targets = self.registered_targets();
        self.projections
            .subscribe_live(&registered_targets, &cursor)
    }

    fn attach_terminal(
        &self,
        _session: SessionHandle,
        _attach: TerminalAttach,
    ) -> Result<TerminalChannel, KernelProblem> {
        // T3(mf-terminal)接管前显式 fail-closed,不给半吊子终端通道。
        Err(KernelProblem::ServiceUnavailable(
            "attach_terminal 在 T3 Terminal 管线落地后才可用".into(),
        ))
    }

    fn shutdown(&self, intent: ShutdownIntent) -> ShutdownAssessment {
        self.assess_shutdown(intent)
    }
}

impl InProcessCoreKernel {
    fn dispatch_project_workflow(
        &self,
        request: KernelCommandRequest,
    ) -> Result<KernelOutcome, KernelProblem> {
        let KernelCommand::ProjectWorkflow(command) = request.command.clone() else {
            return Err(KernelProblem::InvalidEnvelope(
                "workflow dispatcher 收到其它命令".into(),
            ));
        };
        let project = project_of(&command).clone();
        let target_aggregate = match workflow_of(&command) {
            Some(workflow) => {
                AggregateRef::new(AggregateKind::ProjectWorkflow, workflow.as_str().to_owned())
            }
            None => AggregateRef::new(AggregateKind::Project, project.as_str().to_owned()),
        }
        .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        let expected = expected_revisions(&command, &project, &target_aggregate)?;
        let payload = project_workflow_payload(&command)?;
        let command_type = command.command_type();
        let envelope = CommandEnvelope::new(
            request.command_id.clone(),
            request.client_id.clone(),
            request.principal.clone(),
            request.controller_epoch,
            None,
            CommandTarget {
                store: TargetStoreKind::Project,
                store_handle: project.as_str().to_owned(),
                aggregate: target_aggregate,
            },
            expected,
            command_type,
            CommandPayload::Plain(payload),
        )?;
        let targets = self.registered_targets();
        self.projections.linearize(&targets, |hub| {
            let registration = self.project_registration(&project)?;
            let outcome = self.coordinator.dispatch_internal(
                &envelope, &registration.target, &self.authorizer,
                |tx| project_workflow_effect(tx, &project, &command), None, || {},
            );
            let outcome = match outcome {
                Ok(value) => value,
                Err(error) => {
                    let committed = registration.target.with_conn(|conn| conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM command_receipt WHERE command_id=?1 AND state='applied' AND finalized_at IS NOT NULL)",
                        [request.command_id.as_str()], |row| row.get::<_, bool>(0),
                    ).map_err(|e| CommandProblem::Internal(e.to_string())));
                    if !matches!(committed, Ok(false)) { hub.abort_publication(&registration.target)?; }
                    return Err(KernelProblem::from(error));
                }
            };
            hub.publish_pending(&registration.target)?;
            let CommandOutcome::Applied { result_revisions, replayed } = outcome;
            Ok(KernelOutcome::Applied { revisions: revision_vector_of(&result_revisions)?, replayed })
        })
    }
}

fn project_of(command: &ProjectWorkflowCommand) -> &ProjectStoreHandle {
    match command {
        ProjectWorkflowCommand::Create { project, .. }
        | ProjectWorkflowCommand::Delete { project, .. }
        | ProjectWorkflowCommand::AddNode { project, .. }
        | ProjectWorkflowCommand::UpdateNode { project, .. }
        | ProjectWorkflowCommand::RemoveNode { project, .. }
        | ProjectWorkflowCommand::MoveNode { project, .. }
        | ProjectWorkflowCommand::Connect { project, .. }
        | ProjectWorkflowCommand::Disconnect { project, .. }
        | ProjectWorkflowCommand::SetViewport { project, .. }
        | ProjectWorkflowCommand::SetUnsafeParallel { project, .. } => project,
    }
}

fn workflow_of(command: &ProjectWorkflowCommand) -> Option<&WorkflowHandle> {
    match command {
        ProjectWorkflowCommand::Create { .. } => None,
        ProjectWorkflowCommand::Delete { workflow, .. }
        | ProjectWorkflowCommand::AddNode { workflow, .. }
        | ProjectWorkflowCommand::UpdateNode { workflow, .. }
        | ProjectWorkflowCommand::RemoveNode { workflow, .. }
        | ProjectWorkflowCommand::MoveNode { workflow, .. }
        | ProjectWorkflowCommand::Connect { workflow, .. }
        | ProjectWorkflowCommand::Disconnect { workflow, .. }
        | ProjectWorkflowCommand::SetViewport { workflow, .. }
        | ProjectWorkflowCommand::SetUnsafeParallel { workflow, .. } => Some(workflow),
    }
}

fn expected_revisions(
    command: &ProjectWorkflowCommand,
    project: &ProjectStoreHandle,
    target: &AggregateRef,
) -> Result<Vec<ExpectedRevision>, KernelProblem> {
    let expected = |aggregate: AggregateRef, pairs: &[(&str, u64)]| ExpectedRevision {
        aggregate,
        revisions: pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
    };
    Ok(match command {
        ProjectWorkflowCommand::Create {
            expected_collection_revision,
            ..
        } => vec![expected(
            target.clone(),
            &[(
                "workflow_collection_revision",
                *expected_collection_revision,
            )],
        )],
        ProjectWorkflowCommand::Delete {
            expected_collection_revision,
            expected_semantic_revision,
            expected_presentation_revision,
            ..
        } => vec![
            expected(
                AggregateRef::new(AggregateKind::Project, project.as_str().to_owned())
                    .map_err(|e| KernelProblem::Internal(e.to_string()))?,
                &[(
                    "workflow_collection_revision",
                    *expected_collection_revision,
                )],
            ),
            expected(
                target.clone(),
                &[
                    ("semantic_revision", *expected_semantic_revision),
                    ("presentation_revision", *expected_presentation_revision),
                ],
            ),
        ],
        ProjectWorkflowCommand::MoveNode {
            expected_presentation_revision,
            ..
        }
        | ProjectWorkflowCommand::SetViewport {
            expected_presentation_revision,
            ..
        } => vec![expected(
            target.clone(),
            &[("presentation_revision", *expected_presentation_revision)],
        )],
        ProjectWorkflowCommand::AddNode {
            expected_semantic_revision,
            ..
        }
        | ProjectWorkflowCommand::UpdateNode {
            expected_semantic_revision,
            ..
        }
        | ProjectWorkflowCommand::RemoveNode {
            expected_semantic_revision,
            ..
        }
        | ProjectWorkflowCommand::Connect {
            expected_semantic_revision,
            ..
        }
        | ProjectWorkflowCommand::Disconnect {
            expected_semantic_revision,
            ..
        }
        | ProjectWorkflowCommand::SetUnsafeParallel {
            expected_semantic_revision,
            ..
        } => vec![expected(
            target.clone(),
            &[("semantic_revision", *expected_semantic_revision)],
        )],
    })
}

fn project_workflow_payload(command: &ProjectWorkflowCommand) -> Result<Value, KernelProblem> {
    serde_json::to_value(match command {
        ProjectWorkflowCommand::Create { draft, .. } => serde_json::json!({"draft": draft}),
        ProjectWorkflowCommand::Delete { workflow, .. } => serde_json::json!({"workflow": workflow.as_str()}),
        ProjectWorkflowCommand::AddNode { workflow, node, .. } => serde_json::json!({"workflow": workflow.as_str(), "node": node}),
        ProjectWorkflowCommand::UpdateNode { workflow, node_handle, title, instructions, agent_instance_id, .. } => serde_json::json!({"workflow":workflow.as_str(),"node_handle":node_handle,"title":title,"instructions":instructions,"agent_instance_id":agent_instance_id}),
        ProjectWorkflowCommand::RemoveNode { workflow, node_handle, .. } => serde_json::json!({"workflow":workflow.as_str(),"node_handle":node_handle}),
        ProjectWorkflowCommand::MoveNode { workflow, node_handle, x, y, .. } => serde_json::json!({"workflow":workflow.as_str(),"node_handle":node_handle,"x":x,"y":y}),
        ProjectWorkflowCommand::Connect { workflow, upstream_node_handle, downstream_node_handle, .. } => serde_json::json!({"workflow":workflow.as_str(),"upstream_node_handle":upstream_node_handle,"downstream_node_handle":downstream_node_handle}),
        ProjectWorkflowCommand::Disconnect { workflow, edge_handle, .. } => serde_json::json!({"workflow":workflow.as_str(),"edge_handle":edge_handle}),
        ProjectWorkflowCommand::SetViewport { workflow, viewport, .. } => serde_json::json!({"workflow":workflow.as_str(),"viewport":viewport}),
        ProjectWorkflowCommand::SetUnsafeParallel { workflow, allow, .. } => serde_json::json!({"workflow":workflow.as_str(),"allow":allow}),
    }).map_err(|e| KernelProblem::InvalidEnvelope(e.to_string()))
}

fn project_workflow_effect(
    tx: &Transaction<'_>,
    project: &ProjectStoreHandle,
    command: &ProjectWorkflowCommand,
) -> Result<EffectOutput, CommandProblem> {
    use mf_agent::{ProjectWorkflowMutation as M, Store};
    let internal = workflow_domain_problem;
    let workflow_record = |handle: &WorkflowHandle| {
        Store::project_workflow_by_handle_tx(tx, handle.as_str())
            .map_err(internal)?
            .ok_or(CommandProblem::ResourceNotFound)
    };
    let (mutation, delta_type, delta_data) = match command {
        ProjectWorkflowCommand::Create {
            draft,
            expected_collection_revision,
            ..
        } => (
            M::Create {
                draft: draft.clone(),
                expected_collection_revision: *expected_collection_revision as i64,
            },
            "workflow.replace",
            serde_json::json!({"draft": draft}),
        ),
        ProjectWorkflowCommand::Delete {
            workflow,
            expected_collection_revision,
            expected_semantic_revision,
            expected_presentation_revision,
            ..
        } => (
            M::Delete {
                workflow_handle: workflow.as_str().to_owned(),
                expected_collection_revision: *expected_collection_revision as i64,
                expected_semantic_revision: *expected_semantic_revision as i64,
                expected_presentation_revision: *expected_presentation_revision as i64,
            },
            "workflow.delete",
            Value::Null,
        ),
        ProjectWorkflowCommand::MoveNode {
            workflow,
            node_handle,
            x,
            y,
            expected_presentation_revision,
            ..
        } => (
            M::SetNodePosition {
                workflow_handle: workflow.as_str().to_owned(),
                node_handle: node_handle.clone(),
                expected_presentation_revision: *expected_presentation_revision as i64,
                x: *x,
                y: *y,
            },
            "workflow.node_position_set",
            serde_json::json!({"node_handle":node_handle,"x":x,"y":y}),
        ),
        ProjectWorkflowCommand::SetViewport {
            workflow,
            viewport,
            expected_presentation_revision,
            ..
        } => (
            M::SetPresentation {
                workflow_handle: workflow.as_str().to_owned(),
                expected_presentation_revision: *expected_presentation_revision as i64,
                viewport_json: Some(
                    serde_json::to_string(viewport)
                        .map_err(|e| CommandProblem::InvalidEnvelope(e.to_string()))?,
                ),
                collapse_json: None,
                layout_json: None,
            },
            "workflow.viewport_set",
            serde_json::json!({"viewport":viewport}),
        ),
        semantic => {
            let workflow = workflow_of(semantic).ok_or_else(|| {
                CommandProblem::InvalidEnvelope("semantic workflow handle 缺失".into())
            })?;
            let before = workflow_record(workflow)?;
            let mut draft = mf_agent::ProjectWorkflowDraft {
                key: before.key.clone(),
                name: before.name.clone(),
                nodes: before.nodes.clone(),
                allow_unsafe_parallel: before.allow_unsafe_parallel,
            };
            let identities =
                Store::workflow_node_identities_tx(tx, workflow.as_str()).map_err(internal)?;
            let key_of = |handle: &str| {
                identities
                    .iter()
                    .find(|row| row.node_handle == handle)
                    .map(|row| row.node_key.clone())
                    .ok_or(CommandProblem::ResourceNotFound)
            };
            let (delta_type, data, expected) = match semantic {
                ProjectWorkflowCommand::AddNode {
                    node,
                    expected_semantic_revision,
                    ..
                } => {
                    draft.nodes.push(node.clone());
                    (
                        "workflow.add_node",
                        serde_json::json!({"node":node}),
                        *expected_semantic_revision,
                    )
                }
                ProjectWorkflowCommand::UpdateNode {
                    node_handle,
                    title,
                    instructions,
                    agent_instance_id,
                    expected_semantic_revision,
                    ..
                } => {
                    let key = key_of(node_handle)?;
                    let node = draft
                        .nodes
                        .iter_mut()
                        .find(|n| n.key == key)
                        .ok_or(CommandProblem::ResourceNotFound)?;
                    node.title = title.clone();
                    node.instructions = instructions.clone();
                    node.agent_instance_id = agent_instance_id.clone();
                    (
                        "workflow.update_node",
                        serde_json::json!({"node_handle":node_handle,"title":title,"instructions":instructions,"agent_instance_id":agent_instance_id}),
                        *expected_semantic_revision,
                    )
                }
                ProjectWorkflowCommand::RemoveNode {
                    node_handle,
                    expected_semantic_revision,
                    ..
                } => {
                    let key = key_of(node_handle)?;
                    let incident: Vec<String> =
                        Store::workflow_edge_identities_tx(tx, workflow.as_str())
                            .map_err(internal)?
                            .into_iter()
                            .filter(|edge| {
                                edge.upstream_node_key == key || edge.downstream_node_key == key
                            })
                            .map(|edge| edge.edge_handle)
                            .collect();
                    if draft.nodes.len() <= 1 {
                        return Err(CommandProblem::InvalidEnvelope(
                            "工作流至少保留一个节点".into(),
                        ));
                    }
                    draft.nodes.retain(|n| n.key != key);
                    for node in &mut draft.nodes {
                        node.deps.retain(|dep| dep != &key);
                    }
                    (
                        "workflow.remove_node",
                        serde_json::json!({"node_handle":node_handle,"incident_edge_handles":incident}),
                        *expected_semantic_revision,
                    )
                }
                ProjectWorkflowCommand::Connect {
                    upstream_node_handle,
                    downstream_node_handle,
                    expected_semantic_revision,
                    ..
                } => {
                    let up = key_of(upstream_node_handle)?;
                    let down = key_of(downstream_node_handle)?;
                    let node = draft
                        .nodes
                        .iter_mut()
                        .find(|n| n.key == down)
                        .ok_or(CommandProblem::ResourceNotFound)?;
                    if !node.deps.contains(&up) {
                        node.deps.push(up);
                    }
                    (
                        "workflow.connect",
                        serde_json::json!({"upstream_node_handle":upstream_node_handle,"downstream_node_handle":downstream_node_handle}),
                        *expected_semantic_revision,
                    )
                }
                ProjectWorkflowCommand::Disconnect {
                    edge_handle,
                    expected_semantic_revision,
                    ..
                } => {
                    let edges = Store::workflow_edge_identities_tx(tx, workflow.as_str())
                        .map_err(internal)?;
                    let edge = edges
                        .iter()
                        .find(|e| e.edge_handle == *edge_handle)
                        .ok_or(CommandProblem::ResourceNotFound)?;
                    let node = draft
                        .nodes
                        .iter_mut()
                        .find(|n| n.key == edge.downstream_node_key)
                        .ok_or(CommandProblem::ResourceNotFound)?;
                    node.deps.retain(|dep| dep != &edge.upstream_node_key);
                    (
                        "workflow.disconnect",
                        serde_json::json!({"edge_handle":edge_handle}),
                        *expected_semantic_revision,
                    )
                }
                ProjectWorkflowCommand::SetUnsafeParallel {
                    allow,
                    expected_semantic_revision,
                    ..
                } => {
                    draft.allow_unsafe_parallel = *allow;
                    (
                        "workflow.set_unsafe_parallel",
                        serde_json::json!({"allow":allow}),
                        *expected_semantic_revision,
                    )
                }
                _ => return Err(CommandProblem::InvalidEnvelope("命令轴错误".into())),
            };
            (
                M::ReplaceSemantic {
                    draft,
                    expected_semantic_revision: expected as i64,
                },
                delta_type,
                data,
            )
        }
    };
    let result = Store::apply_project_workflow_mutation_tx(tx, mutation).map_err(internal)?;
    let delta_data = match command {
        ProjectWorkflowCommand::Create { .. } => {
            let record = result
                .after
                .as_ref()
                .ok_or_else(|| CommandProblem::Internal("create result 缺失".into()))?;
            let nodes =
                Store::workflow_node_identities_tx(tx, &record.public_handle).map_err(internal)?;
            let edges =
                Store::workflow_edge_identities_tx(tx, &record.public_handle).map_err(internal)?;
            serde_json::json!({"workflow":record,"node_identities":nodes,"edge_identities":edges})
        }
        ProjectWorkflowCommand::AddNode { workflow, node, .. } => {
            let identity = Store::workflow_node_identities_tx(tx, workflow.as_str())
                .map_err(internal)?
                .into_iter()
                .find(|row| row.node_key == node.key)
                .ok_or(CommandProblem::ResourceNotFound)?;
            serde_json::json!({"node_handle":identity.node_handle,"node":node})
        }
        ProjectWorkflowCommand::Connect {
            workflow,
            upstream_node_handle,
            downstream_node_handle,
            ..
        } => {
            let nodes =
                Store::workflow_node_identities_tx(tx, workflow.as_str()).map_err(internal)?;
            let up = nodes
                .iter()
                .find(|row| row.node_handle == *upstream_node_handle)
                .ok_or(CommandProblem::ResourceNotFound)?;
            let down = nodes
                .iter()
                .find(|row| row.node_handle == *downstream_node_handle)
                .ok_or(CommandProblem::ResourceNotFound)?;
            let edge = Store::workflow_edge_identities_tx(tx, workflow.as_str())
                .map_err(internal)?
                .into_iter()
                .find(|edge| {
                    edge.upstream_node_key == up.node_key
                        && edge.downstream_node_key == down.node_key
                })
                .ok_or(CommandProblem::ResourceNotFound)?;
            serde_json::json!({"edge_handle":edge.edge_handle,"upstream_node_handle":upstream_node_handle,"downstream_node_handle":downstream_node_handle})
        }
        _ => delta_data,
    };
    workflow_mutation_output(project, command, result, delta_type, delta_data)
}

fn workflow_domain_problem(error: anyhow::Error) -> CommandProblem {
    if error
        .downcast_ref::<mf_agent::model::RevisionConflict>()
        .is_some()
    {
        return CommandProblem::RevisionConflict;
    }
    if let Some(validation) = error.downcast_ref::<mf_agent::WorkflowValidationErrors>() {
        let message = validation.to_string();
        if validation
            .iter()
            .any(|error| error.code() == mf_agent::WorkflowValidationCode::Cycle)
        {
            return CommandProblem::WorkflowCycle(message);
        }
        if validation
            .iter()
            .any(|error| error.code() == mf_agent::WorkflowValidationCode::UnknownDependency)
        {
            return CommandProblem::UnknownDependency(message);
        }
        return CommandProblem::ValidationFailed(message);
    }
    if let Some(mutation) = error.downcast_ref::<mf_agent::WorkflowMutationError>() {
        return match mutation {
            mf_agent::WorkflowMutationError::ScopeMismatch => CommandProblem::ResourceNotFound,
            mf_agent::WorkflowMutationError::Validation(message) => {
                CommandProblem::ValidationFailed(message.clone())
            }
        };
    }
    CommandProblem::Internal(format!("{error:#}"))
}

fn workflow_mutation_output(
    project: &ProjectStoreHandle,
    command: &ProjectWorkflowCommand,
    result: mf_agent::WorkflowMutationResult,
    delta_type: &str,
    delta_data: Value,
) -> Result<EffectOutput, CommandProblem> {
    let record = result
        .after
        .as_ref()
        .or(result.before.as_ref())
        .ok_or_else(|| CommandProblem::Internal("mutation 无 revision 记录".into()))?;
    let revisions = serde_json::json!({"semantic_revision":record.semantic_revision,"presentation_revision":record.presentation_revision,"workflow_collection_revision":result.collection_revision});
    if result.no_op {
        return Ok(EffectOutput {
            result_revisions: revisions,
            projections: Vec::new(),
        });
    }
    let workflow_handle = record.public_handle.clone();
    let base = result
        .before
        .as_ref()
        .map(|r| (r.semantic_revision, r.presentation_revision))
        .unwrap_or((0, 0));
    let aggregate = result
        .after
        .as_ref()
        .map(|r| (r.semantic_revision, r.presentation_revision))
        .unwrap_or(base);
    let mode = match command {
        ProjectWorkflowCommand::Create { .. } => "replace",
        ProjectWorkflowCommand::Delete { .. } => "tombstone",
        _ => "typed_delta",
    };
    let delta = if mode == "replace" {
        serde_json::json!({"mode":"replace","data":delta_data})
    } else if mode == "tombstone" {
        serde_json::json!({"mode":"tombstone"})
    } else {
        serde_json::json!({"mode":"typed_delta","delta_type":delta_type,"data":delta_data})
    };
    let mut projections = vec![ProjectionEffect {
        aggregate: Some(
            AggregateRef::new(AggregateKind::ProjectWorkflow, workflow_handle.clone())
                .map_err(|e| CommandProblem::Internal(e.to_string()))?,
        ),
        event_type: Some(delta_type.to_string()),
        projection_critical: true,
        payload: serde_json::json!({"base_revision":{"semantic_revision":base.0,"presentation_revision":base.1},"aggregate_revision":{"semantic_revision":aggregate.0,"presentation_revision":aggregate.1},"delta":delta}),
    }];
    if matches!(
        command,
        ProjectWorkflowCommand::Create { .. } | ProjectWorkflowCommand::Delete { .. }
    ) {
        projections.push(ProjectionEffect { aggregate: Some(AggregateRef::new(AggregateKind::Project,project.as_str().to_owned()).map_err(|e|CommandProblem::Internal(e.to_string()))?), event_type: Some("project.workflow_collection_changed".into()), projection_critical:true, payload:serde_json::json!({"base_revision":{"revision":result.collection_revision-1},"aggregate_revision":{"revision":result.collection_revision},"delta":{"mode":"typed_delta","delta_type":"project.workflow_collection_changed","data":{"workflow_handle":workflow_handle}}}) });
    }
    Ok(EffectOutput {
        result_revisions: revisions,
        projections,
    })
}

/// 封闭命令 → L-CMD 目标事务内的业务效果段。任意 SQL 只存在于
/// kernel 内部的 effect 编译器,transport/UI 无法提交。
fn workflow_rename_effect(
    tx: &Transaction<'_>,
    workflow_handle: &str,
    new_name: &str,
) -> Result<EffectOutput, CommandProblem> {
    let internal = |error: rusqlite::Error| CommandProblem::Internal(error.to_string());
    let row = tx
        .query_row(
            "SELECT name, semantic_revision, presentation_revision
             FROM project_workflows WHERE public_handle = ?1",
            [workflow_handle],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(internal)?;
    let Some((current_name, semantic, presentation)) = row else {
        return Err(CommandProblem::ResourceNotFound);
    };
    let (semantic, presentation) = (
        u64::try_from(semantic).map_err(|_| CommandProblem::Internal("revision 溢出".into()))?,
        u64::try_from(presentation)
            .map_err(|_| CommandProblem::Internal("revision 溢出".into()))?,
    );
    if current_name == new_name {
        // 幂等 no-op:receipt 照写,无投影变化 → 不产生 outbox 事件。
        return Ok(EffectOutput {
            result_revisions: serde_json::json!({
                "semantic_revision": semantic,
                "presentation_revision": presentation,
            }),
            projections: Vec::new(),
        });
    }
    let changed = tx
        .execute(
            "UPDATE project_workflows
             SET name = ?2, presentation_revision = presentation_revision + 1, updated_at = ?3
             WHERE public_handle = ?1",
            params![workflow_handle, new_name, mf_agent::store::now(),],
        )
        .map_err(internal)?;
    if changed != 1 {
        return Err(CommandProblem::Internal(
            "rename UPDATE 未命中唯一行".into(),
        ));
    }
    let revisions = serde_json::json!({
        "semantic_revision": semantic,
        "presentation_revision": presentation + 1,
    });
    let projection = serde_json::json!({
        "base_revision": {
            "semantic_revision": semantic,
            "presentation_revision": presentation,
        },
        "aggregate_revision": {
            "semantic_revision": semantic,
            "presentation_revision": presentation + 1,
        },
        "delta": {
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": { "name": new_name },
        },
    });
    Ok(EffectOutput {
        result_revisions: revisions,
        projections: vec![crate::command::ProjectionEffect::primary(projection)],
    })
}

fn revision_vector_of(value: &Value) -> Result<RevisionVector, KernelProblem> {
    let semantic_revision = value
        .get("semantic_revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| KernelProblem::Internal("result_revisions 缺少 semantic_revision".into()))?;
    let presentation_revision = value
        .get("presentation_revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            KernelProblem::Internal("result_revisions 缺少 presentation_revision".into())
        })?;
    Ok(RevisionVector {
        semantic_revision,
        presentation_revision,
    })
}

// ---------------------------------------------------------------------------
// opaque in-process assembly + legacy adapter
// ---------------------------------------------------------------------------

/// Bridge A 前的进程内 owner assembly。隐藏 service/key/具体 kernel，只向
/// AppCtx 暴露 Project 生命周期登记与已经绑定 facade 的 legacy client。
pub struct InProcessKernelRuntime {
    kernel: Arc<InProcessCoreKernel>,
    _owner: Option<Arc<CoreOwnerLock>>,
}

/// Core-owned Project Store registration。legacy Orchestrator 只借用 store
/// clone；handle/path 映射与 recovery 均留在 runtime 内。
pub struct InProcessProject {
    handle: ProjectStoreHandle,
    store: Arc<Store>,
}

impl InProcessProject {
    pub fn handle(&self) -> &ProjectStoreHandle {
        &self.handle
    }

    pub fn legacy_store(&self) -> Arc<Store> {
        self.store.clone()
    }
}

impl InProcessKernelRuntime {
    pub fn acquire_default(
        build: &str,
        client_id: ClientId,
        principal: Principal,
    ) -> Result<(Arc<Self>, LegacyKernelClient), KernelProblem> {
        let service_path = crate::service_schema::service_db_path();
        let owner = Arc::new(
            CoreOwnerLock::acquire(OwnerLockSetup::platform(&service_path, build, 0))
                .map_err(|error| KernelProblem::ServiceUnavailable(error.to_string()))?,
        );
        let service = ServiceStore::open(&service_path)
            .map_err(|error| KernelProblem::ServiceUnavailable(format!("{error:#}")))?;
        let key = ServiceIdempotencyKey::load_or_create().map_err(KernelProblem::from)?;
        let kernel = Arc::new(InProcessCoreKernel::new(service, key));
        let epoch = kernel.grant_controller(&client_id, &principal)?;
        let client = LegacyKernelClient::new(kernel.clone(), principal, client_id, epoch);
        Ok((
            Arc::new(Self {
                kernel,
                _owner: Some(owner),
            }),
            client,
        ))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        service: Arc<ServiceStore>,
        key: ServiceIdempotencyKey,
        client_id: ClientId,
        principal: Principal,
    ) -> Result<(Arc<Self>, LegacyKernelClient), KernelProblem> {
        let kernel = Arc::new(InProcessCoreKernel::new(service, key));
        let epoch = kernel.grant_controller(&client_id, &principal)?;
        let client = LegacyKernelClient::new(kernel.clone(), principal, client_id, epoch);
        Ok((
            Arc::new(Self {
                kernel,
                _owner: None,
            }),
            client,
        ))
    }

    pub fn open_project(&self, root: &Path) -> Result<InProcessProject, KernelProblem> {
        let store = Store::open(&mf_agent::project_db_path(root))
            .map_err(|error| KernelProblem::ServiceUnavailable(format!("{error:#}")))?;
        let handle = self.kernel.register_project_store(root, store.clone())?;
        Ok(InProcessProject { handle, store })
    }

    pub fn unregister_project_store(
        &self,
        project: &ProjectStoreHandle,
    ) -> Result<(), KernelProblem> {
        self.kernel.unregister_project_store(project)
    }

    pub fn prepare_project_close(
        &self,
        project: &ProjectStoreHandle,
    ) -> Result<ProjectCloseToken, KernelProblem> {
        self.kernel.prepare_project_close(project)
    }

    pub fn finalize_project_close(&self, token: ProjectCloseToken) {
        self.kernel.finalize_project_close(token);
    }

    /// WebGateway/diagnostics adapter 的生产可观测 seam；不包含 payload、
    /// Project 路径或 Secret。
    pub fn projection_diagnostics(&self) -> crate::projection::ProjectionDiagnostics {
        self.kernel.projections.stats()
    }
}

// ---------------------------------------------------------------------------
// in-process legacy adapter(GPUI → facade 的唯一编译点)
// ---------------------------------------------------------------------------

/// legacy GPUI 的 facade 客户端:对 Store 只做只读定位(workflow_key →
/// 持久 handle + expected revision),写只经 [`CoreKernel::dispatch`]。
/// 拆进程后由 `mf.legacy-transport.v1` 承载同样的请求形状。
pub struct LegacyKernelClient {
    kernel: Arc<InProcessCoreKernel>,
    principal: Principal,
    client_id: ClientId,
    controller_epoch: AtomicU64,
}

impl LegacyKernelClient {
    pub(crate) fn new(
        kernel: Arc<InProcessCoreKernel>,
        principal: Principal,
        client_id: ClientId,
        controller_epoch: u64,
    ) -> Self {
        Self {
            kernel,
            principal,
            client_id,
            controller_epoch: AtomicU64::new(controller_epoch),
        }
    }

    /// Controller 重授/接管后刷新本地 epoch。
    pub fn set_controller_epoch(&self, epoch: u64) {
        self.controller_epoch.store(epoch, Ordering::SeqCst);
    }

    fn controller_epoch(&self) -> u64 {
        self.controller_epoch.load(Ordering::SeqCst)
    }

    /// `workflow.rename` tracer：legacy key 解析留在私有 adapter；公开
    /// CoreKernel facade 始终只接受 opaque WorkflowHandle。
    pub fn rename_workflow(
        &self,
        project: &ProjectStoreHandle,
        workflow_key: &str,
        new_name: &str,
    ) -> Result<KernelOutcome, KernelProblem> {
        let name = new_name.trim();
        if name.is_empty() {
            return Err(KernelProblem::InvalidEnvelope("工作流名称不能为空".into()));
        }
        let (workflow, expected_presentation_revision) =
            self.kernel.legacy_workflow_locator(project, workflow_key)?;
        self.kernel.dispatch(KernelCommandRequest::new(
            CommandId::new(),
            self.client_id.clone(),
            self.principal.clone(),
            self.controller_epoch(),
            KernelCommand::workflow_rename(
                project.clone(),
                workflow,
                name,
                expected_presentation_revision,
            ),
        ))
    }

    pub fn dispatch_project_workflow(
        &self,
        command: ProjectWorkflowCommand,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.kernel.dispatch(KernelCommandRequest::new(
            CommandId::new(),
            self.client_id.clone(),
            self.principal.clone(),
            self.controller_epoch(),
            KernelCommand::ProjectWorkflow(command),
        ))
    }

    pub fn workflow_snapshot(
        &self,
        project: &ProjectStoreHandle,
        workflow_key: &str,
    ) -> Result<SnapshotEnvelope, KernelProblem> {
        let (workflow, _) = self.kernel.legacy_workflow_locator(project, workflow_key)?;
        self.kernel.snapshot(SnapshotQuery::Workflow {
            project: project.clone(),
            workflow,
        })
    }
}
