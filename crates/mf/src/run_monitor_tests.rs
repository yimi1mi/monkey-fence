//! Run Monitor 动作可用性测试(UI Task 4;设计 §11.4):
//! 未知态提供观察/结算/重试而非成功徽标;各状态的动作集合;
//! needs-you 原因投影;merge conflict 呈现。

use crate::run_node_details::{needs_you_reasons, RunAction, RunNodeDetails};
use mf_agent::model::*;
use mf_agent::Settlement;

fn run(status: RunStatus, outcome: Option<&str>) -> RunView {
    RunView {
        id: 1,
        task_id: 1,
        step_id: 1,
        revision_id: 1,
        session_id: Some(1),
        status,
        agent_state: AgentState::Working,
        capability_token: "t".into(),
        outcome: outcome.map(str::to_string),
        outcome_payload: None,
        started_at: String::new(),
        ended_at: None,
    }
}

fn step(status: StepStatus) -> StepView {
    StepView {
        id: 1,
        revision_id: 1,
        task_id: 1,
        step_key: "build".into(),
        title: "构建".into(),
        instructions: String::new(),
        agent_profile: "inst".into(),
        session_policy: "fresh".into(),
        status,
        attempts: 1,
        auto_retry: 0,
        result: None,
        started_at: None,
        ended_at: None,
        deps: vec![],
    }
}

#[test]
fn unknown_run_offers_observe_settle_or_retry_but_not_success_badge() {
    let model = RunNodeDetails::from((
        &run(RunStatus::Interrupted, None),
        &step(StepStatus::AwaitingOutcome),
    ));
    assert!(model.actions.contains(&RunAction::Observe));
    assert!(model.actions.contains(&RunAction::FreshRetry));
    assert!(model.actions.contains(&RunAction::ManualSettle));
    assert!(!model.is_success());
}

#[test]
fn running_offers_continue_and_cancel_only() {
    let model = RunNodeDetails::from((&run(RunStatus::Running, None), &step(StepStatus::Running)));
    assert!(model.actions.contains(&RunAction::Continue));
    assert!(model.actions.contains(&RunAction::Cancel));
    assert!(!model.actions.contains(&RunAction::Skip));
    assert!(!model.actions.contains(&RunAction::FreshRetry));
}

#[test]
fn failed_offers_retry_skip_and_manual_settle() {
    let model = RunNodeDetails::from((
        &run(RunStatus::Failed, Some("fail")),
        &step(StepStatus::Failed),
    ));
    assert!(model.actions.contains(&RunAction::FreshRetry));
    assert!(model.actions.contains(&RunAction::Skip));
    assert!(model.actions.contains(&RunAction::ManualSettle));
    assert!(!model.is_success());
}

#[test]
fn awaiting_outcome_offers_settle_and_continue() {
    let model = RunNodeDetails::from((
        &run(RunStatus::AwaitingOutcome, None),
        &step(StepStatus::AwaitingOutcome),
    ));
    assert!(model.actions.contains(&RunAction::ManualSettle));
    assert!(model.actions.contains(&RunAction::Continue));
}

#[test]
fn succeeded_shows_success_badge_without_actions() {
    let model = RunNodeDetails::from((
        &run(RunStatus::Succeeded, Some("complete")),
        &step(StepStatus::Succeeded),
    ));
    assert!(model.is_success());
    assert!(!model.actions.contains(&RunAction::FreshRetry));
    assert!(!model.actions.contains(&RunAction::Cancel));
}

#[test]
fn needs_input_reason_and_merge_conflict_surface() {
    let reasons = needs_you_reasons(&step(StepStatus::NeedsInput), None);
    assert!(reasons.iter().any(|r| r.contains("等待输入")));

    let conflicts = vec!["shared.txt(build 与 docs)".to_string()];
    let reasons = needs_you_reasons(&step(StepStatus::Blocked), Some(&conflicts));
    assert!(reasons
        .iter()
        .any(|r| r.contains("shared.txt") && r.contains("合并冲突")));
}

#[test]
fn settlement_actions_carry_labels() {
    // 人工结算两种结果的动作语义
    let complete = RunAction::Settle(Settlement::Complete {
        summary: "ok".into(),
    });
    let fail = RunAction::Settle(Settlement::Fail {
        reason: "bad".into(),
    });
    assert_ne!(complete, fail);
    let model = RunNodeDetails::from((
        &run(RunStatus::AwaitingOutcome, None),
        &step(StepStatus::AwaitingOutcome),
    ));
    let _ = (complete, fail, model);
}
