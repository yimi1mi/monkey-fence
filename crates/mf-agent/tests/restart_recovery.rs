//! 重启恢复与未知态语义(Orchestration Task 6):
//! 存活会话重连、未结算等待确认、丢失进程 interrupted(绝不判失败)、
//! 未知状态保持执行租约、人工结算通道保留。

use crossbeam_channel::Sender;
use mf_agent::model::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::pipeline::{PipelineDraft, SessionPolicy, StepDraft};
use mf_agent::runtime::{LaunchSpec, RuntimeEvent, RuntimeHost};
use mf_agent::store::Store;
use mf_agent::{AdHocLaunchSpec, RetryMode};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct MockHost {
    launches: Mutex<Vec<LaunchSpec>>,
    senders: Mutex<HashMap<String, Sender<(i64, RuntimeEvent)>>>,
    /// 重启后上报的会话存活状态
    session_alive: Mutex<HashMap<i64, bool>>,
}

impl RuntimeHost for MockHost {
    fn launch_workflow(
        &self,
        _spec: mf_agent::runtime::WorkflowLaunchSpec,
        _events: Sender<(i64, RuntimeEvent)>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn launch(&self, spec: LaunchSpec, events: Sender<(i64, RuntimeEvent)>) {
        let run_id = spec.run_id;
        self.senders
            .lock()
            .insert(spec.capability_token.clone(), events.clone());
        self.launches.lock().push(spec);
        let _ = events.send((run_id, RuntimeEvent::Launched));
    }
    fn launch_ad_hoc(&self, _spec: AdHocLaunchSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn send_prompt(&self, _p: &str, _r: i64, _s: i64, _t: &str) {}
    fn stop_run(&self, _p: &str, _r: i64) {}
    fn kill_session(&self, _p: &str, _s: i64) {}
    fn kill_ad_hoc(&self, _p: &str, _s: i64) {}
    fn answer_question(&self, _p: &str, _r: i64, _a: &str) {}
    fn is_session_alive(&self, _project: &str, session_id: i64) -> bool {
        self.session_alive
            .lock()
            .get(&session_id)
            .copied()
            .unwrap_or(false)
    }
}

struct Recovered {
    orch: Arc<Orchestrator>,
    store: Arc<Store>,
    task_id: i64,
    step_id: i64,
    run_id: i64,
    token: String,
    session_id: i64,
}

/// 建库 → 运行一个 step → 崩溃(直接 stop)→ 按 session_alive 重启恢复。
fn recover_fixture(session_alive: bool, pre_exit_code: Option<i32>) -> Recovered {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    let (task_id, step_id, run_id, token, session_id);
    {
        let store = Store::open(&db).unwrap();
        let host = Arc::new(MockHost::default());
        let orch = Orchestrator::start(
            store.clone(),
            tmp.path().to_path_buf(),
            mf_agent::Config::default(),
            host,
            mock_profiles(),
            GlobalLimiter::new(4),
            "test-pipe".into(),
            Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
        )
        .unwrap();
        let task = orch.create_task("恢复任务", "goal").unwrap();
        let draft = PipelineDraft {
            steps: vec![step("only")],
        };
        orch.save_pipeline(task.id, &draft).unwrap();
        orch.confirm_and_run(task.id).unwrap();
        let ok = wait_until(5_000, || {
            orch.store
                .task_steps(task.id)
                .unwrap()
                .first()
                .map(|s| matches!(s.status, StepStatus::Running))
                .unwrap_or(false)
        });
        assert!(ok, "重启前 step 应处于 running");
        let step = orch.store.task_steps(task.id).unwrap().remove(0);
        let run = orch.store.list_runs_of_step(step.id).unwrap().remove(0);
        if pre_exit_code.is_some() {
            // 模拟"已退出但未结算"的一次性进程
            orch.push_runtime_event(run.id, RuntimeEvent::Exited { code: None });
            let ok = wait_until(5_000, || {
                orch.store
                    .run_view(run.id)
                    .unwrap()
                    .map(|r| r.status == RunStatus::AwaitingOutcome)
                    .unwrap_or(false)
            });
            assert!(ok, "退出未结算应进入 awaiting-outcome");
        }
        task_id = task.id;
        step_id = step.id;
        run_id = run.id;
        token = run.capability_token.clone();
        session_id = run.session_id.unwrap();
        let lease_held = orch
            .store
            .list_execution_leases(task_id)
            .unwrap()
            .first()
            .map(|l| l.status == "held")
            .unwrap_or(false);
        assert!(lease_held, "运行期租约应处于 held");
        orch.stop();
    }
    // 重启:宿主上报会话存活状态
    let store = Store::open(&db).unwrap();
    let host = Arc::new(MockHost::default());
    host.session_alive.lock().insert(session_id, session_alive);
    let orch = Orchestrator::start(
        store.clone(),
        tmp.path().to_path_buf(),
        mf_agent::Config::default(),
        host,
        mock_profiles(),
        GlobalLimiter::new(4),
        "test-pipe".into(),
        Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
    )
    .unwrap();
    Recovered {
        orch,
        store,
        task_id,
        step_id,
        run_id,
        token,
        session_id,
    }
}

fn step(key: &str) -> StepDraft {
    StepDraft {
        key: key.into(),
        title: key.into(),
        instructions: String::new(),
        agent_profile: "mock".into(),
        session_policy: SessionPolicy::Fresh,
        deps: vec![],
    }
}

fn mock_profiles() -> Arc<RwLock<ProfileCatalog>> {
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
        mf_agent::runtime::AgentProfileSpec {
            id: "mock".into(),
            display_name: "Mock".into(),
            runtime: mf_agent::runtime::RuntimeKind::Pty,
            command: "cmd.exe".into(),
            args: vec![],
            env: vec![],
            permission_args: vec![],
            provider: None,
            icon: None,
            homepage: None,
            hook: None,
        },
    );
    Arc::new(RwLock::new(ProfileCatalog { index, specs }))
}

fn wait_until(timeout_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(timeout_ms) {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

// ---------- 未知态:不是失败 ----------

#[test]
fn lost_process_becomes_interrupted_not_failed() {
    let fx = recover_fixture(false, None);
    let run = fx.store.run_view(fx.run_id).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Interrupted);
    let step = fx.store.step_view(fx.step_id).unwrap().unwrap();
    assert_eq!(
        step.status,
        StepStatus::AwaitingOutcome,
        "未知状态等待确认,不判失败"
    );
    let task = fx.store.task_view(fx.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::NeedsYou);
    assert_ne!(task.status, TaskStatus::Failed);
    // 未知状态保持执行租约
    let lease = fx
        .store
        .list_execution_leases(fx.task_id)
        .unwrap()
        .remove(0);
    assert_eq!(lease.status, "held", "未知状态不得释放租约");
    fx.orch.stop();
}

#[test]
fn reattached_run_keeps_running() {
    let fx = recover_fixture(true, None);
    let run = fx.store.run_view(fx.run_id).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Running, "宿主确认存活的会话应重连");
    let step = fx.store.step_view(fx.step_id).unwrap().unwrap();
    assert_eq!(step.status, StepStatus::Running);
    let task = fx.store.task_view(fx.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Running);
    // 重连的会话进程仍在运行:不得被恢复逻辑批量标记为 dead
    let session = fx.store.session_view(fx.session_id).unwrap().unwrap();
    assert_ne!(
        session.status,
        mf_agent::model::SessionStatus::Dead,
        "reattached 会话不得被标记 dead(实际 {:?})",
        session.status
    );
    fx.orch.stop();
}

#[test]
fn exited_without_result_awaits_confirmation() {
    // 崩溃前进程已退出但无结果:恢复后等待人工确认,不判失败
    let fx = recover_fixture(true, Some(0));
    let run = fx.store.run_view(fx.run_id).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::AwaitingOutcome);
    let step = fx.store.step_view(fx.step_id).unwrap().unwrap();
    assert_eq!(step.status, StepStatus::AwaitingOutcome);
    let task = fx.store.task_view(fx.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::NeedsYou);
    assert_ne!(task.status, TaskStatus::Failed);
    fx.orch.stop();
}

#[test]
fn interrupted_run_can_still_be_settled_manually() {
    let fx = recover_fixture(false, None);
    // 人工结算通道保留:用户确认后提交成功
    let outcome = fx
        .orch
        .settle_by_token(
            &fx.token,
            Settlement::Complete {
                summary: "人工确认完成".into(),
            },
        )
        .unwrap();
    assert_eq!(outcome, SettleOutcome::Applied);
    let run = fx.store.run_view(fx.run_id).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    let task = fx.store.task_view(fx.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Succeeded);
    // 终态结算后租约才释放
    let lease = fx
        .store
        .list_execution_leases(fx.task_id)
        .unwrap()
        .remove(0);
    assert_eq!(lease.status, "released");
    fx.orch.stop();
}

#[test]
fn interrupted_run_supports_fresh_retry() {
    let fx = recover_fixture(false, None);
    let step = fx
        .orch
        .retry_step(fx.step_id, RetryMode::FreshSession)
        .unwrap();
    assert_eq!(step.status, StepStatus::Ready);
    fx.orch.stop();
}
