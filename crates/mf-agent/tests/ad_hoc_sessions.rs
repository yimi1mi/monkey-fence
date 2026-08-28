//! 离散 CLI 会话(Ad-hoc):挂在 Task 下但不属于 Pipeline Revision,
//! 没有 Step / Agent Run,绝不改变 Task 状态(设计 §4.7 / §10)。

use crossbeam_channel::Sender;
use mf_agent::agent_adapter::{CompletionDetector, InputInjection, LaunchPlan};
use mf_agent::agent_instance::AgentInstanceSnapshot;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::runtime::{AdHocLaunchSpec, LaunchSpec, RuntimeEvent, RuntimeHost};
use mf_agent::store::Store;
use mf_agent::{HandoffDraft, RunMode, SessionStatus, TaskStatus};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;

// ---------- 测试宿主:记录离散启动,不真正拉进程 ----------

/// 快速退出模拟:launch_ad_hoc 返回前同步上报一次退出事件。
#[derive(Clone, Copy)]
struct FastExit {
    exit_code: Option<i32>,
    marker_seen: bool,
    result_file_present: bool,
}

struct MockHost {
    ad_hoc_launches: Mutex<Vec<AdHocLaunchSpec>>,
    fail_ad_hoc: bool,
    fast_exit: Option<FastExit>,
}

impl MockHost {
    fn healthy() -> MockHost {
        MockHost {
            ad_hoc_launches: Mutex::new(Vec::new()),
            fail_ad_hoc: false,
            fast_exit: None,
        }
    }

    fn failing() -> MockHost {
        MockHost {
            ad_hoc_launches: Mutex::new(Vec::new()),
            fail_ad_hoc: true,
            fast_exit: None,
        }
    }

    fn fast_exiting(exit: FastExit) -> MockHost {
        MockHost {
            ad_hoc_launches: Mutex::new(Vec::new()),
            fail_ad_hoc: false,
            fast_exit: Some(exit),
        }
    }
}

impl RuntimeHost for MockHost {
    fn launch(&self, _spec: LaunchSpec, _events: Sender<(i64, RuntimeEvent)>) {}
    fn send_prompt(&self, _project: &str, _run_id: i64, _session_id: i64, _text: &str) {}
    fn stop_run(&self, _project: &str, _run_id: i64) {}
    fn kill_session(&self, _project: &str, _session_id: i64) {}
    fn kill_ad_hoc(&self, _project: &str, _session_id: i64) {}
    fn answer_question(&self, _project: &str, _run_id: i64, _answer: &str) {}
    fn launch_ad_hoc(&self, spec: AdHocLaunchSpec) -> anyhow::Result<()> {
        let session_id = spec.session_id;
        let events = spec.events.clone();
        self.ad_hoc_launches.lock().push(spec);
        if self.fail_ad_hoc {
            anyhow::bail!("spawn failed")
        }
        if let Some(fast) = self.fast_exit {
            let _ = events.send((
                session_id,
                RuntimeEvent::AdHocExited {
                    session_id,
                    exit_code: fast.exit_code,
                    marker_seen: fast.marker_seen,
                    result_file_present: fast.result_file_present,
                },
            ));
        }
        Ok(())
    }
}

// ---------- Fixture ----------

struct Fixture {
    orch: Arc<Orchestrator>,
    host: Arc<MockHost>,
}

impl Fixture {
    fn memory() -> Fixture {
        let store = Store::memory().unwrap();
        Self::with_store_and_host(store, MockHost::healthy())
    }

    fn failing() -> Fixture {
        let store = Store::memory().unwrap();
        Self::with_store_and_host(store, MockHost::failing())
    }

    fn fast_exit(exit: FastExit) -> Fixture {
        let store = Store::memory().unwrap();
        Self::with_store_and_host(store, MockHost::fast_exiting(exit))
    }

    fn file(path: &std::path::Path) -> Fixture {
        let store = Store::open(path).unwrap();
        Self::with_store_and_host(store, MockHost::healthy())
    }

    fn with_store_and_host(store: Arc<Store>, host: MockHost) -> Fixture {
        let host = Arc::new(host);
        let orch = Orchestrator::start(
            store,
            PathBuf::from("."),
            mf_agent::Config::default(),
            host.clone(),
            Arc::new(parking_lot::RwLock::new(ProfileCatalog::default())),
            GlobalLimiter::new(4),
            "test-pipe".into(),
            Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
        )
        .unwrap();
        Fixture { orch, host }
    }

    fn create_ad_hoc(&self, task_id: i64) -> anyhow::Result<mf_agent::AdHocSessionView> {
        self.orch.create_ad_hoc_session(
            task_id,
            &snapshot(),
            RunMode::Interactive,
            PathBuf::from("C:/tmp/run"),
            launch_plan(),
        )
    }

    fn create_ad_hoc_with(
        &self,
        task_id: i64,
        snapshot: &AgentInstanceSnapshot,
    ) -> anyhow::Result<mf_agent::AdHocSessionView> {
        self.orch.create_ad_hoc_session(
            task_id,
            snapshot,
            snapshot.run_mode,
            PathBuf::from("C:/tmp/run"),
            launch_plan(),
        )
    }
}

fn launch_plan() -> LaunchPlan {
    LaunchPlan {
        run_temp: PathBuf::from("C:/tmp/run"),
        executable: PathBuf::from("agent.exe"),
        argv: vec!["--chat".into()],
        env: vec![("LANG".into(), "C".into())],
        secret_env: vec![],
        cwd: Some(PathBuf::from(".")),
        temp_files: vec![],
        input: InputInjection::Argv(String::new()),
        completion: CompletionDetector::Manual,
        uses_shell: false,
    }
}

fn snapshot() -> AgentInstanceSnapshot {
    AgentInstanceSnapshot {
        id: "inst_x".into(),
        name: "咨询 Agent".into(),
        agent_type: "generic-command".into(),
        version: 1,
        enabled: true,
        run_mode: RunMode::Interactive,
        executable: "agent.exe".into(),
        argv: vec!["--chat".into()],
        env: vec![("LANG".into(), "C".into())],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({}),
        sealed_secret_ids: vec![],
    }
}

/// 一次性 + 指定完成契约的快照变体。
fn snapshot_with(run_mode: RunMode, contract: serde_json::Value) -> AgentInstanceSnapshot {
    AgentInstanceSnapshot {
        run_mode,
        execution_contract: contract,
        ..snapshot()
    }
}

fn wait_until(timeout: std::time::Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    cond()
}

// ---------- 测试 ----------

#[test]
fn ad_hoc_session_does_not_change_task_status() {
    let fixture = Fixture::memory();
    let task = fixture.orch.create_task("t", "goal").unwrap();
    fixture.create_ad_hoc(task.id).unwrap();
    assert_eq!(
        fixture
            .orch
            .store
            .task_view(task.id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Draft
    );
}

#[test]
fn ad_hoc_launches_host_without_inventing_step_or_run() {
    let fixture = Fixture::memory();
    let task = fixture.orch.create_task("t", "goal").unwrap();
    let view = fixture.create_ad_hoc(task.id).unwrap();

    // 宿主收到离散启动:路由键是 ad_hoc 行号,无 run/step
    let launches = fixture.host.ad_hoc_launches.lock();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].session_id, view.id);
    assert_eq!(launches[0].task_id, task.id);
    assert_eq!(launches[0].plan.executable, PathBuf::from("agent.exe"));
    assert_eq!(launches[0].plan.argv, vec!["--chat".to_string()]);
    assert_eq!(launches[0].run_temp, PathBuf::from("C:/tmp/run"));
    assert_eq!(launches[0].run_mode, RunMode::Interactive);
    drop(launches);

    // 已启动:状态 working + launched_at
    assert_eq!(view.status, SessionStatus::Working);
    assert!(view.launched_at.is_some());
    assert_eq!(view.snapshot.name, "咨询 Agent");

    // 不产生 Step / Agent Run
    assert!(fixture.orch.store.task_steps(task.id).unwrap().is_empty());
    assert!(fixture
        .orch
        .store
        .list_runs_of_task(task.id)
        .unwrap()
        .is_empty());
    assert!(!fixture.orch.store.task_has_active_runs(task.id).unwrap());

    // 列表挂在 Task 下
    let listed = fixture.orch.store.list_ad_hoc_sessions(task.id).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, view.id);
}

#[test]
fn unknown_task_rejects_ad_hoc_session() {
    let fixture = Fixture::memory();
    assert!(fixture
        .orch
        .create_ad_hoc_session(
            999,
            &snapshot(),
            RunMode::Interactive,
            PathBuf::from("C:/tmp/run"),
            launch_plan(),
        )
        .is_err());
}

#[test]
fn adapter_cannot_replace_trusted_run_temp() {
    let fixture = Fixture::memory();
    let task = fixture.orch.create_task("t", "goal").unwrap();
    let mut plan = launch_plan();
    plan.run_temp = PathBuf::from("C:/Users/example/.codex");

    let error = fixture
        .orch
        .create_ad_hoc_session(
            task.id,
            &snapshot(),
            RunMode::Interactive,
            PathBuf::from("C:/tmp/run"),
            plan,
        )
        .unwrap_err();
    assert!(error.to_string().contains("run-temp"));
    assert!(fixture.host.ad_hoc_launches.lock().is_empty());
    assert!(fixture
        .orch
        .store
        .list_ad_hoc_sessions(task.id)
        .unwrap()
        .is_empty());
}

#[test]
fn spawn_failure_marks_session_dead_without_touching_task() {
    let fixture = Fixture::failing();
    let task = fixture.orch.create_task("t", "goal").unwrap();

    let err = fixture.create_ad_hoc(task.id).unwrap_err();
    assert!(err.to_string().contains("spawn failed"));

    let sessions = fixture.orch.store.list_ad_hoc_sessions(task.id).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].status, SessionStatus::Dead);
    assert!(sessions[0].launched_at.is_none());
    assert!(sessions[0].ended_at.is_some());
    assert_eq!(
        fixture
            .orch
            .store
            .task_view(task.id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Draft
    );
    assert!(fixture.orch.store.task_steps(task.id).unwrap().is_empty());
    assert!(fixture
        .orch
        .store
        .list_runs_of_task(task.id)
        .unwrap()
        .is_empty());
}

#[test]
fn submit_handoff_records_without_task_mutation() {
    let fixture = Fixture::memory();
    let task = fixture.orch.create_task("t", "goal").unwrap();
    let view = fixture.create_ad_hoc(task.id).unwrap();

    let handoff = HandoffDraft {
        status: "completed".into(),
        summary: "分析完成".into(),
        output: serde_json::json!({ "notes": "ok" }),
        ..Default::default()
    };
    let submitted = fixture
        .orch
        .submit_ad_hoc_handoff(view.id, &handoff)
        .unwrap();
    assert_eq!(submitted.status, SessionStatus::Done);
    assert!(submitted.handoff.is_some());
    let stored: serde_json::Value = serde_json::from_str(&submitted.handoff.unwrap()).unwrap();
    assert_eq!(stored["summary"], "分析完成");
    assert!(submitted.ended_at.is_some());

    // Task 状态依旧不变
    assert_eq!(
        fixture
            .orch
            .store
            .task_view(task.id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Draft
    );
    assert!(!fixture.orch.store.task_has_active_runs(task.id).unwrap());
}

#[test]
fn restart_preserves_ad_hoc_sessions_and_task_status() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("workflow-v1.db");

    let task_id;
    let ad_hoc_id;
    {
        let fixture = Fixture::file(&db);
        let task = fixture.orch.create_task("t", "goal").unwrap();
        let view = fixture.create_ad_hoc(task.id).unwrap();
        task_id = task.id;
        ad_hoc_id = view.id;
        fixture.orch.stop();
    }

    // 重新打开:恢复逻辑不得把 Task 误判为 needs-you/failed
    let fixture = Fixture::file(&db);
    let task = fixture.orch.store.task_view(task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Draft);

    let listed = fixture.orch.store.list_ad_hoc_sessions(task_id).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, ad_hoc_id);
    assert_eq!(listed[0].snapshot.executable, "agent.exe");
    assert_eq!(listed[0].snapshot.name, "咨询 Agent");
    fixture.orch.stop();
}

// ---------- 退出通知与完成分类 ----------

fn status_of(orch: &Orchestrator, task_id: i64) -> SessionStatus {
    orch.store
        .list_ad_hoc_sessions(task_id)
        .unwrap()
        .first()
        .map(|s| s.status)
        .unwrap_or(SessionStatus::Idle)
}

#[test]
fn fast_oneshot_exit_zero_is_done_not_working() {
    let fixture = Fixture::fast_exit(FastExit {
        exit_code: Some(0),
        marker_seen: false,
        result_file_present: false,
    });
    let task = fixture.orch.create_task("t", "goal").unwrap();
    let snapshot = snapshot_with(
        RunMode::OneShot,
        serde_json::json!({ "completion": "process-exit" }),
    );
    fixture.create_ad_hoc_with(task.id, &snapshot).unwrap();

    // 无论退出事件先于/后于 mark_ad_hoc_launched 处理,最终都是 Done
    let ok = wait_until(std::time::Duration::from_secs(5), || {
        status_of(&fixture.orch, task.id) == SessionStatus::Done
    });
    assert!(ok, "快速退出不得被覆盖为 Working");
    let view = fixture.orch.store.list_ad_hoc_sessions(task.id).unwrap()[0].clone();
    assert!(view.ended_at.is_some());
    assert_eq!(
        fixture
            .orch
            .store
            .task_view(task.id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Draft
    );
}

#[test]
fn oneshot_nonzero_exit_is_dead() {
    let fixture = Fixture::fast_exit(FastExit {
        exit_code: Some(3),
        marker_seen: false,
        result_file_present: false,
    });
    let task = fixture.orch.create_task("t", "goal").unwrap();
    let snapshot = snapshot_with(
        RunMode::OneShot,
        serde_json::json!({ "completion": "process-exit" }),
    );
    fixture.create_ad_hoc_with(task.id, &snapshot).unwrap();
    assert!(wait_until(std::time::Duration::from_secs(5), || {
        status_of(&fixture.orch, task.id) == SessionStatus::Dead
    }));
}

#[test]
fn interactive_exit_is_never_reported_success() {
    // 交互式会话即使退出码 0 也不得标记 Done(不误报成功)
    let fixture = Fixture::fast_exit(FastExit {
        exit_code: Some(0),
        marker_seen: true,
        result_file_present: true,
    });
    let task = fixture.orch.create_task("t", "goal").unwrap();
    fixture.create_ad_hoc(task.id).unwrap();
    assert!(
        wait_until(std::time::Duration::from_secs(5), || {
            status_of(&fixture.orch, task.id) == SessionStatus::Dead
        }),
        "interactive 退出应为 Dead,而非 Done"
    );
}

#[test]
fn stdout_marker_classifies_by_marker_not_exit_code() {
    // 标记未见:即使退出码 0 也是 Dead
    let fixture = Fixture::fast_exit(FastExit {
        exit_code: Some(0),
        marker_seen: false,
        result_file_present: false,
    });
    let task = fixture.orch.create_task("t", "goal").unwrap();
    let snapshot = snapshot_with(
        RunMode::OneShot,
        serde_json::json!({ "completion": "stdout-marker", "stdout_marker": "DONE##" }),
    );
    fixture.create_ad_hoc_with(task.id, &snapshot).unwrap();
    assert!(wait_until(std::time::Duration::from_secs(5), || {
        status_of(&fixture.orch, task.id) == SessionStatus::Dead
    }));

    // 标记已见:即使退出码非 0 也是 Done(完成以标记为准)
    let fixture = Fixture::fast_exit(FastExit {
        exit_code: Some(1),
        marker_seen: true,
        result_file_present: false,
    });
    let task = fixture.orch.create_task("t", "goal").unwrap();
    fixture.create_ad_hoc_with(task.id, &snapshot).unwrap();
    assert!(wait_until(std::time::Duration::from_secs(5), || {
        status_of(&fixture.orch, task.id) == SessionStatus::Done
    }));
}

#[test]
fn result_file_classifies_done_only_when_present() {
    let fixture = Fixture::fast_exit(FastExit {
        exit_code: Some(0),
        marker_seen: false,
        result_file_present: true,
    });
    let task = fixture.orch.create_task("t", "goal").unwrap();
    let snapshot = snapshot_with(
        RunMode::OneShot,
        serde_json::json!({ "completion": "result-file", "result_file": "result.json" }),
    );
    fixture.create_ad_hoc_with(task.id, &snapshot).unwrap();
    assert!(wait_until(std::time::Duration::from_secs(5), || {
        status_of(&fixture.orch, task.id) == SessionStatus::Done
    }));
}

#[test]
fn late_exit_event_does_not_revive_settled_session() {
    let fixture = Fixture::memory();
    let task = fixture.orch.create_task("t", "goal").unwrap();
    let view = fixture.create_ad_hoc(task.id).unwrap();

    // 先人工提交 Handoff(终结为 Done),迟到的退出事件不得复活/改写
    let handoff = HandoffDraft {
        status: "completed".into(),
        summary: "人工收口".into(),
        ..Default::default()
    };
    let submitted = fixture
        .orch
        .submit_ad_hoc_handoff(view.id, &handoff)
        .unwrap();
    assert_eq!(submitted.status, SessionStatus::Done);
    let ended = submitted.ended_at.clone();

    fixture
        .orch
        .handle_ad_hoc_exit(view.id, Some(0), false, false);
    let after = fixture.orch.store.list_ad_hoc_sessions(task.id).unwrap()[0].clone();
    assert_eq!(after.status, SessionStatus::Done);
    assert_eq!(after.ended_at, ended);
    assert!(after.handoff.is_some());
}

#[test]
fn mark_launched_only_transitions_from_starting() {
    // 存储层:快速退出已终结的行,mark 不得覆盖为 Working
    let store = Store::memory().unwrap();
    let task = store.create_task("t", "goal").unwrap();
    let view = store.insert_ad_hoc_session(task.id, &snapshot()).unwrap();
    store
        .set_ad_hoc_status(view.id, SessionStatus::Dead)
        .unwrap();
    let marked = store.mark_ad_hoc_launched(view.id).unwrap().unwrap();
    assert_eq!(
        marked.status,
        SessionStatus::Dead,
        "不得把 Dead 覆盖为 Working"
    );
    assert!(marked.launched_at.is_none());

    // 正常路径:starting → working + launched_at
    let view2 = store.insert_ad_hoc_session(task.id, &snapshot()).unwrap();
    let marked2 = store.mark_ad_hoc_launched(view2.id).unwrap().unwrap();
    assert_eq!(marked2.status, SessionStatus::Working);
    assert!(marked2.launched_at.is_some());
}
