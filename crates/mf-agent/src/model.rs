//! v2 领域视图类型:Task / Pipeline Revision / Step / Agent Session / Agent Run / Question。
//! 状态定义见 `CONTEXT.md` 与 ADR 0001。

use serde::{Deserialize, Serialize};

macro_rules! str_enum {
    ($name:ident { $($variant:ident => $s:expr),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub fn as_str(&self) -> &'static str { match self { $($name::$variant => $s),+ } }
            pub fn parse(s: &str) -> Option<Self> { match s { $($s => Some($name::$variant),)+ _ => None } }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

str_enum!(TaskStatus {
    Draft => "draft",
    Ready => "ready",
    Running => "running",
    NeedsYou => "needs-you",
    Succeeded => "succeeded",
    Failed => "failed",
    Cancelled => "cancelled",
    Archived => "archived",
});

impl TaskStatus {
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
    pub fn label_cn(&self) -> &'static str {
        match self {
            TaskStatus::Draft => "草稿",
            TaskStatus::Ready => "就绪",
            TaskStatus::Running => "运行中",
            TaskStatus::NeedsYou => "需要你",
            TaskStatus::Succeeded => "已成功",
            TaskStatus::Failed => "已失败",
            TaskStatus::Cancelled => "已取消",
            TaskStatus::Archived => "已归档",
        }
    }
}

str_enum!(StepStatus {
    Pending => "pending",
    Ready => "ready",
    Running => "running",
    AwaitingOutcome => "awaiting-outcome",
    NeedsInput => "needs-input",
    Succeeded => "succeeded",
    Failed => "failed",
    Blocked => "blocked",
    Skipped => "skipped",
    Cancelled => "cancelled",
});

impl StepStatus {
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            StepStatus::Succeeded
                | StepStatus::Failed
                | StepStatus::Skipped
                | StepStatus::Cancelled
        )
    }
    pub fn started(&self) -> bool {
        !matches!(
            self,
            StepStatus::Pending | StepStatus::Ready | StepStatus::Blocked
        )
    }
    pub fn label_cn(&self) -> &'static str {
        match self {
            StepStatus::Pending => "等待依赖",
            StepStatus::Ready => "就绪",
            StepStatus::Running => "执行中",
            StepStatus::AwaitingOutcome => "待结算",
            StepStatus::NeedsInput => "等待输入",
            StepStatus::Succeeded => "成功",
            StepStatus::Failed => "失败",
            StepStatus::Blocked => "被阻塞",
            StepStatus::Skipped => "已跳过",
            StepStatus::Cancelled => "已取消",
        }
    }
}

str_enum!(RunStatus {
    Running => "running",
    Succeeded => "succeeded",
    Failed => "failed",
    AwaitingOutcome => "awaiting-outcome",
    Interrupted => "interrupted",
    Cancelled => "cancelled",
});

str_enum!(AgentState {
    Starting => "starting",
    Working => "working",
    Waiting => "waiting",
    BlockedState => "blocked",
    Done => "done",
    Idle => "idle",
    Dead => "dead",
});

str_enum!(SessionStatus {
    Starting => "starting",
    Working => "working",
    Waiting => "waiting",
    BlockedState => "blocked",
    Done => "done",
    Idle => "idle",
    Dead => "dead",
    Hidden => "hidden",
});

str_enum!(RevisionStatus {
    Draft => "draft",
    Active => "active",
    Superseded => "superseded",
    Cancelled => "cancelled",
});

// Agent Instance 作用域:用户级全局,或绑定单个项目。
str_enum!(InstanceScope {
    User => "user",
    Project => "project",
});

// Agent Instance 默认运行模式(设计 §2:支持一次性和交互式 CLI)。
str_enum!(RunMode {
    OneShot => "oneshot",
    Interactive => "interactive",
});

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub id: i64,
    pub title: String,
    pub goal: String,
    pub status: TaskStatus,
    pub paused: bool,
    pub unread: bool,
    pub active_revision: Option<i64>,
    pub revision_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionView {
    pub id: i64,
    pub task_id: i64,
    pub revision: i64,
    pub status: RevisionStatus,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepView {
    pub id: i64,
    pub revision_id: i64,
    pub task_id: i64,
    pub step_key: String,
    pub title: String,
    pub instructions: String,
    pub agent_profile: String,
    pub session_policy: String,
    pub status: StepStatus,
    pub attempts: i32,
    pub result: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub deps: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub id: i64,
    pub session_key: Option<String>,
    pub runtime: String,
    pub agent_profile: String,
    pub title: String,
    pub status: SessionStatus,
    pub last_instruction: Option<String>,
    pub last_reply: Option<String>,
    pub unread: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunView {
    pub id: i64,
    pub task_id: i64,
    pub step_id: i64,
    pub revision_id: i64,
    pub session_id: Option<i64>,
    pub status: RunStatus,
    pub agent_state: AgentState,
    pub capability_token: String,
    pub outcome: Option<String>,
    pub outcome_payload: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepQuestionView {
    pub id: i64,
    pub task_id: i64,
    pub step_id: Option<i64>,
    pub run_id: Option<i64>,
    pub question: String,
    pub answer: Option<String>,
    pub status: String,
    pub created_at: String,
}

/// Agent Run 的显式结算:唯一成功依据(见 ADR 0001 与 `mfctl`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Settlement {
    Complete { summary: String },
    Fail { reason: String },
}

impl Settlement {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Settlement::Complete { .. } => "complete",
            Settlement::Fail { .. } => "fail",
        }
    }
    pub fn payload(&self) -> &str {
        match self {
            Settlement::Complete { summary } => summary,
            Settlement::Fail { reason } => reason,
        }
    }
    pub fn result_status(&self) -> RunStatus {
        match self {
            Settlement::Complete { .. } => RunStatus::Succeeded,
            Settlement::Fail { .. } => RunStatus::Failed,
        }
    }
    pub fn step_status(&self) -> StepStatus {
        match self {
            Settlement::Complete { .. } => StepStatus::Succeeded,
            Settlement::Fail { .. } => StepStatus::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleOutcome {
    Applied,
    /// 相同结算重复提交:幂等成功。
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettleError {
    #[error("能力令牌无效")]
    UnknownToken,
    #[error("Agent Run 已不在活动状态({0})")]
    RunNotActive(RunStatus),
    #[error("冲突结算:已有 `{existing}`,拒绝 `{attempted}`")]
    Conflict { existing: String, attempted: String },
    #[error("数据库错误: {0}")]
    Db(String),
}

/// Orchestrator → UI 事件。
#[derive(Debug, Clone)]
pub enum SchedulerEvent {
    TaskUpdated(TaskView),
    TaskRemoved(i64),
    RevisionCreated(RevisionView),
    StepUpdated(StepView),
    RunUpdated(RunView),
    SessionUpdated(SessionView),
    QuestionOpened(StepQuestionView),
    QuestionAnswered(StepQuestionView),
    /// 运行日志/终端输出摘要(推移给详情视图)。
    Log {
        run_id: i64,
        text: String,
    },
    /// API transcript 消息(session 维度)。
    Transcript {
        session_id: i64,
        role: String,
        text: String,
    },
    Error(String),
}
