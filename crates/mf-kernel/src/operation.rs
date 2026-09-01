//! 跨 Store 长任务的 Operation saga(canonical spec §4:幂等 step receipt、
//! compensation/Needs You、重启 reconcile)。
//!
//! 本模块不是 transport,也不暴露任意 effect seam:step effect 由未来
//! CoreKernel 的封闭 command enum 编译并注入(`run` 只接受与冻结 plan 对齐
//! 的 closure);重启后唯一入口是只读 target receipt 的 reconcile 路径,
//! 绝不重放业务写。durable 事实源:
//!
//! - service-v1 `operation`/`operation_step`(v3 delta):handle、冻结 plan
//!   (`saga_state`)与每个 step 的身份/状态;
//! - Project v7 / Catalog v2 `command_receipt`:以 `step_id` 为幂等键的
//!   target-local receipt(与 #21 单命令链同一张表、同一 digest 口径)。
//!
//! 执行决策只看 durable receipt:「receipt 存在 → 不再执行 effect」;
//! 内存侧(调用方传入的 plan/effects)只用于供给执行体,进入事务前先与
//! durable step 行做身份核对。

use crate::command::{
    canonical_expected_revisions, canonical_json, finish_intent_tx, hex_digest, intent_tx,
    problem_for_terminal_code, validated_receipt_result, value_has_sensitive_field,
    CommandEnvelope, CommandProblem, CommandType, EffectOutput, IntentState, ServiceIdempotencyKey,
    TargetDatabase,
};
use crate::handles::CommandId;
use crate::handles::{AggregateRef, ClientId, CommandTarget, ExpectedRevision, Principal};
use crate::lease::{CommandAuthorizer, LeaseCheck};
use crate::project_registry::ServiceStore;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::Arc;

/// step receipt/replay 的 canonical digest schema(与 `mf.command.v1` 同一
/// 排序口径,只覆盖冻结语义,排除凭据/lease/trace)。
pub const OPERATION_STEP_SCHEMA: &str = "mf.operation.step.v1";

/// 终态 problem 稳定码(operation.progress_json / step.problem_code /
/// command_intent.problem_code 共用)。
pub const CODE_COMPENSATION_MISSING: &str = "compensation_missing";
pub const CODE_COMPENSATION_FAILED: &str = "compensation_failed";
pub const CODE_COMPENSATION_REQUIRED: &str = "compensation_required";
pub const CODE_OPERATION_COMPENSATED: &str = "operation_compensated";

// ───────────────────────── durable DTO(typed IDs) ─────────────────────────

/// Operation 的持久 opaque handle(§7.1 `op_` + UUIDv7 前缀风格);跨 Core
/// 重启稳定,永不复用,不由 rowid/路径派生。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct OperationHandle(String);

impl OperationHandle {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        let uuid = value
            .strip_prefix("op_")
            .ok_or_else(|| anyhow::anyhow!("operation handle 必须以 op_ 开头"))?;
        let uuid = uuid::Uuid::parse_str(uuid)?;
        anyhow::ensure!(
            uuid.get_version_num() == 7,
            "operation handle 必须是 op_ + UUIDv7"
        );
        Ok(Self(format!("op_{uuid}")))
    }

    fn new() -> Self {
        Self(format!("op_{}", uuid::Uuid::now_v7()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for OperationHandle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

/// saga step 的幂等键:与 command id 同一 UUIDv7 口径,作为 target-local
/// `command_receipt` 的主键。step receipt = 以该 id 落盘的 receipt 行。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct StepId(String);

impl StepId {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        let uuid = uuid::Uuid::parse_str(&value)?;
        anyhow::ensure!(uuid.get_version_num() == 7, "step_id 必须是 UUIDv7");
        Ok(Self(uuid.to_string()))
    }

    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for StepId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for StepId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Operation 种类(§2.2:CLI 安装、Workflow Run 启动、模型探测等);封闭
/// command enum 在 T2 编译 plan 时给定,T1 只冻结非空字符串 DTO。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationKind(String);

impl OperationKind {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        anyhow::ensure!(!value.trim().is_empty(), "operation kind 不能为空");
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// §4.2 状态机:`accepted → running(step receipts…) → completed |
/// compensating → completed | needs_you`;重启把未终结状态推入
/// `reconciling`(附录 B),由 reconcile 只读 receipt 终结。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationState {
    Accepted,
    Running,
    Completed,
    Compensating,
    NeedsYou,
    Reconciling,
}

impl OperationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Compensating => "compensating",
            Self::NeedsYou => "needs_you",
            Self::Reconciling => "reconciling",
        }
    }

    fn parse(value: &str) -> Result<Self, OperationProblem> {
        match value {
            "accepted" => Ok(Self::Accepted),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "compensating" => Ok(Self::Compensating),
            "needs_you" => Ok(Self::NeedsYou),
            "reconciling" => Ok(Self::Reconciling),
            other => Err(OperationProblem::Internal(format!(
                "未知 operation.state:{other}"
            ))),
        }
    }

    /// 终态:自动推进停止(GC 的「终态后」口径,§4.6)。
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::NeedsYou)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepRole {
    Forward,
    Compensate,
}

impl StepRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Compensate => "compensate",
        }
    }

    fn parse(value: &str) -> Result<Self, OperationProblem> {
        match value {
            "forward" => Ok(Self::Forward),
            "compensate" => Ok(Self::Compensate),
            other => Err(OperationProblem::Internal(format!(
                "未知 operation_step.role:{other}"
            ))),
        }
    }
}

/// step 协调状态:`pending` 未线性化(reconcile 只终结,不执行)、
/// `succeeded` 持有 target receipt、`failed`/`revoked` 终结失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Succeeded,
    Failed,
    Revoked,
}

impl StepState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Revoked => "revoked",
        }
    }

    fn parse(value: &str) -> Result<Self, OperationProblem> {
        match value {
            "pending" => Ok(Self::Pending),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "revoked" => Ok(Self::Revoked),
            other => Err(OperationProblem::Internal(format!(
                "未知 operation_step.state:{other}"
            ))),
        }
    }
}

// ───────────────────────────── 冻结 plan DTO ─────────────────────────────

/// accept 时冻结的 saga step:身份 + 目标 + canonical digest(覆盖 payload
/// 语义,但 payload 本身不落盘——receipt 比对只需要 digest)。
#[derive(Debug, Clone, PartialEq)]
pub struct SagaStepPlan {
    pub role: StepRole,
    pub step_id: StepId,
    pub command_type: CommandType,
    pub target: CommandTarget,
    pub expected: Vec<ExpectedRevision>,
    /// 补偿目标(forward 恒为 None;compensate 指向被回滚的 forward 下标)。
    pub compensates: Option<usize>,
    semantic_digest: String,
}

impl SagaStepPlan {
    /// 冻结一个 step 并计算 canonical digest。saga payload 不承载明文凭据
    /// (§7.4 WriteOnlySecret 是单命令特例);敏感字段在此 fail-closed。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        role: StepRole,
        step_id: StepId,
        command_type: CommandType,
        target: CommandTarget,
        expected: Vec<ExpectedRevision>,
        payload: &Value,
        compensates: Option<usize>,
        operation_kind: &OperationKind,
    ) -> Result<Self, OperationProblem> {
        if value_has_sensitive_field(payload) {
            return Err(OperationProblem::InvalidPlan(
                "step payload 含凭据或可复用引用(saga payload 不承载明文凭据)".into(),
            ));
        }
        let semantic_digest = step_semantic_digest(
            operation_kind,
            role,
            command_type,
            &target,
            &expected,
            payload,
        )?;
        Ok(Self {
            role,
            step_id,
            command_type,
            target,
            expected,
            compensates,
            semantic_digest,
        })
    }

    pub fn semantic_digest(&self) -> &str {
        &self.semantic_digest
    }
}

/// 完整 saga 计划:forward 步按序执行;compensate 步引用其回滚的 forward。
#[derive(Debug, Clone, PartialEq)]
pub struct OperationPlan {
    pub kind: OperationKind,
    pub steps: Vec<SagaStepPlan>,
}

impl OperationPlan {
    /// 校验:至少一个 forward step;step_id 唯一;compensates 只能指向
    /// forward 步;forward 步不得声明 compensates。
    pub fn validate(&self) -> Result<(), OperationProblem> {
        let forward_total = self
            .steps
            .iter()
            .filter(|step| step.role == StepRole::Forward)
            .count();
        if forward_total == 0 {
            return Err(OperationProblem::InvalidPlan(
                "至少一个 forward step".into(),
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for step in &self.steps {
            if !ids.insert(step.step_id.as_str()) {
                return Err(OperationProblem::InvalidPlan("step_id 重复".into()));
            }
            match step.role {
                StepRole::Forward if step.compensates.is_some() => {
                    return Err(OperationProblem::InvalidPlan(
                        "forward step 不得声明 compensates".into(),
                    ));
                }
                StepRole::Compensate => {
                    let Some(index) = step.compensates else {
                        return Err(OperationProblem::InvalidPlan(
                            "compensate step 必须声明 compensates".into(),
                        ));
                    };
                    match self.steps.get(index) {
                        Some(target) if target.role == StepRole::Forward => {}
                        _ => {
                            return Err(OperationProblem::InvalidPlan(format!(
                                "compensates 只能指向 forward step 下标(得到 {index})"
                            )));
                        }
                    }
                }
                StepRole::Forward => {}
            }
        }
        Ok(())
    }

    /// 冻结为 `operation.saga_state` 的 canonical JSON(排序稳定,幂等比对)。
    #[allow(dead_code)] // 经 accept 使用;accept 属 dark seam。
    fn frozen_json(&self) -> Result<String, OperationProblem> {
        let steps: Vec<Value> = self
            .steps
            .iter()
            .map(|step| {
                serde_json::json!({
                    "step_id": step.step_id.as_str(),
                    "role": step.role.as_str(),
                    "target_store": step.target.store_key(),
                    "aggregate_kind": step.target.aggregate.kind.as_str(),
                    "aggregate_handle": step.target.aggregate.handle,
                    "semantic_digest": step.semantic_digest(),
                    "compensates": step.compensates,
                })
            })
            .collect();
        canonical_json(&serde_json::json!({
            "schema": OPERATION_STEP_SCHEMA,
            "kind": self.kind.as_str(),
            "steps": steps,
        }))
        .map_err(|error| OperationProblem::Internal(error.to_string()))
    }
}

/// step canonical digest:与 `CommandEnvelope::semantic_digest` 同一口径的
/// target/expected 编码,覆盖 payload(冻结执行语义),排除凭据/lease/trace。
fn step_semantic_digest(
    operation_kind: &OperationKind,
    role: StepRole,
    command_type: CommandType,
    target: &CommandTarget,
    expected: &[ExpectedRevision],
    payload: &Value,
) -> Result<String, OperationProblem> {
    let expected = canonical_expected_revisions(expected);
    let canonical = canonical_json(&serde_json::json!({
        "schema": OPERATION_STEP_SCHEMA,
        "operation_kind": operation_kind.as_str(),
        "role": role.as_str(),
        "type": command_type.as_str(),
        "target": {
            "store": target.store_key(),
            "kind": target.aggregate.kind.as_str(),
            "handle": target.aggregate.handle,
        },
        "expected": expected,
        "payload": payload,
    }))
    .map_err(|error| OperationProblem::Internal(error.to_string()))?;
    Ok(hex_digest(Sha256::digest(canonical.as_bytes())))
}

// ───────────────────────────── progress DTO ─────────────────────────────

/// `operation.progress_json` 的稳定 DTO;Needs You 的可诊断 problem 落在
/// `problem.code/step_index`(Issue #22:不得伪造成功)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationProgress {
    pub forward_total: usize,
    pub forward_succeeded: usize,
    /// `None` | `Some("compensated")`(目标未达成但已完整回滚)。
    pub outcome: Option<String>,
    pub problem: Option<OperationProblemDetail>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationProblemDetail {
    pub code: String,
    pub step_index: Option<usize>,
}

impl OperationProgress {
    #[allow(dead_code)] // 经 accept 使用;accept 属 dark seam。
    fn initial(forward_total: usize) -> Self {
        Self {
            forward_total,
            forward_succeeded: 0,
            outcome: None,
            problem: None,
        }
    }
}

// ───────────────────────────── problem/outcome ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OperationProblem {
    #[error("invalid_plan:{0}")]
    InvalidPlan(String),
    #[error("operation_not_found")]
    OperationNotFound,
    #[error("command_id_reused")]
    CommandIdReused,
    #[error("plan_conflict:{0}")]
    PlanConflict(String),
    #[error("operation_state_conflict:{0}")]
    StateConflict(String),
    #[error("target_store_mismatch:{0}")]
    TargetStoreMismatch(String),
    #[error("fault_injected:{0}")]
    FaultInjected(&'static str),
    #[error(transparent)]
    Command(#[from] CommandProblem),
    #[error("internal_error:{0}")]
    Internal(String),
}

impl OperationProblem {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidPlan(_) => "invalid_plan",
            Self::OperationNotFound => "resource_not_found",
            Self::CommandIdReused | Self::PlanConflict(_) => "command_id_reused",
            Self::Command(problem) => problem.code(),
            Self::StateConflict(_)
            | Self::TargetStoreMismatch(_)
            | Self::FaultInjected(_)
            | Self::Internal(_) => "internal_error",
        }
    }
}

/// run/reconcile 的对外结果(`202 accepted` Operation 的终态投影)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationOutcome {
    /// `compensated=true`:目标未达成,但已按声明完整回滚,不伪造成功
    /// (progress.outcome = "compensated"、intent = failed)。
    Completed { compensated: bool },
    /// compensation 无法自动完成,等待用户;`problem_code` 是稳定码。
    NeedsYou { problem_code: String },
    /// acceptance target receipt 从未提交；调用方从未获得 202/handle。
    NotAccepted { problem_code: String },
}

/// 确定性故障注入点(fault harness,§14.2):target commit 后、service
/// finalize 前等崩溃窗口;post-commit 注入点在事务提交后返回错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationFaultPoint {
    /// step target 事务已提交(receipt/outbox 已持久),service step 行
    /// 保持 pending——重启 reconcile 依 receipt 补齐,不重做业务写。
    AfterStepTargetCommit(usize),
    /// step service finalize 已提交,下一 step 前崩溃。
    AfterStepFinalized(usize),
    /// operation 已置 compensating 并提交,补偿执行前崩溃。
    AfterCompensating,
    /// 终态 service 事务已提交,返回前崩溃。
    AfterOperationFinalized,
}

/// Operation 接受链的确定性故障点。只有 acceptance target receipt 已提交
/// 后，调用方才可把返回的 handle 解释为 `202 accepted`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationAcceptFaultPoint {
    /// service intent/plan 已保留，但尚无 target receipt。
    AfterIntentReserve,
    /// acceptance target receipt/outbox 已提交，返回 handle 前崩溃。
    AfterTargetCommit,
}

/// step effect 缝隙:由 T2 封闭 command enum 编译注入,不对 transport 开放。
#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
pub(crate) type StepEffect =
    Box<dyn FnOnce(&Transaction<'_>) -> Result<EffectOutput, CommandProblem> + Send>;

// ───────────────────────────── 读视图 ─────────────────────────────

/// `operation` 行的稳定读视图(handle 跨重启不变的观测面)。
#[derive(Debug, Clone, PartialEq)]
pub struct OperationRecord {
    pub operation_handle: OperationHandle,
    pub command_id: CommandId,
    pub kind: OperationKind,
    pub state: OperationState,
    pub saga_state: String,
    pub progress: OperationProgress,
    pub created_at: String,
    pub updated_at: String,
}

/// `operation_step` 行读视图(reconcile/GC 与测试的观测面)。
#[derive(Debug, Clone, PartialEq)]
pub struct StepRecord {
    pub step_index: usize,
    pub role: StepRole,
    pub step_id: StepId,
    pub target_store: String,
    pub aggregate: String,
    pub semantic_digest: String,
    pub compensates: Option<usize>,
    pub state: StepState,
    pub result: Value,
    pub problem_code: Option<String>,
}

/// 由 durable step 状态作出的 saga 判定(live run 与 restart reconcile 共用;
/// 判定输入只有 durable 状态,不含内存推断)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SagaDecision {
    /// 全部 forward step 持有 receipt。
    Complete,
    /// 存在未回滚完的已生效 forward step:`needed` = 已生效 forward 的
    /// 补偿步下标;`failed` = 已终结失败的补偿步;`missing` = 已生效但
    /// 未声明补偿的 forward 步下标。
    CompensationOpen {
        needed: Vec<usize>,
        failed: Option<usize>,
        missing: Option<usize>,
    },
}

pub(crate) fn decide_saga(steps: &[StepRecord]) -> SagaDecision {
    let forward_succeeded: Vec<usize> = steps
        .iter()
        .filter(|step| step.role == StepRole::Forward && step.state == StepState::Succeeded)
        .map(|step| step.step_index)
        .collect();
    let all_forward_succeeded = steps
        .iter()
        .filter(|step| step.role == StepRole::Forward)
        .all(|step| step.state == StepState::Succeeded);
    if all_forward_succeeded {
        return SagaDecision::Complete;
    }
    let mut needed = Vec::new();
    let mut failed = None;
    let mut missing = None;
    for forward_index in forward_succeeded {
        let Some(compensation) = steps.iter().find(|step| {
            step.role == StepRole::Compensate && step.compensates == Some(forward_index)
        }) else {
            missing = Some(forward_index);
            continue;
        };
        needed.push(compensation.step_index);
        // 只把「执行过且失败」计为 compensation_failed;revoked/pending 的
        // 补偿属于「无法自动完成」,由调用方按场景给出 compensation_required。
        if compensation.state == StepState::Failed && failed.is_none() {
            failed = Some(compensation.step_index);
        }
    }
    SagaDecision::CompensationOpen {
        needed,
        failed,
        missing,
    }
}

// ───────────────────────────── coordinator ─────────────────────────────

/// Operation saga 协调器:accept(冻结 plan)/run(执行 step)。
/// 与 `CommandCoordinator` 同一 command_gate 串行;跨进程唯一性由 #20
/// CoreOwnerLock 保证。T2 前为 dark seam,不接 transport,也不暴露任意
/// effect 公共入口;重启恢复走 `reconcile` 模块的自由函数。
#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
pub(crate) struct OperationCoordinator {
    service: Arc<ServiceStore>,
    idempotency_key: ServiceIdempotencyKey,
}

#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
impl OperationCoordinator {
    pub(crate) fn new(service: Arc<ServiceStore>, idempotency_key: ServiceIdempotencyKey) -> Self {
        Self {
            service,
            idempotency_key,
        }
    }

    /// 接受长 Operation。第一阶段在 service DB 保留稳定 handle/plan；第二
    /// 阶段在 initiating target 的 L-CMD 事务内复验 lease/CAS，并写入
    /// acceptance receipt + outbox。只有第二阶段提交后才返回 handle，因此
    /// 调用方不会在“只有 service intent、没有 target receipt”时发送 202。
    pub(crate) fn accept(
        &self,
        envelope: &CommandEnvelope,
        plan: &OperationPlan,
        target: &TargetDatabase,
        authorizer: &dyn CommandAuthorizer,
    ) -> Result<OperationHandle, OperationProblem> {
        self.accept_internal(envelope, plan, target, authorizer, None)
    }

    #[cfg(test)]
    pub(crate) fn accept_with_fault(
        &self,
        envelope: &CommandEnvelope,
        plan: &OperationPlan,
        target: &TargetDatabase,
        authorizer: &dyn CommandAuthorizer,
        fault: OperationAcceptFaultPoint,
    ) -> Result<OperationHandle, OperationProblem> {
        self.accept_internal(envelope, plan, target, authorizer, Some(fault))
    }

    fn accept_internal(
        &self,
        envelope: &CommandEnvelope,
        plan: &OperationPlan,
        target: &TargetDatabase,
        authorizer: &dyn CommandAuthorizer,
        fault: Option<OperationAcceptFaultPoint>,
    ) -> Result<OperationHandle, OperationProblem> {
        plan.validate()?;
        if envelope.target().store_key() != target.store_key() {
            return Err(OperationProblem::TargetStoreMismatch(format!(
                "acceptance target 不匹配:{} != {}",
                envelope.target().store_key(),
                target.store_key()
            )));
        }
        let digest = envelope
            .semantic_digest(&self.idempotency_key)
            .map_err(|error| OperationProblem::Internal(error.to_string()))?;
        let saga_state = plan.frozen_json()?;
        let handle = OperationHandle::new();
        let now = chrono::Utc::now().to_rfc3339();
        let controller_epoch = i64::try_from(envelope.controller_epoch())
            .map_err(|_| OperationProblem::InvalidPlan("controller epoch 越界".into()))?;
        let root_epoch = envelope
            .root_epoch()
            .map(i64::try_from)
            .transpose()
            .map_err(|_| OperationProblem::InvalidPlan("root epoch 越界".into()))?;
        let progress = OperationProgress::initial(
            plan.steps
                .iter()
                .filter(|step| step.role == StepRole::Forward)
                .count(),
        );
        let _gate = self.service.command_gate();
        let handle = self
            .service
            .with_tx(|tx| {
                if let Some((existing, frozen, existing_digest)) = tx
                    .query_row(
                        "SELECT o.operation_handle, o.saga_state, i.semantic_digest
                         FROM operation o
                         JOIN command_intent i ON i.command_id=o.command_id
                         WHERE o.command_id=?1",
                        [envelope.command_id().as_str()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(internal_sql)?
                {
                    if frozen != saga_state || existing_digest != digest {
                        return Err(anyhow::Error::new(OperationProblem::CommandIdReused));
                    }
                    return OperationHandle::parse(existing)
                        .map_err(|e| anyhow::Error::new(OperationProblem::Internal(e.to_string())));
                }
                if let Some(intent) = intent_tx(tx, envelope.command_id().as_str())
                    .map_err(|error| anyhow::Error::new(OperationProblem::Internal(error.to_string())))?
                {
                    // intent 存在但没有 operation:同 id 异 digest 报 reused;
                    // 同 digest 属单命令链残留,不接受改造成 saga。
                    if intent.semantic_digest != digest {
                        return Err(anyhow::Error::new(OperationProblem::CommandIdReused));
                    }
                    return Err(anyhow::Error::new(OperationProblem::PlanConflict(
                        "command_id 已被单命令 intent 使用".into(),
                    )));
                }
                tx.execute(
                    "INSERT INTO command_intent
                     (command_id, semantic_digest, target_store, aggregate, principal,
                      client_id, controller_epoch, root_epoch, state, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'reserved', ?9)",
                    params![
                        envelope.command_id().as_str(),
                        digest,
                        envelope.target().store_key(),
                        envelope.target().aggregate.handle,
                        envelope.principal().as_str(),
                        envelope.client_id().as_str(),
                        controller_epoch,
                        root_epoch,
                        now,
                    ],
                )
                .map_err(internal_sql)?;
                tx.execute(
                    "INSERT INTO operation
                     (operation_handle, command_id, kind, state, saga_state, progress_json,
                      created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'accepted', ?4, ?5, ?6, ?6)",
                    params![
                        handle.as_str(),
                        envelope.command_id().as_str(),
                        plan.kind.as_str(),
                        saga_state,
                        serde_json::to_string(&progress).map_err(internal_sql)?,
                        now,
                    ],
                )
                .map_err(internal_sql)?;
                for (index, step) in plan.steps.iter().enumerate() {
                    let expected = canonical_json(&Value::Array(canonical_expected_revisions(
                        &step.expected,
                    )))
                    .map_err(|e| anyhow::Error::new(OperationProblem::Internal(e.to_string())))?;
                    tx.execute(
                        "INSERT INTO operation_step
                         (operation_handle, step_index, role, step_id, target_store, aggregate,
                          semantic_digest, expected_json, compensates, state, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10, ?10)",
                        params![
                            handle.as_str(),
                            i64::try_from(index).map_err(internal_sql)?,
                            step.role.as_str(),
                            step.step_id.as_str(),
                            step.target.store_key(),
                            step.target.aggregate.handle,
                            step.semantic_digest(),
                            expected,
                            step.compensates
                                .map(i64::try_from)
                                .transpose()
                                .map_err(internal_sql)?,
                            now,
                        ],
                    )
                    .map_err(internal_sql)?;
                }
                Ok(handle.clone())
            })
            .map_err(problem_from_anyhow)?;

        if fault == Some(OperationAcceptFaultPoint::AfterIntentReserve) {
            return Err(OperationProblem::FaultInjected("after_intent_reserve"));
        }

        let prior_error = self
            .service
            .with_conn(|conn| {
                let intent = intent_tx(conn, envelope.command_id().as_str())?
                    .ok_or_else(|| anyhow::anyhow!("operation intent 消失"))?;
                let error = match intent.state {
                    IntentState::Reserved => None,
                    IntentState::Applied => Some(CommandProblem::Internal(
                        "terminal applied intent 缺少 acceptance receipt".into(),
                    )),
                    IntentState::Failed => intent
                        .problem_code
                        .as_deref()
                        .and_then(problem_for_terminal_code)
                        .or(Some(CommandProblem::PreviousCommandFailed)),
                    IntentState::Cancelled => Some(CommandProblem::PreviousCommandCancelled),
                    IntentState::Revoked => intent
                        .problem_code
                        .as_deref()
                        .and_then(problem_for_terminal_code)
                        .or(Some(CommandProblem::ControllerLeaseExpired)),
                };
                Ok(error)
            })
            .map_err(problem_from_anyhow)?;

        // 与单命令链相同：service IMMEDIATE coordinator transaction 覆盖
        // target commit 到返回 handle 的窗口。target receipt 是 accepted 的
        // durable 证明；重试先复验当前 lease，再读取 receipt。
        let target_result = self
            .service
            .with_tx(|service_tx| {
                let target_result = target.with_tx(|tx| {
                    let lease_check = envelope.lease_check();
                    let permit = authorizer.acquire(tx, &lease_check)?;
                    if let Some(existing_handle) = acceptance_receipt(
                        tx,
                        envelope.command_id().as_str(),
                        &digest,
                        &envelope.target().aggregate.handle,
                    )? {
                        if existing_handle != handle {
                            return Err(CommandProblem::CommandIdReused);
                        }
                        return Ok(permit);
                    }
                    if let Some(error) = prior_error.clone() {
                        return Err(error);
                    }
                    permit.validate_expected(tx, &lease_check)?;
                    let result = canonical_json(&serde_json::json!({
                        "operation_handle": handle.as_str(),
                    }))?;
                    let event = canonical_json(&serde_json::json!({
                        "type": format!("{}.accepted", envelope.command_type().as_str()),
                        "aggregate": {
                            "kind": envelope.target().aggregate.kind.as_str(),
                            "handle": envelope.target().aggregate.handle,
                        },
                        "caused_by_command_id": envelope.command_id().as_str(),
                        "operation": { "handle": handle.as_str() },
                    }))?;
                    let now = chrono::Utc::now().to_rfc3339();
                    tx.execute(
                        "INSERT INTO command_receipt
                         (command_id, semantic_digest, aggregate_handle, result_revisions,
                          state, created_at, finalized_at)
                         VALUES (?1, ?2, ?3, ?4, 'applied', ?5, ?5)",
                        params![
                            envelope.command_id().as_str(),
                            digest,
                            envelope.target().aggregate.handle,
                            result,
                            now,
                        ],
                    )
                    .map_err(internal_command)?;
                    tx.execute(
                        "INSERT INTO projection_outbox(event_json, published_at)
                         VALUES (?1, NULL)",
                        [event],
                    )
                    .map_err(internal_command)?;
                    Ok(permit)
                });
                match target_result {
                    Ok(_permit) => {}
                    Err(error) => {
                        let terminal = match &error {
                            CommandProblem::ControllerLeaseExpired
                            | CommandProblem::RootEpochExpired => Some(IntentState::Revoked),
                            CommandProblem::RevisionConflict
                            | CommandProblem::InvalidEnvelope(_)
                            | CommandProblem::CommandIdReused => Some(IntentState::Failed),
                            _ => None,
                        };
                        if let Some(state) = terminal {
                            for step in steps_tx(service_tx, &handle)? {
                                if step.state == StepState::Pending {
                                    finish_step_tx(
                                        service_tx,
                                        &handle,
                                        step.step_index,
                                        StepState::Revoked,
                                        Some(error.code()),
                                        None,
                                    )?;
                                }
                            }
                            finalize_tx(
                                service_tx,
                                &handle,
                                envelope.command_id().as_str(),
                                &TerminalKind::RejectedBeforeAccept {
                                    intent_state: state,
                                    intent_code: Some(error.code().to_string()),
                                    problem_code: error.code().to_string(),
                                },
                            )?;
                        }
                        return Ok(Err(error));
                    }
                }
                if fault == Some(OperationAcceptFaultPoint::AfterTargetCommit) {
                    return Ok(Err(CommandProblem::FaultInjected(
                        "after_operation_accept_target_commit",
                    )));
                }
                Ok(Ok(()))
            })
            .map_err(problem_from_anyhow)?;
        target_result.map_err(OperationProblem::Command)?;
        Ok(handle)
    }

    /// 执行 saga:forward 按序,失败进入 compensation;全程以 durable
    /// receipt/step 状态推进,已 succeeded 的 step 跳过 effect(幂等 resume)。
    /// 重启后的恢复不走这里——reconcile 只读 receipt 终结,不重放业务写。
    pub(crate) fn run(
        &self,
        handle: &OperationHandle,
        plan: &OperationPlan,
        targets: &[TargetDatabase],
        authorizer: &dyn CommandAuthorizer,
        effects: Vec<StepEffect>,
        fault: Option<OperationFaultPoint>,
    ) -> Result<OperationOutcome, OperationProblem> {
        plan.validate()?;
        if effects.len() != plan.steps.len() {
            return Err(OperationProblem::PlanConflict(
                "effects 与 plan steps 数量不一致".into(),
            ));
        }
        let _gate = self.service.command_gate();
        let identity = self.load_identity(handle)?;
        self.verify_plan(handle, plan)?;
        let mut effects: Vec<Option<StepEffect>> = effects.into_iter().map(Some).collect();
        let take_effect = |effects: &mut Vec<Option<StepEffect>>, index: usize| {
            effects[index].take().ok_or_else(|| {
                OperationProblem::PlanConflict(format!("step {index} 的 effect 已被消费"))
            })
        };
        for index in 0..plan.steps.len() {
            if plan.steps[index].role != StepRole::Forward {
                continue;
            }
            let outcome = self.execute_step(
                handle,
                index,
                &plan.steps[index],
                &identity,
                targets,
                authorizer,
                take_effect(&mut effects, index)?,
                fault,
            )?;
            if fault == Some(OperationFaultPoint::AfterStepFinalized(index)) {
                return Err(OperationProblem::FaultInjected("after_step_finalized"));
            }
            if matches!(outcome, StepExecution::Failed) {
                break;
            }
        }
        // 决策循环只读 durable step 状态(不含内存计数)。
        loop {
            let steps = steps_of(&self.service, handle)?;
            let command_id = self.command_id(handle)?;
            match decide_saga(&steps) {
                SagaDecision::Complete => {
                    return self.finalize(handle, &command_id, TerminalKind::Completed, fault);
                }
                SagaDecision::CompensationOpen {
                    needed,
                    failed,
                    missing,
                } => {
                    if let Some(index) = missing {
                        return self.finalize(
                            handle,
                            &command_id,
                            TerminalKind::NeedsYou {
                                code: CODE_COMPENSATION_MISSING,
                                step_index: Some(index),
                            },
                            fault,
                        );
                    }
                    if let Some(index) = failed {
                        return self.finalize(
                            handle,
                            &command_id,
                            TerminalKind::NeedsYou {
                                code: CODE_COMPENSATION_FAILED,
                                step_index: Some(index),
                            },
                            fault,
                        );
                    }
                    let pending: Vec<usize> = needed
                        .iter()
                        .copied()
                        .filter(|index| steps[*index].state == StepState::Pending)
                        .collect();
                    if pending.is_empty() {
                        if needed
                            .iter()
                            .any(|index| steps[*index].state != StepState::Succeeded)
                        {
                            // 存在被终结为 revoked 的补偿步(此前 lease 失效),
                            // 自动补偿无法完成。
                            return self.finalize(
                                handle,
                                &command_id,
                                TerminalKind::NeedsYou {
                                    code: CODE_COMPENSATION_REQUIRED,
                                    step_index: None,
                                },
                                fault,
                            );
                        }
                        // 全部补偿步持有 receipt:完整回滚,不伪造成功。
                        return self.finalize(
                            handle,
                            &command_id,
                            TerminalKind::CompletedCompensated,
                            fault,
                        );
                    }
                    self.mark_compensating(handle, fault)?;
                    // 撤销按 forward 完成序逆序执行(最新生效的最先回滚)。
                    for index in pending.iter().rev() {
                        let outcome = self.execute_step(
                            handle,
                            *index,
                            &plan.steps[*index],
                            &identity,
                            targets,
                            authorizer,
                            take_effect(&mut effects, *index)?,
                            fault,
                        )?;
                        if fault == Some(OperationFaultPoint::AfterStepFinalized(*index)) {
                            return Err(OperationProblem::FaultInjected("after_step_finalized"));
                        }
                        if matches!(outcome, StepExecution::Failed) {
                            return self.finalize(
                                handle,
                                &command_id,
                                TerminalKind::NeedsYou {
                                    code: CODE_COMPENSATION_FAILED,
                                    step_index: Some(*index),
                                },
                                fault,
                            );
                        }
                    }
                }
            }
        }
    }

    /// 单个 step 的原子链:service 事务内推进状态,目标 Store 事务
    /// (L-CMD)写 effect + receipt + outbox;receipt 已存在则跳过 effect。
    #[allow(clippy::too_many_arguments)]
    fn execute_step(
        &self,
        handle: &OperationHandle,
        index: usize,
        step: &SagaStepPlan,
        identity: &IntentIdentity,
        targets: &[TargetDatabase],
        authorizer: &dyn CommandAuthorizer,
        effect: StepEffect,
        fault: Option<OperationFaultPoint>,
    ) -> Result<StepExecution, OperationProblem> {
        let target = targets
            .iter()
            .find(|target| target.store_key() == step.target.store_key())
            .ok_or_else(|| {
                OperationProblem::TargetStoreMismatch(format!(
                    "step {index} 的目标 store 未打开:{}",
                    step.target.store_key()
                ))
            })?;
        let lease_check = LeaseCheck {
            principal: &identity.principal,
            client_id: &identity.client_id,
            controller_epoch: identity.controller_epoch,
            root_epoch: identity.root_epoch,
            command_type: step.command_type,
            target: &step.target,
            expected: &step.expected,
        };
        let mut effect = Some(effect);
        let result = self
            .service
            .with_tx(|service_tx| {
                let operation = operation_tx(service_tx, handle)?
                    .ok_or_else(|| anyhow::Error::new(OperationProblem::OperationNotFound))?;
                match step.role {
                    StepRole::Forward => {
                        if !matches!(
                            operation.state,
                            OperationState::Accepted | OperationState::Running
                        ) {
                            return Err(anyhow::Error::new(OperationProblem::StateConflict(
                                format!(
                                    "forward step 只能在 accepted/running 执行(当前 {})",
                                    operation.state.as_str()
                                ),
                            )));
                        }
                    }
                    StepRole::Compensate => {
                        if operation.state != OperationState::Compensating {
                            return Err(anyhow::Error::new(OperationProblem::StateConflict(
                                format!(
                                    "compensate step 只能在 compensating 执行(当前 {})",
                                    operation.state.as_str()
                                ),
                            )));
                        }
                    }
                }
                if operation.state == OperationState::Accepted {
                    service_tx
                        .execute(
                            "UPDATE operation SET state='running', updated_at=?2
                             WHERE operation_handle=?1 AND state='accepted'",
                            params![handle.as_str(), chrono::Utc::now().to_rfc3339()],
                        )
                        .map_err(internal_sql)?;
                }
                let record = step_tx(service_tx, handle, index)?.ok_or_else(|| {
                    anyhow::Error::new(OperationProblem::Internal(format!(
                        "operation_step 行缺失:{index}"
                    )))
                })?;
                match record.state {
                    StepState::Succeeded => return Ok(Ok(StepExecution::Succeeded)),
                    StepState::Pending => {}
                    other => {
                        return Err(anyhow::Error::new(OperationProblem::StateConflict(
                            format!("step {index} 已终结:{}", other.as_str()),
                        )));
                    }
                }
                let target_result = target.with_tx(|tx| {
                    let permit = authorizer.acquire(tx, &lease_check)?;
                    if let Some(revisions) = receipt_revisions(
                        tx,
                        step.step_id.as_str(),
                        step.semantic_digest(),
                        &step.target.aggregate,
                    )? {
                        return Ok((revisions, permit));
                    }
                    permit.validate_expected(tx, &lease_check)?;
                    let output = effect.take().expect("effect 只消费一次")(tx)?;
                    if value_has_sensitive_field(&output.result_revisions)
                        || value_has_sensitive_field(&output.projection)
                    {
                        return Err(CommandProblem::InvalidEnvelope(
                            "Secret 明文或可复用引用进入 step result/event".into(),
                        ));
                    }
                    let now = chrono::Utc::now().to_rfc3339();
                    tx.execute(
                        "INSERT INTO command_receipt
                         (command_id, semantic_digest, aggregate_handle, result_revisions,
                          state, created_at, finalized_at)
                         VALUES (?1, ?2, ?3, ?4, 'applied', ?5, ?5)",
                        params![
                            step.step_id.as_str(),
                            step.semantic_digest(),
                            step.target.aggregate.handle,
                            canonical_json(&output.result_revisions)?,
                            now,
                        ],
                    )
                    .map_err(internal_command)?;
                    let event = canonical_json(&serde_json::json!({
                        "type": format!("{}.applied", step.command_type.as_str()),
                        "aggregate": {
                            "kind": step.target.aggregate.kind.as_str(),
                            "handle": step.target.aggregate.handle,
                        },
                        "caused_by_command_id": operation.command_id,
                        "operation": {
                            "handle": handle.as_str(),
                            "step_index": index,
                            "step_id": step.step_id.as_str(),
                            "role": step.role.as_str(),
                        },
                        "projection": output.projection,
                    }))?;
                    tx.execute(
                        "INSERT INTO projection_outbox(event_json, published_at)
                         VALUES (?1, NULL)",
                        [event],
                    )
                    .map_err(internal_command)?;
                    Ok((output.result_revisions, permit))
                });
                let (revisions, _permit) = match target_result {
                    Ok(value) => value,
                    Err(error) => {
                        // 终态问题(lease/CAS/envelope/receipt 身份)把 step
                        // 行终结为 failed/revoked;瞬态错误保持 pending。
                        if let Some(state) = terminal_step_state(&error) {
                            finish_step_tx(
                                service_tx,
                                handle,
                                index,
                                state,
                                Some(error.code()),
                                None,
                            )?;
                        }
                        return Ok(Err(error));
                    }
                };
                if fault == Some(OperationFaultPoint::AfterStepTargetCommit(index)) {
                    // service 事务提交但 step 行保持 pending:重启后由
                    // reconcile 依 receipt 补齐(fault harness,§14.2)。
                    return Ok(Err(CommandProblem::FaultInjected(
                        "after_step_target_commit",
                    )));
                }
                finish_step_tx(
                    service_tx,
                    handle,
                    index,
                    StepState::Succeeded,
                    None,
                    Some(&revisions),
                )?;
                Ok(Ok(StepExecution::Succeeded))
            })
            .map_err(problem_from_anyhow);
        match result {
            Ok(Ok(execution)) => Ok(execution),
            Ok(Err(CommandProblem::FaultInjected(_))) => {
                Err(OperationProblem::FaultInjected("after_step_target_commit"))
            }
            Ok(Err(problem)) => {
                if terminal_step_state(&problem).is_some() {
                    Ok(StepExecution::Failed)
                } else {
                    // 瞬态错误:step 仍 pending,不触发补偿,交调用方重试。
                    Err(problem_from_anyhow(anyhow::Error::new(problem)))
                }
            }
            Err(problem) => Err(problem),
        }
    }

    /// live run 的补偿入口标记(崩溃窗口:标记已提交、补偿未开始)。
    fn mark_compensating(
        &self,
        handle: &OperationHandle,
        fault: Option<OperationFaultPoint>,
    ) -> Result<(), OperationProblem> {
        self.service
            .with_tx(|tx| {
                tx.execute(
                    "UPDATE operation SET state='compensating', updated_at=?2
                     WHERE operation_handle=?1 AND state IN ('accepted','running')",
                    params![handle.as_str(), chrono::Utc::now().to_rfc3339()],
                )
                .map_err(internal_sql)?;
                Ok(())
            })
            .map_err(problem_from_anyhow)?;
        if fault == Some(OperationFaultPoint::AfterCompensating) {
            return Err(OperationProblem::FaultInjected("after_compensating"));
        }
        Ok(())
    }

    /// 终态落盘:operation 终态 + progress + intent 终结,单一 service 事务;
    /// `CompletedCompensated`/`NeedsYou` 的 intent 落 failed(不伪造成功)。
    fn finalize(
        &self,
        handle: &OperationHandle,
        command_id: &str,
        kind: TerminalKind,
        fault: Option<OperationFaultPoint>,
    ) -> Result<OperationOutcome, OperationProblem> {
        let outcome = match &kind {
            TerminalKind::Completed => OperationOutcome::Completed { compensated: false },
            TerminalKind::CompletedCompensated => OperationOutcome::Completed { compensated: true },
            TerminalKind::NeedsYou { code, .. } => OperationOutcome::NeedsYou {
                problem_code: (*code).to_string(),
            },
            TerminalKind::RejectedBeforeAccept { problem_code, .. } => {
                OperationOutcome::NotAccepted {
                    problem_code: problem_code.clone(),
                }
            }
        };
        self.service
            .with_tx(|tx| finalize_tx(tx, handle, command_id, &kind).map_err(anyhow::Error::new))
            .map_err(problem_from_anyhow)?;
        if fault == Some(OperationFaultPoint::AfterOperationFinalized) {
            return Err(OperationProblem::FaultInjected("after_operation_finalized"));
        }
        Ok(outcome)
    }

    /// 与 durable step 行核对 plan 身份(digest/store/aggregate/id/role);
    /// 执行决策本身从不依赖内存状态——这里只拒绝「拿错 plan 来跑」。
    fn verify_plan(
        &self,
        handle: &OperationHandle,
        plan: &OperationPlan,
    ) -> Result<(), OperationProblem> {
        let durable = steps_of(&self.service, handle)?;
        if durable.len() != plan.steps.len() {
            return Err(OperationProblem::PlanConflict(
                "plan 与冻结 step 数量不一致".into(),
            ));
        }
        for (index, step) in plan.steps.iter().enumerate() {
            let record = &durable[index];
            if record.step_id.as_str() != step.step_id.as_str()
                || record.role != step.role
                || record.target_store != step.target.store_key()
                || record.aggregate != step.target.aggregate.handle
                || record.semantic_digest != step.semantic_digest()
            {
                return Err(OperationProblem::PlanConflict(format!(
                    "step {index} 与冻结 plan 不一致"
                )));
            }
        }
        Ok(())
    }

    /// initiating intent 的身份(re-run 的 lease 复验输入)。
    fn load_identity(&self, handle: &OperationHandle) -> Result<IntentIdentity, OperationProblem> {
        let command_id = self.command_id(handle)?;
        let row = self
            .service
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT principal, client_id, controller_epoch, root_epoch
                     FROM command_intent WHERE command_id=?1",
                    [&command_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )
                .optional()
                .map_err(internal_sql)?
                .ok_or_else(|| {
                    anyhow::Error::new(OperationProblem::Internal(
                        "operation 的 command intent 不存在".into(),
                    ))
                })
            })
            .map_err(problem_from_anyhow)?;
        let parse = |error: anyhow::Error| problem_from_anyhow(error);
        Ok(IntentIdentity {
            principal: Principal::parse(row.0).map_err(&parse)?,
            client_id: ClientId::parse(row.1).map_err(&parse)?,
            controller_epoch: u64::try_from(row.2).map_err(|e| parse(e.into()))?,
            root_epoch: row
                .3
                .map(u64::try_from)
                .transpose()
                .map_err(|e| parse(e.into()))?,
        })
    }

    fn command_id(&self, handle: &OperationHandle) -> Result<String, OperationProblem> {
        command_id_of(&self.service, handle)
    }
}

// ──────────────── 重启 reconcile 侧的自由函数(service 直连) ────────────────
// 这些入口不需要 idempotency key(只读 receipt,不重算 digest——receipt
// 内冻结的 digest 就是比对基准);调用方(reconcile_startup)必须已持有
// command_gate,函数自身不再获取(parking_lot 不可重入)。

/// 全部 step 行(按 step_index 排序)——重启后恢复/测试的观测面。
pub(crate) fn steps_of(
    service: &Arc<ServiceStore>,
    handle: &OperationHandle,
) -> Result<Vec<StepRecord>, OperationProblem> {
    service
        .with_conn(|conn| steps_tx(conn, handle).map_err(anyhow::Error::new))
        .map_err(problem_from_anyhow)
}

/// operation 行读视图(handle 跨重启稳定)。
#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
pub(crate) fn operation_of(
    service: &Arc<ServiceStore>,
    handle: &OperationHandle,
) -> Result<OperationRecord, OperationProblem> {
    service
        .with_conn(|conn| operation_read_tx(conn, handle).map_err(anyhow::Error::new))
        .map_err(problem_from_anyhow)
}

#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
fn command_id_of(
    service: &Arc<ServiceStore>,
    handle: &OperationHandle,
) -> Result<String, OperationProblem> {
    service
        .with_conn(|conn| {
            conn.query_row(
                "SELECT command_id FROM operation WHERE operation_handle=?1",
                [handle.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal_sql)?
            .ok_or_else(|| anyhow::Error::new(OperationProblem::OperationNotFound))
        })
        .map_err(problem_from_anyhow)
}

/// 未终结 operation 的 handle 列表(reconcile/GC 观测面)。
#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
pub(crate) fn open_operations(
    service: &Arc<ServiceStore>,
) -> Result<Vec<OperationHandle>, OperationProblem> {
    service
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT operation_handle FROM operation
                     WHERE state NOT IN ('completed','needs_you')
                     ORDER BY created_at, operation_handle",
                )
                .map_err(internal_sql)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(internal_sql)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(internal_sql)?;
            rows.into_iter()
                .map(|raw| {
                    OperationHandle::parse(raw)
                        .map_err(|e| anyhow::Error::new(OperationProblem::Internal(e.to_string())))
                })
                .collect()
        })
        .map_err(problem_from_anyhow)
}

/// 重启 reconcile 专用:把未终结 operation 推进为 `reconciling`
/// (附录 B)。幂等;返回推进数量。调用方须持有 command_gate。
#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
pub(crate) fn mark_reconciling(service: &Arc<ServiceStore>) -> Result<usize, OperationProblem> {
    service
        .with_tx(|tx| {
            let changed = tx
                .execute(
                    "UPDATE operation SET state='reconciling', updated_at=?1
                     WHERE state IN ('accepted','running','compensating')",
                    [chrono::Utc::now().to_rfc3339()],
                )
                .map_err(internal_sql)?;
            Ok(changed)
        })
        .map_err(problem_from_anyhow)
}

/// 单个 reconciling operation 的只读 receipt 终结:
/// 有 receipt 的 pending step 补 `succeeded`,无 receipt 的只终结
/// `revoked`(epoch 已失效,不重做业务写),随后按 decide_saga 终态化。
/// 无 step 行的 legacy operation 返回 None(不重放、不补造)。
/// 调用方须持有 command_gate;函数自身不再获取(不可重入)。
#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
pub(crate) fn reconcile_operation(
    service: &Arc<ServiceStore>,
    handle: &OperationHandle,
    targets: &[TargetDatabase],
) -> Result<Option<OperationOutcome>, OperationProblem> {
    let steps = steps_of(service, handle)?;
    if steps.is_empty() {
        return Ok(None);
    }
    let record = operation_of(service, handle)?;
    let command_id = record.command_id.clone();
    let intent = service
        .with_conn(|conn| {
            intent_tx(conn, command_id.as_str())
                .map_err(anyhow::Error::new)?
                .ok_or_else(|| anyhow::anyhow!("operation 的 command intent 不存在"))
        })
        .map_err(problem_from_anyhow)?;
    let Some(acceptance_target) = targets
        .iter()
        .find(|target| target.store_key() == intent.target_store)
    else {
        return Err(OperationProblem::TargetStoreMismatch(format!(
            "acceptance target store 未打开:{}",
            intent.target_store
        )));
    };
    let acceptance = acceptance_target
        .with_conn(|conn| {
            acceptance_receipt(
                conn,
                command_id.as_str(),
                &intent.semantic_digest,
                &intent.aggregate_handle,
            )
        })
        .map_err(OperationProblem::Command);
    match acceptance {
        Ok(Some(receipt_handle)) if receipt_handle == *handle => {}
        Ok(None) => {
            let (intent_state, intent_code, problem_code) = match intent.state {
                IntentState::Reserved => (
                    IntentState::Revoked,
                    Some(CommandProblem::ControllerLeaseExpired.code().to_string()),
                    CommandProblem::ControllerLeaseExpired.code().to_string(),
                ),
                IntentState::Revoked | IntentState::Failed | IntentState::Cancelled => {
                    let problem = intent
                        .problem_code
                        .clone()
                        .unwrap_or_else(|| intent.state.as_str().to_string());
                    (intent.state, intent.problem_code.clone(), problem)
                }
                IntentState::Applied => {
                    return Err(OperationProblem::Internal(
                        "applied operation intent 缺少 acceptance receipt".into(),
                    ));
                }
            };
            let kind = TerminalKind::RejectedBeforeAccept {
                intent_state,
                intent_code,
                problem_code: problem_code.clone(),
            };
            service
                .with_tx(|tx| {
                    for step in steps_tx(tx, handle)? {
                        if step.state == StepState::Pending {
                            finish_step_tx(
                                tx,
                                handle,
                                step.step_index,
                                StepState::Revoked,
                                Some(problem_code.as_str()),
                                None,
                            )?;
                        }
                    }
                    finalize_tx(tx, handle, command_id.as_str(), &kind)?;
                    Ok(())
                })
                .map_err(problem_from_anyhow)?;
            return Ok(Some(OperationOutcome::NotAccepted { problem_code }));
        }
        Ok(Some(_)) => {
            let kind = TerminalKind::NeedsYou {
                code: "command_id_reused",
                step_index: None,
            };
            service
                .with_tx(|tx| {
                    finalize_tx(tx, handle, command_id.as_str(), &kind).map_err(anyhow::Error::new)
                })
                .map_err(problem_from_anyhow)?;
            return Ok(Some(OperationOutcome::NeedsYou {
                problem_code: "command_id_reused".to_string(),
            }));
        }
        Err(error) => {
            let code = error.code();
            let kind = TerminalKind::NeedsYou {
                code,
                step_index: None,
            };
            service
                .with_tx(|tx| {
                    finalize_tx(tx, handle, command_id.as_str(), &kind).map_err(anyhow::Error::new)
                })
                .map_err(problem_from_anyhow)?;
            return Ok(Some(OperationOutcome::NeedsYou {
                problem_code: code.to_string(),
            }));
        }
    }
    service
        .with_tx(|tx| {
            let operation = operation_tx(tx, handle)?
                .ok_or_else(|| anyhow::Error::new(OperationProblem::OperationNotFound))?;
            if operation.state.is_terminal() {
                // 竞态下已被终结:幂等返回既有终态。
                return Ok(Some(match operation.state {
                    OperationState::NeedsYou => OperationOutcome::NeedsYou {
                        problem_code: operation
                            .progress
                            .problem
                            .as_ref()
                            .map(|problem| problem.code.clone())
                            .unwrap_or_else(|| "operation_needs_you".to_string()),
                    },
                    OperationState::Completed
                        if operation.progress.outcome.as_deref() == Some("not_accepted") =>
                    {
                        OperationOutcome::NotAccepted {
                            problem_code: operation
                                .progress
                                .problem
                                .as_ref()
                                .map(|problem| problem.code.clone())
                                .unwrap_or_else(|| "operation_not_accepted".to_string()),
                        }
                    }
                    _ => OperationOutcome::Completed { compensated: false },
                }));
            }
            if operation.state != OperationState::Reconciling {
                return Err(anyhow::Error::new(OperationProblem::StateConflict(
                    format!(
                        "只有 reconciling operation 可被 reconcile(当前 {})",
                        operation.state.as_str()
                    ),
                )));
            }
            let mut integrity_violation: Option<usize> = None;
            for record in steps_tx(tx, handle)? {
                if record.state != StepState::Pending {
                    continue;
                }
                let Some(target) = targets
                    .iter()
                    .find(|t| t.store_key() == record.target_store)
                else {
                    return Err(anyhow::Error::new(OperationProblem::TargetStoreMismatch(
                        format!("target store 未打开:{}", record.target_store),
                    )));
                };
                let receipt: Option<(String, String, String, String, Option<String>)> = target
                    .with_conn(|conn| {
                        conn.query_row(
                            "SELECT semantic_digest, aggregate_handle, result_revisions,
                                    state, finalized_at
                             FROM command_receipt WHERE command_id=?1",
                            [record.step_id.as_str()],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                    row.get::<_, String>(3)?,
                                    row.get::<_, Option<String>>(4)?,
                                ))
                            },
                        )
                        .optional()
                        .map_err(|e| CommandProblem::Internal(e.to_string()))
                    })
                    .map_err(|e| anyhow::Error::new(internal_op(e)))?;
                match receipt {
                    Some((digest, aggregate, result, state, finalized_at))
                        if digest == record.semantic_digest
                            && aggregate == record.aggregate
                            && state == "applied"
                            && finalized_at.is_some() =>
                    {
                        let result: Value = serde_json::from_str(&result).map_err(|error| {
                            anyhow::Error::new(OperationProblem::Internal(error.to_string()))
                        })?;
                        if value_has_sensitive_field(&result) {
                            return Err(anyhow::Error::new(OperationProblem::Internal(
                                "target receipt result 含敏感字段".into(),
                            )));
                        }
                        finish_step_tx(
                            tx,
                            handle,
                            record.step_index,
                            StepState::Succeeded,
                            None,
                            Some(&result),
                        )?;
                    }
                    Some(_) => {
                        // receipt 存在但身份不符:数据完整性告警,step 稳定
                        // 标记 failed、operation 强制 Needs You(即使没有已
                        // 生效 step 可补偿,也不得宣称平凡回滚完成)。
                        finish_step_tx(
                            tx,
                            handle,
                            record.step_index,
                            StepState::Failed,
                            Some("command_id_reused"),
                            None,
                        )?;
                        if integrity_violation.is_none() {
                            integrity_violation = Some(record.step_index);
                        }
                    }
                    None => {
                        finish_step_tx(
                            tx,
                            handle,
                            record.step_index,
                            StepState::Revoked,
                            Some("controller_lease_expired"),
                            None,
                        )?;
                    }
                }
            }
            let steps = steps_tx(tx, handle)?;
            let kind = if let Some(index) = integrity_violation {
                TerminalKind::NeedsYou {
                    code: "command_id_reused",
                    step_index: Some(index),
                }
            } else {
                match decide_saga(&steps) {
                    SagaDecision::Complete => TerminalKind::Completed,
                    SagaDecision::CompensationOpen {
                        needed,
                        failed,
                        missing,
                    } => {
                        if let Some(index) = missing {
                            TerminalKind::NeedsYou {
                                code: CODE_COMPENSATION_MISSING,
                                step_index: Some(index),
                            }
                        } else if let Some(index) = failed {
                            TerminalKind::NeedsYou {
                                code: CODE_COMPENSATION_FAILED,
                                step_index: Some(index),
                            }
                        } else if needed
                            .iter()
                            .any(|index| steps[*index].state != StepState::Succeeded)
                        {
                            // 重启后 lease/epoch 已失效,补偿无法自动完成。
                            TerminalKind::NeedsYou {
                                code: CODE_COMPENSATION_REQUIRED,
                                step_index: None,
                            }
                        } else {
                            // 全部补偿步持有 receipt:完整回滚,不伪造成功。
                            TerminalKind::CompletedCompensated
                        }
                    }
                }
            };
            finalize_tx(tx, handle, command_id.as_str(), &kind)?;
            Ok(Some(match kind {
                TerminalKind::Completed => OperationOutcome::Completed { compensated: false },
                TerminalKind::CompletedCompensated => {
                    OperationOutcome::Completed { compensated: true }
                }
                TerminalKind::NeedsYou { code, .. } => OperationOutcome::NeedsYou {
                    problem_code: code.to_string(),
                },
                TerminalKind::RejectedBeforeAccept { problem_code, .. } => {
                    OperationOutcome::NotAccepted { problem_code }
                }
            }))
        })
        .map_err(problem_from_anyhow)
}

// ───────────────────────────── 内部类型与 SQL ─────────────────────────────

/// run 的 lease 复验身份(来自 initiating intent 行,跨重启后不再用于
/// 执行——reconcile 路径完全不构造 lease)。
#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
struct IntentIdentity {
    principal: Principal,
    client_id: ClientId,
    controller_epoch: u64,
    root_epoch: Option<u64>,
}

#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
enum StepExecution {
    Succeeded,
    Failed,
}

/// operation 终态种类(run 与 reconcile 共用)。
enum TerminalKind {
    Completed,
    CompletedCompensated,
    NeedsYou {
        code: &'static str,
        step_index: Option<usize>,
    },
    RejectedBeforeAccept {
        intent_state: IntentState,
        intent_code: Option<String>,
        problem_code: String,
    },
}

#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
fn terminal_step_state(problem: &CommandProblem) -> Option<StepState> {
    match problem {
        CommandProblem::ControllerLeaseExpired | CommandProblem::RootEpochExpired => {
            Some(StepState::Revoked)
        }
        CommandProblem::RevisionConflict
        | CommandProblem::InvalidEnvelope(_)
        | CommandProblem::CommandIdReused => Some(StepState::Failed),
        _ => None,
    }
}

/// 终态写库:operation 终态 + progress(outcome/problem)+ intent 终结。
fn finalize_tx(
    tx: &Transaction<'_>,
    handle: &OperationHandle,
    command_id: &str,
    kind: &TerminalKind,
) -> Result<(), OperationProblem> {
    let steps = steps_tx(tx, handle)?;
    let mut progress = progress_tx(tx, handle)?;
    progress.forward_succeeded = steps
        .iter()
        .filter(|step| step.role == StepRole::Forward && step.state == StepState::Succeeded)
        .count();
    let (state, intent_state, intent_code) = match kind {
        TerminalKind::Completed => (OperationState::Completed, IntentState::Applied, None),
        TerminalKind::CompletedCompensated => (
            OperationState::Completed,
            IntentState::Failed,
            Some(CODE_OPERATION_COMPENSATED),
        ),
        TerminalKind::NeedsYou { code, step_index } => {
            progress.problem = Some(OperationProblemDetail {
                code: (*code).to_string(),
                step_index: *step_index,
            });
            (OperationState::NeedsYou, IntentState::Failed, Some(*code))
        }
        TerminalKind::RejectedBeforeAccept {
            intent_state,
            intent_code,
            problem_code,
        } => {
            progress.outcome = Some("not_accepted".to_string());
            progress.problem = Some(OperationProblemDetail {
                code: problem_code.clone(),
                step_index: None,
            });
            (
                OperationState::Completed,
                *intent_state,
                intent_code.as_deref(),
            )
        }
    };
    if matches!(kind, TerminalKind::CompletedCompensated) {
        progress.outcome = Some("compensated".to_string());
    }
    let changed = tx
        .execute(
            "UPDATE operation SET state=?2, progress_json=?3, updated_at=?4
             WHERE operation_handle=?1 AND state NOT IN ('completed','needs_you')",
            params![
                handle.as_str(),
                state.as_str(),
                serde_json::to_string(&progress).map_err(op_internal)?,
                chrono::Utc::now().to_rfc3339(),
            ],
        )
        .map_err(op_internal)?;
    if changed != 1 {
        let current = operation_tx(tx, handle)?.ok_or(OperationProblem::OperationNotFound)?;
        if current.state == state {
            return Ok(()); // 幂等重放同一终态。
        }
        return Err(OperationProblem::StateConflict(format!(
            "operation 终态化冲突:current={} attempted={}",
            current.state.as_str(),
            state.as_str()
        )));
    }
    finish_intent_tx(tx, command_id, intent_state, intent_code)
        .map_err(|error| OperationProblem::Internal(error.to_string()))
}

fn operation_tx(
    conn: &Connection,
    handle: &OperationHandle,
) -> Result<Option<OperationRecord>, OperationProblem> {
    conn.query_row(
        "SELECT operation_handle, command_id, kind, state, saga_state, progress_json,
                created_at, updated_at
         FROM operation WHERE operation_handle=?1",
        [handle.as_str()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        },
    )
    .optional()
    .map_err(|e| OperationProblem::Internal(e.to_string()))?
    .map(
        |(handle, command_id, kind, state, saga_state, progress, created_at, updated_at)| {
            Ok(OperationRecord {
                operation_handle: OperationHandle::parse(handle)
                    .map_err(|e| OperationProblem::Internal(e.to_string()))?,
                command_id: CommandId::parse(command_id)
                    .map_err(|e| OperationProblem::Internal(e.to_string()))?,
                kind: OperationKind::parse(kind)
                    .map_err(|e| OperationProblem::Internal(e.to_string()))?,
                state: OperationState::parse(&state)?,
                saga_state,
                progress: serde_json::from_str(&progress)
                    .map_err(|e| OperationProblem::Internal(e.to_string()))?,
                created_at,
                updated_at,
            })
        },
    )
    .transpose()
}

#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
fn operation_read_tx(
    conn: &Connection,
    handle: &OperationHandle,
) -> Result<OperationRecord, OperationProblem> {
    operation_tx(conn, handle)?.ok_or(OperationProblem::OperationNotFound)
}

type StepRow = (
    i64,
    String,
    String,
    String,
    String,
    String,
    Option<i64>,
    String,
    String,
    Option<String>,
);

fn parse_step_row(row: StepRow) -> Result<StepRecord, OperationProblem> {
    let (
        index,
        role,
        step_id,
        target_store,
        aggregate,
        digest,
        compensates,
        state,
        result_json,
        problem_code,
    ) = row;
    Ok(StepRecord {
        step_index: usize::try_from(index)
            .map_err(|e| OperationProblem::Internal(e.to_string()))?,
        role: StepRole::parse(&role)?,
        step_id: StepId::parse(step_id).map_err(|e| OperationProblem::Internal(e.to_string()))?,
        target_store,
        aggregate,
        semantic_digest: digest,
        compensates: compensates
            .map(usize::try_from)
            .transpose()
            .map_err(|e| OperationProblem::Internal(e.to_string()))?,
        state: StepState::parse(&state)?,
        result: serde_json::from_str(&result_json)
            .map_err(|error| OperationProblem::Internal(error.to_string()))?,
        problem_code,
    })
}

const STEP_COLUMNS: &str = "step_index, role, step_id, target_store, aggregate, semantic_digest,
                           compensates, state, result_json, problem_code";

fn step_tx(
    conn: &Connection,
    handle: &OperationHandle,
    index: usize,
) -> Result<Option<StepRecord>, OperationProblem> {
    conn.query_row(
        &format!(
            "SELECT {STEP_COLUMNS} FROM operation_step
                  WHERE operation_handle=?1 AND step_index=?2"
        ),
        params![
            handle.as_str(),
            i64::try_from(index).map_err(|e| OperationProblem::Internal(e.to_string()))?
        ],
        map_step_row,
    )
    .optional()
    .map_err(|e| OperationProblem::Internal(e.to_string()))?
    .map(parse_step_row)
    .transpose()
}

fn steps_tx(
    conn: &Connection,
    handle: &OperationHandle,
) -> Result<Vec<StepRecord>, OperationProblem> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {STEP_COLUMNS} FROM operation_step
             WHERE operation_handle=?1 ORDER BY step_index"
        ))
        .map_err(|e| OperationProblem::Internal(e.to_string()))?;
    let rows = stmt
        .query_map([handle.as_str()], map_step_row)
        .map_err(|e| OperationProblem::Internal(e.to_string()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| OperationProblem::Internal(e.to_string()))?;
    rows.into_iter().map(parse_step_row).collect()
}

fn map_step_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StepRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn progress_tx(
    conn: &Connection,
    handle: &OperationHandle,
) -> Result<OperationProgress, OperationProblem> {
    let raw: String = conn
        .query_row(
            "SELECT progress_json FROM operation WHERE operation_handle=?1",
            [handle.as_str()],
            |row| row.get(0),
        )
        .map_err(|e| OperationProblem::Internal(e.to_string()))?;
    serde_json::from_str(&raw).map_err(|e| OperationProblem::Internal(e.to_string()))
}

fn finish_step_tx(
    tx: &Transaction<'_>,
    handle: &OperationHandle,
    index: usize,
    state: StepState,
    problem_code: Option<&str>,
    result: Option<&Value>,
) -> Result<(), OperationProblem> {
    let result_json = match result {
        Some(value) => {
            canonical_json(value).map_err(|e| OperationProblem::Internal(e.to_string()))?
        }
        None => "{}".to_string(),
    };
    let index_i64 = i64::try_from(index).map_err(|e| OperationProblem::Internal(e.to_string()))?;
    let changed = tx
        .execute(
            "UPDATE operation_step SET state=?3, problem_code=?4, result_json=?5, updated_at=?6
             WHERE operation_handle=?1 AND step_index=?2 AND state='pending'",
            params![
                handle.as_str(),
                index_i64,
                state.as_str(),
                problem_code,
                result_json,
                chrono::Utc::now().to_rfc3339(),
            ],
        )
        .map_err(|e| OperationProblem::Internal(e.to_string()))?;
    if changed == 1 {
        return Ok(());
    }
    let current = step_tx(tx, handle, index)?
        .ok_or_else(|| OperationProblem::Internal("operation_step 行消失".into()))?;
    if current.state == state && current.problem_code.as_deref() == problem_code {
        Ok(())
    } else {
        Err(OperationProblem::Internal(format!(
            "step {index} 终态冲突:current={} attempted={}",
            current.state.as_str(),
            state.as_str()
        )))
    }
}

/// initiating command 的 target-local acceptance receipt。只有 applied 且
/// finalized 的行可证明 202 已被接受；结果中的 handle 必须与 service
/// reservation 一致，避免 target commit 后重试生成第二个 handle。
fn acceptance_receipt(
    conn: &Connection,
    command_id: &str,
    semantic_digest: &str,
    aggregate_handle: &str,
) -> Result<Option<OperationHandle>, CommandProblem> {
    let Some(value) =
        validated_receipt_result(conn, command_id, semantic_digest, aggregate_handle)?
    else {
        return Ok(None);
    };
    let raw = value
        .get("operation_handle")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CommandProblem::Internal("acceptance receipt 缺少 operation_handle".into())
        })?;
    OperationHandle::parse(raw)
        .map(Some)
        .map_err(internal_command)
}

/// 依 step_id 查 target receipt:digest/aggregate 不符 → `command_id_reused`
/// fail-closed(与 #21 `receipt_outcome` 同口径),receipt 即幂等凭证。
#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
fn receipt_revisions(
    tx: &Transaction<'_>,
    step_id: &str,
    semantic_digest: &str,
    aggregate: &AggregateRef,
) -> Result<Option<Value>, CommandProblem> {
    validated_receipt_result(tx, step_id, semantic_digest, &aggregate.handle)
}

#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
fn internal_command(error: impl fmt::Display) -> CommandProblem {
    CommandProblem::Internal(error.to_string())
}

fn op_internal(error: impl fmt::Display) -> OperationProblem {
    OperationProblem::Internal(error.to_string())
}

#[allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.
fn internal_op(error: CommandProblem) -> OperationProblem {
    match error {
        CommandProblem::CommandIdReused => OperationProblem::CommandIdReused,
        other => OperationProblem::Internal(other.to_string()),
    }
}

fn internal_sql(error: impl std::error::Error + Send + Sync + 'static) -> anyhow::Error {
    anyhow::Error::new(OperationProblem::Internal(error.to_string()))
}

fn problem_from_anyhow(error: anyhow::Error) -> OperationProblem {
    error
        .downcast_ref::<OperationProblem>()
        .cloned()
        .unwrap_or_else(|| OperationProblem::Internal(format!("{error:#}")))
}
