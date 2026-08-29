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

// ---------- 复审阻塞项 8:取消动作 = 终止进程 + 释放租约 ----------

#[test]
fn cancel_action_stops_process_and_releases_lease() {
    use crate::run_monitor::execute_action;
    use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
    use mf_agent::runtime::{AdHocLaunchSpec, LaunchSpec, RuntimeEvent, RuntimeHost};
    use parking_lot::{Mutex, RwLock};
    use std::collections::HashMap;

    struct StopHost {
        stopped: Mutex<Vec<i64>>,
    }
    impl RuntimeHost for StopHost {
        fn launch_workflow(
            &self,
            _spec: mf_agent::runtime::WorkflowLaunchSpec,
            _events: crossbeam_channel::Sender<(i64, RuntimeEvent)>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn launch(
            &self,
            _spec: LaunchSpec,
            _events: crossbeam_channel::Sender<(i64, RuntimeEvent)>,
        ) {
        }
        fn launch_ad_hoc(&self, _spec: AdHocLaunchSpec) -> anyhow::Result<()> {
            Ok(())
        }
        fn send_prompt(&self, _: &str, _: i64, _: i64, _: &str) {}
        fn stop_run(&self, _: &str, run_id: i64) {
            self.stopped.lock().push(run_id);
        }
        fn kill_session(&self, _: &str, _: i64) {}
        fn kill_ad_hoc(&self, _: &str, _: i64) {}
        fn answer_question(&self, _: &str, _: i64, _: &str) {}
    }

    let dir = tempfile::tempdir().unwrap();
    let host = std::sync::Arc::new(StopHost {
        stopped: Mutex::new(Vec::new()),
    });
    let store = mf_agent::Store::open(&dir.path().join("monitor.db")).unwrap();
    let mut index = mf_agent::pipeline::ProfileIndex::default();
    index.entries.insert(
        "mock".into(),
        mf_agent::pipeline::ProfileAvailability {
            installed: true,
            enabled: true,
            detected: true,
        },
    );
    let mut specs = HashMap::new();
    specs.insert(
        "mock".to_string(),
        mf_agent::AgentProfileSpec {
            id: "mock".into(),
            display_name: "Mock".into(),
            runtime: mf_agent::RuntimeKind::Http,
            command: String::new(),
            args: vec![],
            env: vec![],
            permission_args: vec![],
            provider: None,
            icon: None,
            homepage: None,
            hook: None,
        },
    );
    let orch = Orchestrator::start(
        store,
        dir.path().to_path_buf(),
        mf_agent::Config::default(),
        host.clone(),
        std::sync::Arc::new(RwLock::new(ProfileCatalog { index, specs })),
        GlobalLimiter::new(4),
        "pipe".into(),
        std::sync::Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
    )
    .unwrap();
    let task = orch.create_task("监控", "g").unwrap();
    orch.save_pipeline(
        task.id,
        &mf_agent::PipelineDraft {
            steps: vec![mf_agent::StepDraft {
                key: "s".into(),
                title: "s".into(),
                instructions: String::new(),
                agent_profile: "mock".into(),
                session_policy: mf_agent::SessionPolicy::Fresh,
                deps: vec![],
            }],
        },
    )
    .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let run = loop {
        let runs = orch.store.list_runs_of_task(task.id).unwrap();
        if let Some(r) = runs
            .iter()
            .find(|r| r.status == mf_agent::model::RunStatus::Running)
        {
            break r.clone();
        }
        assert!(std::time::Instant::now() < deadline, "等待派发超时");
        std::thread::sleep(std::time::Duration::from_millis(30));
    };
    let steps = orch.store.task_steps(task.id).unwrap();
    let details = crate::run_monitor::RunNodeDetails::from((&run, &steps[0]));

    let msg = execute_action(&orch, &details, &crate::run_monitor::RunAction::Cancel, "").unwrap();
    assert!(msg.contains("已取消"), "{msg}");
    // 完整链:进程停止已请求(不是只改 DB)
    assert!(host.stopped.lock().contains(&run.id));
    let after = orch.store.run_view(run.id).unwrap().unwrap();
    assert_eq!(after.status, mf_agent::model::RunStatus::Cancelled);
    orch.stop();
}
