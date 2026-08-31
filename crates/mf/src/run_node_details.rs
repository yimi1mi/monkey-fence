//! Run Monitor 节点详情模型(UI 计划 Task 4;设计 §11.4)。
//!
//! 按运行状态投影可用动作(继续/新会话重试/跳过/人工结算/取消/
//! 观察/解决冲突);needs-you 原因集中呈现。渲染接线在
//! agent_workspace / run_monitor。

use mf_agent::model::{RunStatus, RunView, Settlement, StepStatus, StepView};
use mf_agent::RetryMode;

/// Run Monitor 节点动作。
#[derive(Debug, Clone, PartialEq)]
pub enum RunAction {
    /// 继续会话:向运行中的 Agent 追加提示。
    Continue,
    /// 新会话重试(自动重试耗尽后的手动重试)。
    FreshRetry,
    /// 跳过(必须人工确认)。
    Skip,
    /// 人工结算(unknown / awaiting-outcome)。
    ManualSettle,
    /// 结算动作(成功/失败)。
    Settle(Settlement),
    /// 取消运行。
    Cancel,
    /// 观察(未知状态:继续观察,不判失败)。
    Observe,
}

/// 节点附加投影(Run Monitor 富显示:Session/Handoff/租约/日志引用)。
#[derive(Debug, Clone, Default)]
pub struct NodeExtras {
    pub session_status: Option<String>,
    pub handoff_summary: Option<String>,
    pub handoff_files: Vec<String>,
    pub handoff_verification: Option<String>,
    pub handoff_artifacts: Vec<String>,
    pub handoff_blockers: Vec<String>,
    pub handoff_recommendations: Vec<String>,
    /// 结构化自定义输出(紧凑 JSON)。
    pub handoff_output: Option<String>,
    pub lease: Option<String>,
    pub log_ref: Option<String>,
}

/// Run 节点详情投影。
#[derive(Debug, Clone)]
pub struct RunNodeDetails {
    pub run_id: i64,
    /// 步骤数据库 id(「需要你」直达定位用;Task 7)。
    pub step_id: i64,
    pub step_key: String,
    pub step_title: String,
    pub status: RunStatus,
    pub step_status: StepStatus,
    pub attempts: i32,
    pub actions: Vec<RunAction>,
    pub success: bool,
    pub extras: NodeExtras,
}

impl<'a> From<(&'a RunView, &'a StepView)> for RunNodeDetails {
    fn from((run, step): (&'a RunView, &'a StepView)) -> RunNodeDetails {
        let mut actions = Vec::new();
        let success = matches!(
            (run.status, step.status),
            (RunStatus::Succeeded, StepStatus::Succeeded)
        );
        match (run.status, step.status) {
            (RunStatus::Running, StepStatus::Running) => {
                actions.push(RunAction::Continue);
                actions.push(RunAction::Cancel);
            }
            (RunStatus::Interrupted, _) => {
                // 未知状态:观察 / 人工结算 / 新会话重试;绝不显示成功徽标
                actions.push(RunAction::Observe);
                actions.push(RunAction::ManualSettle);
                actions.push(RunAction::FreshRetry);
            }
            (RunStatus::AwaitingOutcome, _) | (_, StepStatus::AwaitingOutcome) => {
                actions.push(RunAction::ManualSettle);
                actions.push(RunAction::Continue);
                actions.push(RunAction::Cancel);
            }
            (RunStatus::Failed, StepStatus::Failed) => {
                actions.push(RunAction::FreshRetry);
                actions.push(RunAction::Skip);
                actions.push(RunAction::ManualSettle);
            }
            (RunStatus::Cancelled, StepStatus::Cancelled) => {
                actions.push(RunAction::FreshRetry);
            }
            (RunStatus::Succeeded, StepStatus::Succeeded) => {}
            _ => {
                actions.push(RunAction::Observe);
            }
        }
        RunNodeDetails {
            run_id: run.id,
            step_id: step.id,
            step_key: step.step_key.clone(),
            step_title: step.title.clone(),
            status: run.status,
            step_status: step.status,
            attempts: step.attempts,
            actions,
            success,
            extras: NodeExtras::default(),
        }
    }
}

impl RunNodeDetails {
    pub fn is_success(&self) -> bool {
        self.success
    }

    /// 重试动作携带的模式(新会话;继续会话经 Continue)。
    pub fn retry_mode(&self) -> RetryMode {
        RetryMode::FreshSession
    }

    /// 富显示行:Session / Handoff(摘要+文件+验证)/ 租约 / 日志引用。
    pub fn extra_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(session) = &self.extras.session_status {
            lines.push(format!("Session: {session}"));
        }
        if let Some(summary) = &self.extras.handoff_summary {
            let mut line = format!("Handoff: {summary}");
            if !self.extras.handoff_files.is_empty() {
                line.push_str(&format!("(文件 {})", self.extras.handoff_files.join(", ")));
            }
            if let Some(verification) = &self.extras.handoff_verification {
                line.push_str(&format!("(验证 {verification})"));
            }
            lines.push(line);
        }
        if !self.extras.handoff_artifacts.is_empty() {
            lines.push(format!(
                "产物: {}",
                self.extras.handoff_artifacts.join(", ")
            ));
        }
        if !self.extras.handoff_blockers.is_empty() {
            lines.push(format!("阻塞: {}", self.extras.handoff_blockers.join("; ")));
        }
        if !self.extras.handoff_recommendations.is_empty() {
            lines.push(format!(
                "建议: {}",
                self.extras.handoff_recommendations.join("; ")
            ));
        }
        if let Some(output) = &self.extras.handoff_output {
            // null 在收集端已过滤;空串与字面 "null" 兜底不显示,
            // 其余(含空对象 "{}")都是合法结构化输出,如实展示
            if !output.trim().is_empty() && output != "null" {
                lines.push(format!("输出: {output}"));
            }
        }
        if let Some(lease) = &self.extras.lease {
            lines.push(format!("Lease: {lease}"));
        }
        if let Some(log) = &self.extras.log_ref {
            lines.push(format!("日志: {log}"));
        }
        lines
    }
}

/// 「需要你」原因投影(needs-input / 合并冲突 / 失败阻塞 / 等待确认)。
pub fn needs_you_reasons(step: &StepView, merge_conflicts: Option<&[String]>) -> Vec<String> {
    let mut out = Vec::new();
    match step.status {
        StepStatus::NeedsInput => out.push(format!("步骤「{}」等待输入", step.title)),
        StepStatus::Failed => out.push(format!("步骤「{}」失败,需要重试/跳过/结算", step.title)),
        StepStatus::AwaitingOutcome => out.push(format!("步骤「{}」等待人工结算", step.title)),
        StepStatus::Blocked => out.push(format!("步骤「{}」被上游失败阻塞", step.title)),
        _ => {}
    }
    if let Some(conflicts) = merge_conflicts {
        for file in conflicts {
            out.push(format!("合并冲突:{file}(需要人工处理或启动集成 Agent)"));
        }
    }
    out
}
