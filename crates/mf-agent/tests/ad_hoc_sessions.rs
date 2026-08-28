//! 离散 CLI 会话(Ad-hoc):挂在 Task 下但不属于 Pipeline Revision,
//! 没有 Step / Agent Run,绝不改变 Task 状态(设计 §4.7 / §10)。

use crossbeam_channel::Sender;
use mf_agent::agent_instance::AgentInstanceSnapshot;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::runtime::{AdHocLaunchSpec, LaunchSpec, RuntimeEvent, RuntimeHost};
use mf_agent::store::Store;
use mf_agent::{HandoffDraft, RunMode, SessionStatus, TaskStatus};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// ---------- 测试宿主:记录离散启动,不真正拉进程 ----------

#[derive(Default)]
struct MockHost {
    ad_hoc_launches: Mutex<Vec<AdHocLaunchSpec>>,
}

impl RuntimeHost for MockHost {
    fn launch(&self, _spec: LaunchSpec, _events: Sender<(i64, RuntimeEvent)>) {}
    fn send_prompt(&self, _project: &str, _run_id: i64, _session_id: i64, _text: &str) {}
    fn stop_run(&self, _project: &str, _run_id: i64) {}
    fn kill_session(&self, _project: &str, _session_id: i64) {}
    fn answer_question(&self, _project: &str, _run_id: i64, _answer: &str) {}
    fn launch_ad_hoc(&self, spec: AdHocLaunchSpec) {
        self.ad_hoc_launches.lock().push(spec);
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
        Self::with_store(store)
    }

    fn file(path: &std::path::Path) -> Fixture {
        let store = Store::open(path).unwrap();
        Self::with_store(store)
    }

    fn with_store(store: Arc<Store>) -> Fixture {
        let host = Arc::new(MockHost::default());
        let orch = Orchestrator::start(
            store,
            PathBuf::from("."),
            mf_agent::Config::default(),
            host.clone(),
            Arc::new(parking_lot::RwLock::new(ProfileCatalog::default())),
            GlobalLimiter::new(4),
            "test-pipe".into(),
        )
        .unwrap();
        Fixture { orch, host }
    }

    fn create_ad_hoc(&self, task_id: i64) -> anyhow::Result<mf_agent::AdHocSessionView> {
        self.orch
            .create_ad_hoc_session(task_id, &snapshot(), RunMode::Interactive)
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
    assert_eq!(launches[0].profile.command, "agent.exe");
    assert_eq!(launches[0].profile.args, vec!["--chat".to_string()]);
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
        .create_ad_hoc_session(999, &snapshot(), RunMode::Interactive)
        .is_err());
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

#[test]
fn fixture_store_helper_exists() {
    // 编译期确认:fixture.store 计划式用法(Orchestrator::store 公有)
    let fixture = Fixture::memory();
    let task = fixture.orch.store.create_task("t", "goal").unwrap();
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
    let _view: Option<mf_agent::AdHocSessionView> =
        fixture.orch.store.ad_hoc_session_view(1).unwrap();
    let _events: HashMap<(), ()> = HashMap::new();
}
