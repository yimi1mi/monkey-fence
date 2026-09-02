//! Workflow Run Start 的 Orchestrator 编译边界与 Operation 装配。
//!
//! `WorkflowStartPort` 是可信编译 seam:生产 Orchestrator adapter 落地前
//! `WorkflowRunCommand::Start` 保持显式 fail-closed,不伪装可用。
//! dispatch 只做 accept:prepare 得到的 [`PreparedWorkflowStartPlan`] 作为
//! durable frozen payload 随 `operation.saga_state` 落盘,然后立即返回
//! `202 accepted`;完整启动由后台 worker(`run_workflow_start_operation`)
//! 在 L-PUBLISH 下按 Operation steps 执行,同 command_id 绝不重复创建
//! Task/Workflow Run。
//!
//! saga 结构(§4.2):
//!
//! - step 0 `materialize`(forward):Draft Task + 冻结 Pipeline Revision
//!   (含 Agent Instance/plugin/directory provider pins 与 content digest);
//! - step 1 `activate`(forward):激活 Revision、Run 进入 running、
//!   DispatchReady 进 durable run-action outbox——此后失败保留
//!   Run/Needs You,不回滚;
//! - step 2 `discard`(compensate 0):调度未开始的失败清理 Draft Task;
//!   已有 Agent Run 时拒绝删除(Needs You)。
//!
//! step 身份(step_id/semantic digest)全部从 initiating command_id 确定性
//! 派生或由 durable payload 重算,worker/重启恢复只凭 durable 行重建,
//! 不存在内存闭包。

use crate::command::{
    canonical_json, hex_digest, is_sensitive_key, CommandProblem, CommandType, EffectOutput,
    ProjectionEffect,
};
use crate::handles::{
    AggregateKind, AggregateRef, CommandId, CommandTarget, ExpectedRevision, ProjectStoreHandle,
    TargetStoreKind, WorkflowHandle, WorkflowRunHandle,
};
use crate::kernel::KernelProblem;
use crate::operation::{
    OperationKind, OperationPlan, SagaStepPlan, StepEffect, StepId, StepRecord, StepRole,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const PREPARED_WORKFLOW_START_SCHEMA: &str = "mf.workflow-start.prepared.v1";
pub const WORKFLOW_START_OPERATION_KIND: &str = "workflow_run.start";
/// durable payload 顶层 `schema` 字段值(`mf.operation.step.v1` 的 payload 侧)。
pub const WORKFLOW_START_PAYLOAD_SCHEMA: &str = "mf.workflow-start.plan.v1";

/// Run 标题取 goal 首个非空行并截断的上限(与 legacy
/// `crates/mf` Composer 的 `PROJECT_WORKFLOW_TITLE_MAX_CHARS` 同值)。
pub const WORKFLOW_START_TITLE_MAX_CHARS: usize = 80;

/// Run Start 事件类型(step 投影与 acceptance 事件共用 wire 名)。
pub const WORKFLOW_START_EVENT_TYPE: &str = "workflow_run.start";

const PHASE_MATERIALIZE: u8 = 0;
const PHASE_ACTIVATE: u8 = 1;
const PHASE_DISCARD: u8 = 2;

/// Orchestrator 编译得到、且重启后足以重新取得同一组 pin 并构造 Project
/// Store mutation 的冻结输入。Agent Instance、Agent Type plugin 与目录
/// provider identity 均包含在 `pipeline` 内;Secret 明文不得进入本 DTO
/// (`sealed_secret_ids` 一类 sealed ref 是唯一允许的凭据形态)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreparedWorkflowStartPlan {
    pub schema: String,
    pub workflow: WorkflowHandle,
    pub goal: String,
    pub pipeline: mf_agent::workflow::WorkflowSnapshot,
    pub content_digest: String,
    pub allow_unsafe_parallel: bool,
}

impl PreparedWorkflowStartPlan {
    pub fn new(
        workflow: WorkflowHandle,
        goal: impl Into<String>,
        pipeline: mf_agent::workflow::WorkflowSnapshot,
        content_digest: impl Into<String>,
        allow_unsafe_parallel: bool,
    ) -> Result<Self, KernelProblem> {
        let goal = goal.into();
        if goal.trim().is_empty() {
            return Err(KernelProblem::InvalidEnvelope(
                "Workflow Run goal 不能为空".into(),
            ));
        }
        let content_digest = content_digest.into();
        if content_digest.trim().is_empty() {
            return Err(KernelProblem::ValidationFailed(
                "prepared Workflow Start 缺少 content digest".into(),
            ));
        }
        Ok(Self {
            schema: PREPARED_WORKFLOW_START_SCHEMA.into(),
            workflow,
            goal: goal.trim().to_owned(),
            pipeline,
            content_digest,
            allow_unsafe_parallel,
        })
    }

    /// Run 标题:goal 首个非空行,截断到 [`WORKFLOW_START_TITLE_MAX_CHARS`]。
    pub fn run_title(&self) -> String {
        self.goal
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .chars()
            .take(WORKFLOW_START_TITLE_MAX_CHARS)
            .collect()
    }
}

/// Orchestrator-backed 编译端口的最小契约。
///
/// 同一 command id 可被多次调用;实现必须返回语义完全相同的 prepared
/// plan(同 content digest/pins)。Operation accept 先把该 plan 持久化为
/// durable frozen payload,后台 worker/重启 resume 只从 durable plan
/// 重建执行,绝不依赖内存 closure。
pub trait WorkflowStartPort: Send + Sync {
    fn prepare(
        &self,
        command_id: &CommandId,
        workflow: &WorkflowHandle,
        goal: &str,
    ) -> Result<PreparedWorkflowStartPlan, KernelProblem>;

    /// 调度开始前(activate step 未生效)的失败回滚钩子:释放本次 start
    /// 固定的插件/目录 provider pins 等外部资源。必须幂等;Core 在 saga
    /// 补偿完成后调用。默认 no-op(无外部资源的 port)。
    fn release_pre_start_resources(
        &self,
        _command_id: &CommandId,
        _plan: &PreparedWorkflowStartPlan,
    ) -> Result<(), KernelProblem> {
        Ok(())
    }
}

// ───────────────────────── durable payload ─────────────────────────

/// durable frozen payload 顶层 DTO(schema + plan 本体)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct DurableWorkflowStartPayload {
    schema: String,
    plan: PreparedWorkflowStartPlan,
}

/// 把 prepared plan 规范化为随 `saga_state` 落盘的 durable payload。
/// 「敏感键 + 字符串值」视为明文凭据,在此 fail-closed(sealed ref 容器
/// 如 `sealed_secret_ids` 是数组,不受影响)。
pub fn workflow_start_payload(plan: &PreparedWorkflowStartPlan) -> Result<Value, KernelProblem> {
    if plan.schema != PREPARED_WORKFLOW_START_SCHEMA {
        return Err(KernelProblem::ValidationFailed(
            "prepared Workflow Start schema 不匹配".into(),
        ));
    }
    if plan
        .pipeline
        .nodes
        .iter()
        .any(|node| instance_snapshot_has_plaintext_secret(&node.instance))
    {
        return Err(KernelProblem::ValidationFailed(
            "prepared Workflow Start 含明文凭据(普通 env/config/执行契约只允许非敏感值，凭据必须使用 sealed ref)".into(),
        ));
    }
    let payload = serde_json::json!({
        "schema": WORKFLOW_START_PAYLOAD_SCHEMA,
        "plan": plan,
    });
    if plaintext_secret_in_payload(&payload) {
        return Err(KernelProblem::ValidationFailed(
            "prepared Workflow Start 含明文凭据(只允许 sealed ref)".into(),
        ));
    }
    Ok(payload)
}

/// `AgentInstanceSnapshot.env` 的 wire 形状是 `[[key,value], ...]`，通用
/// JSON object 扫描无法把 tuple 第一项识别为键；必须在 typed DTO 上先
/// 检查。config / execution_contract 仍按对象键递归检查。sealed ref 不在
/// 这三处，`sealed_secret_ids` 因而保持允许。
fn instance_snapshot_has_plaintext_secret(instance: &mf_agent::AgentInstanceSnapshot) -> bool {
    instance
        .env
        .iter()
        .any(|(key, _value)| is_sensitive_key(key))
        || plaintext_secret_in_payload(&instance.config)
        || plaintext_secret_in_payload(&instance.execution_contract)
}

fn plaintext_secret_in_payload(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (is_sensitive_key(key) && value.is_string()) || plaintext_secret_in_payload(value)
        }),
        Value::Array(values) => values.iter().any(plaintext_secret_in_payload),
        _ => false,
    }
}

/// durable payload 的内容摘要:step semantic digest 只锚定该摘要,plan
/// 本体不进 digest 输入(与 #21「digest 覆盖语义、不重复落盘」同口径)。
pub fn workflow_start_plan_digest(payload: &Value) -> Result<String, KernelProblem> {
    canonical_json(payload)
        .map(|json| hex_digest(Sha256::digest(json.as_bytes())))
        .map_err(|error| KernelProblem::Internal(error.to_string()))
}

/// 解析 durable payload 回 prepared plan(schema 不匹配即 fail-closed)。
pub fn prepared_plan_of_payload(
    payload: &Value,
) -> Result<PreparedWorkflowStartPlan, KernelProblem> {
    let durable: DurableWorkflowStartPayload =
        serde_json::from_value(payload.clone()).map_err(|error| {
            KernelProblem::ValidationFailed(format!(
                "durable Workflow Start payload 解析失败:{error}"
            ))
        })?;
    if durable.schema != WORKFLOW_START_PAYLOAD_SCHEMA {
        return Err(KernelProblem::ValidationFailed(
            "durable Workflow Start payload schema 不匹配".into(),
        ));
    }
    if durable.plan.schema != PREPARED_WORKFLOW_START_SCHEMA {
        return Err(KernelProblem::ValidationFailed(
            "prepared Workflow Start schema 不匹配".into(),
        ));
    }
    Ok(durable.plan)
}

// ───────────────────────── plan 编译/重建 ─────────────────────────

/// 从 initiating command_id 确定性派生 step_id:同 command_id 重试/重启
/// 重新编译得到完全相同的 saga 身份(accept 幂等比对依赖这一点)。
fn derived_step_id(command_id: &CommandId, phase: u8) -> StepId {
    let base =
        uuid::Uuid::parse_str(command_id.as_str()).expect("CommandId 持有合法 UUIDv7 字符串");
    let mut bytes = *base.as_bytes();
    bytes[7] = phase;
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let derived = uuid::Uuid::from_bytes(bytes);
    StepId::parse(derived.to_string()).expect("派生 step id 保持 UUIDv7 版本位")
}

fn step_target(project: &ProjectStoreHandle, workflow: &WorkflowHandle) -> CommandTarget {
    CommandTarget {
        store: TargetStoreKind::Project,
        store_handle: project.as_str().to_owned(),
        aggregate: AggregateRef::new(AggregateKind::ProjectWorkflow, workflow.as_str().to_owned())
            .expect("typed Workflow handle 非空"),
    }
}

/// 编译 Workflow Start 的三步 saga。step payload 只含 phase 与 plan
/// digest;durable plan 本体经 [`OperationPlan::payload`] 随 saga_state 落盘。
pub fn compile_workflow_start_plan(
    command_id: &CommandId,
    project: &ProjectStoreHandle,
    workflow: &WorkflowHandle,
    semantic_revision: u64,
    payload: &Value,
) -> Result<OperationPlan, KernelProblem> {
    let plan_digest = workflow_start_plan_digest(payload)?;
    let kind = OperationKind::parse(WORKFLOW_START_OPERATION_KIND)
        .map_err(|error| KernelProblem::Internal(error.to_string()))?;
    let target = step_target(project, workflow);
    let step_payload = |phase: u8| {
        serde_json::json!({
            "phase": phase,
            "plan_digest": plan_digest,
        })
    };
    let materialize = SagaStepPlan::new(
        StepRole::Forward,
        derived_step_id(command_id, PHASE_MATERIALIZE),
        CommandType::WorkflowRun,
        target.clone(),
        vec![ExpectedRevision {
            aggregate: target.aggregate.clone(),
            revisions: [("semantic_revision".to_string(), semantic_revision)]
                .into_iter()
                .collect(),
        }],
        &step_payload(PHASE_MATERIALIZE),
        None,
        &kind,
    )
    .map_err(plan_problem)?;
    let activate = SagaStepPlan::new(
        StepRole::Forward,
        derived_step_id(command_id, PHASE_ACTIVATE),
        CommandType::WorkflowRun,
        target.clone(),
        Vec::new(),
        &step_payload(PHASE_ACTIVATE),
        None,
        &kind,
    )
    .map_err(plan_problem)?;
    // 补偿只回滚 materialize:activate 已生效(DispatchReady 已提交)时
    // saga 已 Complete,保留 Run/Needs You,不删除调度中的 Run。
    let discard = SagaStepPlan::new(
        StepRole::Compensate,
        derived_step_id(command_id, PHASE_DISCARD),
        CommandType::WorkflowRun,
        target,
        Vec::new(),
        &step_payload(PHASE_DISCARD),
        Some(0),
        &kind,
    )
    .map_err(plan_problem)?;
    let payload_json =
        canonical_json(payload).map_err(|error| KernelProblem::Internal(error.to_string()))?;
    let plan = OperationPlan {
        kind,
        steps: vec![materialize, activate, discard],
        payload: Some(payload_json),
    };
    plan.validate().map_err(plan_problem)?;
    Ok(plan)
}

/// 重启/worker 恢复:从 durable payload 与 durable step 行重建同一 saga。
/// step 身份(digest/target/expected)逐一与 accept 时冻结的行核对,
/// 任何偏差 fail-closed,绝不执行未冻结的语义。
pub fn rebuild_workflow_start_plan(
    command_id: &CommandId,
    project: &ProjectStoreHandle,
    payload: &Value,
    steps: &[StepRecord],
) -> Result<(OperationPlan, PreparedWorkflowStartPlan), KernelProblem> {
    let plan = prepared_plan_of_payload(payload)?;
    let semantic_revision = materialize_expected_revision(steps)?;
    let rebuilt = compile_workflow_start_plan(
        command_id,
        project,
        &plan.workflow,
        semantic_revision,
        payload,
    )?;
    if rebuilt.steps.len() != steps.len() {
        return Err(KernelProblem::CommandIdReused);
    }
    for (step, record) in rebuilt.steps.iter().zip(steps) {
        if step.step_id.as_str() != record.step_id.as_str()
            || step.role != record.role
            || step.target.store_key() != record.target_store
            || step.target.aggregate.handle != record.aggregate
            || step.semantic_digest() != record.semantic_digest
            || step.compensates != record.compensates
        {
            return Err(KernelProblem::CommandIdReused);
        }
    }
    Ok((rebuilt, plan))
}

/// durable materialize 行冻结的 workflow semantic revision(CAS 前提)。
/// `expected_json` 是 `canonical_expected_revisions` 的输出格式。
fn materialize_expected_revision(steps: &[StepRecord]) -> Result<u64, KernelProblem> {
    let materialize = steps
        .iter()
        .find(|step| step.role == StepRole::Forward && step.compensates.is_none())
        .ok_or_else(|| KernelProblem::ValidationFailed("durable steps 缺少 materialize".into()))?;
    let expected: Value = serde_json::from_str(&materialize.expected_json)
        .map_err(|error| KernelProblem::Internal(error.to_string()))?;
    let invalid = || KernelProblem::ValidationFailed("materialize expected 格式非法".into());
    let Value::Array(entries) = expected else {
        return Err(invalid());
    };
    for entry in entries {
        let revision = entry
            .pointer("/revisions/semantic_revision")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok());
        if let Some(revision) = revision {
            return Ok(revision);
        }
    }
    Err(invalid())
}

// ───────────────────────── step effects ─────────────────────────

/// 编译三个 step 的 effect 闭包。effect 只依赖 durable plan 与 durable
/// step 身份;跨 step 数据(Workflow Run handle)从事务内 target receipt
/// 读取,resume/重启后依然成立。
pub(crate) fn workflow_start_effects(
    plan: &PreparedWorkflowStartPlan,
    steps: &[StepRecord],
) -> Result<Vec<StepEffect>, KernelProblem> {
    let materialize_id = forward_step_id(steps, 0)?;
    Ok(vec![
        materialize_effect(plan.clone())?,
        activate_effect(materialize_id.clone())?,
        discard_effect(materialize_id)?,
    ])
}

fn forward_step_id(steps: &[StepRecord], forward_index: usize) -> Result<StepId, KernelProblem> {
    steps
        .iter()
        .filter(|step| step.role == StepRole::Forward)
        .nth(forward_index)
        .map(|step| step.step_id.clone())
        .ok_or_else(|| {
            KernelProblem::ValidationFailed(format!(
                "durable steps 缺少 forward step {forward_index}"
            ))
        })
}

/// materialize:Draft Task + 冻结 Revision(同一 target 事务)。
fn materialize_effect(plan: PreparedWorkflowStartPlan) -> Result<StepEffect, KernelProblem> {
    let title = plan.run_title();
    if title.is_empty() {
        return Err(KernelProblem::InvalidEnvelope(
            "Workflow Run goal 不能为空".into(),
        ));
    }
    Ok(Box::new(move |tx| {
        let task = mf_agent::store::Store::create_task_tx(tx, &title, &plan.goal)
            .map_err(domain_problem)?;
        mf_agent::store::Store::create_workflow_revision_tx(
            tx,
            task.id,
            &plan.pipeline,
            Some(&plan.content_digest),
        )
        .map_err(domain_problem)?;
        run_start_output_tx(tx, &task.public_handle, 0)
    }))
}

/// activate:激活 Revision、Run 进入 running、DispatchReady 进 outbox。
fn activate_effect(materialize_id: StepId) -> Result<StepEffect, KernelProblem> {
    Ok(Box::new(move |tx| {
        let (task_handle, base_revision) = workflow_run_of_receipt_tx(tx, materialize_id.as_str())?;
        let task_id = scalar_task_id_tx(tx, &task_handle)?;
        let result = mf_agent::store::Store::apply_run_mutation_tx(
            tx,
            mf_agent::RunMutation::Start { task_id },
        )
        .map_err(domain_problem)?;
        let task = match result.output {
            mf_agent::run_mutation::RunMutationOutput::Started(task) => task,
            _ => return Err(CommandProblem::Internal("start mutation 输出缺失".into())),
        };
        let mut output = run_start_output_tx(tx, &task.public_handle, base_revision)?;
        let primary = output
            .projections
            .first_mut()
            .ok_or_else(|| CommandProblem::Internal("start 投影缺失".into()))?;
        primary.run_actions = result.actions;
        Ok(output)
    }))
}

/// discard(补偿):调度未开始时删除 Draft Task;已有 Agent Run 则拒绝。
fn discard_effect(materialize_id: StepId) -> Result<StepEffect, KernelProblem> {
    Ok(Box::new(move |tx| {
        let (task_handle, _) = workflow_run_of_receipt_tx(tx, materialize_id.as_str())?;
        let task_id = scalar_task_id_tx(tx, &task_handle)?;
        let deleted = mf_agent::store::Store::delete_task_if_unused_tx(tx, task_id)
            .map_err(domain_problem)?;
        if !deleted {
            // 已有 Agent Run:调度已开始,保留 Run/Needs You,补偿失败终态化。
            return Err(CommandProblem::RevisionConflict);
        }
        Ok(EffectOutput {
            result_revisions: serde_json::json!({
                "workflow_run": task_handle.as_str(),
                "discarded": true,
            }),
            projections: Vec::new(),
        })
    }))
}

/// Workflow Run 的权威 replace 投影(step receipt 的 result_revisions 与
/// outbox 事件共用同一 snapshot 口径)。`base_revision` 必须与 journal 中
/// 该聚合的当前 head 衔接(activate 锚定 materialize 的结果)。
fn run_start_output_tx(
    tx: &rusqlite::Transaction<'_>,
    task_handle: &str,
    base_revision: u64,
) -> Result<EffectOutput, CommandProblem> {
    let aggregate = AggregateRef::new(AggregateKind::WorkflowRun, task_handle.to_owned())
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
    let (revision, data) = crate::kernel::run_aggregate_snapshot_tx(tx, &aggregate)?;
    Ok(EffectOutput {
        result_revisions: serde_json::json!({
            "workflow_run": task_handle,
            "revision": revision,
        }),
        projections: vec![ProjectionEffect {
            aggregate: Some(aggregate),
            event_type: Some(WORKFLOW_START_EVENT_TYPE.to_string()),
            projection_critical: true,
            payload: serde_json::json!({
                "base_revision": {"revision": base_revision},
                "aggregate_revision": {"revision": revision},
                "delta": {"mode": "replace", "data": data},
            }),
            run_actions: Vec::new(),
        }],
    })
}

/// 从 materialize step 的 target receipt 读取 Workflow Run handle 与
/// revision。effect 只会在该 step 已持有 receipt(或同事务刚写入)后执行。
fn workflow_run_of_receipt_tx(
    tx: &rusqlite::Transaction<'_>,
    step_id: &str,
) -> Result<(WorkflowRunHandle, u64), CommandProblem> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT result_revisions FROM command_receipt WHERE command_id=?1",
            [step_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
    let Some(raw) = raw else {
        return Err(CommandProblem::Internal(
            "materialize step 缺少 target receipt,activate/discard 编排违约".into(),
        ));
    };
    let value: Value =
        serde_json::from_str(&raw).map_err(|error| CommandProblem::Internal(error.to_string()))?;
    let handle = value
        .get("workflow_run")
        .and_then(Value::as_str)
        .ok_or_else(|| CommandProblem::Internal("materialize receipt 缺少 workflow_run".into()))?;
    let revision = value
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| CommandProblem::Internal("materialize receipt 缺少 revision".into()))?;
    let handle = WorkflowRunHandle::parse(handle)
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
    Ok((handle, revision))
}

fn scalar_task_id_tx(
    tx: &rusqlite::Transaction<'_>,
    handle: &WorkflowRunHandle,
) -> Result<i64, CommandProblem> {
    tx.query_row(
        "SELECT id FROM agent_tasks WHERE public_handle=?1",
        [handle.as_str()],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| CommandProblem::Internal(error.to_string()))?
    .ok_or(CommandProblem::ResourceNotFound)
}

fn domain_problem(error: anyhow::Error) -> CommandProblem {
    CommandProblem::ValidationFailed(format!("{error:#}"))
}

fn plan_problem(problem: crate::operation::OperationProblem) -> KernelProblem {
    problem.into()
}

/// 从 durable step 行提取最终 Workflow Run handle(Operation Snapshot
/// 最终结果字段;只有 forward step 的 receipt result 才可信)。
pub fn workflow_run_handle_of(steps: &[StepRecord]) -> Option<WorkflowRunHandle> {
    steps
        .iter()
        .filter(|step| step.role == StepRole::Forward)
        .find_map(|step| {
            step.result
                .get("workflow_run")
                .and_then(Value::as_str)
                .and_then(|raw| WorkflowRunHandle::parse(raw).ok())
        })
}
