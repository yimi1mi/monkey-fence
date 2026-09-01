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
    CommandType, EffectOutput, FaultPoint, ServiceIdempotencyKey, TargetDatabase,
};
use crate::handles::{
    AggregateKind, AggregateRef, ClientId, CommandId, CommandTarget, ExpectedRevision, Principal,
    ProjectStoreHandle, SessionHandle, TargetStoreKind, WorkflowHandle,
};
use crate::lease::{CommandAuthorizer, CommandPermit, LeaseCheck};
use crate::project_registry::ServiceStore;
use crate::projection::{
    EventCursor, EventJournal, EventSubscription, RevisionVector, SnapshotData, SnapshotEnvelope,
    SnapshotQuery, WorkflowSnapshotData, SNAPSHOT_SCHEMA,
};
use crate::reconcile::reconcile_startup;
use crate::shutdown::{ShutdownAssessment, ShutdownIntent};
use crate::singleton::{CoreOwnerLock, OwnerLockSetup};
use mf_agent::store::Store;
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
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
            if expected.aggregate.kind != AggregateKind::ProjectWorkflow {
                return Err(CommandProblem::InvalidEnvelope(
                    "T2a tracer 只支持 project_workflow expected revision".into(),
                ));
            }
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
                            "未知 revision 轴:{other}"
                        )))
                    }
                };
                if actual != *expected_revision as i64 {
                    return Err(CommandProblem::RevisionConflict);
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
    /// L-CMD commit→journal publication 与 Snapshot(cursor+Store read)共用。
    publication: Mutex<()>,
    journal: Arc<EventJournal>,
}

impl InProcessCoreKernel {
    pub(crate) fn new(service: Arc<ServiceStore>, idempotency_key: ServiceIdempotencyKey) -> Self {
        Self::with_journal(service, idempotency_key, EventJournal::new())
    }

    #[cfg(test)]
    pub(crate) fn new_with_journal_limits(
        service: Arc<ServiceStore>,
        idempotency_key: ServiceIdempotencyKey,
        max_events: usize,
        max_bytes: usize,
    ) -> Self {
        Self::with_journal(
            service,
            idempotency_key,
            EventJournal::for_test(max_events, max_bytes),
        )
    }

    fn with_journal(
        service: Arc<ServiceStore>,
        idempotency_key: ServiceIdempotencyKey,
        journal: EventJournal,
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
            publication: Mutex::new(()),
            journal: Arc::new(journal),
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
        self.projects.write().insert(
            project.as_str().to_string(),
            ProjectRegistration { store, target },
        );
        Ok(project)
    }

    pub(crate) fn unregister_project_store(&self, project: &ProjectStoreHandle) {
        self.projects.write().remove(project.as_str());
    }

    fn reconcile_registered_projects(
        &self,
        newly_opened: &ProjectStoreHandle,
    ) -> Result<(), KernelProblem> {
        let registrations: Vec<(String, TargetDatabase)> = self
            .projects
            .read()
            .iter()
            .map(|(handle, registration)| (handle.clone(), registration.target.clone()))
            .collect();
        let _barrier = self.publication.lock();
        // 已在当前 epoch 服务的 Project 先正常发布；新打开 Project 的 pending
        // outbox 才属于旧进程，由 startup reconcile 标记 reconciled。
        for (handle, target) in &registrations {
            if handle != newly_opened.as_str() {
                self.journal.publish_pending(target)?;
            }
        }
        let targets: Vec<TargetDatabase> = registrations
            .into_iter()
            .map(|(_, target)| target)
            .collect();
        reconcile_startup(&self.service, &targets, chrono::Utc::now())
            .map_err(KernelProblem::from)?;
        Ok(())
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
        let _barrier = self.publication.lock();
        self.journal.cursor()
    }

    /// 崩溃恢复:以 target receipt 为权威终结 intent 并补发事件;
    /// 绝不重放业务写(`#21/#22` reconcile 语义)。
    #[cfg(test)]
    pub(crate) fn reconcile_command(
        &self,
        project: &ProjectStoreHandle,
        command_id: &CommandId,
    ) -> Result<ReconcileOutcome, KernelProblem> {
        let registration = self.project_registration(project)?;
        let _barrier = self.publication.lock();
        let outcome = self
            .coordinator
            .reconcile(command_id, &registration.target)
            .map_err(KernelProblem::from)?;
        self.journal.publish_pending(&registration.target)?;
        Ok(outcome)
    }

    #[cfg(test)]
    pub(crate) fn publish_pending_for_test(
        &self,
        project: &ProjectStoreHandle,
    ) -> Result<(), KernelProblem> {
        let registration = self.project_registration(project)?;
        let _barrier = self.publication.lock();
        self.journal.publish_pending(&registration.target)
    }

    /// 契约测试故障注入缝隙:模拟 dispatch 在指定线性化点崩溃。
    #[cfg(test)]
    pub(crate) fn dispatch_rename_with_fault(
        &self,
        request: KernelCommandRequest,
        fault: Option<FaultPoint>,
    ) -> Result<KernelOutcome, KernelProblem> {
        match &request.command {
            KernelCommand::WorkflowRename(_) => self.dispatch_workflow_rename(request, fault),
        }
    }

    fn project_registration(
        &self,
        project: &ProjectStoreHandle,
    ) -> Result<ProjectRegistration, KernelProblem> {
        self.projects
            .read()
            .get(project.as_str())
            .cloned()
            .ok_or(KernelProblem::ResourceNotFound)
    }

    fn legacy_workflow_locator(
        &self,
        project: &ProjectStoreHandle,
        workflow_key: &str,
    ) -> Result<(WorkflowHandle, u64), KernelProblem> {
        let registration = self.project_registration(project)?;
        let _barrier = self.publication.lock();
        let row = registration
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
            .ok_or(KernelProblem::ResourceNotFound)?;
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
    ) -> Result<KernelOutcome, KernelProblem> {
        let KernelCommand::WorkflowRename(command) = &request.command;
        let project = &command.project;
        let workflow = &command.workflow;
        let name = &command.name;
        let expected_presentation_revision = &command.expected_presentation_revision;
        let name = name.trim();
        if name.is_empty() {
            return Err(KernelProblem::InvalidEnvelope("工作流名称不能为空".into()));
        }
        let registration = self.project_registration(project)?;
        let _barrier = self.publication.lock();
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
        )?;
        // L-PUBLISH:目标事务 commit + receipt + outbox 之后才对外可见。
        self.journal.publish_pending(&registration.target)?;
        let CommandOutcome::Applied {
            result_revisions,
            replayed,
        } = outcome;
        Ok(KernelOutcome::Applied {
            revisions: revision_vector_of(&result_revisions)?,
            replayed,
        })
    }

    fn workflow_snapshot(&self, query: SnapshotQuery) -> Result<SnapshotEnvelope, KernelProblem> {
        let SnapshotQuery::Workflow { project, workflow } = &query;
        let registration = self.project_registration(project)?;
        // publication barrier 覆盖 cursor + Store read：不可能返回新 Store
        // 状态配旧 through_seq。
        let _barrier = self.publication.lock();
        let cursor = self.journal.cursor();
        let row = registration
            .store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT public_handle, name, semantic_revision, presentation_revision
                     FROM project_workflows WHERE public_handle=?1",
                    [workflow.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()
                .map_err(anyhow::Error::from)
            })
            .map_err(|error| KernelProblem::Internal(format!("{error:#}")))?;
        let Some((workflow_handle, name, semantic, presentation)) = row else {
            return Err(KernelProblem::ResourceNotFound);
        };
        let revisions = RevisionVector {
            semantic_revision: u64::try_from(semantic)
                .map_err(|_| KernelProblem::Internal("semantic_revision 溢出".into()))?,
            presentation_revision: u64::try_from(presentation)
                .map_err(|_| KernelProblem::Internal("presentation_revision 溢出".into()))?,
        };
        Ok(SnapshotEnvelope {
            schema: SNAPSHOT_SCHEMA,
            server_instance_id: self.journal.server_instance_id().clone(),
            cursor,
            data: SnapshotData::Workflow(WorkflowSnapshotData {
                workflow: WorkflowHandle::parse(workflow_handle)
                    .map_err(|error| KernelProblem::Internal(error.to_string()))?,
                name,
                revisions,
            }),
        })
    }

    fn assess_shutdown(&self, _intent: ShutdownIntent) -> ShutdownAssessment {
        let mut assessment = ShutdownAssessment::default();
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
            KernelCommand::WorkflowRename(_) => self.dispatch_workflow_rename(request, None),
        }
    }

    fn snapshot(&self, query: SnapshotQuery) -> Result<SnapshotEnvelope, KernelProblem> {
        self.workflow_snapshot(query)
    }

    fn subscribe_events(&self, cursor: EventCursor) -> Result<EventSubscription, KernelProblem> {
        self.journal.subscribe(&cursor)
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
            projection: None,
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
        projection: Some(projection),
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
        if let Err(error) = self.kernel.reconcile_registered_projects(&handle) {
            self.kernel.unregister_project_store(&handle);
            return Err(error);
        }
        Ok(InProcessProject { handle, store })
    }

    pub fn unregister_project_store(&self, project: &ProjectStoreHandle) {
        self.kernel.unregister_project_store(project);
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
}
