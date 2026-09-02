//! mfctl capability-token RunControl 的 Kernel 入口(canonical spec §6.3)。
//!
//! 认证仍是一次性 `MF_RUN_TOKEN`(mfctl pipe 语义,与 Controller Lease
//! 无关);最终状态写复用 [`mf_agent::Store::apply_run_mutation_tx`] 与
//! durable RunAction outbox —— 与 Web Client settle 走同一条
//! `WorkflowRunCommand::Settle` 命令链,不存在第二套 Settlement。
//!
//! 令牌明文绝不进入 Snapshot、event、command receipt、日志或错误:
//! 路由只在 Project Store 查询参数里使用 token,所有对外文案只描述
//! 命中结果(无效/歧义/冲突),不回显 token 本身。

use crate::handles::{AgentRunHandle, CommandId, ProjectStoreHandle, WorkflowRunHandle};
use crate::kernel::{KernelProblem, LegacyKernelClient};
use mf_agent::model::{AgentState, Settlement};
use mf_agent::pipeline::PipelineDraft;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
#[cfg(test)]
use std::sync::Arc;

/// crate 内恢复/契约使用的已登记 Project 只读快照；token authority 不再
/// 依赖此列表做正常路由。
#[cfg(test)]
#[allow(dead_code)]
pub(crate) struct RunControlProject {
    pub(crate) project: ProjectStoreHandle,
    pub(crate) store: Arc<mf_agent::Store>,
    pub(crate) closing: bool,
}

/// capability-token 结算的对外结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSettleOutcome {
    /// 本次命令完成结算(或补齐了崩溃后遗留的 durable action 投递)。
    Applied { agent_run: AgentRunHandle },
    /// 相同结算此前已提交;本次未改变权威状态。
    AlreadyApplied { agent_run: AgentRunHandle },
}

/// capability-token RunControl 的封闭命令族。认证材料独立传入，永不
/// 成为 Debug/receipt/digest 的一部分。
#[derive(Debug, Clone, PartialEq)]
pub enum RunControlCommand {
    Settle(Settlement),
    ReportState(AgentState),
    ProposePipeline(PipelineDraft),
}

impl RunControlCommand {
    pub(crate) const fn method(&self) -> &'static str {
        match self {
            Self::Settle(Settlement::Complete { .. }) => "step.complete",
            Self::Settle(Settlement::Fail { .. }) => "step.fail",
            Self::ReportState(_) => "agent.state",
            Self::ProposePipeline(_) => "pipeline.propose",
        }
    }
}

/// RunControl 回执只含 opaque handle/公开状态；不得暴露 rowid 或 token。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunControlOutcome {
    Settled(TokenSettleOutcome),
    StateReported {
        agent_run: AgentRunHandle,
        state: AgentState,
    },
    PipelineProposed {
        workflow_run: WorkflowRunHandle,
        revision: String,
    },
}

/// capability-token 结算的失败分类。文案兼容既有 mfctl pipe 契约
/// (「能力令牌无效」「冲突」),且不包含 token 明文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSettleProblem {
    MissingToken,
    UnknownToken,
    /// 防御:token 本应全局唯一,命中多个 Project 时拒绝「第一个命中」。
    AmbiguousToken {
        matches: usize,
    },
    /// 目标 Project 正在两阶段关闭中,fail-closed。
    ProjectClosing,
    /// 已有相反结算,拒绝本次。
    Conflict {
        existing: String,
        attempted: String,
    },
    /// run 已离开可结算状态(cancelled 等),不是结算冲突。
    RunNotActive(String),
    /// command_id 不是合法 UUIDv7。
    InvalidCommandId,
    /// Settlement 试图把当前认证材料持久化；transport 之外的最终防线。
    SensitiveSettlement,
    /// Kernel 命令链错误(dispatch/投影/port 投递)。
    Kernel(KernelProblem),
}

impl fmt::Display for TokenSettleProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingToken => write!(f, "缺少能力令牌(环境变量 MF_RUN_TOKEN)"),
            Self::UnknownToken => write!(f, "能力令牌无效"),
            Self::AmbiguousToken { matches } => {
                write!(f, "能力令牌命中多个项目({matches}),拒绝结算")
            }
            Self::ProjectClosing => write!(f, "项目正在关闭,结算不可用"),
            Self::Conflict {
                existing,
                attempted,
            } => write!(f, "冲突结算:已有 `{existing}`,拒绝 `{attempted}`"),
            Self::RunNotActive(status) => {
                write!(f, "Agent Run 已不在活动状态({status})")
            }
            Self::InvalidCommandId => write!(f, "command_id 必须是 UUIDv7"),
            Self::SensitiveSettlement => write!(f, "结算内容包含认证材料,已拒绝持久化"),
            Self::Kernel(problem) => write!(f, "{problem}"),
        }
    }
}

impl std::error::Error for TokenSettleProblem {}

impl From<KernelProblem> for TokenSettleProblem {
    fn from(problem: KernelProblem) -> Self {
        Self::Kernel(problem)
    }
}

/// mfctl pipe → Core 的 Settlement 路由缝隙。
///
/// 生产实现是 [`LegacyKernelClient`];pipe adapter 惰性解析 runtime,
/// Core 未装配或装配失败必须 fail-closed,不得回退旧直写路径。
pub trait RunControlSettlement: Send + Sync {
    fn settle_agent_run_by_token(
        &self,
        token: &str,
        settlement: Settlement,
        command_id: Option<CommandId>,
    ) -> Result<TokenSettleOutcome, TokenSettleProblem>;
}

/// mfctl pipe → Core 的统一 RunControl seam。旧 Settlement trait 仅为
/// 兼容既有调用；新 transport 一律提交封闭 [`RunControlCommand`]。
pub trait RunControl: Send + Sync {
    fn execute_agent_run_by_token(
        &self,
        token: &str,
        command: RunControlCommand,
        command_id: Option<CommandId>,
    ) -> Result<RunControlOutcome, TokenSettleProblem>;
}

impl LegacyKernelClient {
    /// 以一次性 capability token 定位唯一 Agent Run 并经 Kernel dispatch
    /// 提交显式结算。多项目扫描必须恰好命中一个;命中零个或多个都拒绝。
    pub fn settle_agent_run_by_token(
        &self,
        token: &str,
        settlement: Settlement,
        command_id: Option<CommandId>,
    ) -> Result<TokenSettleOutcome, TokenSettleProblem> {
        self.core_kernel().settle_run_control_capability(
            token,
            settlement,
            command_id.unwrap_or_else(CommandId::new),
        )
    }

    pub fn execute_agent_run_by_token(
        &self,
        token: &str,
        command: RunControlCommand,
        command_id: Option<CommandId>,
    ) -> Result<RunControlOutcome, TokenSettleProblem> {
        self.core_kernel().execute_run_control_capability(
            token,
            command,
            command_id.unwrap_or_else(CommandId::new),
        )
    }
}

/// RunControl 的显式 command-id 语义域。只包含方法、opaque 归属句柄与
/// Settlement payload；token/HMAC、Controller、expected/revision 均不入
/// digest，使 target receipt 能在重新认证后稳定重放原结果。
pub(crate) fn semantic_digest(
    project: &ProjectStoreHandle,
    workflow_run: &WorkflowRunHandle,
    agent_run: &AgentRunHandle,
    command: &RunControlCommand,
) -> Result<String, KernelProblem> {
    let mut root = BTreeMap::<String, Value>::new();
    root.insert("schema".into(), Value::String("mf.run-control.v1".into()));
    root.insert("method".into(), Value::String(command.method().into()));
    root.insert("project".into(), Value::String(project.as_str().into()));
    root.insert(
        "workflow_run".into(),
        Value::String(workflow_run.as_str().into()),
    );
    root.insert("agent_run".into(), Value::String(agent_run.as_str().into()));
    let payload = match command {
        RunControlCommand::Settle(settlement) => serde_json::to_value(settlement),
        RunControlCommand::ReportState(state) => serde_json::to_value(state),
        RunControlCommand::ProposePipeline(draft) => serde_json::to_value(draft),
    }
    .map_err(|error| KernelProblem::Internal(error.to_string()))?;
    root.insert("payload".into(), sorted_json(payload));
    let bytes =
        serde_json::to_vec(&root).map_err(|error| KernelProblem::Internal(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub(crate) fn settlement_contains_token(settlement: &Settlement, token: &str) -> bool {
    fn value_contains(value: &Value, token: &str) -> bool {
        match value {
            Value::String(text) => text.contains(token),
            Value::Array(items) => items.iter().any(|item| value_contains(item, token)),
            Value::Object(fields) => fields
                .iter()
                .any(|(key, value)| key.contains(token) || value_contains(value, token)),
            _ => false,
        }
    }
    match settlement {
        Settlement::Complete { summary, output } => {
            summary.contains(token) || value_contains(output, token)
        }
        Settlement::Fail { reason } => reason.contains(token),
    }
}

pub(crate) fn command_contains_token(command: &RunControlCommand, token: &str) -> bool {
    match command {
        RunControlCommand::Settle(settlement) => settlement_contains_token(settlement, token),
        RunControlCommand::ReportState(_) => false,
        RunControlCommand::ProposePipeline(draft) => serde_json::to_value(draft)
            .map(|value| value_contains_text(&value, token))
            .unwrap_or(true),
    }
}

fn value_contains_text(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text.contains(needle),
        Value::Array(values) => values
            .iter()
            .any(|value| value_contains_text(value, needle)),
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| key.contains(needle) || value_contains_text(value, needle)),
        _ => false,
    }
}

/// 生成稳定、最小差异的 Planner draft；profile availability 在用户确认/
/// 调度时仍会按当前插件目录复验，此处只允许结构合法的提案入库。
pub(crate) fn normalize_pipeline_draft(
    mut draft: PipelineDraft,
) -> Result<PipelineDraft, KernelProblem> {
    if draft.steps.is_empty() {
        return Err(KernelProblem::ValidationFailed(
            "Planner 草案至少包含一个 Step".into(),
        ));
    }
    for step in &mut draft.steps {
        step.key = step.key.trim().to_owned();
        step.title = step.title.trim().to_owned();
        step.agent_profile = step.agent_profile.trim().to_owned();
        step.deps = step
            .deps
            .iter()
            .map(|dependency| dependency.trim().to_owned())
            .collect();
        step.deps.sort();
        step.deps.dedup();
        if let mf_agent::SessionPolicy::Reuse { key } = &mut step.session_policy {
            *key = key.trim().to_owned();
        }
    }
    let mut profiles = mf_agent::ProfileIndex::default();
    for profile in draft.steps.iter().map(|step| step.agent_profile.clone()) {
        profiles.entries.insert(
            profile,
            mf_agent::pipeline::ProfileAvailability {
                installed: true,
                enabled: true,
                detected: true,
            },
        );
    }
    let errors = draft.validate(&profiles);
    if !errors.is_empty() {
        return Err(KernelProblem::ValidationFailed(errors.join("\n")));
    }
    Ok(draft)
}

fn sorted_json(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, sorted_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(sorted_json).collect()),
        scalar => scalar,
    }
}

impl RunControlSettlement for LegacyKernelClient {
    fn settle_agent_run_by_token(
        &self,
        token: &str,
        settlement: Settlement,
        command_id: Option<CommandId>,
    ) -> Result<TokenSettleOutcome, TokenSettleProblem> {
        LegacyKernelClient::settle_agent_run_by_token(self, token, settlement, command_id)
    }
}

impl RunControl for LegacyKernelClient {
    fn execute_agent_run_by_token(
        &self,
        token: &str,
        command: RunControlCommand,
        command_id: Option<CommandId>,
    ) -> Result<RunControlOutcome, TokenSettleProblem> {
        LegacyKernelClient::execute_agent_run_by_token(self, token, command, command_id)
    }
}
