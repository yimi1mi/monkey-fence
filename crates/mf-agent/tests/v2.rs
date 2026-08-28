//! v2 验收测试:数据库迁移、结算令牌、崩溃恢复、调度规则、多项目隔离。

use crossbeam_channel::Sender;
use mf_agent::config::Config;
use mf_agent::model::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::pipeline::{PipelineDraft, ProfileIndex, SessionPolicy, StepDraft};
use mf_agent::runtime::{
    AdHocLaunchSpec, AgentProfileSpec, LaunchSpec, RuntimeEvent, RuntimeHost, RuntimeKind,
};
use mf_agent::store::Store;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------- 辅助 ----------

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
    /// 能力令牌(全局唯一)→ 事件通道;按 run_id 索引会在多项目时碰撞
    senders: Mutex<HashMap<String, Sender<(i64, RuntimeEvent)>>>,
    max_concurrent: AtomicUsize,
    in_flight: AtomicUsize,
    stopped: Mutex<Vec<i64>>,
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
    fn emit(&self, run_id: i64, ev: RuntimeEvent) {
        let token = self
            .launches
            .lock()
            .iter()
            .find(|s| s.run_id == run_id)
            .map(|s| s.capability_token.clone());
        if let Some(t) = token {
            self.emit_token(&t, ev);
        }
    }
    fn launch_count(&self) -> usize {
        self.launches.lock().len()
    }
    fn spec_of(&self, i: usize) -> LaunchSpec {
        self.launches.lock()[i].clone()
    }
    fn run_id_of(&self, i: usize) -> i64 {
        self.launches.lock()[i].run_id
    }
}

impl RuntimeHost for MockHost {
    fn launch(&self, spec: LaunchSpec, events: Sender<(i64, RuntimeEvent)>) {
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
    fn stop_run(&self, _project: &str, run_id: i64) {
        self.stopped.lock().push(run_id);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
    fn kill_session(&self, _project: &str, _session_id: i64) {}
    fn kill_ad_hoc(&self, _project: &str, _session_id: i64) {}
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

fn draft(steps: Vec<StepDraft>) -> PipelineDraft {
    PipelineDraft { steps }
}

fn start_orch(dir: &std::path::Path, host: Arc<MockHost>) -> (Arc<Orchestrator>, PathBuf) {
    let db = dir.join(".mf-agent").join("orchestration.db");
    let store = Store::open(&db).unwrap();
    let orch = Orchestrator::start(
        store,
        dir.to_path_buf(),
        Config::default(),
        host,
        mock_profiles(),
        GlobalLimiter::new(4),
        "\\\\.\\pipe\\test".into(),
    )
    .unwrap();
    (orch, db)
}

#[test]
fn two_project_dbs_isolated() {
    let t1 = tempfile::tempdir().unwrap();
    let t2 = tempfile::tempdir().unwrap();
    let s1 = Store::open(&t1.path().join(".mf-agent/orchestration.db")).unwrap();
    let s2 = Store::open(&t2.path().join(".mf-agent/orchestration.db")).unwrap();
    let a = s1.create_task("项目1任务", "").unwrap();
    let b = s2.create_task("项目2任务", "").unwrap();
    assert_eq!(s1.list_tasks(true).unwrap().len(), 1);
    assert_eq!(s2.list_tasks(true).unwrap().len(), 1);
    assert_ne!(a.title, b.title);
    assert_eq!(s1.list_tasks(true).unwrap()[0].title, "项目1任务");
    assert_eq!(s2.list_tasks(true).unwrap()[0].title, "项目2任务");
    // s1 的 session 不会出现在 s2
    let ses = s1.create_session(Some("k"), "http", "mock", "s1").unwrap();
    assert!(s2.find_reusable_session("k", "mock").unwrap().is_none());
    assert!(s1.find_reusable_session("k", "mock").unwrap().is_some());
    assert_eq!(ses.runtime, "http");
}

#[test]
fn interrupted_recovery_on_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("orchestration.db");
    let token;
    {
        let store = Store::open(&db).unwrap();
        let task = store.create_task("崩溃任务", "").unwrap();
        let rev = store
            .create_draft_revision(task.id, &draft(vec![step("a", &[])]))
            .unwrap();
        store.activate_revision(task.id).unwrap();
        let steps = store.revision_steps(rev.id).unwrap();
        store
            .set_step_status(steps[0].id, StepStatus::Running)
            .unwrap();
        let run = store
            .create_run(task.id, steps[0].id, rev.id, None)
            .unwrap();
        token = run.capability_token.clone();
        // 模拟崩溃:不做清理直接丢弃(进程内 reopen 等价)
    }
    {
        let store = Store::open(&db).unwrap(); // Orchestrator::start 内部也会调用 recover
        let recovered = store.recover_interrupted().unwrap();
        assert_eq!(
            recovered.len(),
            1,
            "崩溃遗留的 running run 应被恢复为 interrupted"
        );
        assert!(
            store.recover_interrupted().unwrap().is_empty(),
            "第二次恢复应为 no-op"
        );
        let run = store.run_by_token(&token).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Interrupted);
        let task = store.task_view(run.task_id).unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::NeedsYou);
        assert!(task.unread);
    }
}

// ---------- 结算令牌 ----------

#[test]
fn settlement_token_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("orchestration.db")).unwrap();
    let task = store.create_task("t", "").unwrap();
    let rev = store
        .create_draft_revision(task.id, &draft(vec![step("a", &[])]))
        .unwrap();
    store.activate_revision(task.id).unwrap();
    let s = store.revision_steps(rev.id).unwrap()[0].clone();
    let run = store.create_run(task.id, s.id, rev.id, None).unwrap();

    // 错误令牌
    assert!(matches!(
        store.settle_run_by_token(
            "mft_wrong",
            Settlement::Complete {
                summary: "x".into()
            }
        ),
        Err(SettleError::UnknownToken)
    ));
    // 正确结算
    let ok = store.settle_run_by_token(
        &run.capability_token,
        Settlement::Complete {
            summary: "done".into(),
        },
    );
    assert!(matches!(ok, Ok((_, SettleOutcome::Applied))));
    // 相同结算重复提交:幂等
    let dup = store.settle_run_by_token(
        &run.capability_token,
        Settlement::Complete {
            summary: "done again".into(),
        },
    );
    assert!(matches!(dup, Ok((_, SettleOutcome::AlreadyApplied))));
    // 冲突结算:拒绝
    let conflict = store.settle_run_by_token(
        &run.capability_token,
        Settlement::Fail {
            reason: "no".into(),
        },
    );
    assert!(matches!(conflict, Err(SettleError::Conflict { .. })));
    // 结算后 step 成功
    let s2 = store.step_view(s.id).unwrap().unwrap();
    assert_eq!(s2.status, StepStatus::Succeeded);
    assert_eq!(s2.result.as_deref(), Some("done"));
}

#[test]
fn settlement_rejected_for_inactive_run() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("orchestration.db")).unwrap();
    let task = store.create_task("t", "").unwrap();
    let rev = store
        .create_draft_revision(task.id, &draft(vec![step("a", &[])]))
        .unwrap();
    store.activate_revision(task.id).unwrap();
    let s = store.revision_steps(rev.id).unwrap()[0].clone();
    let run = store.create_run(task.id, s.id, rev.id, None).unwrap();
    store.set_run_status(run.id, RunStatus::Cancelled).unwrap();
    assert!(matches!(
        store.settle_run_by_token(
            &run.capability_token,
            Settlement::Complete {
                summary: "x".into()
            }
        ),
        Err(SettleError::RunNotActive(_))
    ));
}

// ---------- Revision 编辑规则 ----------

#[test]
fn edit_rules_only_unstarted_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("orchestration.db")).unwrap();
    let task = store.create_task("t", "").unwrap();
    store
        .create_draft_revision(task.id, &draft(vec![step("a", &[]), step("b", &["a"])]))
        .unwrap();
    store.activate_revision(task.id).unwrap();
    let steps = store.task_steps(task.id).unwrap();
    let a = steps.iter().find(|s| s.step_key == "a").unwrap();
    // 模拟 a 已启动
    store.bump_step_attempts(a.id).unwrap();

    // 修改已启动的 a → 拒绝
    let mut edited = draft(vec![
        StepDraft {
            title: "改了".into(),
            ..step("a", &[])
        },
        step("b", &["a"]),
    ]);
    assert!(store.save_edited_revision(task.id, &edited).is_err());

    // 只改未启动的 b → 通过,产生新 revision
    edited = draft(vec![
        step("a", &[]),
        StepDraft {
            title: "b 新标题".into(),
            ..step("b", &["a"])
        },
    ]);
    let rev = store.save_edited_revision(task.id, &edited).unwrap();
    let new_steps = store.revision_steps(rev.id).unwrap();
    assert_eq!(new_steps.len(), 2);
    assert!(new_steps.iter().any(|s| s.title == "b 新标题"));
    // a 的历史状态被原样带入
    let new_a = new_steps.iter().find(|s| s.step_key == "a").unwrap();
    assert_eq!(new_a.attempts, 1);
}

// ---------- 调度器 ----------

#[test]
fn output_event_emits_unread_updates_for_snapshot_consumers() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("output-unread", "").unwrap();
    orch.save_pipeline(task.id, &draft(vec![step("a", &[])]))
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));
    let run = orch
        .runs_of_task(task.id)
        .unwrap()
        .into_iter()
        .find(|run| run.status == RunStatus::Running)
        .unwrap();
    let session_id = run.session_id.unwrap();
    while orch.events_rx.try_recv().is_ok() {}

    host.emit(run.id, RuntimeEvent::Output);

    let mut saw_task = false;
    let mut saw_session = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !(saw_task && saw_session) {
        if let Ok(event) = orch.events_rx.recv_timeout(Duration::from_millis(100)) {
            match event {
                SchedulerEvent::TaskUpdated(updated) if updated.id == task.id && updated.unread => {
                    saw_task = true;
                }
                SchedulerEvent::SessionUpdated(updated)
                    if updated.id == session_id && updated.unread =>
                {
                    saw_session = true;
                }
                _ => {}
            }
        }
    }
    assert!(saw_task, "Output 必须发布 unread TaskUpdated");
    assert!(saw_session, "Output 必须发布 unread SessionUpdated");
    orch.stop();
}

#[test]
fn dispatch_and_settlement_unlocks_downstream() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("DAG", "").unwrap();
    orch.save_pipeline(task.id, &draft(vec![step("a", &[]), step("b", &["a"])]))
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();

    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));
    let run_a = host.run_id_of(0);
    // 显式结算 a 成功 → 下游 b 解锁并被派发
    orch.settle_by_token(
        &host.spec_of(0).capability_token,
        Settlement::Complete {
            summary: "a done".into(),
        },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 2));
    // done 无结算不能自动成功:b 上报 done 但不结算 → awaiting-outcome + needs-you
    host.emit(run_a, RuntimeEvent::AgentState(AgentState::Done)); // (a 已结算,事件应为幂等 no-op)
    orch.settle_run(
        host.run_id_of(1),
        Settlement::Complete {
            summary: "b done".into(),
        },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        orch.store
            .task_view(task.id)
            .unwrap()
            .map(|t| t.status == TaskStatus::Succeeded)
            .unwrap_or(false)
    }));
    orch.stop();
}

#[test]
fn done_without_settlement_goes_needs_you_not_success() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("needs-you", "").unwrap();
    orch.save_pipeline(task.id, &draft(vec![step("a", &[])]))
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));

    let run_id = host.run_id_of(0);
    host.emit(run_id, RuntimeEvent::AgentState(AgentState::Done));
    assert!(wait_until(Duration::from_secs(5), || {
        matches!(
            orch.store.run_view(run_id).unwrap().map(|r| r.status),
            Some(RunStatus::AwaitingOutcome)
        )
    }));
    let t = orch.store.task_view(task.id).unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::NeedsYou);
    let s = orch.store.task_steps(task.id).unwrap()[0].clone();
    assert_eq!(s.status, StepStatus::AwaitingOutcome);
    // tui-idle 也不能结算
    host.emit(run_id, RuntimeEvent::TuiIdle(true));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        orch.store.run_view(run_id).unwrap().unwrap().status,
        RunStatus::AwaitingOutcome
    );

    // 手工判定成功 → 任务成功
    orch.settle_run(
        run_id,
        Settlement::Complete {
            summary: "人工判定".into(),
        },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        orch.store
            .task_view(task.id)
            .unwrap()
            .map(|t| t.status == TaskStatus::Succeeded)
            .unwrap_or(false)
    }));
    orch.stop();
}

#[test]
fn failure_blocks_only_descendants() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    // a → b;c 独立
    let task = orch.create_task("分支", "").unwrap();
    orch.save_pipeline(
        task.id,
        &draft(vec![step("a", &[]), step("b", &["a"]), step("c", &[])]),
    )
    .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 2)); // a + c 并行

    // a 失败 → b blocked,c 继续
    orch.settle_by_token(
        &host.spec_of(0).capability_token,
        Settlement::Fail {
            reason: "挂了".into(),
        },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        orch.store
            .task_steps(task.id)
            .unwrap()
            .iter()
            .any(|s| s.status == StepStatus::Blocked)
    }));
    let steps = orch.store.task_steps(task.id).unwrap();
    let c = steps.iter().find(|s| s.step_key == "c").unwrap();
    assert_eq!(c.status, StepStatus::Running); // 独立分支继续运行

    // c 成功后:任务 needs-you(b blocked 待人工处理)
    let c_run = host
        .launches
        .lock()
        .iter()
        .find(|s| s.step_title == "step c")
        .unwrap()
        .run_id;
    orch.settle_run(
        c_run,
        Settlement::Complete {
            summary: "c ok".into(),
        },
    )
    .unwrap();
    let t = orch.store.task_view(task.id).unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::NeedsYou);

    // 人工跳过 b(必须确认)→ 任务收敛为 failed
    let b = steps.iter().find(|s| s.step_key == "b").unwrap().clone();
    assert!(orch.skip_step(b.id, false).is_err(), "跳过必须人工确认");
    orch.skip_step(b.id, true).unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        orch.store
            .task_view(task.id)
            .unwrap()
            .map(|t| t.status == TaskStatus::Failed)
            .unwrap_or(false)
    }));
    orch.stop();
}

#[test]
fn parallel_branches_run_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("并行", "").unwrap();
    orch.save_pipeline(
        task.id,
        &draft(vec![
            step("a", &[]),
            step("b", &[]),
            step("c", &[]),
            step("d", &[]),
        ]),
    )
    .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    // per_project 默认 2:先 2 并发
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 2));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(host.launch_count(), 2, "每项目并发上限 2");
    assert_eq!(host.max_concurrent.load(Ordering::SeqCst), 2);
    // 释放一个 → 再派发一个
    orch.settle_by_token(
        &host.spec_of(0).capability_token,
        Settlement::Complete { summary: "".into() },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 3));
    orch.stop();
}

#[test]
fn same_session_key_serialized() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("串行", "").unwrap();
    let mut s1 = step("s1", &[]);
    s1.session_policy = SessionPolicy::Reuse {
        key: "shared".into(),
    };
    let mut s2 = step("s2", &["s1"]);
    s2.session_policy = SessionPolicy::Reuse {
        key: "shared".into(),
    };
    // s3 无依赖但同 key:校验应拒绝(与 s1 无顺序)
    let mut s3 = step("s3", &[]);
    s3.session_policy = SessionPolicy::Reuse {
        key: "shared".into(),
    };
    let bad = draft(vec![s1.clone(), s2.clone(), s3]);
    let bad = {
        // s3 与 s2 无依赖 → 并行 reuse → 拒绝
        let mut b = bad;
        b.steps[2].deps = vec![];
        b
    };
    assert!(
        orch.save_pipeline(task.id, &bad).is_err(),
        "无顺序的同 key 必须被校验拒绝"
    );

    orch.save_pipeline(task.id, &draft(vec![s1, s2])).unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(host.launch_count(), 1, "s2 依赖 s1,尚未就绪");
    // s1 结算 → s2 才派发(天然串行)
    orch.settle_by_token(
        &host.spec_of(0).capability_token,
        Settlement::Complete { summary: "".into() },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 2));
    // s2 复用 s1 的 session
    assert_eq!(host.spec_of(1).session_id, host.spec_of(0).session_id);
    orch.stop();
}

#[test]
fn pause_required_before_editing_running_task() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("暂停编辑", "").unwrap();
    orch.save_pipeline(task.id, &draft(vec![step("a", &[]), step("b", &["a"])]))
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));

    // 运行中:拒绝编辑
    let edited = draft(vec![
        step("a", &[]),
        StepDraft {
            title: "b 改".into(),
            ..step("b", &["a"])
        },
    ]);
    assert!(
        orch.save_pipeline(task.id, &edited).is_err(),
        "运行中修改必须先暂停"
    );

    // 暂停后:未启动的 b 可改;已启动的 a 不可改
    orch.pause_task(task.id).unwrap();
    assert!(orch.save_pipeline(task.id, &edited).is_ok());
    let bad = draft(vec![
        StepDraft {
            title: "a 改".into(),
            ..step("a", &[])
        },
        step("b", &["a"]),
    ]);
    assert!(
        orch.save_pipeline(task.id, &bad).is_err(),
        "已启动节点不可修改"
    );
    orch.stop();
}

#[test]
fn retry_creates_new_session_for_fresh_step() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("重试", "").unwrap();
    orch.save_pipeline(task.id, &draft(vec![step("a", &[]), step("b", &["a"])]))
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));
    let first_session = host.spec_of(0).session_id;
    orch.settle_by_token(
        &host.spec_of(0).capability_token,
        Settlement::Fail {
            reason: "重试我".into(),
        },
    )
    .unwrap();
    // 独立 Step 重试:创建新会话
    let a = orch.store.task_steps(task.id).unwrap()[0].clone();
    orch.retry_step(a.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 2));
    assert_ne!(
        host.spec_of(1).session_id,
        first_session,
        "fresh 策略重试应新建会话"
    );
    orch.stop();
}

#[test]
fn planner_cannot_bypass_confirmation() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("Planner", "").unwrap();
    orch.planner_propose(task.id, &draft(vec![step("a", &[])]))
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(host.launch_count(), 0, "Planner 提案不得直接启动");
    let t = orch.store.task_view(task.id).unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Draft);
    // 用户确认后才运行
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));
    orch.stop();
}

#[test]
fn http_runtime_settles_directly() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("HTTP", "").unwrap();
    orch.save_pipeline(task.id, &draft(vec![step("a", &[])]))
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));
    let run_id = host.run_id_of(0);
    // 结构化 API Runtime 直接提交结算事件
    host.emit(
        run_id,
        RuntimeEvent::Settled(Settlement::Complete {
            summary: "api done".into(),
        }),
    );
    assert!(wait_until(Duration::from_secs(5), || {
        orch.store
            .task_view(task.id)
            .unwrap()
            .map(|t| t.status == TaskStatus::Succeeded)
            .unwrap_or(false)
    }));
    orch.stop();
}

#[test]
fn exited_without_settlement_needs_you() {
    let tmp = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let (orch, _) = start_orch(tmp.path(), host.clone());
    let task = orch.create_task("退出", "").unwrap();
    orch.save_pipeline(task.id, &draft(vec![step("a", &[])]))
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 1));
    let run_id = host.run_id_of(0);
    host.emit(run_id, RuntimeEvent::Exited { code: Some(0) });
    assert!(wait_until(Duration::from_secs(5), || {
        matches!(
            orch.store.run_view(run_id).unwrap().map(|r| r.status),
            Some(RunStatus::AwaitingOutcome)
        )
    }));
    let t = orch.store.task_view(task.id).unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::NeedsYou);
    // 用户继续发送提示 → 回到 running
    orch.send_prompt(run_id, "继续修改").unwrap();
    assert_eq!(
        orch.store.run_view(run_id).unwrap().unwrap().status,
        RunStatus::Running
    );
    orch.stop();
}

// ---------- 两项目端到端 ----------

/// 验收 E2E:同时打开两个项目 → 各建任务 → 并行运行 → 汇总/隔离 →
/// 未结算进入需要你 → 手工结算后下游继续 → 历史仍可查看。
#[test]
fn two_projects_end_to_end() {
    let t1 = tempfile::tempdir().unwrap();
    let t2 = tempfile::tempdir().unwrap();
    let host: Arc<MockHost> = Arc::new(Default::default());
    let limiter = GlobalLimiter::new(4);
    let profiles = mock_profiles();
    let mut orchs = Vec::new();
    for dir in [t1.path(), t2.path()] {
        let store = Store::open(&dir.join(".mf-agent/orchestration.db")).unwrap();
        let orch = Orchestrator::start(
            store,
            dir.to_path_buf(),
            Config::default(),
            host.clone(),
            profiles.clone(),
            limiter.clone(),
            r"\.\pipe	est-e2e".into(),
        )
        .unwrap();
        orchs.push(orch);
    }
    let (o1, o2) = (&orchs[0], &orchs[1]);

    // 两个项目分别创建任务;A 手工建 DAG,B 由 Planner 生成
    let task_a = o1.create_task("项目A:手工 DAG", "").unwrap();
    o1.save_pipeline(
        task_a.id,
        &draft(vec![step("a1", &[]), step("a2", &["a1"])]),
    )
    .unwrap();
    o1.confirm_and_run(task_a.id).unwrap();

    let task_b = o2.create_task("项目B:Planner 生成", "").unwrap();
    o2.planner_propose(
        task_b.id,
        &draft(vec![step("b1", &[]), step("b2", &["b1"])]),
    )
    .unwrap();
    assert_eq!(host.launch_count(), 0, "Planner 提案不得直接启动");
    o2.confirm_and_run(task_b.id).unwrap();

    // 并行运行:两个项目各派发首步
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 2));
    // 数据库互不污染
    assert_eq!(o1.tasks().unwrap().len(), 1);
    assert_eq!(o2.tasks().unwrap().len(), 1);

    // 全局汇总:两个项目各有 1 个活动 run
    let total_active: usize = orchs
        .iter()
        .map(|o| o.store.running_runs().unwrap().len())
        .sum();
    assert_eq!(total_active, 2);

    // 未结算(done 无结算)进入需要你:a1 上报 done 不结算
    let a1_spec = {
        let launches = host.launches.lock();
        launches
            .iter()
            .find(|s| s.step_title == "step a1")
            .unwrap()
            .clone()
    };
    let a1_run = a1_spec.run_id;
    host.emit_token(
        &a1_spec.capability_token,
        RuntimeEvent::AgentState(AgentState::Done),
    );
    assert!(
        wait_until(Duration::from_secs(5), || {
            matches!(
                o1.store.run_view(a1_run).unwrap().map(|r| r.status),
                Some(RunStatus::AwaitingOutcome)
            )
        }),
        "a1 未进入 awaiting-outcome"
    );
    let ta = o1.store.task_view(task_a.id).unwrap().unwrap();
    assert_eq!(ta.status, TaskStatus::NeedsYou);

    // 手工结算成功后下游继续(a2 派发)
    o1.settle_run(
        a1_run,
        Settlement::Complete {
            summary: "人工判定".into(),
        },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 3));
    let a2_run = host
        .launches
        .lock()
        .iter()
        .find(|s| s.step_title == "step a2")
        .unwrap()
        .run_id;
    o1.settle_run(
        a2_run,
        Settlement::Complete {
            summary: "a2".into(),
        },
    )
    .unwrap();

    // B 项目正常结算
    let b1_run = host
        .launches
        .lock()
        .iter()
        .find(|s| s.step_title == "step b1")
        .unwrap()
        .run_id;
    o2.settle_by_token(
        &host
            .launches
            .lock()
            .iter()
            .find(|s| s.step_title == "step b1")
            .unwrap()
            .capability_token,
        Settlement::Complete {
            summary: "b1".into(),
        },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(5), || host.launch_count() == 4));
    let b2_run = host
        .launches
        .lock()
        .iter()
        .find(|s| s.step_title == "step b2")
        .unwrap()
        .run_id;
    o2.settle_run(
        b2_run,
        Settlement::Complete {
            summary: "b2".into(),
        },
    )
    .unwrap();

    // 双双收敛;历史仍可查看
    assert!(wait_until(Duration::from_secs(5), || {
        o1.store
            .task_view(task_a.id)
            .unwrap()
            .map(|t| t.status == TaskStatus::Succeeded)
            .unwrap_or(false)
            && o2
                .store
                .task_view(task_b.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::Succeeded)
                .unwrap_or(false)
    }));
    assert!(o1.runs_of_task(task_a.id).unwrap().len() >= 2, "A 历史保留");
    assert!(o2.runs_of_task(task_b.id).unwrap().len() >= 2, "B 历史保留");
    for o in &orchs {
        o.stop();
    }
}
