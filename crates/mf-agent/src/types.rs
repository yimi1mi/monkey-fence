use serde::{Deserialize, Serialize};

/// 任务状态机:pending → ready → dispatched → completed | failed | blocked
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Ready,
    Dispatched,
    Completed,
    Failed,
    Blocked,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Ready => "ready",
            TaskStatus::Dispatched => "dispatched",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Blocked => "blocked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => TaskStatus::Pending,
            "ready" => TaskStatus::Ready,
            "dispatched" => TaskStatus::Dispatched,
            "completed" => TaskStatus::Completed,
            "failed" => TaskStatus::Failed,
            "blocked" => TaskStatus::Blocked,
            _ => return None,
        })
    }

    pub fn label_cn(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "等待依赖",
            TaskStatus::Ready => "就绪",
            TaskStatus::Dispatched => "执行中",
            TaskStatus::Completed => "完成",
            TaskStatus::Failed => "失败",
            TaskStatus::Blocked => "熔断",
        }
    }
}

/// 任务视图(UI 展示用快照)
#[derive(Clone, Debug, Serialize)]
pub struct TaskView {
    pub id: i64,
    pub run_id: i64,
    pub parent_id: Option<i64>,
    pub spec: String,
    pub status: TaskStatus,
    pub deps: Vec<i64>,
    pub result: Option<String>,
    pub failure_count: i32,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunView {
    pub id: i64,
    pub objective: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct QuestionView {
    pub id: i64,
    pub run_id: i64,
    pub task_id: Option<i64>,
    pub question: String,
    pub answer: Option<String>,
}

/// 引擎 → UI 事件(跨线程,crossbeam 通道)
#[derive(Clone, Debug)]
pub enum EngineEvent {
    RunStarted(RunView),
    TaskCreated(TaskView),
    TaskStatus(TaskView),
    /// worker 每轮工具调用/文本输出
    WorkerLog {
        task_id: i64,
        worker: String,
        text: String,
    },
    WorkerTool {
        task_id: i64,
        worker: String,
        tool: String,
        summary: String,
    },
    QuestionOpened(QuestionView),
    QuestionAnswered(QuestionView),
    RunFinished(i64, String),
    EngineError(String),
}
