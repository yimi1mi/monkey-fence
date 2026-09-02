//! Workflow Run lifecycle 的外部执行边界。
//!
//! Project Store mutation 与 durable action envelope 在 L-CMD 事务内一起
//! 落入 projection outbox。全部 action 成功后 kernel 才会移除私有字段并
//! 允许 L-PUBLISH。Port 只负责事务前真实运行时确认，以及事务
//! 后 action 的至少一次投递；它不能绕过 Core 直接写 Store。

use crate::handles::{AgentRunHandle, AgentSessionHandle, CommandId};
use crate::kernel::{KernelProblem, WorkflowRunCommand};

pub const DURABLE_RUN_ACTIONS_SCHEMA: &str = "mf.run-actions.v2";

pub(crate) fn event_has_pending_run_actions(event_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(event_json)
        .ok()
        .and_then(|value| value.get("run_actions").cloned())
        .is_some()
}

/// prepare 的结果是 kernel 编译 RunMutation 时唯一可信的运行时事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunPreparation {
    /// 不需运行时前置条件。
    Ready,
    /// cancel 涉及的每个 active Agent Run 的真实停止结果。
    Cancel { run_stops: Vec<PreparedRunStop> },
    /// ContinueSession 已确认该会话存活。
    ContinueSessionAlive { session: AgentSessionHandle },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRunStop {
    pub agent_run: AgentRunHandle,
    pub outcome: mf_agent::RunStopOutcome,
}

/// command receipt 中持久化的一个 post-commit 投递。
#[derive(Clone, PartialEq)]
pub struct RunActionDelivery {
    pub outbox_id: i64,
    pub command_id: CommandId,
    pub action_index: u32,
    pub action: mf_agent::RunAction,
}

impl RunActionDelivery {
    /// 跨 Kernel → production adapter → Orchestrator/provider 保持不变的
    /// 稳定交付 key。不得用 command_id、随机数或进程内序号替代。
    pub fn delivery_key(&self) -> mf_agent::execution_directory::RunActionDeliveryKey {
        mf_agent::execution_directory::RunActionDeliveryKey::new(self.outbox_id, self.action_index)
    }
}

impl std::fmt::Debug for RunActionDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // RunAction 可能包含 question nonce。Debug 只暴露 action kind，
        // 避免错误链/日志把 capability-like secret 带出生产边界。
        let action = match &self.action {
            mf_agent::RunAction::DispatchReady { .. } => "DispatchReady",
            mf_agent::RunAction::StopRuntime { .. } => "StopRuntime",
            mf_agent::RunAction::AnswerRuntime { .. } => "AnswerRuntime",
            mf_agent::RunAction::ReleaseRunResources { .. } => "ReleaseRunResources",
            mf_agent::RunAction::ReleaseRunSlot { .. } => "ReleaseRunSlot",
            mf_agent::RunAction::ReleaseTaskResources { .. } => "ReleaseTaskResources",
            mf_agent::RunAction::AfterSettlement { .. } => "AfterSettlement",
            mf_agent::RunAction::FlushCompletedJoinBatches { .. } => "FlushCompletedJoinBatches",
            mf_agent::RunAction::AfterSkip { .. } => "AfterSkip",
        };
        f.debug_struct("RunActionDelivery")
            .field("outbox_id", &self.outbox_id)
            .field("command_id", &self.command_id)
            .field("action_index", &self.action_index)
            .field("action", &action)
            .finish()
    }
}

/// 按 Project 注册的 Run lifecycle port。
///
/// `execute_post_commit` 必须以 `(outbox_id, action_index)` 作为幂等键。
/// Kernel 在网络重试、Core 崩溃恢复或 port 重注册时会重复投递；
/// 不具备该保证的实现不得注册。
pub trait RunLifecyclePort: Send + Sync {
    /// 只有实现了「具体 question 身份校验 + 跨进程持久幂等」的 port 才能
    /// 开启 Respond。默认 fail-closed；仅按 run handle 注入文本不安全。
    fn supports_question_bound_answers(&self) -> bool {
        false
    }

    /// 必须幂等：同一 command id 可因超时重试多次 prepare。
    fn prepare(
        &self,
        command_id: &CommandId,
        command: &WorkflowRunCommand,
    ) -> Result<RunPreparation, KernelProblem>;

    /// durable Cancel fence 已提交后，逐 target 幂等停止 Runtime。
    /// 实现无法证明该 handle 仍绑定当前 Runtime incarnation 时必须返回
    /// `Unconfirmed`，绝不能把「本进程中不存在」解释成已停止。
    fn stop_cancel_target(
        &self,
        _command_id: &CommandId,
        _agent_run: &AgentRunHandle,
    ) -> Result<mf_agent::RunStopOutcome, KernelProblem> {
        Err(KernelProblem::ServiceUnavailable(
            "durable_cancel_port_not_registered".into(),
        ))
    }

    /// 领域事务已提交。返回错误不能回滚 Store，Kernel 会保留
    /// durable receipt 并在后续同 id 重试/重启注册时再投递。
    fn execute_post_commit(&self, delivery: &RunActionDelivery) -> Result<(), KernelProblem>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_key_is_stable_and_distinguishes_action_index() {
        let command_id = CommandId::new();
        let first = RunActionDelivery {
            outbox_id: 41,
            command_id: command_id.clone(),
            action_index: 2,
            action: mf_agent::RunAction::DispatchReady { task_id: 7 },
        };
        let replay = first.clone();
        let mut different = first.clone();
        different.action_index = 3;

        assert_eq!(first.delivery_key(), replay.delivery_key());
        assert_ne!(first.delivery_key(), different.delivery_key());
    }

    #[test]
    fn delivery_debug_redacts_question_nonce() {
        let sentinel = "mft-question-nonce-must-not-leak";
        let delivery = RunActionDelivery {
            outbox_id: 9,
            command_id: CommandId::new(),
            action_index: 0,
            action: mf_agent::RunAction::AnswerRuntime {
                question_id: 1,
                run_id: 2,
                run_handle: "run-public".into(),
                nonce: sentinel.into(),
            },
        };
        let debug = format!("{delivery:?}");
        assert!(debug.contains("AnswerRuntime"));
        assert!(!debug.contains(sentinel));
    }
}
