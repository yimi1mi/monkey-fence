//! Run Monitor 的 DAG 状态投影与渲染宿主(UI 计划 Task 4;设计 §11.4)。
//!
//! 复用 Pipeline 页的任务/步骤数据,把 RunNodeDetails 的动作
//! (继续/重试/跳过/结算/取消)接到 Orchestrator;

pub use crate::run_node_details::{needs_you_reasons, RunAction, RunNodeDetails};

use mf_agent::model::{RunView, Settlement, StepView};
use std::sync::Arc;

/// Run Monitor 视图数据(一次任务的投影)。
pub struct RunMonitorSnapshot {
    pub steps: Vec<StepView>,
    pub runs: Vec<RunView>,
    /// 手工结算输入缓冲(空 = 未输入)。
    pub settle_input: String,
}

impl RunMonitorSnapshot {
    pub fn from_parts(steps: Vec<StepView>, runs: Vec<RunView>) -> RunMonitorSnapshot {
        RunMonitorSnapshot {
            steps,
            runs,
            settle_input: String::new(),
        }
    }

    /// 每个步骤的最新 run 详情(按 step_key)。
    pub fn node_details(&self) -> Vec<RunNodeDetails> {
        self.steps
            .iter()
            .map(|step| {
                let latest = self
                    .runs
                    .iter()
                    .filter(|r| r.step_id == step.id)
                    .max_by_key(|r| r.id);
                match latest {
                    Some(run) => RunNodeDetails::from((run, step)),
                    None => {
                        // 无 run(未派发):仅观察
                        let mut details = RunNodeDetails::from((&placeholder_run(), step));
                        details.actions = vec![RunAction::Observe];
                        details
                    }
                }
            })
            .collect()
    }
}

fn placeholder_run() -> RunView {
    RunView {
        id: 0,
        task_id: 0,
        step_id: 0,
        revision_id: 0,
        session_id: None,
        status: mf_agent::model::RunStatus::Running,
        agent_state: mf_agent::model::AgentState::Idle,
        capability_token: String::new(),
        outcome: None,
        outcome_payload: None,
        started_at: String::new(),
        ended_at: None,
    }
}

/// 执行动作(接线 Orchestrator;返回用户可见结果)。
pub fn execute_action(
    orch: &Arc<mf_agent::Orchestrator>,
    details: &RunNodeDetails,
    action: &RunAction,
    settle_text: &str,
) -> Result<String, String> {
    match action {
        RunAction::Continue => {
            let text = if settle_text.trim().is_empty() {
                "请继续"
            } else {
                settle_text
            };
            orch.send_prompt(details.run_id, text)
                .map_err(|e| format!("{e:#}"))?;
            Ok("已发送提示".into())
        }
        RunAction::FreshRetry => orch
            .retry_step(
                latest_step_id(orch, details),
                mf_agent::RetryMode::FreshSession,
            )
            .map(|_| "已用新会话重试".to_string())
            .map_err(|e| format!("{e:#}")),
        RunAction::Skip => orch
            .skip_step(latest_step_id(orch, details), true)
            .map(|_| "已跳过".to_string())
            .map_err(|e| format!("{e:#}")),
        RunAction::Cancel => {
            if details.run_id == 0 {
                return Ok("尚未派发,无需取消".into());
            }
            let runs = orch.store.running_runs().map_err(|e| format!("{e:#}"))?;
            if runs.iter().any(|r| r.id == details.run_id) {
                orch.store
                    .set_run_status(details.run_id, mf_agent::model::RunStatus::Cancelled)
                    .map_err(|e| format!("{e:#}"))?;
            }
            Ok("已取消".into())
        }
        RunAction::ManualSettle | RunAction::Settle(_) => {
            let settlement = if settle_text.trim().eq_ignore_ascii_case("fail")
                || settle_text.starts_with("失败:")
            {
                Settlement::Fail {
                    reason: settle_text.trim_start_matches("失败:").to_string(),
                }
            } else {
                Settlement::Complete {
                    summary: if settle_text.trim().is_empty() {
                        "人工确认完成".into()
                    } else {
                        settle_text.to_string()
                    },
                }
            };
            use mf_agent::orchestrator::Orchestrator;
            Orchestrator::settle_run(orch, details.run_id, settlement)
                .map(|_| "已提交结算".to_string())
                .map_err(|e| format!("{e:#}"))
        }
        RunAction::Observe => Ok("继续观察(未知状态不是失败)".into()),
    }
}

fn latest_step_id(orch: &Arc<mf_agent::Orchestrator>, details: &RunNodeDetails) -> i64 {
    orch.store
        .task_steps(details_task(orch, details))
        .ok()
        .and_then(|steps| {
            steps
                .iter()
                .find(|s| s.step_key == details.step_key)
                .map(|s| s.id)
        })
        .unwrap_or(0)
}

fn details_task(orch: &Arc<mf_agent::Orchestrator>, details: &RunNodeDetails) -> i64 {
    orch.store
        .run_view(details.run_id)
        .ok()
        .flatten()
        .map(|r| r.task_id)
        .unwrap_or(0)
}
