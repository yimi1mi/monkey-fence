//! Shutdown assessment(canonical spec §11.4 的只读子集)。
//!
//! T2a(Issue #23)只做 facade 所需的最小只读评估:列出会阻止安全退出的
//! 活动工作(活动 Workflow Run、未发布 outbox 事件、未终结 command
//! intent)。freeze→drain→stores_closed 状态机与强制退出由 standalone
//! Core/safe shutdown ticket 接管;本模块不关闭任何 Store、不改任何状态。

/// 退出意图。T2a 只有只读评估;带确认的执行属 standalone Core ticket。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownIntent {
    /// 评估当前是否可以安全退出(不改变任何状态)。
    Assess,
}

/// 安全退出评估结果(全部字段只读导出,供 UI/调用方展示)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShutdownAssessment {
    /// 是否没有阻止退出的活动工作。
    pub safe_to_proceed: bool,
    /// 各 Project Store 中活动(run 级)Workflow Run 总数。
    pub active_workflow_runs: usize,
    /// 已提交但尚未发布到事件流的 outbox 事件数。
    pub pending_outbox_events: usize,
    /// service-v1 中仍 `reserved`(未终结)的 command intent 数。
    pub unfinished_intents: usize,
    /// 阻止安全退出的原因(人可读,供确认对话框展示)。
    pub blockers: Vec<String>,
}
