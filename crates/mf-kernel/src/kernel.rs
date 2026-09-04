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

use serde::{Deserialize, Serialize};

use crate::command::ReconcileOutcome;
use crate::command::{
    CommandCoordinator, CommandEnvelope, CommandOutcome, CommandPayload, CommandProblem,
    CommandType, EffectOutput, FaultPoint, ProjectionEffect, ServiceIdempotencyKey, TargetDatabase,
};
use crate::handles::{
    AgentRunHandle, AgentSessionHandle, AggregateKind, AggregateRef, ClientId, CommandId,
    CommandTarget, ExpectedRevision, Principal, ProjectStoreHandle, SessionHandle, StepHandle,
    TargetStoreKind, WorkflowHandle, WorkflowRunHandle,
};
use crate::lease::{CommandAuthorizer, CommandPermit, LeaseCheck};
use crate::operation::{
    durable_payload, operation_of, steps_of, OperationAcceptFaultPoint, OperationCoordinator,
    OperationFaultPoint, OperationHandle, OperationOutcome, OperationRecord, OperationState,
};
use crate::project_registry::{
    RunCapabilityKey, RunCapabilityRegistration, RunCapabilityResolution, RunCapabilityState,
    ServiceStore,
};
use crate::projection::{
    EventCursor, EventSubscription, ProjectionHub, RevisionVector, SnapshotData, SnapshotEnvelope,
    SnapshotQuery, WorkflowSnapshotData, SNAPSHOT_SCHEMA,
};
use crate::reconcile::reconcile_startup;
use crate::run_lifecycle::{
    RunActionDelivery, RunLifecyclePort, RunPreparation, DURABLE_RUN_ACTIONS_SCHEMA,
};
use crate::shutdown::{ShutdownAssessment, ShutdownIntent};
use crate::singleton::{CoreOwnerLock, OwnerLockSetup};
use mf_agent::store::Store;
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use rusqlite::OptionalExtension;
use rusqlite::{params, Transaction};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Weak};
use std::thread;
use std::time::Duration;
use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// facade DTO:封闭命令族 / outcome / problem(§2.2/§7.4/§7.5)
// ---------------------------------------------------------------------------

/// facade 层封闭命令枚举。新增命令 = 显式扩枚举 + 编译到 T1 effect,
/// transport/UI 不能自造命令或 payload 形状。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum KernelCommand {
    /// `workflow.rename`(§7.4:rename 归入 presentation 轴):
    /// 经 Project v7 持久 handle 定位,只推进 presentation revision;
    /// semantic/collection revision 不动。同名重命名是幂等 no-op
    /// (有 receipt、无事件)。
    WorkflowRename(WorkflowRenameCommand),
    ProjectWorkflow(ProjectWorkflowCommand),
    /// Workflow Run 命令先冻结 facade/envelope/authorization 契约。
    /// 在 Orchestrator-backed lifecycle port 注册前 dispatch 恒 fail-closed，
    /// 不会写 intent/receipt/outbox，更不会丢弃 RunAction。
    WorkflowRun(WorkflowRunCommand),
}

/// 引用一个已存在聚合及客户端最后观察到的单轴 revision。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedHandle<H> {
    pub handle: H,
    pub revision: u64,
}

/// Run 命令的全量并发前提。Workflow Run 由命令字段定位，
/// 这里只保留它的 revision；Step/Agent Run/Agent Session 必须
/// 显式列出，不允许 kernel 在 envelope 外暗自补 expected。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunExpected {
    pub workflow_run_revision: u64,
    pub steps: Vec<VersionedHandle<StepHandle>>,
    pub agent_runs: Vec<VersionedHandle<AgentRunHandle>>,
    pub agent_sessions: Vec<VersionedHandle<AgentSessionHandle>>,
}

impl WorkflowRunExpected {
    pub fn only_run(workflow_run_revision: u64) -> Self {
        Self {
            workflow_run_revision,
            steps: Vec::new(),
            agent_runs: Vec::new(),
            agent_sessions: Vec::new(),
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkflowRunCommand {
    /// 启动仍需 Operation saga；本层只冻结「必须是 Core 已确认
    /// semantic revision」的授权契约。
    Start {
        project: ProjectStoreHandle,
        workflow: WorkflowHandle,
        goal: String,
        expected_semantic_revision: u64,
    },
    Cancel {
        project: ProjectStoreHandle,
        workflow_run: WorkflowRunHandle,
        expected: WorkflowRunExpected,
    },
    RetryStep {
        project: ProjectStoreHandle,
        workflow_run: WorkflowRunHandle,
        step: StepHandle,
        mode: mf_agent::RetryMode,
        expected: WorkflowRunExpected,
    },
    SkipStep {
        project: ProjectStoreHandle,
        workflow_run: WorkflowRunHandle,
        step: StepHandle,
        expected: WorkflowRunExpected,
    },
    /// question 是 Step 下的内部行；事务内要求恰有一个 open
    /// question，不把 question rowid 暴露到 facade。
    Respond {
        project: ProjectStoreHandle,
        workflow_run: WorkflowRunHandle,
        step: StepHandle,
        /// Project Store 内部 correlation，只在 Core facade 内流转并进入
        /// semantic digest；外部 Snapshot 不序列化该 rowid。
        question_id: i64,
        answer: String,
        expected: WorkflowRunExpected,
    },
    Settle {
        project: ProjectStoreHandle,
        workflow_run: WorkflowRunHandle,
        step: StepHandle,
        agent_run: AgentRunHandle,
        settlement: mf_agent::Settlement,
        expected: WorkflowRunExpected,
    },
}

/// Run 命令携带 goal、回答和 Settlement 正文；这些内容可能包含用户隐私、
/// API 凭据或本次 capability token，不能被派生 Debug 带入日志/错误链。
impl std::fmt::Debug for WorkflowRunCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start {
                project,
                workflow,
                expected_semantic_revision,
                ..
            } => f
                .debug_struct("Start")
                .field("project", project)
                .field("workflow", workflow)
                .field("goal", &"<redacted>")
                .field("expected_semantic_revision", expected_semantic_revision)
                .finish(),
            Self::Cancel {
                project,
                workflow_run,
                expected,
            } => f
                .debug_struct("Cancel")
                .field("project", project)
                .field("workflow_run", workflow_run)
                .field("expected", expected)
                .finish(),
            Self::RetryStep {
                project,
                workflow_run,
                step,
                mode,
                expected,
            } => f
                .debug_struct("RetryStep")
                .field("project", project)
                .field("workflow_run", workflow_run)
                .field("step", step)
                .field("mode", mode)
                .field("expected", expected)
                .finish(),
            Self::SkipStep {
                project,
                workflow_run,
                step,
                expected,
            } => f
                .debug_struct("SkipStep")
                .field("project", project)
                .field("workflow_run", workflow_run)
                .field("step", step)
                .field("expected", expected)
                .finish(),
            Self::Respond {
                project,
                workflow_run,
                step,
                question_id,
                expected,
                ..
            } => f
                .debug_struct("Respond")
                .field("project", project)
                .field("workflow_run", workflow_run)
                .field("step", step)
                .field("question_id", question_id)
                .field("answer", &"<redacted>")
                .field("expected", expected)
                .finish(),
            Self::Settle {
                project,
                workflow_run,
                step,
                agent_run,
                settlement,
                expected,
            } => f
                .debug_struct("Settle")
                .field("project", project)
                .field("workflow_run", workflow_run)
                .field("step", step)
                .field("agent_run", agent_run)
                .field("settlement_kind", &settlement.kind_str())
                .field("settlement_payload", &"<redacted>")
                .field("expected", expected)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
            Self::WorkflowRun(command) => command.command_type(),
        }
    }
}

impl WorkflowRunCommand {
    pub const fn command_type(&self) -> CommandType {
        match self {
            Self::Start { .. } => CommandType::WorkflowRun,
            Self::Cancel { .. } => CommandType::WorkflowRunCancel,
            Self::RetryStep { .. } => CommandType::WorkflowRetryStep,
            Self::SkipStep { .. } => CommandType::WorkflowSkipStep,
            Self::Respond { .. } => CommandType::WorkflowRespond,
            Self::Settle { .. } => CommandType::WorkflowSettle,
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

    pub fn client_id(&self) -> &ClientId {
        &self.client_id
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    pub fn controller_epoch(&self) -> u64 {
        self.controller_epoch
    }
}

/// dispatch 结果。`202 accepted`(Operation)属后续 ticket;T2a 只有
/// 同步 applied。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelOutcome {
    Accepted {
        operation_handle: crate::operation::OperationHandle,
    },
    Applied {
        revisions: RevisionVector,
        /// true = 命中既有 target receipt 的幂等重放,effect 未重放。
        replayed: bool,
    },
    RunApplied {
        /// 命令 target aggregate 的最终 scalar revision。
        revision: u64,
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

impl From<crate::operation::OperationProblem> for KernelProblem {
    fn from(problem: crate::operation::OperationProblem) -> Self {
        match problem {
            crate::operation::OperationProblem::Command(problem) => problem.into(),
            crate::operation::OperationProblem::OperationNotFound => Self::ResourceNotFound,
            crate::operation::OperationProblem::CommandIdReused
            | crate::operation::OperationProblem::PlanConflict(_) => Self::CommandIdReused,
            crate::operation::OperationProblem::InvalidPlan(message) => {
                Self::ValidationFailed(message)
            }
            other => Self::Internal(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// attach_terminal(T2f:mf-terminal shim 委托 TerminalHost)
// ---------------------------------------------------------------------------

/// 终端 attach 请求(T3 冻结完整形状前的最小占位;shim 忽略 `after_seq`,
/// replay/seq 语义随 mf-terminal 管线落地)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalAttach {
    pub after_seq: u64,
}

pub use mf_terminal::channel::{
    TerminalChannel, TerminalHost, TerminalProblem, TerminalSessionRef,
};

// ---------------------------------------------------------------------------
// CoreKernel trait(§2.2:唯一深模块缝隙)
// ---------------------------------------------------------------------------

/// 所有调用方(WebGateway、legacy GPUI adapter、launcher/tray IPC、测试
/// harness)只允许经这些方法与 Core 交互;字段一律私有。前五个是数据面;
/// `grant_controller`/`controller_epoch` 是 Web bootstrap/takeover 的
/// controller 授予面(L-TAKEOVER 落点,epoch 单调旋转)。
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
    /// 授予 Controller lease(新 controller 使旧 epoch 立即失效)并返回
    /// 新 epoch。Web exchange/takeover 调用;client_id/principal 非空
    /// 字符串,dispatch 时逐字复验。
    fn grant_controller(&self, client_id: &str, principal: &str) -> Result<u64, KernelProblem>;
    /// 当前 controller epoch(Web takeover CAS 的观察对象)。
    fn controller_epoch(&self) -> u64;
    /// 把项目根目录挂载进 Core(打开/初始化 Project Store 并注册投影
    /// target),返回 opaque project handle。多项目同时在线的入口。
    fn attach_project(&self, root: &std::path::Path) -> Result<String, KernelProblem>;
    /// 卸载项目(handle 为 opaque 形态);未注册/关闭中 → not found。
    fn detach_project(&self, project_handle: &str) -> Result<(), KernelProblem>;
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

/// MF_RUN_TOKEN 只授权一个 Project 内的一个 Agent Run 执行 settle。
/// 不持有/读取 Controller principal、client 或 epoch；token 只保存在
/// Zeroizing 内存中，并且只作为 target transaction 的绑定参数。
struct RunControlAuthorizer {
    token: Zeroizing<String>,
    workflow_run: WorkflowRunHandle,
    step: StepHandle,
    agent_run: AgentRunHandle,
    target: AggregateRef,
    command_type: CommandType,
}

struct RunControlPermit;

impl CommandAuthorizer for RunControlAuthorizer {
    fn acquire<'a>(
        &'a self,
        tx: &Transaction<'_>,
        check: &LeaseCheck<'_>,
    ) -> Result<Box<dyn CommandPermit + 'a>, CommandProblem> {
        if check.command_type != self.command_type
            || check.target.aggregate != self.target
            || check.root_epoch.is_some()
        {
            return Err(CommandProblem::InvalidEnvelope(
                "run capability 只能授权其绑定 RunControl target".into(),
            ));
        }
        let binding = tx
            .query_row(
                "SELECT t.public_handle, s.public_handle
                 FROM agent_runs r
                 JOIN agent_tasks t ON t.id=r.task_id
                 JOIN steps s ON s.id=r.step_id
                 WHERE r.public_handle=?1 AND r.capability_token=?2",
                params![self.agent_run.as_str(), self.token.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| CommandProblem::Internal(error.to_string()))?;
        let Some((workflow_run, step)) = binding else {
            return Err(CommandProblem::ResourceNotFound);
        };
        if workflow_run != self.workflow_run.as_str() || step != self.step.as_str() {
            return Err(CommandProblem::ResourceNotFound);
        }
        Ok(Box::new(RunControlPermit))
    }
}

impl CommandPermit for RunControlPermit {
    fn validate_expected(
        &self,
        tx: &Transaction<'_>,
        check: &LeaseCheck<'_>,
    ) -> Result<(), CommandProblem> {
        // RunControl 的 expected 是 Kernel 在 authority 路由后冻结的内部
        // CAS，不属于 semantic digest。新 effect 在同一 target IMMEDIATE
        // transaction 再验证；receipt replay 刻意跳过旧 expected。
        for expected in check.expected {
            let table = match expected.aggregate.kind {
                AggregateKind::WorkflowRun => "agent_tasks",
                AggregateKind::Step => "steps",
                AggregateKind::AgentRun => "agent_runs",
                AggregateKind::AgentSession => "agent_sessions",
                _ => {
                    return Err(CommandProblem::InvalidEnvelope(
                        "run capability expected 含非 Run 聚合".into(),
                    ))
                }
            };
            let sql = format!("SELECT revision FROM {table} WHERE public_handle=?1");
            let actual = tx
                .query_row(&sql, [expected.aggregate.handle.as_str()], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()
                .map_err(|error| CommandProblem::Internal(error.to_string()))?
                .ok_or(CommandProblem::ResourceNotFound)?;
            for (axis, revision) in &expected.revisions {
                if axis != "revision" || actual != *revision as i64 {
                    return Err(CommandProblem::RevisionConflict);
                }
            }
        }
        Ok(())
    }
}

/// Controller takeover 后的 Cancel recovery 只接受已经存在且未 finalized
/// 的 durable fence。它不能创建新 fence，也不能授权其它 command。
struct CancelRecoveryAuthorizer {
    command_id: String,
    task_id: i64,
}

struct CancelRecoveryPermit;

impl CommandAuthorizer for CancelRecoveryAuthorizer {
    fn acquire<'a>(
        &'a self,
        tx: &Transaction<'_>,
        _check: &LeaseCheck<'_>,
    ) -> Result<Box<dyn CommandPermit + 'a>, CommandProblem> {
        let exists = tx
            .query_row(
                "SELECT 1 FROM run_cancel_fence
                 WHERE command_id=?1 AND task_id=?2 AND state!='finalized'",
                params![self.command_id, self.task_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| CommandProblem::Internal(error.to_string()))?
            .is_some();
        if !exists {
            return Err(CommandProblem::ControllerLeaseExpired);
        }
        Ok(Box::new(CancelRecoveryPermit))
    }
}

impl CommandPermit for CancelRecoveryPermit {
    fn validate_expected(
        &self,
        _tx: &Transaction<'_>,
        _check: &LeaseCheck<'_>,
    ) -> Result<(), CommandProblem> {
        // expected 与 target set 已在 reserve fence 的同一事务冻结；v10
        // triggers 阻止相关状态漂移。recovery 不接受调用方新 expected。
        Ok(())
    }
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
                                )));
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
                AggregateKind::WorkflowRun
                | AggregateKind::Step
                | AggregateKind::AgentRun
                | AggregateKind::AgentSession => {
                    let table = match expected.aggregate.kind {
                        AggregateKind::WorkflowRun => "agent_tasks",
                        AggregateKind::Step => "steps",
                        AggregateKind::AgentRun => "agent_runs",
                        AggregateKind::AgentSession => "agent_sessions",
                        _ => unreachable!(),
                    };
                    let sql = format!("SELECT revision FROM {table} WHERE public_handle = ?1");
                    let actual = tx
                        .query_row(&sql, [expected.aggregate.handle.as_str()], |row| {
                            row.get::<_, i64>(0)
                        })
                        .optional()
                        .map_err(|error| CommandProblem::Internal(error.to_string()))?
                        .ok_or(CommandProblem::ResourceNotFound)?;
                    for (axis, expected_revision) in &expected.revisions {
                        if axis != "revision" {
                            return Err(CommandProblem::InvalidEnvelope(format!(
                                "未知 {} revision 轴:{axis}",
                                expected.aggregate.kind.as_str()
                            )));
                        }
                        let actual = u64::try_from(actual).map_err(|_| {
                            CommandProblem::Internal(format!(
                                "{} revision 溢出",
                                expected.aggregate.kind.as_str()
                            ))
                        })?;
                        if actual != *expected_revision {
                            return Err(CommandProblem::RevisionConflict);
                        }
                    }
                }
                _ => {
                    return Err(CommandProblem::InvalidEnvelope(
                        "当前 Project 命令不接受该 expected aggregate".into(),
                    ));
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
pub struct InProcessCoreKernel {
    coordinator: CommandCoordinator,
    service: Arc<ServiceStore>,
    idempotency_key: ServiceIdempotencyKey,
    /// MF_RUN_TOKEN 的独立 authority key；与 command idempotency、
    /// Controller lease 完全分域。
    run_capability_key: RunCapabilityKey,
    lease: Arc<ControllerLeaseShared>,
    authorizer: InProcessAuthorizer,
    projects: RwLock<HashMap<String, ProjectRegistration>>,
    run_lifecycle_ports: RwLock<HashMap<String, Arc<dyn RunLifecyclePort>>>,
    /// 同 command 的并发 Cancel 在进程内串行；进程崩溃后 durable
    /// `stopping` target 由新实例幂等重试。
    cancel_gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    workflow_start_ports:
        RwLock<HashMap<String, Arc<dyn crate::workflow_start::WorkflowStartPort>>>,
    /// 只持有 channel sender；worker 仅持有 Kernel 的 Weak，避免
    /// runtime → kernel → worker → kernel 的强引用环。
    workflow_start_worker: RwLock<Option<Arc<WorkflowStartWorker>>>,
    /// T2f 终端宿主缝隙:由拥有 SessionRuntime 的装配件(AppRuntime/core bin)
    /// 注入;未注入时 attach_terminal 显式 fail-closed,不回退旁路。
    terminal_host: RwLock<Option<Arc<dyn mf_terminal::TerminalHost>>>,
    projections: ProjectionHub,
}

#[derive(Debug, Clone)]
struct WorkflowStartWork {
    project: ProjectStoreHandle,
    operation: OperationHandle,
    attempt: u8,
}

/// Workflow Start 的生产后台队列。业务幂等不依赖内存去重，而依赖
/// Operation step receipt；重复排队/进程重启都安全。瞬态错误有限次退避
/// 后把 Operation 保持在 durable 非终态，下一次 open_project 会恢复。
struct WorkflowStartWorker {
    sender: mpsc::Sender<WorkflowStartWork>,
}

impl WorkflowStartWorker {
    fn spawn(kernel: Weak<InProcessCoreKernel>) -> Result<Arc<Self>, KernelProblem> {
        let (sender, receiver) = mpsc::channel::<WorkflowStartWork>();
        let worker = Arc::new(Self { sender });
        thread::Builder::new()
            .name("mf-workflow-start".into())
            .spawn(move || {
                while let Ok(mut work) = receiver.recv() {
                    loop {
                        let Some(kernel) = kernel.upgrade() else {
                            return;
                        };
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            kernel.run_workflow_start_operation(&work.project, &work.operation)
                        }));
                        match result {
                            Ok(Ok(_)) => break,
                            Ok(Err(error)) => {
                                // close/port 窗口与其它瞬态错误绝不吞掉：有限退避
                                // 重投；耗尽后仍保留 durable Operation 供重启恢复。
                                log::warn!(
                                    "Workflow Start worker {} 执行失败(第 {} 次):{error}",
                                    work.operation,
                                    work.attempt + 1
                                );
                                if work.attempt >= 2 {
                                    break;
                                }
                                work.attempt += 1;
                                thread::sleep(Duration::from_millis(50 * u64::from(work.attempt)));
                            }
                            Err(_) => {
                                // panic 不得杀死整个 worker；Operation 未终态且
                                // receipt 控制幂等，保留给下一次 open/resume。
                                log::error!(
                                    "Workflow Start worker {} panic，Operation 保持可恢复",
                                    work.operation
                                );
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|error| {
                KernelProblem::ServiceUnavailable(format!(
                    "workflow_start_worker_spawn_failed:{error}"
                ))
            })?;
        Ok(worker)
    }

    fn enqueue(&self, project: ProjectStoreHandle, operation: OperationHandle) {
        if self
            .sender
            .send(WorkflowStartWork {
                project,
                operation: operation.clone(),
                attempt: 0,
            })
            .is_err()
        {
            // acceptance 已 durable；队列故障不能伪装回滚 202。保持非终态
            // 并明确记录，下一次 open_project 将扫描恢复。
            log::error!("Workflow Start worker 队列已关闭，Operation {operation} 保持可恢复");
        }
    }
}

impl InProcessCoreKernel {
    pub(crate) fn new(service: Arc<ServiceStore>, idempotency_key: ServiceIdempotencyKey) -> Self {
        #[cfg(test)]
        let run_capability_key =
            RunCapabilityKey::for_test(vec![0x52; 32]).expect("static capability key");
        #[cfg(not(test))]
        let run_capability_key =
            RunCapabilityKey::load_or_create().expect("run capability key 必须由 runtime 预先装配");
        Self::with_projections(
            service,
            idempotency_key,
            run_capability_key,
            ProjectionHub::new(),
        )
    }

    fn new_with_capability_key(
        service: Arc<ServiceStore>,
        idempotency_key: ServiceIdempotencyKey,
        run_capability_key: RunCapabilityKey,
    ) -> Self {
        Self::with_projections(
            service,
            idempotency_key,
            run_capability_key,
            ProjectionHub::new(),
        )
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
            RunCapabilityKey::for_test(vec![0x52; 32]).expect("static capability key"),
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
            RunCapabilityKey::for_test(vec![0x52; 32]).expect("static capability key"),
            ProjectionHub::for_test_limits(limits),
        )
    }

    fn with_projections(
        service: Arc<ServiceStore>,
        idempotency_key: ServiceIdempotencyKey,
        run_capability_key: RunCapabilityKey,
        projections: ProjectionHub,
    ) -> Self {
        let lease = Arc::new(ControllerLeaseShared {
            state: RwLock::new(ControllerLeaseState::default()),
        });
        Self {
            coordinator: CommandCoordinator::new(service.clone(), idempotency_key.clone()),
            authorizer: InProcessAuthorizer {
                lease: lease.clone(),
            },
            lease,
            service,
            idempotency_key,
            run_capability_key,
            projects: RwLock::new(HashMap::new()),
            run_lifecycle_ports: RwLock::new(HashMap::new()),
            cancel_gates: Mutex::new(HashMap::new()),
            workflow_start_ports: RwLock::new(HashMap::new()),
            workflow_start_worker: RwLock::new(None),
            terminal_host: RwLock::new(None),
            projections,
        }
    }

    fn install_workflow_start_worker(self: &Arc<Self>) -> Result<(), KernelProblem> {
        let worker = WorkflowStartWorker::spawn(Arc::downgrade(self))?;
        *self.workflow_start_worker.write() = Some(worker);
        Ok(())
    }

    /// L-INPUT(§8.4)的 Controller epoch 复验:writer lease 授予后,
    /// 每次终端字节入队前调用;takeover 旋转 epoch 后旧 lease 的写入
    /// 被拒绝(已线性化字节不受影响)。
    pub fn verify_controller_epoch(&self, epoch: u64) -> crate::lease::InputLeaseVerdict {
        let guard = self.lease.state.read();
        if guard.epoch == epoch {
            crate::lease::InputLeaseVerdict::Current
        } else {
            crate::lease::InputLeaseVerdict::ControllerTakeover
        }
    }

    /// 幂等注入终端宿主(装配件在创建/注册 kernel 时调用;重复注入以
    /// 先到者为准,不覆盖,避免装配竞态换掉已生效的宿主)。
    pub fn ensure_terminal_host(
        &self,
        populate: impl FnOnce() -> Arc<dyn mf_terminal::TerminalHost>,
    ) {
        if self.terminal_host.read().is_some() {
            return;
        }
        let mut guard = self.terminal_host.write();
        if guard.is_none() {
            *guard = Some(populate());
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
            if target_has_pending_run_actions(&target)? {
                hub.block_for_run_actions(&target);
            }
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

    /// 按 Project 注册 lifecycle port。新 port 对外可见之前先重投
    /// 该 Project 的全部 durable run actions；恢复失败则保持未注册。
    pub(crate) fn register_run_lifecycle_port(
        &self,
        project: &ProjectStoreHandle,
        port: Arc<dyn RunLifecyclePort>,
    ) -> Result<(), KernelProblem> {
        let registration = self.project_registration(project)?;
        self.register_run_lifecycle_port_with_registration(project, registration, port)
    }

    pub(crate) fn register_run_lifecycle_port_for_close(
        &self,
        close: &ProjectCloseToken,
        port: Arc<dyn RunLifecyclePort>,
    ) -> Result<(), KernelProblem> {
        let registration = self.project_registration_for_close(close)?;
        self.register_run_lifecycle_port_with_registration(&close.project, registration, port)
    }

    fn register_run_lifecycle_port_with_registration(
        &self,
        project: &ProjectStoreHandle,
        registration: ProjectRegistration,
        port: Arc<dyn RunLifecyclePort>,
    ) -> Result<(), KernelProblem> {
        let targets = self.registered_targets();
        self.projections.linearize_run_actions(&targets, |hub| {
            self.recover_cancel_fences(project, &registration, port.as_ref())?;
            drain_run_action_outbox(&registration.target, port.as_ref())?;
            hub.clear_run_action_block(&registration.target);
            hub.publish_pending(&registration.target)?;
            Ok(())
        })?;
        self.run_lifecycle_ports
            .write()
            .insert(project.as_str().to_owned(), port);
        Ok(())
    }

    fn recover_cancel_fences(
        &self,
        project: &ProjectStoreHandle,
        registration: &ProjectRegistration,
        port: &dyn RunLifecyclePort,
    ) -> Result<(), KernelProblem> {
        #[derive(serde::Deserialize)]
        struct FrozenVersioned {
            handle: String,
            revision: u64,
        }
        #[derive(serde::Deserialize)]
        struct FrozenExpected {
            workflow_run: String,
            workflow_run_revision: u64,
            steps: Vec<FrozenVersioned>,
            agent_runs: Vec<FrozenVersioned>,
            agent_sessions: Vec<FrozenVersioned>,
        }

        for fence in registration
            .store
            .active_cancel_fences()
            .map_err(|error| KernelProblem::Internal(error.to_string()))?
        {
            let frozen: FrozenExpected = serde_json::from_str(&fence.expected_json)
                .map_err(|error| KernelProblem::Internal(format!("cancel fence 损坏:{error}")))?;
            let command_id = CommandId::parse(&fence.command_id)
                .map_err(|error| KernelProblem::Internal(error.to_string()))?;
            let workflow_run = WorkflowRunHandle::parse(frozen.workflow_run)
                .map_err(|error| KernelProblem::Internal(error.to_string()))?;
            let expected = WorkflowRunExpected {
                workflow_run_revision: frozen.workflow_run_revision,
                steps: frozen
                    .steps
                    .into_iter()
                    .map(|item| {
                        Ok(VersionedHandle {
                            handle: StepHandle::parse(item.handle)
                                .map_err(|error| KernelProblem::Internal(error.to_string()))?,
                            revision: item.revision,
                        })
                    })
                    .collect::<Result<_, KernelProblem>>()?,
                agent_runs: frozen
                    .agent_runs
                    .into_iter()
                    .map(|item| {
                        Ok(VersionedHandle {
                            handle: AgentRunHandle::parse(item.handle)
                                .map_err(|error| KernelProblem::Internal(error.to_string()))?,
                            revision: item.revision,
                        })
                    })
                    .collect::<Result<_, KernelProblem>>()?,
                agent_sessions: frozen
                    .agent_sessions
                    .into_iter()
                    .map(|item| {
                        Ok(VersionedHandle {
                            handle: AgentSessionHandle::parse(item.handle)
                                .map_err(|error| KernelProblem::Internal(error.to_string()))?,
                            revision: item.revision,
                        })
                    })
                    .collect::<Result<_, KernelProblem>>()?,
            };
            let command = WorkflowRunCommand::Cancel {
                project: project.clone(),
                workflow_run,
                expected,
            };
            let run_stops =
                self.drive_cancel_targets(&registration.target, &command_id, fence.task_id, port)?;
            let envelope = CommandEnvelope::new(
                command_id.clone(),
                ClientId::parse("cancel-recovery").expect("static client id"),
                Principal::parse("core-recovery").expect("static principal"),
                0,
                None,
                CommandTarget {
                    store: TargetStoreKind::Project,
                    store_handle: project.as_str().to_owned(),
                    aggregate: workflow_run_target(&command)?,
                },
                workflow_run_expected_revisions(&command)?,
                command.command_type(),
                CommandPayload::Plain(workflow_run_payload(&command)),
            )?;
            let authorizer = CancelRecoveryAuthorizer {
                command_id: fence.command_id,
                task_id: fence.task_id,
            };
            self.coordinator
                .dispatch_internal(
                    &envelope,
                    &registration.target,
                    &authorizer,
                    |tx| {
                        run_lifecycle_effect(
                            tx,
                            &command_id,
                            &command,
                            &RunPreparation::Cancel { run_stops },
                        )
                    },
                    None,
                    || {},
                )
                .map_err(KernelProblem::from)?;
        }
        Ok(())
    }

    fn drive_cancel_targets(
        &self,
        target_db: &TargetDatabase,
        command_id: &CommandId,
        task_id: i64,
        port: &dyn RunLifecyclePort,
    ) -> Result<Vec<crate::run_lifecycle::PreparedRunStop>, KernelProblem> {
        let targets = target_db.with_tx(|tx| {
            Store::reserve_cancel_fence_tx(tx, command_id.as_str(), task_id, &[])
                .map_err(run_domain_problem)
        })?;
        let mut run_stops = Vec::with_capacity(targets.len());
        for target in targets {
            use mf_agent::CancelFenceTargetState;
            let handle = AgentRunHandle::parse(target.run_handle)
                .map_err(|error| KernelProblem::Internal(error.to_string()))?;
            let outcome = match target.state {
                CancelFenceTargetState::Confirmed => mf_agent::RunStopOutcome::Confirmed,
                CancelFenceTargetState::Unconfirmed => mf_agent::RunStopOutcome::Unconfirmed,
                CancelFenceTargetState::Pending | CancelFenceTargetState::Stopping => {
                    if target.state == CancelFenceTargetState::Pending {
                        target_db.with_tx(|tx| {
                            if Store::claim_cancel_target_tx(tx, command_id.as_str(), target.run_id)
                                .map_err(run_domain_problem)?
                            {
                                Ok(())
                            } else {
                                Err(CommandProblem::CommandInProgress)
                            }
                        })?;
                    }
                    let outcome = port
                        .stop_cancel_target(command_id, &handle)
                        .unwrap_or(mf_agent::RunStopOutcome::Unconfirmed);
                    target_db.with_tx(|tx| {
                        Store::record_cancel_outcome_tx(
                            tx,
                            command_id.as_str(),
                            target.run_id,
                            outcome,
                        )
                        .map_err(run_domain_problem)
                    })?;
                    outcome
                }
            };
            run_stops.push(crate::run_lifecycle::PreparedRunStop {
                agent_run: handle,
                outcome,
            });
        }
        Ok(run_stops)
    }

    pub(crate) fn unregister_run_lifecycle_port(&self, project: &ProjectStoreHandle) {
        self.run_lifecycle_ports.write().remove(project.as_str());
    }

    #[cfg(test)]
    pub(crate) fn run_control_projects(&self) -> Vec<crate::run_control::RunControlProject> {
        self.projects
            .read()
            .iter()
            .map(
                |(handle, registration)| crate::run_control::RunControlProject {
                    project: ProjectStoreHandle::parse(handle.clone())
                        .expect("登记时已验证 Project store handle"),
                    store: registration.store.clone(),
                    closing: registration.closing,
                },
            )
            .collect()
    }

    /// MF_RUN_TOKEN 的独立 RunCapability authority 入口。整个 route、
    /// closing 判定、target receipt/effect 与 capability 收口都位于同一个
    /// Kernel publication 线性化区；Controller takeover 与此路径无关。
    pub(crate) fn settle_run_control_capability(
        &self,
        token: &str,
        settlement: mf_agent::Settlement,
        command_id: CommandId,
    ) -> Result<crate::run_control::TokenSettleOutcome, crate::run_control::TokenSettleProblem>
    {
        match self.execute_run_control_capability(
            token,
            crate::run_control::RunControlCommand::Settle(settlement),
            command_id,
        )? {
            crate::run_control::RunControlOutcome::Settled(outcome) => Ok(outcome),
            _ => Err(crate::run_control::TokenSettleProblem::Kernel(
                KernelProblem::Internal("RunControl settle 返回错误结果类型".into()),
            )),
        }
    }

    pub(crate) fn execute_run_control_capability(
        &self,
        token: &str,
        command: crate::run_control::RunControlCommand,
        command_id: CommandId,
    ) -> Result<crate::run_control::RunControlOutcome, crate::run_control::TokenSettleProblem> {
        use crate::run_control::TokenSettleProblem;
        if token.is_empty() {
            return Err(TokenSettleProblem::MissingToken);
        }
        if crate::run_control::command_contains_token(&command, token) {
            return Err(TokenSettleProblem::SensitiveSettlement);
        }
        let command = match command {
            crate::run_control::RunControlCommand::ProposePipeline(draft) => {
                crate::run_control::RunControlCommand::ProposePipeline(
                    crate::run_control::normalize_pipeline_draft(draft)
                        .map_err(TokenSettleProblem::Kernel)?,
                )
            }
            command => command,
        };
        let targets = self.registered_targets();
        self.projections
            .linearize_run_actions(&targets, |hub| {
                Ok(self.execute_run_control_linearized(hub, token, command, command_id))
            })
            .map_err(TokenSettleProblem::Kernel)?
    }

    fn execute_run_control_linearized(
        &self,
        hub: &ProjectionHub,
        token: &str,
        run_control: crate::run_control::RunControlCommand,
        command_id: CommandId,
    ) -> Result<crate::run_control::RunControlOutcome, crate::run_control::TokenSettleProblem> {
        use crate::run_control::{
            RunControlCommand, RunControlOutcome, TokenSettleOutcome, TokenSettleProblem,
        };

        let resolution = self
            .service
            .resolve_run_capability(&self.run_capability_key, token.as_bytes())
            .map_err(|error| {
                TokenSettleProblem::Kernel(KernelProblem::Internal(format!(
                    "run capability resolve 失败:{error:#}"
                )))
            })?;
        // 目前仍允许 legacy Orchestrator 在 Project Store 创建尚未写入
        // service authority 的 Agent Run；因此即使 HMAC 索引返回 One，也
        // 必须在同一 publication 线性化区复验 plaintext token 没有第二个
        // legacy 命中。等所有创建路径都改为 eager authority registration
        // 后，才能以 durable backfill-complete 标记移除此 O(projects) 防线。
        let scan_legacy_hits = || {
            let mut hits = Vec::new();
            for (handle, registration) in self.projects.read().iter() {
                match registration.store.run_by_token(token) {
                    Ok(Some(run)) => hits.push((handle.clone(), run)),
                    Ok(None) => {}
                    Err(error) => {
                        return Err(TokenSettleProblem::Kernel(KernelProblem::Internal(
                            format!("legacy run capability 扫描失败:{error:#}"),
                        )))
                    }
                }
            }
            Ok(hits)
        };
        let authority = match resolution {
            RunCapabilityResolution::Many => {
                return Err(TokenSettleProblem::AmbiguousToken { matches: 2 })
            }
            RunCapabilityResolution::One(capability) => {
                let hits = scan_legacy_hits()?;
                let indexed_identity_is_unique = hits.len() <= 1
                    && hits.iter().all(|(project, run)| {
                        project == capability.project.as_str()
                            && run.public_handle == capability.agent_run.as_str()
                    });
                if !indexed_identity_is_unique {
                    self.service
                        .backfill_quarantined_run_capability(
                            &self.run_capability_key,
                            token.as_bytes(),
                            &capability.project,
                            &capability.agent_run,
                        )
                        .map_err(|error| {
                            TokenSettleProblem::Kernel(KernelProblem::Internal(format!(
                                "indexed run capability quarantine 失败:{error:#}"
                            )))
                        })?;
                    return Err(TokenSettleProblem::AmbiguousToken {
                        matches: hits.len().max(2),
                    });
                }
                capability
            }
            RunCapabilityResolution::Zero => {
                // v4 前创建的 run 没有 service 索引。只在这个线性化区
                // 做一次全量兼容扫描；唯一命中 exact 注册，重复命中写
                // quarantine tombstone，后续不再扫描/猜 winner。
                let mut hits = scan_legacy_hits()?;
                if hits.len() > 1 {
                    let (handle, run) = &hits[0];
                    let project = ProjectStoreHandle::parse(handle.clone()).map_err(|error| {
                        TokenSettleProblem::Kernel(KernelProblem::Internal(error.to_string()))
                    })?;
                    let agent_run =
                        AgentRunHandle::parse(run.public_handle.clone()).map_err(|error| {
                            TokenSettleProblem::Kernel(KernelProblem::Internal(error.to_string()))
                        })?;
                    self.service
                        .backfill_quarantined_run_capability(
                            &self.run_capability_key,
                            token.as_bytes(),
                            &project,
                            &agent_run,
                        )
                        .map_err(|error| {
                            TokenSettleProblem::Kernel(KernelProblem::Internal(format!(
                                "legacy run capability quarantine 失败:{error:#}"
                            )))
                        })?;
                    return Err(TokenSettleProblem::AmbiguousToken {
                        matches: hits.len(),
                    });
                }
                let Some((handle, run)) = hits.pop() else {
                    return Err(TokenSettleProblem::UnknownToken);
                };
                let project = ProjectStoreHandle::parse(handle).map_err(|error| {
                    TokenSettleProblem::Kernel(KernelProblem::Internal(error.to_string()))
                })?;
                let agent_run = AgentRunHandle::parse(run.public_handle).map_err(|error| {
                    TokenSettleProblem::Kernel(KernelProblem::Internal(error.to_string()))
                })?;
                match self
                    .service
                    .register_run_capability(
                        &self.run_capability_key,
                        token.as_bytes(),
                        &project,
                        &agent_run,
                    )
                    .map_err(|error| {
                        TokenSettleProblem::Kernel(KernelProblem::Internal(format!(
                            "legacy run capability 注册失败:{error:#}"
                        )))
                    })? {
                    RunCapabilityRegistration::Registered(capability) => capability,
                    RunCapabilityRegistration::Quarantined => {
                        return Err(TokenSettleProblem::AmbiguousToken { matches: 2 })
                    }
                }
            }
        };

        let registration = self
            .projects
            .read()
            .get(authority.project.as_str())
            .cloned();
        let Some(registration) = registration else {
            return Err(TokenSettleProblem::UnknownToken);
        };
        if registration.closing {
            return Err(TokenSettleProblem::ProjectClosing);
        }
        if authority.state == RunCapabilityState::Revoked {
            return Err(TokenSettleProblem::UnknownToken);
        }
        if authority.state == RunCapabilityState::Quarantined {
            return Err(TokenSettleProblem::AmbiguousToken { matches: 2 });
        }

        // 先以 opaque authority 定位；明文 token + project/run/task/step
        // 绑定还会由 RunControlAuthorizer 在最终 target IMMEDIATE tx
        // 再复验，消除 service index 与 Project Store 之间的 TOCTOU。
        let run = registration
            .store
            .run_view_by_handle(authority.agent_run.as_str())
            .map_err(|error| {
                TokenSettleProblem::Kernel(KernelProblem::Internal(format!(
                    "Agent Run 定位失败:{error:#}"
                )))
            })?
            .ok_or(TokenSettleProblem::UnknownToken)?;
        if run.capability_token != token {
            return Err(TokenSettleProblem::UnknownToken);
        }
        let task = registration
            .store
            .task_view(run.task_id)
            .map_err(|error| {
                TokenSettleProblem::Kernel(KernelProblem::Internal(format!(
                    "Workflow Run 定位失败:{error:#}"
                )))
            })?
            .ok_or(TokenSettleProblem::UnknownToken)?;
        let step = registration
            .store
            .step_view(run.step_id)
            .map_err(|error| {
                TokenSettleProblem::Kernel(KernelProblem::Internal(format!(
                    "Step 定位失败:{error:#}"
                )))
            })?
            .ok_or(TokenSettleProblem::UnknownToken)?;
        if step.task_id != task.id {
            return Err(TokenSettleProblem::UnknownToken);
        }
        let workflow_run = WorkflowRunHandle::parse(task.public_handle).map_err(|error| {
            TokenSettleProblem::Kernel(KernelProblem::Internal(error.to_string()))
        })?;
        let step_handle = StepHandle::parse(step.public_handle).map_err(|error| {
            TokenSettleProblem::Kernel(KernelProblem::Internal(error.to_string()))
        })?;
        let mut expected = WorkflowRunExpected {
            workflow_run_revision: u64::try_from(task.revision).map_err(|_| {
                TokenSettleProblem::Kernel(KernelProblem::Internal(
                    "Workflow Run revision 溢出".into(),
                ))
            })?,
            steps: vec![VersionedHandle {
                handle: step_handle.clone(),
                revision: u64::try_from(step.revision).map_err(|_| {
                    TokenSettleProblem::Kernel(KernelProblem::Internal("Step revision 溢出".into()))
                })?,
            }],
            agent_runs: vec![VersionedHandle {
                handle: authority.agent_run.clone(),
                revision: u64::try_from(run.revision).map_err(|_| {
                    TokenSettleProblem::Kernel(KernelProblem::Internal(
                        "Agent Run revision 溢出".into(),
                    ))
                })?,
            }],
            agent_sessions: Vec::new(),
        };
        if let Some(session_id) = run.session_id {
            let session = registration
                .store
                .session_view(session_id)
                .map_err(|error| {
                    TokenSettleProblem::Kernel(KernelProblem::Internal(format!(
                        "Agent Session 定位失败:{error:#}"
                    )))
                })?
                .ok_or(TokenSettleProblem::UnknownToken)?;
            expected.agent_sessions.push(VersionedHandle {
                handle: AgentSessionHandle::parse(session.public_handle).map_err(|error| {
                    TokenSettleProblem::Kernel(KernelProblem::Internal(error.to_string()))
                })?,
                revision: u64::try_from(session.revision).map_err(|_| {
                    TokenSettleProblem::Kernel(KernelProblem::Internal(
                        "Agent Session revision 溢出".into(),
                    ))
                })?,
            });
        }
        if !matches!(run_control, RunControlCommand::Settle(_))
            && authority.state == RunCapabilityState::Settled
        {
            return Err(TokenSettleProblem::RunNotActive("已结算".into()));
        }
        let command_type = match &run_control {
            RunControlCommand::Settle(_) => CommandType::WorkflowSettle,
            RunControlCommand::ReportState(_) | RunControlCommand::ProposePipeline(_) => {
                CommandType::WorkflowRun
            }
        };
        let target = AggregateRef::new(
            match &run_control {
                RunControlCommand::ProposePipeline(_) => AggregateKind::WorkflowRun,
                _ => AggregateKind::AgentRun,
            },
            match &run_control {
                RunControlCommand::ProposePipeline(_) => workflow_run.as_str(),
                _ => authority.agent_run.as_str(),
            }
            .to_owned(),
        )
        .map_err(|error| TokenSettleProblem::Kernel(KernelProblem::Internal(error.to_string())))?;
        let expected_revisions = run_control_expected_revisions(&workflow_run, &expected);
        let envelope = CommandEnvelope::new(
            command_id.clone(),
            ClientId::parse("run-capability").expect("static client id"),
            Principal::parse("mf-run-token").expect("static principal"),
            0,
            None,
            CommandTarget {
                store: TargetStoreKind::Project,
                store_handle: authority.project.as_str().to_owned(),
                aggregate: target.clone(),
            },
            expected_revisions.clone(),
            command_type,
            CommandPayload::Plain(serde_json::json!({
                "method":run_control.method(),
                "workflow_run":workflow_run.as_str(),
                "agent_run":authority.agent_run.as_str(),
            })),
        )
        .map_err(KernelProblem::from)
        .map_err(TokenSettleProblem::Kernel)?;
        let digest = crate::run_control::semantic_digest(
            &authority.project,
            &workflow_run,
            &authority.agent_run,
            &run_control,
        )
        .map_err(TokenSettleProblem::Kernel)?;
        let authorizer = RunControlAuthorizer {
            token: Zeroizing::new(token.to_owned()),
            workflow_run: workflow_run.clone(),
            step: step_handle,
            agent_run: authority.agent_run.clone(),
            target,
            command_type,
        };
        let port = self
            .run_lifecycle_ports
            .read()
            .get(authority.project.as_str())
            .cloned()
            .ok_or_else(|| {
                TokenSettleProblem::Kernel(KernelProblem::ServiceUnavailable(
                    "run_lifecycle_port_not_registered".into(),
                ))
            })?;

        // 重新认证之后、expected 之前先读 receipt。显式 command_id
        // 命中时返回首次持久化的 run_control_result；异 payload digest
        // 由唯一 command coordinator 稳定判为 CommandIdReused。
        let replay = self
            .coordinator
            .replay_if_applied_with_digest(&envelope, &digest, &registration.target, &authorizer)
            .map_err(KernelProblem::from)
            .map_err(TokenSettleProblem::Kernel)?;

        let already_applied = run.outcome.is_some();
        if replay.is_none() {
            if let RunControlCommand::Settle(settlement) = &run_control {
                if let Some(existing) = &run.outcome {
                    if existing != settlement.kind_str() {
                        return Err(TokenSettleProblem::Conflict {
                            existing: existing.clone(),
                            attempted: settlement.kind_str().to_owned(),
                        });
                    }
                } else if !matches!(
                    run.status,
                    mf_agent::RunStatus::Running
                        | mf_agent::RunStatus::AwaitingOutcome
                        | mf_agent::RunStatus::Interrupted
                ) {
                    return Err(TokenSettleProblem::RunNotActive(
                        run.status.as_str().to_owned(),
                    ));
                }
            } else if run.outcome.is_some()
                || !matches!(
                    run.status,
                    mf_agent::RunStatus::Running
                        | mf_agent::RunStatus::AwaitingOutcome
                        | mf_agent::RunStatus::Interrupted
                )
            {
                return Err(TokenSettleProblem::RunNotActive(
                    run.status.as_str().to_owned(),
                ));
            }
        }

        let outcome = if let Some(replay) = replay {
            replay
        } else {
            let agent_run_result = authority.agent_run.as_str().to_owned();
            self.coordinator
                .dispatch_internal_with_digest(
                    &envelope,
                    &digest,
                    &registration.target,
                    &authorizer,
                    |tx| {
                        let mut output = run_control_mutation_effect(
                            tx,
                            &workflow_run,
                            &authority.agent_run,
                            run.id,
                            run.task_id,
                            &run_control,
                            &expected_revisions,
                            &authorizer.target,
                        )?;
                        let object = output.result_revisions.as_object_mut().ok_or_else(|| {
                            CommandProblem::Internal("run result revisions 必须是 object".into())
                        })?;
                        let result = match &run_control {
                            RunControlCommand::Settle(_) => serde_json::json!({
                                "agent_run":agent_run_result,
                                "outcome":if already_applied {"already_applied"} else {"applied"},
                            }),
                            RunControlCommand::ReportState(state) => serde_json::json!({
                                "agent_run":agent_run_result,
                                "outcome":"state_reported",
                                "state":state.as_str(),
                            }),
                            RunControlCommand::ProposePipeline(_) => {
                                let revision =
                                    object.remove("draft_revision").ok_or_else(|| {
                                        CommandProblem::Internal(
                                            "pipeline propose 缺少 draft revision handle".into(),
                                        )
                                    })?;
                                serde_json::json!({
                                    "workflow_run":authorizer.workflow_run.as_str(),
                                    "outcome":"pipeline_proposed",
                                    "revision":revision,
                                })
                            }
                        };
                        object.insert("run_control_result".into(), result);
                        Ok(output)
                    },
                    None,
                    || {},
                )
                .map_err(KernelProblem::from)
                .map_err(TokenSettleProblem::Kernel)?
        };
        if let Err(error) = drain_run_action_outbox(&registration.target, port.as_ref()) {
            hub.block_for_run_actions(&registration.target);
            return Err(TokenSettleProblem::Kernel(error));
        }
        hub.clear_run_action_block(&registration.target);
        hub.publish_pending(&registration.target)
            .map_err(TokenSettleProblem::Kernel)?;

        if matches!(run_control, RunControlCommand::Settle(_)) {
            match self
                .service
                .settle_run_capability(&self.run_capability_key, token.as_bytes())
                .map_err(|error| {
                    TokenSettleProblem::Kernel(KernelProblem::Internal(format!(
                        "run capability settle 失败:{error:#}"
                    )))
                })? {
                RunCapabilityResolution::One(capability)
                    if capability.state == RunCapabilityState::Settled => {}
                RunCapabilityResolution::Many => {
                    return Err(TokenSettleProblem::AmbiguousToken { matches: 2 })
                }
                _ => {
                    return Err(TokenSettleProblem::Kernel(KernelProblem::Internal(
                        "run capability commit 后未进入 settled".into(),
                    )))
                }
            }
        }

        let CommandOutcome::Applied {
            result_revisions, ..
        } = outcome;
        let result = result_revisions
            .get("run_control_result")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                TokenSettleProblem::Kernel(KernelProblem::Internal(
                    "run control receipt 缺少原始结果".into(),
                ))
            })?;
        match result.get("outcome").and_then(Value::as_str) {
            Some("applied") | Some("already_applied") | Some("state_reported") => {
                let agent_run =
                    result
                        .get("agent_run")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            TokenSettleProblem::Kernel(KernelProblem::Internal(
                                "run control receipt 的 Agent Run handle 损坏".into(),
                            ))
                        })?;
                let agent_run = AgentRunHandle::parse(agent_run.to_owned()).map_err(|error| {
                    TokenSettleProblem::Kernel(KernelProblem::Internal(error.to_string()))
                })?;
                match result.get("outcome").and_then(Value::as_str) {
                    Some("applied") => {
                        Ok(RunControlOutcome::Settled(TokenSettleOutcome::Applied {
                            agent_run,
                        }))
                    }
                    Some("already_applied") => Ok(RunControlOutcome::Settled(
                        TokenSettleOutcome::AlreadyApplied { agent_run },
                    )),
                    Some("state_reported") => {
                        let state = result
                            .get("state")
                            .and_then(Value::as_str)
                            .and_then(mf_agent::AgentState::parse)
                            .ok_or_else(|| {
                                TokenSettleProblem::Kernel(KernelProblem::Internal(
                                    "run control receipt 的 Agent state 损坏".into(),
                                ))
                            })?;
                        Ok(RunControlOutcome::StateReported { agent_run, state })
                    }
                    _ => unreachable!(),
                }
            }
            Some("pipeline_proposed") => {
                let workflow_run = result
                    .get("workflow_run")
                    .and_then(Value::as_str)
                    .and_then(|raw| WorkflowRunHandle::parse(raw.to_owned()).ok())
                    .ok_or_else(|| {
                        TokenSettleProblem::Kernel(KernelProblem::Internal(
                            "run control receipt 的 Workflow Run handle 损坏".into(),
                        ))
                    })?;
                let revision = result
                    .get("revision")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        TokenSettleProblem::Kernel(KernelProblem::Internal(
                            "run control receipt 的 Revision handle 损坏".into(),
                        ))
                    })?
                    .to_owned();
                Ok(RunControlOutcome::PipelineProposed {
                    workflow_run,
                    revision,
                })
            }
            _ => Err(TokenSettleProblem::Kernel(KernelProblem::Internal(
                "run control receipt 的 outcome 损坏".into(),
            ))),
        }
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
            let registration = self
                .projects
                .read()
                .get(project.as_str())
                .cloned()
                .ok_or(KernelProblem::ResourceNotFound)?;
            // L-CLOSE-FREEZE：先标 closing，使新 Start/普通 command 在
            // 任何 drain 动作之前即 fail-closed。重试 prepare 幂等
            // 返回同一 Store 的 token，供上次 drain 失败后安全继续。
            if !registration.closing {
                let mut projects = self.projects.write();
                let current = projects
                    .get_mut(project.as_str())
                    .ok_or(KernelProblem::ResourceNotFound)?;
                if !Arc::ptr_eq(&current.store, &registration.store) {
                    return Err(KernelProblem::ServiceUnavailable(
                        "project_registration_changed".into(),
                    ));
                }
                current.closing = true;
            }
            // v4 前的未索引 run 也必须先 exact/HMAC backfill，随后与已
            // 索引行一起 active→revoked；否则 finalize+reopen 后 legacy
            // scan 会把旧 token 复活。明文只作为 SQL/HMAC 参数存在。
            let legacy_runs = registration
                .store
                .with_conn(|conn| -> anyhow::Result<Vec<(String, String)>> {
                    let mut stmt =
                        conn.prepare("SELECT public_handle, capability_token FROM agent_runs")?;
                    let rows = stmt
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                })
                .map_err(|error| {
                    KernelProblem::Internal(format!("close capability backfill 读取失败:{error:#}"))
                })?;
            for (run_handle, token) in legacy_runs {
                let run = AgentRunHandle::parse(run_handle)
                    .map_err(|error| KernelProblem::Internal(error.to_string()))?;
                self.service
                    .register_run_capability(
                        &self.run_capability_key,
                        token.as_bytes(),
                        project,
                        &run,
                    )
                    .map_err(|error| {
                        KernelProblem::Internal(format!("close capability backfill 失败:{error:#}"))
                    })?;
            }
            self.service
                .revoke_project_run_capabilities(project)
                .map_err(|error| {
                    KernelProblem::Internal(format!("close capability revoke 失败:{error:#}"))
                })?;
            crate::operation::fail_open_operations_for_target(
                &self.service,
                crate::workflow_start::WORKFLOW_START_OPERATION_KIND,
                registration.target.store_key(),
                "project_closing",
            )
            .map_err(KernelProblem::from)?;
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
                self.run_lifecycle_ports
                    .write()
                    .remove(token.project.as_str());
                self.workflow_start_ports
                    .write()
                    .remove(token.project.as_str());
            }
        });
    }

    /// 授予 Controller lease(新 controller 使旧 epoch 立即失效)并返回
    /// 新 epoch(已解析 handle 形态;web 字符串入口经 trait 方法)。
    pub(crate) fn grant_controller_checked(
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
            KernelCommand::ProjectWorkflow(_) | KernelCommand::WorkflowRun(_) => Err(
                KernelProblem::InvalidEnvelope("fault seam 仅支持 workflow.rename".into()),
            ),
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

    fn project_registration_for_close(
        &self,
        token: &ProjectCloseToken,
    ) -> Result<ProjectRegistration, KernelProblem> {
        self.projects
            .read()
            .get(token.project.as_str())
            .filter(|registration| {
                registration.closing && Arc::ptr_eq(&registration.store, &token.store)
            })
            .cloned()
            .ok_or_else(|| KernelProblem::ServiceUnavailable("project_close_token_invalid".into()))
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
        let SnapshotQuery::Workflow { project, workflow } = &query else {
            return Err(KernelProblem::InvalidEnvelope(
                "workflow snapshot 收到其它查询".into(),
            ));
        };
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

    fn workspace_snapshot(&self) -> Result<SnapshotEnvelope, KernelProblem> {
        let registered_targets = self.registered_targets();
        let (cursor, data) = self.projections.snapshot(&registered_targets, || {
            let display_names = self
                .service
                .list_projects()
                .map_err(|error| KernelProblem::Internal(format!("{error:#}")))?
                .into_iter()
                .map(|project| {
                    let name = Path::new(&project.display_path)
                        .file_name()
                        .and_then(|value| value.to_str())
                        .filter(|value| !value.is_empty())
                        .unwrap_or("Project")
                        .to_owned();
                    (project.project_handle, name)
                })
                .collect::<HashMap<_, _>>();
            let mut projects = self
                .projects
                .read()
                .iter()
                .filter(|(_, registration)| !registration.closing)
                .map(|(handle, registration)| {
                    Ok((
                        ProjectStoreHandle::parse(handle.clone())
                            .map_err(|error| KernelProblem::Internal(error.to_string()))?,
                        display_names
                            .get(handle)
                            .cloned()
                            .unwrap_or_else(|| "Project".into()),
                        registration.store.clone(),
                    ))
                })
                .collect::<Result<Vec<_>, KernelProblem>>()?;
            projects.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
            crate::workspace_projection::read_workspace(projects)
        })?;
        Ok(SnapshotEnvelope {
            schema: SNAPSHOT_SCHEMA,
            server_instance_id: self.projections.server_instance_id().clone(),
            cursor,
            data: SnapshotData::Workspace(data),
        })
    }

    fn workflow_run_snapshot(
        &self,
        project: &ProjectStoreHandle,
        workflow_run: &WorkflowRunHandle,
    ) -> Result<SnapshotEnvelope, KernelProblem> {
        let registered_targets = self.registered_targets();
        let (cursor, data) = self.projections.snapshot(&registered_targets, || {
            let registration = self.project_registration(project)?;
            crate::run_projection::read_workflow_run(&registration.store, workflow_run)
                .map_err(|error| KernelProblem::Internal(format!("{error:#}")))?
                .ok_or(KernelProblem::ResourceNotFound)
        })?;
        Ok(SnapshotEnvelope {
            schema: SNAPSHOT_SCHEMA,
            server_instance_id: self.projections.server_instance_id().clone(),
            cursor,
            data: SnapshotData::WorkflowRun(data),
        })
    }

    fn workflow_run_snapshot_for_close(
        &self,
        close: &ProjectCloseToken,
        workflow_run: &WorkflowRunHandle,
    ) -> Result<SnapshotEnvelope, KernelProblem> {
        let registered_targets = self.registered_targets();
        let (cursor, data) = self.projections.snapshot(&registered_targets, || {
            let registration = self.project_registration_for_close(close)?;
            crate::run_projection::read_workflow_run(&registration.store, workflow_run)
                .map_err(|error| KernelProblem::Internal(format!("{error:#}")))?
                .ok_or(KernelProblem::ResourceNotFound)
        })?;
        Ok(SnapshotEnvelope {
            schema: SNAPSHOT_SCHEMA,
            server_instance_id: self.projections.server_instance_id().clone(),
            cursor,
            data: SnapshotData::WorkflowRun(data),
        })
    }

    fn operation_snapshot(
        &self,
        operation: &crate::operation::OperationHandle,
    ) -> Result<SnapshotEnvelope, KernelProblem> {
        let registered_targets = self.registered_targets();
        let (cursor, (record, steps)) = self.projections.snapshot(&registered_targets, || {
            let record = crate::operation::operation_of(&self.service, operation)
                .map_err(KernelProblem::from)?;
            let steps = crate::operation::steps_of(&self.service, operation)
                .map_err(KernelProblem::from)?;
            Ok((record, steps))
        })?;
        let workflow_run = crate::workflow_start::workflow_run_handle_of(&steps);
        Ok(SnapshotEnvelope {
            schema: SNAPSHOT_SCHEMA,
            server_instance_id: self.projections.server_instance_id().clone(),
            cursor,
            data: SnapshotData::Operation(crate::projection::OperationSnapshotData {
                operation: record.operation_handle,
                kind: record.kind.as_str().to_owned(),
                state: record.state.as_str().to_owned(),
                progress: record.progress,
                workflow_run,
                steps: steps
                    .into_iter()
                    .map(|step| crate::projection::OperationStepSnapshot {
                        index: step.step_index,
                        role: step.role.as_str().to_owned(),
                        state: step.state.as_str().to_owned(),
                        target_store: step.target_store,
                        aggregate: step.aggregate,
                        compensates: step.compensates,
                        result: step.result,
                        problem_code: step.problem_code,
                    })
                    .collect(),
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
            KernelCommand::WorkflowRun(_) => self.dispatch_workflow_run(request),
        }
    }

    fn snapshot(&self, query: SnapshotQuery) -> Result<SnapshotEnvelope, KernelProblem> {
        match &query {
            SnapshotQuery::Workspace => self.workspace_snapshot(),
            SnapshotQuery::Workflow { .. } => self.workflow_snapshot(query),
            SnapshotQuery::WorkflowRun {
                project,
                workflow_run,
            } => self.workflow_run_snapshot(project, workflow_run),
            SnapshotQuery::Operation { operation } => self.operation_snapshot(operation),
        }
    }

    fn subscribe_events(&self, cursor: EventCursor) -> Result<EventSubscription, KernelProblem> {
        let registered_targets = self.registered_targets();
        self.projections
            .subscribe_live(&registered_targets, &cursor)
    }

    fn attach_terminal(
        &self,
        session: SessionHandle,
        _attach: TerminalAttach,
    ) -> Result<TerminalChannel, KernelProblem> {
        // T2f shim:委托装配件注入的 TerminalHost(legacy SessionRegistry)。
        // `after_seq` 的 replay 语义随 T3 管线生效,shim 忽略。
        let host = self.terminal_host.read().clone().ok_or_else(|| {
            KernelProblem::ServiceUnavailable(
                "终端宿主未装配:attach_terminal 需要装配件注入 TerminalHost".into(),
            )
        })?;
        let reference = TerminalSessionRef::new(session.as_str().to_owned());
        if !host.session_alive(&reference) {
            log::warn!("attach_terminal:会话不存在或已结束:{session}");
            return Err(KernelProblem::ResourceNotFound);
        }
        Ok(TerminalChannel::attach(host, reference))
    }

    fn shutdown(&self, intent: ShutdownIntent) -> ShutdownAssessment {
        self.assess_shutdown(intent)
    }

    fn grant_controller(&self, client_id: &str, principal: &str) -> Result<u64, KernelProblem> {
        let client = ClientId::parse(client_id)
            .map_err(|e| KernelProblem::Internal(format!("client_id 非法:{e}")))?;
        let principal = Principal::parse(principal)
            .map_err(|e| KernelProblem::Internal(format!("principal 非法:{e}")))?;
        self.grant_controller_checked(&client, &principal)
    }

    fn controller_epoch(&self) -> u64 {
        self.lease.state.read().epoch
    }

    fn attach_project(&self, root: &std::path::Path) -> Result<String, KernelProblem> {
        // 与 InProcessKernelRuntime::open_project 同一装配路径:打开
        // Project Store 并注册投影 target;幂等语义由 service registry
        // 的 path 复用承载(同路径重挂返回既有 handle)。
        let store = mf_agent::Store::open(&mf_agent::project_db_path(root))
            .map_err(|error| KernelProblem::ServiceUnavailable(format!("{error:#}")))?;
        let project = self.register_project_store(root, store)?;
        Ok(project.as_str().to_string())
    }

    fn detach_project(&self, project_handle: &str) -> Result<(), KernelProblem> {
        let handle = crate::handles::ProjectStoreHandle::parse(project_handle)
            .map_err(|error| KernelProblem::ResourceNotFound)?;
        self.unregister_project_store(&handle)
    }
}

impl InProcessCoreKernel {
    fn dispatch_workflow_run(
        &self,
        request: KernelCommandRequest,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.dispatch_workflow_run_sealed(request, None, None)
    }

    fn dispatch_workflow_run_for_close(
        &self,
        request: KernelCommandRequest,
        close: &ProjectCloseToken,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.dispatch_workflow_run_sealed(request, None, Some(close))
    }

    /// 契约测试故障注入缝隙:模拟 Start accept 链在指定线性化点崩溃。
    #[cfg(test)]
    pub(crate) fn dispatch_workflow_run_with_accept_fault(
        &self,
        request: KernelCommandRequest,
        fault: Option<OperationAcceptFaultPoint>,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.dispatch_workflow_run_sealed(request, fault, None)
    }

    fn dispatch_workflow_run_sealed(
        &self,
        request: KernelCommandRequest,
        accept_fault: Option<OperationAcceptFaultPoint>,
        close: Option<&ProjectCloseToken>,
    ) -> Result<KernelOutcome, KernelProblem> {
        let KernelCommand::WorkflowRun(command) = request.command.clone() else {
            return Err(KernelProblem::InvalidEnvelope(
                "run dispatcher 收到其它命令".into(),
            ));
        };
        if let WorkflowRunCommand::Start { goal, .. } = &command {
            if goal.trim().is_empty() {
                return Err(KernelProblem::InvalidEnvelope(
                    "Workflow Run goal 不能为空".into(),
                ));
            }
        }
        let project = workflow_run_project(&command).clone();
        let target_aggregate = workflow_run_target(&command)?;
        let expected = workflow_run_expected_revisions(&command)?;
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
            command.command_type(),
            CommandPayload::Plain(workflow_run_payload(&command)),
        )?;
        let registration = match close {
            Some(close) => {
                if close.project != project || !matches!(command, WorkflowRunCommand::Cancel { .. })
                {
                    return Err(KernelProblem::InvalidEnvelope(
                        "ProjectCloseToken 只允许取消其绑定 Project 的 Workflow Run".into(),
                    ));
                }
                self.project_registration_for_close(close)?
            }
            None => self.project_registration(&project)?,
        };
        if matches!(command, WorkflowRunCommand::Start { .. }) {
            // Start 走 Operation accept 链,但授权契约与其它 run 命令一致:
            // Observer/旧 epoch 在进入任何 durable 写之前 fail-closed。
            registration.target.with_tx(|tx| {
                let permit = self.authorizer.acquire(tx, &envelope.lease_check())?;
                permit.validate_expected(tx, &envelope.lease_check())
            })?;
            let WorkflowRunCommand::Start {
                workflow,
                goal,
                expected_semantic_revision,
                ..
            } = &command
            else {
                unreachable!("checked Start above");
            };
            let port = self
                .workflow_start_ports
                .read()
                .get(project.as_str())
                .cloned();
            // 生产 Orchestrator adapter 落地前显式 fail-closed:没有可信
            // 编译 seam 就没有 durable plan,更不能把内存闭包伪装成可恢复
            // Operation。
            let Some(port) = port else {
                return Err(KernelProblem::ServiceUnavailable(
                    "workflow_start_port_not_registered".into(),
                ));
            };
            let outcome = self.dispatch_workflow_start_accept(
                &request.command_id,
                &project,
                workflow,
                goal.trim(),
                *expected_semantic_revision,
                &registration,
                &envelope,
                port,
                accept_fault,
            )?;
            // durable acceptance 已提交且已构造稳定 handle 后才允许排队。
            // worker 不持有调用栈内 closure，只凭 service-v1 durable plan
            // 重建；因此即使此刻进程退出也能在下一次 open_project 恢复。
            if let KernelOutcome::Accepted { operation_handle } = &outcome {
                if let Some(worker) = self.workflow_start_worker.read().clone() {
                    worker.enqueue(project, operation_handle.clone());
                }
            }
            return Ok(outcome);
        }
        // Observer 不得借「port 未注册」分支绕过 Controller 复验。
        registration.target.with_tx(|tx| {
            let _permit = self.authorizer.acquire(tx, &envelope.lease_check())?;
            Ok(())
        })?;
        let port = self
            .run_lifecycle_ports
            .read()
            .get(project.as_str())
            .cloned();
        if port.is_none() {
            registration.target.with_tx(|tx| {
                let permit = self.authorizer.acquire(tx, &envelope.lease_check())?;
                permit.validate_expected(tx, &envelope.lease_check())?;
                validate_workflow_run_scope_tx(tx, &command)
            })?;
            return Err(KernelProblem::ServiceUnavailable(
                "run_lifecycle_port_not_registered".into(),
            ));
        }
        let port = port.expect("checked above");
        if matches!(command, WorkflowRunCommand::Respond { .. })
            && !port.supports_question_bound_answers()
        {
            return Err(KernelProblem::ServiceUnavailable(
                "question_bound_answer_port_not_registered".into(),
            ));
        }

        // 已有 applied receipt 的同 id retry 不重做 prepare；它只从
        // pending outbox 重投 durable actions 并收口发布。
        let replay = self
            .coordinator
            .replay_if_applied(&envelope, &registration.target, &self.authorizer)
            .map_err(KernelProblem::from)?;
        // Cancel 的 durable fence 必须先于任何 Runtime stop。进程内 gate
        // 仅消除同 command 并发重复 stop；崩溃恢复依赖 Project Store 中
        // 的 stopping/outcome 行，而不依赖此内存锁。
        let cancel_gate =
            if replay.is_none() && matches!(command, WorkflowRunCommand::Cancel { .. }) {
                let gate = self
                    .cancel_gates
                    .lock()
                    .entry(request.command_id.as_str().to_owned())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
                Some(gate)
            } else {
                None
            };
        let _cancel_guard = cancel_gate.as_ref().map(|gate| gate.lock());
        let preparation = if replay.is_some() {
            None
        } else if let WorkflowRunCommand::Cancel {
            workflow_run,
            expected,
            ..
        } = &command
        {
            let task_id = registration.target.with_tx(|tx| {
                // reserve 与完整 expected/scope 复验在同一 IMMEDIATE tx；
                // 这是 Cancel 的线性化点。
                let permit = self.authorizer.acquire(tx, &envelope.lease_check())?;
                permit.validate_expected(tx, &envelope.lease_check())?;
                validate_workflow_run_scope_tx(tx, &command)?;
                let task_id = workflow_run_id_tx(tx, workflow_run)?;
                let targets = expected
                    .agent_runs
                    .iter()
                    .map(|run| {
                        Ok((
                            agent_run_id_tx(tx, &run.handle)?,
                            run.handle.as_str().to_owned(),
                            i64::try_from(run.revision).map_err(|_| {
                                CommandProblem::InvalidEnvelope("Agent Run revision 溢出".into())
                            })?,
                        ))
                    })
                    .collect::<Result<Vec<_>, CommandProblem>>()?;
                Store::reserve_cancel_fence_tx(tx, request.command_id.as_str(), task_id, &targets)
                    .map_err(run_domain_problem)?;
                Ok(task_id)
            })?;
            let targets = registration.target.with_tx(|tx| {
                Store::reserve_cancel_fence_tx(tx, request.command_id.as_str(), task_id, &[])
                    .map_err(run_domain_problem)
            })?;
            let mut run_stops = Vec::with_capacity(targets.len());
            for target in targets {
                use mf_agent::CancelFenceTargetState;
                match target.state {
                    CancelFenceTargetState::Confirmed => {
                        run_stops.push(crate::run_lifecycle::PreparedRunStop {
                            agent_run: AgentRunHandle::parse(target.run_handle)
                                .map_err(|e| KernelProblem::Internal(e.to_string()))?,
                            outcome: mf_agent::RunStopOutcome::Confirmed,
                        });
                        continue;
                    }
                    CancelFenceTargetState::Unconfirmed => {
                        run_stops.push(crate::run_lifecycle::PreparedRunStop {
                            agent_run: AgentRunHandle::parse(target.run_handle)
                                .map_err(|e| KernelProblem::Internal(e.to_string()))?,
                            outcome: mf_agent::RunStopOutcome::Unconfirmed,
                        });
                        continue;
                    }
                    CancelFenceTargetState::Pending => {
                        registration.target.with_tx(|tx| {
                            if Store::claim_cancel_target_tx(
                                tx,
                                request.command_id.as_str(),
                                target.run_id,
                            )
                            .map_err(run_domain_problem)?
                            {
                                Ok(())
                            } else {
                                Err(CommandProblem::CommandInProgress)
                            }
                        })?;
                    }
                    // crash-after-claim:当前进程 gate 保证这不是并发调用，
                    // 可安全按同 opaque run identity 重做幂等 stop。
                    CancelFenceTargetState::Stopping => {}
                }
                let handle = AgentRunHandle::parse(target.run_handle)
                    .map_err(|e| KernelProblem::Internal(e.to_string()))?;
                let outcome = port
                    .stop_cancel_target(&request.command_id, &handle)
                    .unwrap_or(mf_agent::RunStopOutcome::Unconfirmed);
                registration.target.with_tx(|tx| {
                    Store::record_cancel_outcome_tx(
                        tx,
                        request.command_id.as_str(),
                        target.run_id,
                        outcome,
                    )
                    .map_err(run_domain_problem)
                })?;
                run_stops.push(crate::run_lifecycle::PreparedRunStop {
                    agent_run: handle,
                    outcome,
                });
            }
            Some(RunPreparation::Cancel { run_stops })
        } else {
            // 非 Cancel 也必须在 port.prepare 前复验完整 CAS/scope；
            // applied receipt replay 则跳过 expected，避免已推进 revision
            // 把同 command 幂等重放误判为 stale。
            registration.target.with_tx(|tx| {
                let permit = self.authorizer.acquire(tx, &envelope.lease_check())?;
                permit.validate_expected(tx, &envelope.lease_check())?;
                validate_workflow_run_scope_tx(tx, &command)
            })?;
            Some(port.prepare(&request.command_id, &command)?)
        };

        let targets = self.registered_targets();
        self.projections.linearize_run_actions(&targets, |hub| {
            let outcome = if let Some(replay) = replay {
                replay
            } else {
                self.coordinator
                    .dispatch_internal(
                        &envelope,
                        &registration.target,
                        &self.authorizer,
                        |tx| {
                            run_lifecycle_effect(
                                tx,
                                &request.command_id,
                                &command,
                                preparation.as_ref().expect("non-replay prepared"),
                            )
                        },
                        None,
                        || {},
                    )
                    .map_err(KernelProblem::from)?
            };
            if let Err(error) = drain_run_action_outbox(&registration.target, port.as_ref()) {
                hub.block_for_run_actions(&registration.target);
                return Err(error);
            }
            hub.clear_run_action_block(&registration.target);
            hub.publish_pending(&registration.target)?;
            let CommandOutcome::Applied {
                result_revisions,
                replayed,
            } = outcome;
            let revision = result_revisions
                .get("revision")
                .and_then(Value::as_u64)
                .ok_or_else(|| KernelProblem::Internal("run result 缺少 target revision".into()))?;
            Ok(KernelOutcome::RunApplied { revision, replayed })
        })
    }

    /// Workflow Start 的 accept 链:可信 port 编译 prepared plan → durable
    /// frozen payload 落 `saga_state` → 两阶段 accept(service intent/plan +
    /// initiating target acceptance receipt/outbox)。acceptance receipt
    /// 提交后立即返回 `202 accepted`;完整启动绝不在这里同步执行。
    #[allow(clippy::too_many_arguments)]
    fn dispatch_workflow_start_accept(
        &self,
        command_id: &CommandId,
        project: &ProjectStoreHandle,
        workflow: &WorkflowHandle,
        goal: &str,
        semantic_revision: u64,
        registration: &ProjectRegistration,
        envelope: &CommandEnvelope,
        port: Arc<dyn crate::workflow_start::WorkflowStartPort>,
        fault: Option<OperationAcceptFaultPoint>,
    ) -> Result<KernelOutcome, KernelProblem> {
        let prepared = port.prepare(command_id, workflow, goal)?;
        if prepared.workflow != *workflow {
            return Err(KernelProblem::ValidationFailed(
                "prepared Workflow Start plan 与目标工作流不一致".into(),
            ));
        }
        let payload = crate::workflow_start::workflow_start_payload(&prepared)?;
        let plan = crate::workflow_start::compile_workflow_start_plan(
            command_id,
            project,
            workflow,
            semantic_revision,
            &payload,
        )?;
        let coordinator =
            OperationCoordinator::new(self.service.clone(), self.idempotency_key.clone());
        let targets = self.registered_targets();
        self.projections.linearize(&targets, |hub| {
            #[cfg(test)]
            let accepted = match fault {
                Some(point) => coordinator.accept_with_fault(
                    envelope,
                    &plan,
                    &registration.target,
                    &self.authorizer,
                    point,
                ),
                None => coordinator.accept(envelope, &plan, &registration.target, &self.authorizer),
            };
            #[cfg(not(test))]
            let accepted = {
                let _ = fault;
                coordinator.accept(envelope, &plan, &registration.target, &self.authorizer)
            };
            let handle = accepted.map_err(KernelProblem::from)?;
            // acceptance 的 durable 证据是 target receipt;outbox 里的
            // acceptance 事件不是 projection delta(journal 只发布可解释的
            // 投影事件),在 L-PUBLISH 内移除。若此处前崩溃,遗留行由
            // startup reconcile 标记 reconciled 收尾,不毒化 publication。
            registration.target.with_tx(|tx| {
                tx.execute(
                    "DELETE FROM projection_outbox
                     WHERE published_at IS NULL
                       AND instr(event_json, ?1) > 0
                       AND instr(event_json, ?2) > 0",
                    rusqlite::params![
                        format!("\"type\":\"{}.accepted\"", envelope.command_type().as_str()),
                        format!(
                            "\"caused_by_command_id\":\"{}\"",
                            envelope.command_id().as_str()
                        ),
                    ],
                )
                .map_err(|error| CommandProblem::Internal(error.to_string()))
            })?;
            // step 的业务投影(WorkflowRun replace)与 DispatchReady 由
            // worker 发布;这里只收口 acceptance 后的空 outbox。
            hub.publish_pending(&registration.target)?;
            Ok(KernelOutcome::Accepted {
                operation_handle: handle,
            })
        })
    }

    /// 注册按 Project 的 Workflow Start 编译 port。生产 Orchestrator
    /// adapter 落地前不注册,`WorkflowRunCommand::Start` 保持 fail-closed。
    pub(crate) fn register_workflow_start_port(
        &self,
        project: &ProjectStoreHandle,
        port: Arc<dyn crate::workflow_start::WorkflowStartPort>,
    ) -> Result<(), KernelProblem> {
        // lifecycle port 必须先就绪；否则 worker 可能完成业务事务却无法投递
        // DispatchReady。生产 open_project 按此顺序装配。
        if !self
            .run_lifecycle_ports
            .read()
            .contains_key(project.as_str())
        {
            return Err(KernelProblem::ServiceUnavailable(
                "run_lifecycle_port_not_registered".into(),
            ));
        }
        self.project_registration(project)?;
        self.workflow_start_ports
            .write()
            .insert(project.as_str().to_owned(), port);
        self.resume_workflow_start_operations(project);
        Ok(())
    }

    pub(crate) fn unregister_workflow_start_port(&self, project: &ProjectStoreHandle) {
        self.workflow_start_ports.write().remove(project.as_str());
    }

    /// 扫描 service-v1 中属于该 Project 的未终态 Start Operation。只把
    /// handle 入队；payload/step/effect 均由 worker 重新读取并复验。
    fn resume_workflow_start_operations(&self, project: &ProjectStoreHandle) {
        let Some(worker) = self.workflow_start_worker.read().clone() else {
            return;
        };
        let handles = match crate::operation::open_operations(&self.service) {
            Ok(handles) => handles,
            Err(error) => {
                log::error!("扫描 Workflow Start Operation 失败:{error}");
                return;
            }
        };
        let project_store = format!("project:{}", project.as_str());
        for operation in handles {
            let belongs = operation_of(&self.service, &operation)
                .ok()
                .filter(|record| {
                    record.kind.as_str() == crate::workflow_start::WORKFLOW_START_OPERATION_KIND
                })
                .and_then(|_| steps_of(&self.service, &operation).ok())
                .is_some_and(|steps| steps.iter().any(|step| step.target_store == project_store));
            if belongs {
                worker.enqueue(project.clone(), operation);
            }
        }
    }

    /// Workflow Start 后台 worker:在 L-PUBLISH 临界区内执行 Operation
    /// steps。全部执行材料(durable payload、step 身份、effects)从
    /// service/target 的 durable 行重建;已持有 receipt 的 step 被跳过,
    /// 同 command_id 绝不重复创建 Task/Workflow Run。启动前失败的完整回滚
    /// 交由 saga 补偿(discard step)+ port 的资源释放钩子。
    #[allow(dead_code)] // 由调度循环/契约测试驱动;生产装配在 port 注册后接线。
    pub(crate) fn run_workflow_start_operation(
        &self,
        project: &ProjectStoreHandle,
        operation: &OperationHandle,
    ) -> Result<OperationOutcome, KernelProblem> {
        self.run_workflow_start_operation_sealed(project, operation, None)
    }

    #[cfg(test)]
    pub(crate) fn run_workflow_start_operation_with_fault(
        &self,
        project: &ProjectStoreHandle,
        operation: &OperationHandle,
        fault: Option<OperationFaultPoint>,
    ) -> Result<OperationOutcome, KernelProblem> {
        self.run_workflow_start_operation_sealed(project, operation, fault)
    }

    #[allow(dead_code)] // 与 run_workflow_start_operation 同一生命周期。
    fn run_workflow_start_operation_sealed(
        &self,
        project: &ProjectStoreHandle,
        operation: &OperationHandle,
        fault: Option<OperationFaultPoint>,
    ) -> Result<OperationOutcome, KernelProblem> {
        let registration = self.project_registration(project)?;
        let start_port = self
            .workflow_start_ports
            .read()
            .get(project.as_str())
            .cloned()
            .ok_or_else(|| {
                KernelProblem::ServiceUnavailable("workflow_start_port_not_registered".into())
            })?;
        let lifecycle_port = self
            .run_lifecycle_ports
            .read()
            .get(project.as_str())
            .cloned()
            .ok_or_else(|| {
                KernelProblem::ServiceUnavailable("run_lifecycle_port_not_registered".into())
            })?;
        let record = operation_of(&self.service, operation).map_err(KernelProblem::from)?;
        // durable payload 是重建的唯一依据;缺失即数据完整性问题,fail-closed。
        let payload = durable_payload(&self.service, operation)
            .map_err(KernelProblem::from)?
            .ok_or_else(|| {
                KernelProblem::Internal("Workflow Start operation 缺少 durable payload".into())
            })?;
        let steps = steps_of(&self.service, operation).map_err(KernelProblem::from)?;
        let command_id = record.command_id.clone();
        let (plan, prepared) = crate::workflow_start::rebuild_workflow_start_plan(
            &command_id,
            project,
            &payload,
            &steps,
        )?;
        let effects = crate::workflow_start::workflow_start_effects(&prepared, &steps)?;
        let coordinator =
            OperationCoordinator::new(self.service.clone(), self.idempotency_key.clone());
        let targets = self.registered_targets();
        let outcome = self.projections.linearize_run_actions(&targets, |hub| {
            // 已终态:幂等返回既有终态,但先收口遗留 run actions
            // (上次 run 成功后投递失败的场景),绝不重跑 steps。
            let outcome = if record.state.is_terminal() {
                terminal_outcome_of(&record)
            } else {
                coordinator
                    .run(operation, &plan, &targets, &self.authorizer, effects, fault)
                    .map_err(|error| {
                        // step target 可能已在故障窗口提交:未投递的 run actions
                        // 必须阻断 publication,等待重试/重启重投,绝不丢弃。
                        if target_has_pending_run_actions(&registration.target).unwrap_or(true) {
                            hub.block_for_run_actions(&registration.target);
                        }
                        KernelProblem::from(error)
                    })?
            };
            if let Err(error) =
                drain_run_action_outbox(&registration.target, lifecycle_port.as_ref())
            {
                hub.block_for_run_actions(&registration.target);
                return Err(error);
            }
            hub.clear_run_action_block(&registration.target);
            hub.publish_pending(&registration.target)?;
            Ok(outcome)
        })?;
        // 「目标未达成但已完整回滚」:调度从未开始,释放本次 start 固定的
        // 外部资源(pins);失败不掩盖既有终态,只记录待人工清理。
        if matches!(outcome, OperationOutcome::Completed { compensated: true }) {
            if let Err(error) = start_port.release_pre_start_resources(&command_id, &prepared) {
                log::warn!("Workflow Start {operation} 补偿后释放外部资源失败:{error:#}");
            }
        }
        Ok(outcome)
    }

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

/// 已终态 operation 的幂等观测映射(worker 重复调用/测试断言用)。
#[allow(dead_code)] // 与 run_workflow_start_operation 同一生命周期。
fn terminal_outcome_of(record: &OperationRecord) -> OperationOutcome {
    match record.state {
        OperationState::NeedsYou => OperationOutcome::NeedsYou {
            problem_code: record
                .progress
                .problem
                .as_ref()
                .map(|problem| problem.code.clone())
                .unwrap_or_else(|| "operation_needs_you".to_string()),
        },
        OperationState::Completed if record.progress.outcome.as_deref() == Some("compensated") => {
            OperationOutcome::Completed { compensated: true }
        }
        OperationState::Completed if record.progress.outcome.as_deref() == Some("not_accepted") => {
            OperationOutcome::NotAccepted {
                problem_code: record
                    .progress
                    .problem
                    .as_ref()
                    .map(|problem| problem.code.clone())
                    .unwrap_or_else(|| "operation_not_accepted".to_string()),
            }
        }
        _ => OperationOutcome::Completed { compensated: false },
    }
}

fn workflow_run_project(command: &WorkflowRunCommand) -> &ProjectStoreHandle {
    match command {
        WorkflowRunCommand::Start { project, .. }
        | WorkflowRunCommand::Cancel { project, .. }
        | WorkflowRunCommand::RetryStep { project, .. }
        | WorkflowRunCommand::SkipStep { project, .. }
        | WorkflowRunCommand::Respond { project, .. }
        | WorkflowRunCommand::Settle { project, .. } => project,
    }
}

fn workflow_run_target(command: &WorkflowRunCommand) -> Result<AggregateRef, KernelProblem> {
    let (kind, handle) = match command {
        WorkflowRunCommand::Start { workflow, .. } => {
            (AggregateKind::ProjectWorkflow, workflow.as_str())
        }
        WorkflowRunCommand::Cancel { workflow_run, .. } => {
            (AggregateKind::WorkflowRun, workflow_run.as_str())
        }
        WorkflowRunCommand::RetryStep { step, .. }
        | WorkflowRunCommand::SkipStep { step, .. }
        | WorkflowRunCommand::Respond { step, .. } => (AggregateKind::Step, step.as_str()),
        WorkflowRunCommand::Settle { agent_run, .. } => {
            (AggregateKind::AgentRun, agent_run.as_str())
        }
    };
    AggregateRef::new(kind, handle.to_owned())
        .map_err(|error| KernelProblem::Internal(error.to_string()))
}

fn workflow_run_expected_revisions(
    command: &WorkflowRunCommand,
) -> Result<Vec<ExpectedRevision>, KernelProblem> {
    let one = |kind: AggregateKind, handle: &str, axis: &str, revision: u64| ExpectedRevision {
        aggregate: AggregateRef::new(kind, handle.to_owned())
            .expect("validated typed handle cannot be empty"),
        revisions: [(axis.to_owned(), revision)].into_iter().collect(),
    };
    let WorkflowRunCommand::Start {
        workflow,
        expected_semantic_revision,
        ..
    } = command
    else {
        let (workflow_run, expected, required_step, required_agent_run) = match command {
            WorkflowRunCommand::Cancel {
                workflow_run,
                expected,
                ..
            } => (workflow_run, expected, None, None),
            WorkflowRunCommand::RetryStep {
                workflow_run,
                step,
                expected,
                ..
            }
            | WorkflowRunCommand::SkipStep {
                workflow_run,
                step,
                expected,
                ..
            }
            | WorkflowRunCommand::Respond {
                workflow_run,
                step,
                expected,
                ..
            } => (workflow_run, expected, Some(step), None),
            WorkflowRunCommand::Settle {
                workflow_run,
                step,
                agent_run,
                expected,
                ..
            } => (workflow_run, expected, Some(step), Some(agent_run)),
            WorkflowRunCommand::Start { .. } => unreachable!(),
        };
        if required_step
            .is_some_and(|required| !expected.steps.iter().any(|item| &item.handle == required))
        {
            return Err(KernelProblem::InvalidEnvelope(
                "expected 缺少目标 Step".into(),
            ));
        }
        if required_agent_run.is_some_and(|required| {
            !expected
                .agent_runs
                .iter()
                .any(|item| &item.handle == required)
        }) {
            return Err(KernelProblem::InvalidEnvelope(
                "expected 缺少目标 Agent Run".into(),
            ));
        }
        let mut out = vec![one(
            AggregateKind::WorkflowRun,
            workflow_run.as_str(),
            "revision",
            expected.workflow_run_revision,
        )];
        out.extend(expected.steps.iter().map(|item| {
            one(
                AggregateKind::Step,
                item.handle.as_str(),
                "revision",
                item.revision,
            )
        }));
        out.extend(expected.agent_runs.iter().map(|item| {
            one(
                AggregateKind::AgentRun,
                item.handle.as_str(),
                "revision",
                item.revision,
            )
        }));
        out.extend(expected.agent_sessions.iter().map(|item| {
            one(
                AggregateKind::AgentSession,
                item.handle.as_str(),
                "revision",
                item.revision,
            )
        }));
        return Ok(out);
    };
    Ok(vec![one(
        AggregateKind::ProjectWorkflow,
        workflow.as_str(),
        "semantic_revision",
        *expected_semantic_revision,
    )])
}

pub(crate) fn workflow_run_payload(command: &WorkflowRunCommand) -> Value {
    match command {
        WorkflowRunCommand::Start { workflow, goal, .. } => {
            serde_json::json!({"workflow": workflow.as_str(), "goal": goal.trim()})
        }
        WorkflowRunCommand::Cancel { workflow_run, .. } => {
            serde_json::json!({"workflow_run": workflow_run.as_str()})
        }
        WorkflowRunCommand::RetryStep {
            workflow_run,
            step,
            mode,
            ..
        } => serde_json::json!({
            "workflow_run": workflow_run.as_str(),
            "step": step.as_str(),
            "mode": match mode {
                mf_agent::RetryMode::ContinueSession => "continue_session",
                mf_agent::RetryMode::FreshSession => "fresh_session",
            },
        }),
        WorkflowRunCommand::SkipStep {
            workflow_run, step, ..
        } => serde_json::json!({
            "workflow_run": workflow_run.as_str(),
            "step": step.as_str(),
        }),
        WorkflowRunCommand::Respond {
            workflow_run,
            step,
            question_id,
            answer,
            ..
        } => serde_json::json!({
            "workflow_run": workflow_run.as_str(),
            "step": step.as_str(),
            "question_id": question_id.to_string(),
            "answer": answer,
        }),
        WorkflowRunCommand::Settle {
            workflow_run,
            step,
            agent_run,
            settlement,
            ..
        } => serde_json::json!({
            "workflow_run": workflow_run.as_str(),
            "step": step.as_str(),
            "agent_run": agent_run.as_str(),
            "settlement": settlement,
        }),
    }
}

fn validate_workflow_run_scope_tx(
    tx: &Transaction<'_>,
    command: &WorkflowRunCommand,
) -> Result<(), CommandProblem> {
    let WorkflowRunCommand::Start { .. } = command else {
        let (workflow_run, expected) = match command {
            WorkflowRunCommand::Cancel {
                workflow_run,
                expected,
                ..
            }
            | WorkflowRunCommand::RetryStep {
                workflow_run,
                expected,
                ..
            }
            | WorkflowRunCommand::SkipStep {
                workflow_run,
                expected,
                ..
            }
            | WorkflowRunCommand::Respond {
                workflow_run,
                expected,
                ..
            }
            | WorkflowRunCommand::Settle {
                workflow_run,
                expected,
                ..
            } => (workflow_run, expected),
            WorkflowRunCommand::Start { .. } => unreachable!(),
        };
        let task_id = tx
            .query_row(
                "SELECT id FROM agent_tasks WHERE public_handle=?1",
                [workflow_run.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| CommandProblem::Internal(error.to_string()))?
            .ok_or(CommandProblem::ResourceNotFound)?;
        let step_id = match command {
            WorkflowRunCommand::RetryStep { step, .. }
            | WorkflowRunCommand::SkipStep { step, .. }
            | WorkflowRunCommand::Respond { step, .. }
            | WorkflowRunCommand::Settle { step, .. } => Some(scope_step_id(tx, step, task_id)?),
            WorkflowRunCommand::Cancel { .. } => None,
            WorkflowRunCommand::Start { .. } => unreachable!(),
        };
        if let Some(step) = match command {
            WorkflowRunCommand::RetryStep { step, .. }
            | WorkflowRunCommand::SkipStep { step, .. }
            | WorkflowRunCommand::Respond { step, .. }
            | WorkflowRunCommand::Settle { step, .. } => Some(step),
            WorkflowRunCommand::Cancel { .. } => None,
            WorkflowRunCommand::Start { .. } => unreachable!(),
        } {
            let target = [step.as_str().to_owned()].into_iter().collect();
            ensure_expected_set(
                &target,
                expected.steps.iter().map(|item| item.handle.as_str()),
                "Step",
            )?;
        }
        match command {
            WorkflowRunCommand::Cancel { .. } => {
                let steps = query_handle_set(
                    tx,
                    "SELECT public_handle FROM steps
                     WHERE revision_id=(SELECT active_revision FROM agent_tasks WHERE id=?1)
                       AND status NOT IN ('succeeded','failed','skipped','cancelled')",
                    task_id,
                )?;
                let runs = query_handle_set(
                    tx,
                    "SELECT public_handle FROM agent_runs WHERE task_id=?1 AND status IN ('running','awaiting-outcome')",
                    task_id,
                )?;
                ensure_expected_set(
                    &steps,
                    expected.steps.iter().map(|item| item.handle.as_str()),
                    "Step",
                )?;
                ensure_expected_set(
                    &runs,
                    expected.agent_runs.iter().map(|item| item.handle.as_str()),
                    "Agent Run",
                )?;
                ensure_session_set_for_runs(tx, &runs, expected)?;
            }
            WorkflowRunCommand::RetryStep { mode, .. } => {
                let step_id = step_id.expect("retry has step");
                let runs = query_handle_set(
                    tx,
                    "SELECT public_handle FROM agent_runs WHERE step_id=?1 AND status IN ('running','awaiting-outcome')",
                    step_id,
                )?;
                ensure_expected_set(
                    &runs,
                    expected.agent_runs.iter().map(|item| item.handle.as_str()),
                    "Agent Run",
                )?;
                let active_sessions = session_set_for_runs(tx, &runs)?;
                let expected_sessions = expected
                    .agent_sessions
                    .iter()
                    .map(|item| item.handle.as_str().to_owned())
                    .collect::<BTreeSet<_>>();
                match mode {
                    mf_agent::RetryMode::FreshSession => ensure_expected_set(
                        &active_sessions,
                        expected
                            .agent_sessions
                            .iter()
                            .map(|item| item.handle.as_str()),
                        "Agent Session",
                    )?,
                    mf_agent::RetryMode::ContinueSession => {
                        if !active_sessions.is_subset(&expected_sessions)
                            || expected_sessions.len() > active_sessions.len().saturating_add(1)
                        {
                            return Err(CommandProblem::InvalidEnvelope(
                                "ContinueSession expected 必须包含 active session，且至多增加一个继续目标".into(),
                            ));
                        }
                        let live_sessions = query_handle_set(
                            tx,
                            "SELECT DISTINCT s.public_handle
                             FROM agent_runs r JOIN agent_sessions s ON s.id=r.session_id
                             WHERE r.step_id=?1 AND s.status NOT IN ('dead','hidden')",
                            step_id,
                        )?;
                        if !expected_sessions.is_subset(&live_sessions) {
                            return Err(CommandProblem::InvalidEnvelope(
                                "ContinueSession expected 含不属于该 Step 的存活会话".into(),
                            ));
                        }
                    }
                }
            }
            WorkflowRunCommand::SkipStep { .. } => {
                let step_id = step_id.expect("skip has step");
                let status: String = tx
                    .query_row("SELECT status FROM steps WHERE id=?1", [step_id], |row| {
                        row.get(0)
                    })
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                if !matches!(status.as_str(), "failed" | "blocked") {
                    return Err(CommandProblem::InvalidEnvelope(
                        "skip 目标 Step 必须处于 failed/blocked".into(),
                    ));
                }
                let runs = query_handle_set(
                    tx,
                    "SELECT public_handle FROM agent_runs
                     WHERE step_id=?1 AND status IN ('running','awaiting-outcome')",
                    step_id,
                )?;
                ensure_expected_set(
                    &runs,
                    expected.agent_runs.iter().map(|item| item.handle.as_str()),
                    "Agent Run",
                )?;
                ensure_session_set_for_runs(tx, &runs, expected)?;
            }
            WorkflowRunCommand::Respond { .. } => {
                let step_id = step_id.expect("respond has step");
                let question_runs = query_optional_handle_set(
                    tx,
                    "SELECT r.public_handle
                     FROM step_questions q
                     LEFT JOIN agent_runs r ON r.id=q.run_id
                     WHERE q.task_id=?1 AND q.step_id=?2 AND q.status='open'",
                    task_id,
                    step_id,
                )?;
                if question_runs.len() != 1 {
                    return Err(CommandProblem::InvalidEnvelope(
                        "respond 要求目标 Step 恰有一个 open question".into(),
                    ));
                }
                let status: String = tx
                    .query_row("SELECT status FROM steps WHERE id=?1", [step_id], |row| {
                        row.get(0)
                    })
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                if status != "needs-input" {
                    return Err(CommandProblem::InvalidEnvelope(
                        "respond 目标 Step 必须处于 needs-input".into(),
                    ));
                }
                let runs: BTreeSet<String> = question_runs.into_iter().flatten().collect();
                ensure_expected_set(
                    &runs,
                    expected.agent_runs.iter().map(|item| item.handle.as_str()),
                    "Agent Run",
                )?;
                ensure_session_set_for_runs(tx, &runs, expected)?;
            }
            WorkflowRunCommand::Settle { agent_run, .. } => {
                let step_id = step_id.expect("settle has step");
                let run_step_key = tx
                    .query_row(
                        "SELECT s.step_key
                         FROM agent_runs r JOIN steps s ON s.id=r.step_id
                         WHERE r.public_handle=?1 AND r.task_id=?2",
                        rusqlite::params![agent_run.as_str(), task_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?
                    .ok_or(CommandProblem::ResourceNotFound)?;
                let active_step_key = tx
                    .query_row(
                        "SELECT s.step_key
                         FROM steps s JOIN agent_tasks t ON t.active_revision=s.revision_id
                         WHERE s.id=?1 AND t.id=?2",
                        rusqlite::params![step_id, task_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?
                    .ok_or(CommandProblem::ResourceNotFound)?;
                if run_step_key != active_step_key {
                    return Err(CommandProblem::ResourceNotFound);
                }
                let runs = [agent_run.as_str().to_owned()].into_iter().collect();
                ensure_expected_set(
                    &runs,
                    expected.agent_runs.iter().map(|item| item.handle.as_str()),
                    "Agent Run",
                )?;
                ensure_session_set_for_runs(tx, &runs, expected)?;
            }
            WorkflowRunCommand::Start { .. } => unreachable!(),
        }
        return Ok(());
    };
    Ok(())
}

fn scope_step_id(
    tx: &Transaction<'_>,
    step: &StepHandle,
    task_id: i64,
) -> Result<i64, CommandProblem> {
    tx.query_row(
        "SELECT id FROM steps WHERE public_handle=?1 AND task_id=?2",
        rusqlite::params![step.as_str(), task_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| CommandProblem::Internal(error.to_string()))?
    .ok_or(CommandProblem::ResourceNotFound)
}

fn query_handle_set(
    tx: &Transaction<'_>,
    sql: &str,
    id: i64,
) -> Result<BTreeSet<String>, CommandProblem> {
    let mut stmt = tx
        .prepare(sql)
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
    let rows = stmt
        .query_map([id], |row| row.get(0))
        .map_err(|error| CommandProblem::Internal(error.to_string()))?
        .collect::<Result<_, _>>()
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
    Ok(rows)
}

fn query_optional_handle_set(
    tx: &Transaction<'_>,
    sql: &str,
    task_id: i64,
    step_id: i64,
) -> Result<Vec<Option<String>>, CommandProblem> {
    let mut stmt = tx
        .prepare(sql)
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params![task_id, step_id], |row| row.get(0))
        .map_err(|error| CommandProblem::Internal(error.to_string()))?
        .collect::<Result<_, _>>()
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
    Ok(rows)
}

fn ensure_expected_set<'a>(
    actual: &BTreeSet<String>,
    expected: impl Iterator<Item = &'a str>,
    label: &str,
) -> Result<(), CommandProblem> {
    let expected: BTreeSet<String> = expected.map(str::to_owned).collect();
    if actual != &expected {
        return Err(CommandProblem::InvalidEnvelope(format!(
            "expected {label} 并发前提不完整"
        )));
    }
    Ok(())
}

fn ensure_session_set_for_runs(
    tx: &Transaction<'_>,
    runs: &BTreeSet<String>,
    expected: &WorkflowRunExpected,
) -> Result<(), CommandProblem> {
    let sessions = session_set_for_runs(tx, runs)?;
    ensure_expected_set(
        &sessions,
        expected
            .agent_sessions
            .iter()
            .map(|item| item.handle.as_str()),
        "Agent Session",
    )
}

fn session_set_for_runs(
    tx: &Transaction<'_>,
    runs: &BTreeSet<String>,
) -> Result<BTreeSet<String>, CommandProblem> {
    let mut sessions = BTreeSet::new();
    for run in runs {
        let session = tx
            .query_row(
                "SELECT s.public_handle
                 FROM agent_runs r
                 LEFT JOIN agent_sessions s ON s.id=r.session_id
                 WHERE r.public_handle=?1",
                [run],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|error| CommandProblem::Internal(error.to_string()))?
            .flatten();
        if let Some(session) = session {
            sessions.insert(session);
        }
    }
    Ok(sessions)
}

fn run_control_expected_revisions(
    workflow_run: &WorkflowRunHandle,
    expected: &WorkflowRunExpected,
) -> Vec<ExpectedRevision> {
    let one = |kind: AggregateKind, handle: &str, revision: u64| ExpectedRevision {
        aggregate: AggregateRef::new(kind, handle.to_owned())
            .expect("typed RunControl handle cannot be empty"),
        revisions: [("revision".to_owned(), revision)].into_iter().collect(),
    };
    let mut values = vec![one(
        AggregateKind::WorkflowRun,
        workflow_run.as_str(),
        expected.workflow_run_revision,
    )];
    values.extend(
        expected
            .steps
            .iter()
            .map(|value| one(AggregateKind::Step, value.handle.as_str(), value.revision)),
    );
    values.extend(expected.agent_runs.iter().map(|value| {
        one(
            AggregateKind::AgentRun,
            value.handle.as_str(),
            value.revision,
        )
    }));
    values.extend(expected.agent_sessions.iter().map(|value| {
        one(
            AggregateKind::AgentSession,
            value.handle.as_str(),
            value.revision,
        )
    }));
    values
}

/// RunControl 的领域 effect：capability resolver/authorizer 只负责身份，
/// 所有权威变更仍在唯一 L-CMD target transaction 内完成并产出 receipt、
/// projection outbox 与 durable RunAction。
#[allow(clippy::too_many_arguments)]
fn run_control_mutation_effect(
    tx: &Transaction<'_>,
    _workflow_run: &WorkflowRunHandle,
    _agent_run: &AgentRunHandle,
    run_id: i64,
    task_id: i64,
    command: &crate::run_control::RunControlCommand,
    expected: &[ExpectedRevision],
    target: &AggregateRef,
) -> Result<EffectOutput, CommandProblem> {
    let mutation = match command {
        crate::run_control::RunControlCommand::Settle(settlement) => {
            mf_agent::RunMutation::Settle {
                run_id,
                settlement: settlement.clone(),
            }
        }
        crate::run_control::RunControlCommand::ReportState(state) => {
            mf_agent::RunMutation::ReportState {
                run_id,
                state: *state,
            }
        }
        crate::run_control::RunControlCommand::ProposePipeline(draft) => {
            mf_agent::RunMutation::ProposePipeline {
                task_id,
                draft: draft.clone(),
            }
        }
    };
    let mut result = Store::apply_run_mutation_tx(tx, mutation).map_err(run_domain_problem)?;
    if matches!(command, crate::run_control::RunControlCommand::Settle(_))
        && matches!(
            result.output,
            mf_agent::run_mutation::RunMutationOutput::Settled {
                already_applied: true,
                ..
            }
        )
    {
        result.actions.clear();
    }
    let draft_revision = match &result.output {
        mf_agent::run_mutation::RunMutationOutput::PipelineProposed { revision, .. } => {
            Some(revision.public_handle.clone())
        }
        _ => None,
    };
    let mut projections = Vec::new();
    let mut target_revision = None;
    for expected in expected {
        let Some(base) = expected.revisions.get("revision").copied() else {
            continue;
        };
        let (revision, data) = run_aggregate_snapshot_tx(tx, &expected.aggregate)?;
        if &expected.aggregate == target {
            target_revision = Some(revision);
        }
        if revision == base {
            continue;
        }
        projections.push(ProjectionEffect {
            aggregate: Some(expected.aggregate.clone()),
            event_type: Some(command.method().to_owned()),
            projection_critical: true,
            payload: serde_json::json!({
                "base_revision":{"revision":base},
                "aggregate_revision":{"revision":revision},
                "delta":{"mode":"replace","data":data},
            }),
            run_actions: Vec::new(),
        });
    }
    if !result.actions.is_empty() {
        let primary = projections.first_mut().ok_or_else(|| {
            CommandProblem::Internal("RunControl action 缺少可承载的权威投影".into())
        })?;
        primary.run_actions = result.actions;
    }
    let revision = target_revision
        .ok_or_else(|| CommandProblem::Internal("RunControl target revision 读取失败".into()))?;
    let mut result_revisions = serde_json::json!({"revision":revision});
    if let Some(draft_revision) = draft_revision {
        result_revisions["draft_revision"] = Value::String(draft_revision);
    }
    Ok(EffectOutput {
        result_revisions,
        projections,
    })
}

fn run_lifecycle_effect(
    tx: &Transaction<'_>,
    command_id: &CommandId,
    command: &WorkflowRunCommand,
    preparation: &RunPreparation,
) -> Result<EffectOutput, CommandProblem> {
    validate_workflow_run_scope_tx(tx, command)?;
    let mutation = match command {
        WorkflowRunCommand::Start { .. } => {
            return Err(CommandProblem::InvalidEnvelope(
                "workflow.run start 必须由 Operation 承载".into(),
            ));
        }
        WorkflowRunCommand::Cancel { workflow_run, .. } => {
            let RunPreparation::Cancel { run_stops } = preparation else {
                return Err(CommandProblem::InvalidEnvelope(
                    "cancel 缺少逐 Agent Run 的真实 runtime stop 结果".into(),
                ));
            };
            let stop_handles = run_stops
                .iter()
                .map(|stop| stop.agent_run.as_str().to_owned())
                .collect::<BTreeSet<_>>();
            if stop_handles.len() != run_stops.len() {
                return Err(CommandProblem::InvalidEnvelope(
                    "cancel runtime stop 结果包含重复 Agent Run".into(),
                ));
            }
            let WorkflowRunCommand::Cancel { expected, .. } = command else {
                unreachable!()
            };
            ensure_expected_set(
                &stop_handles,
                expected.agent_runs.iter().map(|item| item.handle.as_str()),
                "runtime stop Agent Run",
            )?;
            // fence 已在事务外 stop 前提交；finalize transaction 先把
            // fence 切到 finalizing 以打开 trigger gate，再消费 durable
            // outcomes。prepare 仅用于一致性复验，不是权威结果。
            let durable_stops = Store::begin_cancel_finalize_tx(tx, command_id.as_str())
                .map_err(run_domain_problem)?;
            let durable_handles = durable_stops
                .iter()
                .map(|stop| stop.run_id)
                .collect::<BTreeSet<_>>();
            let prepared_ids = run_stops
                .iter()
                .map(|stop| agent_run_id_tx(tx, &stop.agent_run))
                .collect::<Result<BTreeSet<_>, _>>()?;
            if durable_handles != prepared_ids {
                return Err(CommandProblem::InvalidEnvelope(
                    "cancel durable outcome 与 fenced target 不一致".into(),
                ));
            }
            mf_agent::RunMutation::Cancel {
                task_id: workflow_run_id_tx(tx, workflow_run)?,
                run_stops: durable_stops,
            }
        }
        WorkflowRunCommand::RetryStep {
            step,
            mode,
            expected,
            ..
        } => {
            let step_id = step_id_tx(tx, step)?;
            let continue_session_id = match mode {
                mf_agent::RetryMode::FreshSession => {
                    if preparation != &RunPreparation::Ready {
                        return Err(CommandProblem::InvalidEnvelope(
                            "fresh retry prepare 结果不匹配".into(),
                        ));
                    }
                    None
                }
                mf_agent::RetryMode::ContinueSession => {
                    let RunPreparation::ContinueSessionAlive { session } = preparation else {
                        return Err(CommandProblem::InvalidEnvelope(
                            "continue retry 缺少存活 session 确认".into(),
                        ));
                    };
                    if !expected
                        .agent_sessions
                        .iter()
                        .any(|item| item.handle == *session)
                    {
                        return Err(CommandProblem::InvalidEnvelope(
                            "prepare session 未列入 expected".into(),
                        ));
                    }
                    Some(agent_session_id_tx(tx, session)?)
                }
            };
            mf_agent::RunMutation::Retry {
                step_id,
                mode: *mode,
                continue_session_id,
            }
        }
        WorkflowRunCommand::SkipStep { step, .. } => {
            if preparation != &RunPreparation::Ready {
                return Err(CommandProblem::InvalidEnvelope(
                    "skip prepare 结果不匹配".into(),
                ));
            }
            mf_agent::RunMutation::Skip {
                step_id: step_id_tx(tx, step)?,
            }
        }
        WorkflowRunCommand::Respond {
            step,
            question_id,
            answer,
            ..
        } => {
            if preparation != &RunPreparation::Ready {
                return Err(CommandProblem::InvalidEnvelope(
                    "respond prepare 结果不匹配".into(),
                ));
            }
            let step_id = step_id_tx(tx, step)?;
            // 哨兵 0:web facade 不携带 Project Store rowid;按 step 解析
            // 唯一 open question(数量≠1 → fail-closed,与显式 id 语义同严)。
            let question_id = if *question_id <= 0 {
                let open: Vec<i64> = tx
                    .prepare("SELECT id FROM step_questions WHERE step_id=?1 AND status='open'")
                    .and_then(|mut stmt| {
                        stmt.query_map([step_id], |row| row.get::<_, i64>(0))?
                            .collect()
                    })
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                if open.len() != 1 {
                    return Err(CommandProblem::InvalidEnvelope(format!(
                        "目标 Step 的 open question 数量为 {},须恰好一个才能匿名应答",
                        open.len()
                    )));
                }
                open[0]
            } else {
                *question_id
            };
            let question_step_id = tx
                .query_row(
                    "SELECT step_id FROM step_questions WHERE id=?1 AND status='open'",
                    [question_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|error| CommandProblem::Internal(error.to_string()))?
                .ok_or_else(|| {
                    CommandProblem::InvalidEnvelope("目标 question 不存在、已回答或已被替换".into())
                })?;
            if question_step_id != step_id {
                return Err(CommandProblem::InvalidEnvelope(
                    "目标 question 不属于目标 Step".into(),
                ));
            }
            mf_agent::RunMutation::Respond {
                question_id,
                answer: answer.clone(),
            }
        }
        WorkflowRunCommand::Settle {
            agent_run,
            settlement,
            ..
        } => {
            if preparation != &RunPreparation::Ready {
                return Err(CommandProblem::InvalidEnvelope(
                    "settle prepare 结果不匹配".into(),
                ));
            }
            mf_agent::RunMutation::Settle {
                run_id: agent_run_id_tx(tx, agent_run)?,
                settlement: settlement.clone(),
            }
        }
    };
    let expected = workflow_run_expected_revisions(command)
        .map_err(|error| CommandProblem::InvalidEnvelope(error.to_string()))?;
    let mut result = Store::apply_run_mutation_tx(tx, mutation).map_err(run_domain_problem)?;
    if matches!(command, WorkflowRunCommand::Cancel { .. }) {
        Store::finish_cancel_fence_tx(tx, command_id.as_str()).map_err(run_domain_problem)?;
    }
    // Settle 同向重放(already_applied):首次结算的 AfterSettlement 已进入
    // durable outbox(或已投递),这里不得生成第二条 action;崩溃遗留的
    // pending 由同 target 的 drain_run_action_outbox 按 outbox 键补投。
    if matches!(command, WorkflowRunCommand::Settle { .. })
        && matches!(
            result.output,
            mf_agent::run_mutation::RunMutationOutput::Settled {
                already_applied: true,
                ..
            }
        )
    {
        result.actions.clear();
    }
    let target = workflow_run_target(command)
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
    let mut projections = Vec::new();
    let mut target_revision = None;
    for expected in expected {
        let Some(base) = expected.revisions.get("revision").copied() else {
            continue;
        };
        let (revision, data) = run_aggregate_snapshot_tx(tx, &expected.aggregate)?;
        if expected.aggregate == target {
            target_revision = Some(revision);
        }
        if revision == base {
            continue;
        }
        projections.push(ProjectionEffect {
            aggregate: Some(expected.aggregate),
            event_type: Some(command.command_type().as_str().to_owned()),
            projection_critical: true,
            payload: serde_json::json!({
                "base_revision":{"revision":base},
                "aggregate_revision":{"revision":revision},
                "delta":{"mode":"replace","data":data},
            }),
            run_actions: Vec::new(),
        });
    }
    if !result.actions.is_empty() {
        let primary = projections
            .first_mut()
            .ok_or_else(|| CommandProblem::Internal("run actions 缺少可承载的权威投影".into()))?;
        primary.run_actions = result.actions;
    }
    let revision = target_revision
        .ok_or_else(|| CommandProblem::Internal("run target revision 读取失败".into()))?;
    Ok(EffectOutput {
        result_revisions: serde_json::json!({"revision":revision}),
        projections,
    })
}

fn run_domain_problem(error: anyhow::Error) -> CommandProblem {
    if error
        .downcast_ref::<mf_agent::model::RevisionConflict>()
        .is_some()
    {
        CommandProblem::RevisionConflict
    } else {
        CommandProblem::ValidationFailed(format!("{error:#}"))
    }
}

fn workflow_run_id_tx(
    tx: &Transaction<'_>,
    handle: &WorkflowRunHandle,
) -> Result<i64, CommandProblem> {
    scalar_id_tx(tx, "agent_tasks", handle.as_str())
}

fn step_id_tx(tx: &Transaction<'_>, handle: &StepHandle) -> Result<i64, CommandProblem> {
    scalar_id_tx(tx, "steps", handle.as_str())
}

fn agent_run_id_tx(tx: &Transaction<'_>, handle: &AgentRunHandle) -> Result<i64, CommandProblem> {
    scalar_id_tx(tx, "agent_runs", handle.as_str())
}

fn agent_session_id_tx(
    tx: &Transaction<'_>,
    handle: &AgentSessionHandle,
) -> Result<i64, CommandProblem> {
    scalar_id_tx(tx, "agent_sessions", handle.as_str())
}

fn scalar_id_tx(tx: &Transaction<'_>, table: &str, handle: &str) -> Result<i64, CommandProblem> {
    tx.query_row(
        &format!("SELECT id FROM {table} WHERE public_handle=?1"),
        [handle],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| CommandProblem::Internal(error.to_string()))?
    .ok_or(CommandProblem::ResourceNotFound)
}

pub(crate) fn run_aggregate_snapshot_tx(
    tx: &Transaction<'_>,
    aggregate: &AggregateRef,
) -> Result<(u64, Value), CommandProblem> {
    let missing = || CommandProblem::ResourceNotFound;
    let internal = |error: anyhow::Error| CommandProblem::Internal(format!("{error:#}"));
    match aggregate.kind {
        AggregateKind::WorkflowRun => {
            let value = Store::task_view_by_handle_tx(tx, &aggregate.handle)
                .map_err(internal)?
                .ok_or_else(missing)?;
            let revision = u64::try_from(value.revision)
                .map_err(|_| CommandProblem::Internal("Workflow Run revision 溢出".into()))?;
            Ok((
                revision,
                serde_json::json!({
                    "public_handle":value.public_handle,
                    "revision":value.revision,
                    "title":value.title,
                    "goal":value.goal,
                    "status":value.status,
                    "paused":value.paused,
                    "unread":value.unread,
                }),
            ))
        }
        AggregateKind::Step => {
            let value = Store::step_view_by_handle_tx(tx, &aggregate.handle)
                .map_err(internal)?
                .ok_or_else(missing)?;
            let revision = u64::try_from(value.revision)
                .map_err(|_| CommandProblem::Internal("Step revision 溢出".into()))?;
            let dependencies = {
                let mut stmt = tx
                    .prepare(
                        "SELECT dependency.public_handle
                         FROM step_deps relation
                         JOIN steps dependency ON dependency.id=relation.dep_step_id
                         WHERE relation.step_id=?1 ORDER BY dependency.id",
                    )
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                let dependencies = stmt
                    .query_map([value.id], |row| row.get::<_, String>(0))
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                dependencies
            };
            Ok((
                revision,
                serde_json::json!({
                    "public_handle":value.public_handle,
                    "revision":value.revision,
                    "key":value.step_key,
                    "title":value.title,
                    "status":value.status,
                    "attempts":value.attempts,
                    "auto_retry":value.auto_retry,
                    "result":value.result,
                    "dependencies":dependencies,
                }),
            ))
        }
        AggregateKind::AgentRun => {
            let value = Store::run_view_by_handle_tx(tx, &aggregate.handle)
                .map_err(internal)?
                .ok_or_else(missing)?;
            let revision = u64::try_from(value.revision)
                .map_err(|_| CommandProblem::Internal("Agent Run revision 溢出".into()))?;
            let (step_handle, session_handle) = tx
                .query_row(
                    "SELECT step.public_handle, session.public_handle
                     FROM agent_runs run
                     JOIN steps step ON step.id=run.step_id
                     LEFT JOIN agent_sessions session ON session.id=run.session_id
                     WHERE run.public_handle=?1",
                    [aggregate.handle.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            Ok((
                revision,
                serde_json::json!({
                    "public_handle":value.public_handle,
                    "revision":value.revision,
                    "step_handle":step_handle,
                    "session_handle":session_handle,
                    "status":value.status,
                    "agent_state":value.agent_state,
                    "outcome":value.outcome,
                    "outcome_payload":value.outcome_payload,
                    "started_at":value.started_at,
                    "ended_at":value.ended_at,
                }),
            ))
        }
        AggregateKind::AgentSession => {
            let value = Store::session_view_by_handle_tx(tx, &aggregate.handle)
                .map_err(internal)?
                .ok_or_else(missing)?;
            let revision = u64::try_from(value.revision)
                .map_err(|_| CommandProblem::Internal("Agent Session revision 溢出".into()))?;
            Ok((
                revision,
                serde_json::json!({
                    "public_handle":value.public_handle,
                    "revision":value.revision,
                    "runtime":value.runtime,
                    "title":value.title,
                    "status":value.status,
                    "unread":value.unread,
                }),
            ))
        }
        _ => Err(CommandProblem::InvalidEnvelope(
            "run projection aggregate 非法".into(),
        )),
    }
}

fn target_has_pending_run_actions(target: &TargetDatabase) -> Result<bool, KernelProblem> {
    target
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT event_json FROM projection_outbox
                     WHERE published_at IS NULL ORDER BY outbox_id",
                )
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| CommandProblem::Internal(error.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            Ok(rows
                .iter()
                .any(|event| crate::run_lifecycle::event_has_pending_run_actions(event)))
        })
        .map_err(KernelProblem::from)
}

fn drain_run_action_outbox(
    target: &TargetDatabase,
    port: &dyn RunLifecyclePort,
) -> Result<(), KernelProblem> {
    let rows = target
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT outbox_id,event_json FROM projection_outbox
                     WHERE published_at IS NULL ORDER BY outbox_id",
                )
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| CommandProblem::Internal(error.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            Ok(rows)
        })
        .map_err(KernelProblem::from)?;
    for (outbox_id, original) in rows {
        let mut event: Value = serde_json::from_str(&original)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        let Some(private) = event.get("run_actions").cloned() else {
            continue;
        };
        if private.get("schema").and_then(Value::as_str) != Some(DURABLE_RUN_ACTIONS_SCHEMA) {
            return Err(KernelProblem::ServiceUnavailable(
                "unsupported_run_actions_schema".into(),
            ));
        }
        let command_id = event
            .get("caused_by_command_id")
            .and_then(Value::as_str)
            .ok_or_else(|| KernelProblem::Internal("run action outbox 缺少 command id".into()))?;
        let command_id = CommandId::parse(command_id)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        let actions: Vec<mf_agent::RunAction> = serde_json::from_value(
            private
                .get("actions")
                .cloned()
                .ok_or_else(|| KernelProblem::Internal("run action outbox 缺少 actions".into()))?,
        )
        .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        for (index, action) in actions.into_iter().enumerate() {
            port.execute_post_commit(&RunActionDelivery {
                outbox_id,
                command_id: command_id.clone(),
                action_index: u32::try_from(index)
                    .map_err(|_| KernelProblem::Internal("run action index 溢出".into()))?,
                action,
            })?;
        }
        event
            .as_object_mut()
            .ok_or_else(|| KernelProblem::Internal("outbox event 非 object".into()))?
            .remove("run_actions");
        let cleaned = crate::command::canonical_json(&event).map_err(KernelProblem::from)?;
        target
            .with_tx(|tx| {
                let changed = tx
                    .execute(
                        "UPDATE projection_outbox SET event_json=?1
                         WHERE outbox_id=?2 AND event_json=?3 AND published_at IS NULL",
                        rusqlite::params![cleaned, outbox_id, original],
                    )
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                if changed != 1 {
                    return Err(CommandProblem::CommandInProgress);
                }
                Ok(())
            })
            .map_err(KernelProblem::from)?;
    }
    Ok(())
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
        run_actions: Vec::new(),
    }];
    if matches!(
        command,
        ProjectWorkflowCommand::Create { .. } | ProjectWorkflowCommand::Delete { .. }
    ) {
        projections.push(ProjectionEffect { aggregate: Some(AggregateRef::new(AggregateKind::Project,project.as_str().to_owned()).map_err(|e|CommandProblem::Internal(e.to_string()))?), event_type: Some("project.workflow_collection_changed".into()), projection_critical:true, payload:serde_json::json!({"base_revision":{"revision":result.collection_revision-1},"aggregate_revision":{"revision":result.collection_revision},"delta":{"mode":"typed_delta","delta_type":"project.workflow_collection_changed","data":{"workflow_handle":workflow_handle}}}), run_actions:Vec::new() });
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
    /// kernel facade 句柄(装配件用于注入 TerminalHost 等宿主缝隙;
    /// 不改变命令/投影所有权)。
    pub fn kernel(&self) -> &Arc<InProcessCoreKernel> {
        &self.kernel
    }

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
        let capability_key = RunCapabilityKey::load_or_create()
            .map_err(|error| KernelProblem::ServiceUnavailable(format!("{error:#}")))?;
        let kernel = Arc::new(InProcessCoreKernel::new_with_capability_key(
            service,
            key,
            capability_key,
        ));
        kernel.install_workflow_start_worker()?;
        let epoch = kernel.grant_controller_checked(&client_id, &principal)?;
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
        let kernel = Arc::new(InProcessCoreKernel::new_with_capability_key(
            service,
            key,
            RunCapabilityKey::for_test(vec![0x52; 32])
                .map_err(|error| KernelProblem::Internal(format!("{error:#}")))?,
        ));
        kernel.install_workflow_start_worker()?;
        let epoch = kernel.grant_controller_checked(&client_id, &principal)?;
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

    pub fn register_run_lifecycle_port(
        &self,
        project: &ProjectStoreHandle,
        port: Arc<dyn RunLifecyclePort>,
    ) -> Result<(), KernelProblem> {
        self.kernel.register_run_lifecycle_port(project, port)
    }

    pub fn register_run_lifecycle_port_for_close(
        &self,
        close: &ProjectCloseToken,
        port: Arc<dyn RunLifecyclePort>,
    ) -> Result<(), KernelProblem> {
        self.kernel
            .register_run_lifecycle_port_for_close(close, port)
    }

    pub fn unregister_run_lifecycle_port(&self, project: &ProjectStoreHandle) {
        self.kernel.unregister_run_lifecycle_port(project);
    }

    /// 注册 Workflow Start 编译 port(生产 Orchestrator adapter 落地前的
    /// 装配缝隙;不注册则 Start 保持 fail-closed)。
    pub fn register_workflow_start_port(
        &self,
        project: &ProjectStoreHandle,
        port: Arc<dyn crate::workflow_start::WorkflowStartPort>,
    ) -> Result<(), KernelProblem> {
        self.kernel.register_workflow_start_port(project, port)
    }

    pub fn unregister_workflow_start_port(&self, project: &ProjectStoreHandle) {
        self.kernel.unregister_workflow_start_port(project);
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

    /// Legacy/Web adapter 的 Start 入口：只读定位持久 workflow handle +
    /// semantic revision，写入仍完全经封闭 WorkflowRunCommand。
    pub fn start_workflow_run(
        &self,
        project: &ProjectStoreHandle,
        workflow_key: &str,
        goal: impl Into<String>,
    ) -> Result<KernelOutcome, KernelProblem> {
        let (workflow, expected_semantic_revision) =
            self.kernel.legacy_workflow_locator(project, workflow_key)?;
        self.dispatch_run_command(
            CommandId::new(),
            WorkflowRunCommand::Start {
                project: project.clone(),
                workflow,
                goal: goal.into(),
                expected_semantic_revision,
            },
        )
    }

    pub fn workspace_snapshot(&self) -> Result<SnapshotEnvelope, KernelProblem> {
        self.kernel.snapshot(SnapshotQuery::Workspace)
    }

    pub fn workflow_run_snapshot(
        &self,
        project: &ProjectStoreHandle,
        workflow_run: &WorkflowRunHandle,
    ) -> Result<SnapshotEnvelope, KernelProblem> {
        self.kernel.snapshot(SnapshotQuery::WorkflowRun {
            project: project.clone(),
            workflow_run: workflow_run.clone(),
        })
    }

    pub fn cancel_workflow_run(
        &self,
        project: &ProjectStoreHandle,
        workflow_run: &WorkflowRunHandle,
    ) -> Result<KernelOutcome, KernelProblem> {
        let data = self.workflow_run_data(project, workflow_run)?;
        let active_steps = data
            .steps
            .iter()
            .filter(|step| {
                !matches!(
                    step.status.as_str(),
                    "succeeded" | "failed" | "skipped" | "cancelled"
                )
            })
            .map(versioned_step)
            .collect();
        let active_runs = data
            .agent_runs
            .iter()
            .filter(|run| matches!(run.status.as_str(), "running" | "awaiting-outcome"))
            .collect::<Vec<_>>();
        let expected = run_expected(&data, active_steps, &active_runs);
        self.dispatch_run_command(
            CommandId::new(),
            WorkflowRunCommand::Cancel {
                project: project.clone(),
                workflow_run: workflow_run.clone(),
                expected,
            },
        )
    }

    /// Project 关闭 drain 的唯一写 seam。只有由
    /// `prepare_project_close` 生成且仍绑定当前 Store 的 token
    /// 能在 closing 状态下提交 Cancel；其他命令仍全部拒绝。
    pub fn cancel_workflow_run_for_close(
        &self,
        close: &ProjectCloseToken,
        workflow_run: &WorkflowRunHandle,
    ) -> Result<KernelOutcome, KernelProblem> {
        let snapshot = self
            .kernel
            .workflow_run_snapshot_for_close(close, workflow_run)?;
        let SnapshotData::WorkflowRun(data) = snapshot.data else {
            return Err(KernelProblem::Internal(
                "Core 返回了错误的 Workflow Run Snapshot 类型".into(),
            ));
        };
        let active_steps = data
            .steps
            .iter()
            .filter(|step| {
                !matches!(
                    step.status.as_str(),
                    "succeeded" | "failed" | "skipped" | "cancelled"
                )
            })
            .map(versioned_step)
            .collect();
        let active_runs = data
            .agent_runs
            .iter()
            .filter(|run| matches!(run.status.as_str(), "running" | "awaiting-outcome"))
            .collect::<Vec<_>>();
        let expected = run_expected(&data, active_steps, &active_runs);
        self.kernel.dispatch_workflow_run_for_close(
            KernelCommandRequest::new(
                CommandId::new(),
                self.client_id.clone(),
                self.principal.clone(),
                self.controller_epoch(),
                KernelCommand::WorkflowRun(WorkflowRunCommand::Cancel {
                    project: close.project.clone(),
                    workflow_run: workflow_run.clone(),
                    expected,
                }),
            ),
            close,
        )
    }

    pub fn retry_workflow_step(
        &self,
        project: &ProjectStoreHandle,
        workflow_run: &WorkflowRunHandle,
        step: &StepHandle,
        mode: mf_agent::RetryMode,
    ) -> Result<KernelOutcome, KernelProblem> {
        let data = self.workflow_run_data(project, workflow_run)?;
        let step_row = data
            .steps
            .iter()
            .find(|row| &row.step == step)
            .ok_or(KernelProblem::ResourceNotFound)?;
        let active_runs = data
            .agent_runs
            .iter()
            .filter(|run| {
                &run.step == step && matches!(run.status.as_str(), "running" | "awaiting-outcome")
            })
            .collect::<Vec<_>>();
        let mut expected = run_expected(&data, vec![versioned_step(step_row)], &active_runs);
        if mode == mf_agent::RetryMode::ContinueSession {
            let session_handle = data
                .agent_runs
                .iter()
                .find(|run| &run.step == step && run.agent_session.is_some())
                .and_then(|run| run.agent_session.as_ref())
                .ok_or_else(|| {
                    KernelProblem::ValidationFailed("目标 Step 没有可继续的 Agent Session".into())
                })?;
            if !expected
                .agent_sessions
                .iter()
                .any(|session| &session.handle == session_handle)
            {
                let session = data
                    .agent_sessions
                    .iter()
                    .find(|session| &session.agent_session == session_handle)
                    .filter(|session| !matches!(session.status.as_str(), "dead" | "hidden"))
                    .ok_or_else(|| {
                        KernelProblem::ValidationFailed(
                            "目标 Step 的 Agent Session 已不可继续".into(),
                        )
                    })?;
                expected.agent_sessions.push(VersionedHandle {
                    handle: session.agent_session.clone(),
                    revision: session.revision.revision,
                });
            }
        }
        self.dispatch_run_command(
            CommandId::new(),
            WorkflowRunCommand::RetryStep {
                project: project.clone(),
                workflow_run: workflow_run.clone(),
                step: step.clone(),
                mode,
                expected,
            },
        )
    }

    /// 显式跳过失败/阻塞 Step。facade 从权威 Snapshot 构造完整 expected，
    /// 写入仍经 L-CMD transaction + receipt/outbox；有 active Agent Run 时
    /// Core 会 fail-closed，要求先取消或结算。
    pub fn skip_workflow_step(
        &self,
        project: &ProjectStoreHandle,
        workflow_run: &WorkflowRunHandle,
        step: &StepHandle,
    ) -> Result<KernelOutcome, KernelProblem> {
        let data = self.workflow_run_data(project, workflow_run)?;
        let step_row = data
            .steps
            .iter()
            .find(|row| &row.step == step)
            .ok_or(KernelProblem::ResourceNotFound)?;
        let active_runs = data
            .agent_runs
            .iter()
            .filter(|run| {
                &run.step == step && matches!(run.status.as_str(), "running" | "awaiting-outcome")
            })
            .collect::<Vec<_>>();
        let expected = run_expected(&data, vec![versioned_step(step_row)], &active_runs);
        self.dispatch_run_command(
            CommandId::new(),
            WorkflowRunCommand::SkipStep {
                project: project.clone(),
                workflow_run: workflow_run.clone(),
                step: step.clone(),
                expected,
            },
        )
    }

    pub fn respond_to_workflow_step(
        &self,
        project: &ProjectStoreHandle,
        workflow_run: &WorkflowRunHandle,
        step: &StepHandle,
        answer: impl Into<String>,
    ) -> Result<KernelOutcome, KernelProblem> {
        let data = self.workflow_run_data(project, workflow_run)?;
        let step_row = data
            .steps
            .iter()
            .find(|row| &row.step == step)
            .ok_or(KernelProblem::ResourceNotFound)?;
        let questions = data
            .open_questions
            .iter()
            .filter(|question| question.step.as_ref() == Some(step))
            .collect::<Vec<_>>();
        if questions.len() != 1 {
            return Err(KernelProblem::InvalidEnvelope(
                "respond 要求目标 Step 恰有一个 open question".into(),
            ));
        }
        let runs = questions[0]
            .agent_run
            .as_ref()
            .map(|handle| {
                data.agent_runs
                    .iter()
                    .find(|run| &run.agent_run == handle)
                    .ok_or(KernelProblem::ResourceNotFound)
            })
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        let expected = run_expected(&data, vec![versioned_step(step_row)], &runs);
        self.dispatch_run_command(
            CommandId::new(),
            WorkflowRunCommand::Respond {
                project: project.clone(),
                workflow_run: workflow_run.clone(),
                step: step.clone(),
                question_id: questions[0].question_id,
                answer: answer.into(),
                expected,
            },
        )
    }

    pub fn settle_agent_run(
        &self,
        project: &ProjectStoreHandle,
        workflow_run: &WorkflowRunHandle,
        agent_run: &AgentRunHandle,
        settlement: mf_agent::Settlement,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.settle_agent_run_with_command(
            project,
            workflow_run,
            agent_run,
            settlement,
            CommandId::new(),
        )
    }

    /// run_control(canonical §6.3):显式 command_id 版 settle,供
    /// mfctl capability-token 路由复用同一条 WorkflowRunCommand::Settle
    /// 链(L-CMD 事务 + durable RunAction outbox)。
    pub(crate) fn settle_agent_run_with_command(
        &self,
        project: &ProjectStoreHandle,
        workflow_run: &WorkflowRunHandle,
        agent_run: &AgentRunHandle,
        settlement: mf_agent::Settlement,
        command_id: CommandId,
    ) -> Result<KernelOutcome, KernelProblem> {
        let data = self.workflow_run_data(project, workflow_run)?;
        let run = data
            .agent_runs
            .iter()
            .find(|run| &run.agent_run == agent_run)
            .ok_or(KernelProblem::ResourceNotFound)?;
        let step = data
            .steps
            .iter()
            .find(|step| step.step == run.step)
            .ok_or(KernelProblem::ResourceNotFound)?;
        let expected = run_expected(&data, vec![versioned_step(step)], &[run]);
        self.dispatch_run_command(
            command_id,
            WorkflowRunCommand::Settle {
                project: project.clone(),
                workflow_run: workflow_run.clone(),
                step: run.step.clone(),
                agent_run: agent_run.clone(),
                settlement,
                expected,
            },
        )
    }

    /// run_control 路由使用的 Core 句柄(私有 adapter 语义)。
    pub(crate) fn core_kernel(&self) -> &Arc<InProcessCoreKernel> {
        &self.kernel
    }

    fn workflow_run_data(
        &self,
        project: &ProjectStoreHandle,
        workflow_run: &WorkflowRunHandle,
    ) -> Result<crate::projection::WorkflowRunSnapshotData, KernelProblem> {
        let snapshot = self.workflow_run_snapshot(project, workflow_run)?;
        let SnapshotData::WorkflowRun(data) = snapshot.data else {
            return Err(KernelProblem::Internal(
                "Core 返回了错误的 Workflow Run Snapshot 类型".into(),
            ));
        };
        Ok(data)
    }

    fn dispatch_run_command(
        &self,
        command_id: CommandId,
        command: WorkflowRunCommand,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.kernel.dispatch(KernelCommandRequest::new(
            command_id,
            self.client_id.clone(),
            self.principal.clone(),
            self.controller_epoch(),
            KernelCommand::WorkflowRun(command),
        ))
    }
}

fn versioned_step(
    step: &crate::projection::WorkflowRunStepSnapshot,
) -> VersionedHandle<StepHandle> {
    VersionedHandle {
        handle: step.step.clone(),
        revision: step.revision.revision,
    }
}

fn run_expected(
    data: &crate::projection::WorkflowRunSnapshotData,
    steps: Vec<VersionedHandle<StepHandle>>,
    runs: &[&crate::projection::AgentRunSnapshot],
) -> WorkflowRunExpected {
    let agent_runs = runs
        .iter()
        .map(|run| VersionedHandle {
            handle: run.agent_run.clone(),
            revision: run.revision.revision,
        })
        .collect::<Vec<_>>();
    let session_handles = runs
        .iter()
        .filter_map(|run| run.agent_session.as_ref().map(|handle| handle.as_str()))
        .collect::<BTreeSet<_>>();
    let agent_sessions = data
        .agent_sessions
        .iter()
        .filter(|session| session_handles.contains(session.agent_session.as_str()))
        .map(|session| VersionedHandle {
            handle: session.agent_session.clone(),
            revision: session.revision.revision,
        })
        .collect();
    WorkflowRunExpected {
        workflow_run_revision: data.revision.revision,
        steps,
        agent_runs,
        agent_sessions,
    }
}
