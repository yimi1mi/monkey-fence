//! Workflow Run 生命周期的 transaction-scoped mutation seam。
//!
//! 这里只描述并执行 Project Store 内的权威状态迁移。运行时停止、输入注入、
//! 目录合并/租约释放与事件发布必须在外层事务提交后按 [`RunAction`] 执行。

use crate::model::{
    AgentState, RetryMode, RevisionView, RunView, Settlement, StepQuestionView, StepView, TaskView,
};
use crate::pipeline::PipelineDraft;

/// Store 中持久化的「下一次 attempt」会话选择。
///
/// 它不是 Step 的长期 session policy；只允许创建下一条 Agent Run 的事务
/// 以 CAS 方式消费一次。这样 retry 已提交而进程崩溃时，恢复调度仍看到同一
/// 选择；创建 run 后再重放 retry 的 post-commit 投递也不会复活旧选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NextAttemptSession {
    pub mode: RetryMode,
    pub session_id: Option<i64>,
}

impl NextAttemptSession {
    pub fn fresh() -> Self {
        Self {
            mode: RetryMode::FreshSession,
            session_id: None,
        }
    }

    pub fn continue_session(session_id: i64) -> Self {
        Self {
            mode: RetryMode::ContinueSession,
            session_id: Some(session_id),
        }
    }
}

#[derive(Clone)]
pub enum RunMutation {
    Start {
        task_id: i64,
    },
    /// `run_stops` 必须逐一来自事务外 RuntimeHost 的真实终止结果；
    /// Store 会在事务内复验它与当前全部 active Agent Run 完全一致。
    Cancel {
        task_id: i64,
        run_stops: Vec<RunStopResult>,
    },
    /// ContinueSession 时必须带外层已确认存活的 session id。
    Retry {
        step_id: i64,
        mode: RetryMode,
        continue_session_id: Option<i64>,
    },
    /// 用户显式跳过失败/阻塞 Step。只允许没有 active Agent Run 的节点；
    /// Runtime 仍活动时必须先走 Cancel/Settlement，不能先改领域终态。
    Skip {
        step_id: i64,
    },
    Respond {
        question_id: i64,
        answer: String,
    },
    Settle {
        run_id: i64,
        settlement: Settlement,
    },
    /// Agent 自报状态。done/dead 只进入 awaiting-outcome，绝不隐式
    /// Settlement；Run/Session/Step/Task 必须在同一 Project transaction
    /// 内收敛，避免 NeedsYou 与运行状态撕裂。
    ReportState {
        run_id: i64,
        state: AgentState,
    },
    /// Planner 只创建未激活的 draft Revision；用户确认仍是独立的
    /// Controller 命令，capability 不能借此启动工作流。
    ProposePipeline {
        task_id: i64,
        draft: PipelineDraft,
    },
}

/// 回答明文不得进入 Debug 输出(日志/错误链会携带它);
/// `Respond` 一律以 `<redacted>` 占位。
impl std::fmt::Debug for RunMutation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start { task_id } => f.debug_struct("Start").field("task_id", task_id).finish(),
            Self::Cancel { task_id, run_stops } => f
                .debug_struct("Cancel")
                .field("task_id", task_id)
                .field("run_stops", run_stops)
                .finish(),
            Self::Retry {
                step_id,
                mode,
                continue_session_id,
            } => f
                .debug_struct("Retry")
                .field("step_id", step_id)
                .field("mode", mode)
                .field("continue_session_id", continue_session_id)
                .finish(),
            Self::Skip { step_id } => f.debug_struct("Skip").field("step_id", step_id).finish(),
            Self::Respond { question_id, .. } => f
                .debug_struct("Respond")
                .field("question_id", question_id)
                .field("answer", &"<redacted>")
                .finish(),
            Self::Settle { run_id, settlement } => f
                .debug_struct("Settle")
                .field("run_id", run_id)
                .field("settlement", settlement)
                .finish(),
            Self::ReportState { run_id, state } => f
                .debug_struct("ReportState")
                .field("run_id", run_id)
                .field("state", state)
                .finish(),
            Self::ProposePipeline { task_id, draft } => f
                .debug_struct("ProposePipeline")
                .field("task_id", task_id)
                .field("step_count", &draft.steps.len())
                .finish(),
        }
    }
}

/// RuntimeHost 对一个 active Agent Run 的真实停止结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunStopResult {
    pub run_id: i64,
    pub outcome: RunStopOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStopOutcome {
    Confirmed,
    Unconfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CancelFenceTarget {
    pub run_id: i64,
    pub run_handle: String,
    pub run_revision: i64,
    pub state: CancelFenceTargetState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelFenceRecord {
    pub command_id: String,
    pub task_id: i64,
    pub expected_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelFenceTargetState {
    Pending,
    Stopping,
    Confirmed,
    Unconfirmed,
}

impl CancelFenceTargetState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Stopping => "stopping",
            Self::Confirmed => "confirmed",
            Self::Unconfirmed => "unconfirmed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunAction {
    /// 提交后由调度 tick 派发 ready step。
    DispatchReady {
        task_id: i64,
    },
    /// 必须先在事务外确认进程终止；本动作仅用于已确认/无进程的收口路径。
    StopRuntime {
        run_handle: String,
    },
    /// question-bound 回答的持久投递动作。不携带回答明文:明文只存在于
    /// 项目库私有表 `question_answer_deliveries`(answer 列,delivered 后
    /// 置 NULL),本 action 会被序列化进 kernel 投影 outbox 的事件 JSON,
    /// 因此只以 `(question_id, run_id, run_handle, nonce)` 寻址。执行端
    /// 必须复验 nonce 与 run 绑定,并以 pending 行为幂等键;不能仅按 run
    /// 寻址,否则崩溃重放可能把旧答案注入下一题。
    AnswerRuntime {
        question_id: i64,
        run_id: i64,
        run_handle: String,
        /// 绑定 question identity + run identity + accept 时刻 run revision
        /// 的投递一次性凭证;与私有表中的 nonce 严格相等才允许投递。
        nonce: String,
    },
    ReleaseRunResources {
        run_id: i64,
    },
    /// 仅释放并发槽；重试会复用既有执行位置租约，不能提前释放目录。
    ReleaseRunSlot {
        run_id: i64,
    },
    /// 释放 Workflow Run 级别的租约、待决合并、插件 pin 与目录基线。
    ReleaseTaskResources {
        task_id: i64,
    },
    AfterSettlement {
        run_id: i64,
        settlement: Settlement,
    },
    /// 失败终态可能补齐 join 父批；状态决策已经在 Store 事务内完成，
    /// 本动作只触发具备 merge_batches CAS 的外部汇合冲刷。
    FlushCompletedJoinBatches {
        task_id: i64,
    },
    /// Skip 提交后的 durable 收口：冲刷 join、复验收敛、按交付 key
    /// 幂等释放任务资源。不得由 UI/transport 直接调用。
    AfterSkip {
        task_id: i64,
    },
}

#[derive(Debug, Clone)]
pub struct RunMutationResult {
    pub output: RunMutationOutput,
    pub actions: Vec<RunAction>,
}

#[derive(Debug, Clone)]
pub enum RunMutationOutput {
    Started(TaskView),
    Cancelled(TaskView),
    CancelNeedsYou(TaskView),
    Retried(StepView),
    Skipped {
        task: TaskView,
        step: StepView,
    },
    Responded(StepQuestionView),
    Settled {
        run: RunView,
        already_applied: bool,
    },
    StateReported {
        run: RunView,
        task: TaskView,
        step: StepView,
        session_changed: bool,
    },
    PipelineProposed {
        task: TaskView,
        revision: RevisionView,
    },
}
