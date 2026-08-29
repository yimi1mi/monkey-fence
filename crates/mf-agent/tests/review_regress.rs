//! Review 修复回归测试:取消不复活、终态事件丢弃、retry/skip 取消孤儿 run、
//! 崩溃窗口孤儿 step 修复、done 会话保留。

use mf_agent::config::Config;
use mf_agent::model::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::pipeline::{PipelineDraft, ProfileIndex, SessionPolicy, StepDraft};
use mf_agent::runtime::{
    AdHocLaunchSpec, AgentProfileSpec, LaunchSpec, RuntimeEvent, RuntimeHost, RuntimeKind,
};
use mf_agent::store::{gen_capability_token, Store};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn mock_profiles() -> Arc<RwLock<ProfileCatalog>> {
    let mut index = ProfileIndex::default();
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
        AgentProfileSpec {
            id: "mock".into(),
            display_name: "Mock".into(),
            runtime: RuntimeKind::Http,
            command: String::new(),
            args: vec![],
            env: vec![],
            permission_args: vec![],
            provider: Some(mf_agent::ProviderConfig {
                kind: mf_agent::ProviderKind::Mock,
                base_url: String::new(),
                api_key: String::new(),
                model: String::new(),
            }),
            icon: None,
            homepage: None,
            hook: None,
        },
    );
    Arc::new(RwLock::new(ProfileCatalog { index, specs }))
}

#[derive(Default)]
struct MockHost {
    launches: Mutex<Vec<LaunchSpec>>,
    senders: Mutex<HashMap<String, crossbeam_channel::Sender<(i64, RuntimeEvent)>>>,
    max_concurrent: AtomicUsize,
    in_flight: AtomicUsize,
}

impl MockHost {
    fn emit_token(&self, token: &str, ev: RuntimeEvent) {
        let run_id = self
            .launches
            .lock()
            .iter()
            .find(|s| s.capability_token == token)
            .map(|s| s.run_id)
            .unwrap_or(0);
        if let Some(tx) = self.senders.lock().get(token) {
            let _ = tx.send((run_id, ev));
        }
    }
    fn token_of_step(&self, title: &str) -> Option<String> {
        self.launches
            .lock()
            .iter()
            .find(|s| s.step_title == title)
            .map(|s| s.capability_token.clone())
    }
    fn launch_count(&self) -> usize {
        self.launches.lock().len()
    }
}

impl RuntimeHost for MockHost {
    fn launch_workflow(
        &self,
        _spec: mf_agent::runtime::WorkflowLaunchSpec,
        _events: crossbeam_channel::Sender<(i64, RuntimeEvent)>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn launch(&self, spec: LaunchSpec, events: crossbeam_channel::Sender<(i64, RuntimeEvent)>) {
        let run_id = spec.run_id;
        self.senders
            .lock()
            .insert(spec.capability_token.clone(), events.clone());
        self.launches.lock().push(spec);
        let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let max = self.max_concurrent.load(Ordering::SeqCst);
        if n > max {
            self.max_concurrent.store(n, Ordering::SeqCst);
        }
        let _ = events.send((run_id, RuntimeEvent::Launched));
    }
    fn send_prompt(&self, _project: &str, _run_id: i64, _session_id: i64, _text: &str) {}
    fn stop_run(&self, _project: &str, _run_id: i64) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
    fn kill_session(&self, _project: &str, _session_id: i64) {}
    fn kill_ad_hoc(&self, _project: &str, _session_id: i64) {}
    fn answer_question(&self, _project: &str, _run_id: i64, _answer: &str) {}
    fn launch_ad_hoc(&self, _spec: AdHocLaunchSpec) -> anyhow::Result<()> {
        Ok(())
    }
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    cond()
}

fn step(key: &str, deps: &[&str]) -> StepDraft {
    StepDraft {
        key: key.into(),
        title: format!("step {key}"),
        instructions: String::new(),
        agent_profile: "mock".into(),
        session_policy: SessionPolicy::Fresh,
        deps: deps.iter().map(|s| s.to_string()).collect(),
    }
}

fn start_orch(dir: &std::path::Path, host: Arc<MockHost>) -> Arc<Orchestrator> {
    let store = Store::open(&dir.join(".mf-agent").join("orchestration.db")).unwrap();
    Orchestrator::start(
        store,
        dir.to_path_buf(),
        Config::default(),
        host,
        mock_profiles(),
        GlobalLimiter::new(4),
        r"\\.\pipe\review-test".into(),
        Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
    )
    .unwrap()
}

/// 取消任务后,迟到的退出/状态事件不得复活 run(取消 → interrupted 恢复 → needs-you 的复活链)。
#[test]
fn cancelled_task_not_resurrected_by_late_events() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let orch = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("取消", "").unwrap();
    orch.save_pipeline(
        task.id,
        &PipelineDraft {
            steps: vec![step("a", &[])],
        },
    )
    .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));
    let token = host.token_of_step("step a").unwrap();
    let run_id = orch.runs_of_task(task.id).unwrap()[0].id;

    orch.cancel_task(task.id).unwrap();
    let t = orch.store.task_view(task.id).unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Cancelled);
    let r = orch.store.run_view(run_id).unwrap().unwrap();
    assert_eq!(r.status, RunStatus::Cancelled);

    // 迟到的退出/状态/提问事件
    host.emit_token(&token, RuntimeEvent::Exited { code: Some(0) });
    host.emit_token(&token, RuntimeEvent::AgentState(AgentState::Done));
    host.emit_token(&token, RuntimeEvent::Question("还活着吗?".into()));
    std::thread::sleep(Duration::from_millis(500));

    let r = orch.store.run_view(run_id).unwrap().unwrap();
    assert_eq!(
        r.status,
        RunStatus::Cancelled,
        "取消的 run 不得被迟到事件复活"
    );
    let t = orch.store.task_view(task.id).unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Cancelled);
    // 重开(recover)后也不得变成 needs-you
    drop(orch);
    let store2 = Store::open(&tmp.path().join(".mf-agent").join("orchestration.db")).unwrap();
    store2.recover_interrupted().unwrap();
    let t = store2.task_view(task.id).unwrap().unwrap();
    assert_eq!(
        t.status,
        TaskStatus::Cancelled,
        "取消的任务在恢复后保持取消"
    );
}

/// retry/skip 必须取消 step 的 awaiting-outcome run,否则 needs-you 永不清除。
#[test]
fn retry_cancels_orphan_awaiting_run() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let orch = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("孤儿 run", "").unwrap();
    orch.save_pipeline(
        task.id,
        &PipelineDraft {
            steps: vec![step("a", &[])],
        },
    )
    .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));
    let token = host.token_of_step("step a").unwrap();
    host.emit_token(&token, RuntimeEvent::AgentState(AgentState::Done));
    assert!(wait_until(Duration::from_secs(5), || {
        orch.store
            .running_runs()
            .unwrap()
            .iter()
            .all(|r| r.status == RunStatus::AwaitingOutcome)
    }));
    let run_id = orch.runs_of_task(task.id).unwrap()[0].id;

    let s = orch.task_detail(task.id).unwrap().unwrap().1;
    orch.retry_step(s[0].id, mf_agent::RetryMode::FreshSession)
        .unwrap();
    let r = orch.store.run_view(run_id).unwrap().unwrap();
    assert_eq!(
        r.status,
        RunStatus::Cancelled,
        "retry 必须取消旧的 awaiting-outcome run"
    );
    // task_attention_cleared 视角:不再有 awaiting/interrupted run
    assert!(!orch
        .store
        .list_runs_of_task(task.id)
        .unwrap()
        .iter()
        .any(|r| matches!(
            r.status,
            RunStatus::AwaitingOutcome | RunStatus::Interrupted
        )));
    orch.stop();
}

/// 崩溃窗口:step 处于 running 但没有活动 run → 重开时被修复为 failed + needs-you。
#[test]
fn orphan_step_repaired_on_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".mf-agent").join("orchestration.db");
    {
        let store = Store::open(&db).unwrap();
        let task = store.create_task("孤儿", "").unwrap();
        let rev = store
            .create_draft_revision(
                task.id,
                &PipelineDraft {
                    steps: vec![step("a", &[])],
                },
            )
            .unwrap();
        store.activate_revision(task.id).unwrap();
        let steps = store.revision_steps(rev.id).unwrap();
        // 直接制造孤儿:step=running,但没有 run
        store
            .set_step_status(steps[0].id, StepStatus::Running)
            .unwrap();
        store.bump_step_attempts(steps[0].id).unwrap();
    }
    let store2 = Store::open(&db).unwrap();
    let repaired = store2.repair_orphan_steps().unwrap();
    assert_eq!(repaired.len(), 1, "孤儿 step 应被修复");
    let tasks = store2.list_tasks(true).unwrap();
    assert_eq!(tasks[0].status, TaskStatus::NeedsYou);
    // 二次修复 no-op
    assert!(store2.repair_orphan_steps().unwrap().is_empty());
}

/// done 会话在崩溃恢复后保留(可复用),不再一律判死。
#[test]
fn done_sessions_survive_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".mf-agent").join("orchestration.db");
    {
        let store = Store::open(&db).unwrap();
        store
            .create_session(Some("k"), "http", "mock", "done-session")
            .unwrap();
        // 手动置为 done(正常结算后钩子上报的状态)
        store
            .update_session(1, Some(SessionStatus::Done), None, None)
            .unwrap();
    }
    let store2 = Store::open(&db).unwrap();
    store2.recover_interrupted().unwrap();
    let s = store2.session_view(1).unwrap().unwrap();
    assert_eq!(
        s.status,
        SessionStatus::Done,
        "done 会话是合法终态,恢复不得判死"
    );
    assert!(store2.find_reusable_session("k", "mock").unwrap().is_some());
}

/// 同 tick 内两个同 session key 的 ready step 不得都被派发(跨任务串行化)。
#[test]
fn same_session_key_serialized_across_tasks_same_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let orch = start_orch(tmp.path(), host.clone());
    let mut mk = |key: &str, policy: SessionPolicy| StepDraft {
        key: format!("{key}-1"),
        title: format!("step {key}-1"),
        instructions: String::new(),
        agent_profile: "mock".into(),
        session_policy: policy,
        deps: vec![],
    };
    let shared = SessionPolicy::Reuse {
        key: "shared".into(),
    };
    let t1 = orch.create_task("T1", "").unwrap();
    orch.save_pipeline(
        t1.id,
        &PipelineDraft {
            steps: vec![mk("s1", shared.clone())],
        },
    )
    .unwrap();
    let t2 = orch.create_task("T2", "").unwrap();
    orch.save_pipeline(
        t2.id,
        &PipelineDraft {
            steps: vec![mk("s2", shared.clone())],
        },
    )
    .unwrap();
    orch.confirm_and_run(t1.id).unwrap();
    orch.confirm_and_run(t2.id).unwrap();
    // 每项目并发 2、全局 4:若无串行化,两个同 key step 会同时跑
    std::thread::sleep(Duration::from_millis(900));
    let count = host.launch_count();
    assert_eq!(count, 1, "同 session key 必须串行(实际派发 {count})");
    let first = host
        .token_of_step("step s1-1")
        .or_else(|| host.token_of_step("step s2-1"))
        .unwrap();
    orch.settle_by_token(
        &first,
        Settlement::Complete {
            summary: String::new(),
            output: Default::default(),
        },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 2));
    orch.stop();
}

/// 结算写入活动 revision 的同 key step(暂停+编辑后 settlement 不丢)。
#[test]
fn settlement_targets_active_revision_step() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let orch = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("编辑后结算", "").unwrap();
    orch.save_pipeline(
        task.id,
        &PipelineDraft {
            steps: vec![step("a", &[]), step("b", &["a"])],
        },
    )
    .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));

    // 暂停 → 编辑 b(未启动)产生新 revision(a 以 running 状态带入)
    orch.pause_task(task.id).unwrap();
    let edited = PipelineDraft {
        steps: vec![
            step("a", &[]),
            StepDraft {
                title: "b 改".into(),
                ..step("b", &["a"])
            },
        ],
    };
    orch.save_pipeline(task.id, &edited).unwrap();
    let token = host.token_of_step("step a").unwrap();
    orch.settle_by_token(
        &token,
        Settlement::Complete {
            summary: "done".into(),
            output: Default::default(),
        },
    )
    .unwrap();

    // 新 revision 中 a 必须是 succeeded(不能卡在 running)
    assert!(wait_until(Duration::from_secs(5), || {
        orch.store
            .task_steps(task.id)
            .unwrap()
            .iter()
            .find(|s| s.step_key == "a")
            .map(|s| s.status == StepStatus::Succeeded)
            .unwrap_or(false)
    }));
    // b 解锁
    assert!(wait_until(Duration::from_secs(5), || {
        orch.store
            .task_steps(task.id)
            .unwrap()
            .iter()
            .find(|s| s.step_key == "b")
            .map(|s| s.status == StepStatus::Running || s.status == StepStatus::Ready)
            .unwrap_or(false)
    }));
    orch.stop();
}

/// 令牌唯一性 + 生成格式冒掌(供 mfctl 使用)。
#[test]
fn capability_tokens_unique() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..1000 {
        let t = gen_capability_token();
        assert!(t.starts_with("mft_"), "令牌前缀: {t}");
        assert!(t.len() >= 32);
        assert!(seen.insert(t), "令牌重复");
    }
}
