//! Event Hub 背压与统一快照路由的自动化测试。

use crate::project_overview::{HubCtx, ProjectOverviewHub, ProjectOverviewSnapshot};
use crate::runtime_host::SessionRegistry;
use mf_agent::config::Config;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::runtime::{AdHocLaunchSpec, LaunchSpec, RuntimeEvent, RuntimeHost};
use mf_agent::store::Store;
use mf_plugins::PluginRegistry;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct NoopHost;
impl RuntimeHost for NoopHost {
    fn launch_workflow(
        &self,
        _spec: mf_agent::runtime::WorkflowLaunchSpec,
        _events: crossbeam_channel::Sender<(i64, mf_agent::RuntimeEvent)>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn launch(&self, _spec: LaunchSpec, _events: crossbeam_channel::Sender<(i64, RuntimeEvent)>) {}
    fn launch_ad_hoc(&self, _spec: AdHocLaunchSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn send_prompt(
        &self,
        _run_handle: &str,
        _session_handle: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop_run(&self, _run_handle: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill_session(&self, _session_handle: &str) {}
    fn kill_ad_hoc(&self, _display_session_handle: &str) {}
    fn answer_question(&self, _run_handle: &str, _answer: &str) {}
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mf-hub-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start_orch(dir: &std::path::Path) -> Arc<Orchestrator> {
    let db_dir = dir.join(".mf-agent");
    std::fs::create_dir_all(&db_dir).unwrap();
    let store = Store::open(&db_dir.join("orchestration.db")).unwrap();
    Orchestrator::start(
        store,
        dir.to_path_buf(),
        Config::default(),
        Arc::new(NoopHost),
        Arc::new(RwLock::new(ProfileCatalog::default())),
        GlobalLimiter::new(4),
        "test-pipe".into(),
        Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
    )
    .unwrap()
}

fn make_hub() -> Arc<ProjectOverviewHub> {
    let config = Config::default();
    let skills = mf_skills::load_skills(None);
    let plugins: Arc<PluginRegistry> = PluginRegistry::load(&config, &skills).into();
    let catalog = Arc::new(RwLock::new(ProfileCatalog::default()));
    ProjectOverviewHub::new(Arc::new(HubCtx {
        registry: SessionRegistry::new(config),
        catalog,
        plugins,
        limiter: GlobalLimiter::new(4),
        keep_awake: Arc::new(crate::runtime_host::KeepAwake::new()),
        kernel: Arc::new(RwLock::new(None)),
    }))
}

/// 测试装配:AppCtx + CoreKernel tracer + 打开临时项目(全部经 tempfile,
/// 不触碰用户真实 ~/.monkeyfence)。
fn kernel_ctx(
    tmp: &std::path::Path,
) -> (
    Arc<crate::app_ctx::AppCtx>,
    std::path::PathBuf,
    Arc<Orchestrator>,
) {
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(
        mf_agent::CatalogStore::memory().expect("内存目录库初始化不可能失败"),
    );
    let service =
        mf_kernel::project_registry::ServiceStore::open(&tmp.join("service-v1.db")).unwrap();
    let (runtime, client) = mf_kernel::kernel::InProcessKernelRuntime::for_test(
        service,
        mf_kernel::command::ServiceIdempotencyKey::for_test(vec![0x61; 32]).unwrap(),
        mf_kernel::handles::ClientId::parse("overview-hub-client").unwrap(),
        mf_kernel::handles::Principal::parse("overview-hub-user").unwrap(),
    )
    .unwrap();
    ctx.install_kernel_tracer_for_tests(runtime, client);
    let root = tmp.join("project");
    std::fs::create_dir_all(&root).unwrap();
    let orch = ctx.open_project(root.clone()).unwrap();
    (ctx, root, orch)
}

/// 造一次工作流运行:失败根因 + 两个纯 blocked 后代(Kernel 只计直接原因)。
fn seed_failed_run_with_blocked_descendants(orch: &Arc<Orchestrator>) -> (i64, i64) {
    use mf_agent::pipeline::{PipelineDraft, SessionPolicy, StepDraft};
    let task = orch.store.create_task("运行", "goal").unwrap();
    orch.store
        .create_draft_revision(
            task.id,
            &PipelineDraft {
                steps: vec![
                    StepDraft {
                        key: "build".into(),
                        title: "构建".into(),
                        instructions: String::new(),
                        agent_profile: "inst".into(),
                        session_policy: SessionPolicy::Fresh,
                        deps: vec![],
                    },
                    StepDraft {
                        key: "test".into(),
                        title: "测试".into(),
                        instructions: String::new(),
                        agent_profile: "inst".into(),
                        session_policy: SessionPolicy::Fresh,
                        deps: vec!["build".into()],
                    },
                    StepDraft {
                        key: "report".into(),
                        title: "报告".into(),
                        instructions: String::new(),
                        agent_profile: "inst".into(),
                        session_policy: SessionPolicy::Fresh,
                        deps: vec!["build".into()],
                    },
                ],
            },
        )
        .unwrap();
    orch.store.activate_revision(task.id).unwrap();
    let steps = orch.store.task_steps(task.id).unwrap();
    let build = steps.iter().find(|s| s.step_key == "build").unwrap();
    orch.store
        .set_step_status(build.id, mf_agent::StepStatus::Failed)
        .unwrap();
    for step in steps.iter().filter(|s| s.step_key != "build") {
        orch.store
            .set_step_status(step.id, mf_agent::StepStatus::Blocked)
            .unwrap();
    }
    // 模拟 Orchestrator 状态机:直接原因出现 → 运行进入 Needs You
    orch.store
        .set_task_status(task.id, mf_agent::TaskStatus::NeedsYou)
        .unwrap();
    (task.id, build.id)
}

/// 事件驱动等待 revision ≥ min(带超时,不做无语义 sleep 断言)。
fn wait_revision(hub: &ProjectOverviewHub, min: u64, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if hub.current().revision >= min {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    hub.current().revision >= min
}

/// 超过 8192 个状态事件(bounded channel 容量)时,
/// drain 线程持续消费,创建方(调度侧)不会永久阻塞。
#[test]
fn scheduler_not_blocked_when_more_than_channel_capacity() {
    let dir = scratch("backpressure");
    let orch = start_orch(&dir);
    let hub = make_hub();
    hub.attach(dir.clone(), orch.clone());
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let worker = std::thread::spawn(move || {
        // 9000 > 8192:若无持续消费者,TaskUpdated 的阻塞 send 会在第 8193 个死锁
        for i in 0..9000 {
            orch.create_task(&format!("t{i}"), "g").unwrap();
        }
        let _ = done_tx.send(());
    });
    let finished = done_rx
        .recv_timeout(Duration::from_secs(60))
        .map(|_| true)
        .unwrap_or(false);
    assert!(finished, "9000 个状态事件未造成调度侧永久阻塞");
    worker.join().unwrap();
}

/// UI 暂停读取 snapshot 后恢复:最终 snapshot 与 Store 最新状态一致。
#[test]
fn snapshot_eventually_matches_store_after_ui_pause() {
    let dir = scratch("pause");
    let orch = start_orch(&dir);
    let hub = make_hub();
    hub.attach(dir.clone(), orch.clone());
    for i in 0..50 {
        orch.create_task(&format!("任务{i}"), "g").unwrap();
    }
    // 模拟 UI 暂停:一段时间不读 snapshot
    std::thread::sleep(Duration::from_millis(300));
    // 恢复读取:轮询 revision 直到包含全部任务(最终一致)
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        let snap: Arc<ProjectOverviewSnapshot> = hub.current();
        let total: usize = snap.projects.iter().map(|p| p.tasks.len()).sum();
        if total >= 50 {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(ok, "恢复读取后 snapshot 与 Store 最终一致");
    let in_store = orch.tasks().unwrap().len();
    let snap = hub.current();
    let total: usize = snap.projects.iter().map(|p| p.tasks.len()).sum();
    assert_eq!(total, in_store);
}

/// 关闭(detach)项目后不再出现在 snapshot。
#[test]
fn detached_project_disappears_from_snapshot() {
    let dir = scratch("detach");
    let orch = start_orch(&dir);
    let hub = make_hub();
    hub.attach(dir.clone(), orch);
    assert!(wait_revision(&hub, 1, Duration::from_secs(5)));
    let before_detach = hub.current().revision;
    hub.detach(&dir);
    assert!(wait_revision(
        &hub,
        before_detach + 1,
        Duration::from_secs(5)
    ));
    let snap = hub.current();
    assert!(
        !snap.projects.iter().any(|p| p.root == dir),
        "关闭的项目不得留在 snapshot"
    );
}

/// 双 Project 同 id 的 Task 仍按项目正确路由(snapshot 中各自归属)。
#[test]
fn same_task_ids_route_by_project() {
    let dir_a = scratch("route-a");
    let dir_b = scratch("route-b");
    let orch_a = start_orch(&dir_a);
    let orch_b = start_orch(&dir_b);
    let hub = make_hub();
    hub.attach(dir_a.clone(), orch_a.clone());
    hub.attach(dir_b.clone(), orch_b.clone());
    let t_a = orch_a.create_task("A1", "g").unwrap();
    let t_b = orch_b.create_task("B1", "g").unwrap();
    // SQLite 行号独立,两项目各自由 1 开始
    assert_eq!(t_a.id, t_b.id);
    assert!(wait_revision(&hub, 1, Duration::from_secs(5)));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snap = hub.current();
        let a = snap.projects.iter().find(|p| p.root == dir_a);
        let b = snap.projects.iter().find(|p| p.root == dir_b);
        if let (Some(a), Some(b)) = (a, b) {
            if a.tasks.iter().any(|t| t.task.id == t_a.id)
                && b.tasks.iter().any(|t| t.task.id == t_b.id)
            {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "同 id 任务按 ProjectId 路由失败"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn idle_hub_does_not_poll_or_advance_revision() {
    let dir = scratch("no-poll");
    let orch = start_orch(&dir);
    let hub = make_hub();
    hub.attach(dir, orch);
    assert!(wait_revision(&hub, 1, Duration::from_secs(5)));
    let revision = hub.current().revision;

    std::thread::sleep(Duration::from_millis(750));

    assert_eq!(
        hub.current().revision,
        revision,
        "无事件时不应退化为每 500ms 全库轮询"
    );
}

#[test]
fn continuous_events_have_bounded_snapshot_latency() {
    let dir = scratch("bounded-latency");
    let orch = start_orch(&dir);
    let hub = make_hub();
    hub.attach(dir, orch.clone());
    assert!(wait_revision(&hub, 1, Duration::from_secs(5)));
    let initial_revision = hub.current().revision;
    let producer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_millis(900);
        let mut i = 0;
        while std::time::Instant::now() < deadline {
            orch.create_task(&format!("continuous-{i}"), "g").unwrap();
            i += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    assert!(
        wait_revision(&hub, initial_revision + 1, Duration::from_millis(350)),
        "连续事件不能无限延后快照发布"
    );
    producer.join().unwrap();
}

#[test]
fn dropping_last_hub_reference_stops_background_ownership() {
    let hub = make_hub();
    let weak = Arc::downgrade(&hub);

    drop(hub);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline && weak.upgrade().is_some() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(weak.upgrade().is_none(), "rebuilder 线程不能永久强持有 Hub");
}

// ---------- Issue #26:总览读取迁到 Core Kernel Snapshot ----------

/// Core Workspace Snapshot 驱动总览重建:一次失败根因 + 两个纯 blocked
/// 后代 → 一个运行最多一项「需要你」,reason_count 只计直接原因,
/// focus 定位失败根因;卡片状态/标题等事实来自 Kernel 摘要。
#[test]
fn kernel_workspace_snapshot_feeds_attention_and_run_cards() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, root, orch) = kernel_ctx(tmp.path());
    let (task_id, build_step_id) = seed_failed_run_with_blocked_descendants(&orch);

    ctx.overview().request_refresh();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snap = ctx.overview().current();
        let attention = snap
            .attention_runs
            .iter()
            .find(|a| a.project_root == root && a.task_id == task_id);
        if let Some(attention) = attention {
            assert_eq!(
                attention.reason_count, 1,
                "纯 blocked 后代不计入 Kernel 原因数"
            );
            assert_eq!(attention.focus_step_id, Some(build_step_id));
            assert_eq!(snap.attention_run_count, 1, "一个运行最多贡献一个徽标");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Kernel Workspace 快照必须驱动「需要你」投影"
        );
        std::thread::sleep(Duration::from_millis(30));
    }

    // 运行卡片事实来自 Kernel 摘要(标题/状态/未读),身份 rowid 可接线
    let snap = ctx.overview().current();
    let project = snap
        .projects
        .iter()
        .find(|p| p.root == root)
        .expect("项目必须在快照中");
    let card = project
        .tasks
        .iter()
        .find(|card| card.task.id == task_id)
        .expect("工作流运行必须投影为任务卡片");
    assert_eq!(card.task.title, "运行");
    assert_eq!(card.task.status, mf_agent::TaskStatus::NeedsYou);
    assert_eq!(card.active_runs, 0);
    assert!(
        card.needs_you_reasons.iter().any(|r| r.contains("构建")),
        "Kernel 原因明细必须带步骤标题: {:?}",
        card.needs_you_reasons
    );
    ctx.try_close_project(&root).unwrap();
}

/// 原因消除后重算即消失(统一快照口径,无手工减计数)。
#[test]
fn kernel_attention_clears_after_reason_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, root, orch) = kernel_ctx(tmp.path());
    let (task_id, build_step_id) = seed_failed_run_with_blocked_descendants(&orch);

    // 等待初始徽标出现
    ctx.overview().request_refresh();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ctx
        .overview()
        .current()
        .attention_runs
        .iter()
        .any(|a| a.task_id == task_id)
    {
        assert!(std::time::Instant::now() < deadline, "初始徽标必须出现");
        std::thread::sleep(Duration::from_millis(30));
    }

    // 处理唯一直接原因:失败节点改为成功 + 任务收敛
    orch.store
        .set_step_status(build_step_id, mf_agent::StepStatus::Succeeded)
        .unwrap();
    for step in orch.store.task_steps(task_id).unwrap() {
        if step.step_key != "build" {
            orch.store
                .set_step_status(step.id, mf_agent::StepStatus::Succeeded)
                .unwrap();
        }
    }
    orch.store
        .set_task_status(task_id, mf_agent::TaskStatus::Succeeded)
        .unwrap();
    ctx.overview().request_refresh();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while ctx
        .overview()
        .current()
        .attention_runs
        .iter()
        .any(|a| a.task_id == task_id)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "唯一直接原因处理后徽标必须清零"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    ctx.try_close_project(&root).unwrap();
}

/// 无 Core tracer(测试回滚模式):Hub 回退旧 Store 扫描投影,行为不变。
#[test]
fn hub_without_kernel_source_falls_back_to_legacy_scan() {
    let ctx =
        crate::app_ctx::AppCtx::with_catalog_for_tests(mf_agent::CatalogStore::memory().unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let orch = ctx.open_project(root.clone()).unwrap();
    let (task_id, _) = seed_failed_run_with_blocked_descendants(&orch);

    ctx.overview().request_refresh();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snap = ctx.overview().current();
        if snap
            .attention_runs
            .iter()
            .any(|a| a.project_root == root && a.task_id == task_id)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "回退路径(旧扫描)必须继续产生「需要你」投影"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    ctx.try_close_project(&root).unwrap();
}

/// Kernel Snapshot reason → 文案映射(Issue #26):run 级
/// awaiting-outcome / interrupted 原因也必须带步骤标题呈现,
/// 措辞与 run_node_details 口径一致(风险2:判定为收口而非回归,
/// 此处钉住映射不回退)。
#[test]
fn kernel_reason_copy_maps_run_level_kinds_with_step_title() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, root, orch) = kernel_ctx(tmp.path());
    let task = orch.store.create_task("运行", "g").unwrap();
    orch.store
        .create_draft_revision(
            task.id,
            &mf_agent::pipeline::PipelineDraft {
                steps: vec![mf_agent::pipeline::StepDraft {
                    key: "build".into(),
                    title: "构建".into(),
                    instructions: String::new(),
                    agent_profile: "inst".into(),
                    session_policy: mf_agent::SessionPolicy::Fresh,
                    deps: vec![],
                }],
            },
        )
        .unwrap();
    orch.store.activate_revision(task.id).unwrap();
    let step = orch.store.task_steps(task.id).unwrap().remove(0);
    orch.store
        .set_step_status(step.id, mf_agent::StepStatus::Ready)
        .unwrap();
    let session = orch
        .store
        .create_session(None, "pty", "inst", "构建")
        .unwrap();
    let run = orch
        .store
        .dispatch_run(task.id, step.id, step.revision_id, session.id)
        .unwrap();

    let wait_for_reason = |fragment: &str| {
        ctx.overview().request_refresh();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let snap = ctx.overview().current();
            let card = snap
                .projects
                .iter()
                .find(|p| p.root == root)
                .and_then(|p| p.tasks.iter().find(|c| c.task.id == task.id));
            if let Some(card) = card {
                if card.needs_you_reasons.iter().any(|r| r.contains(fragment)) {
                    return;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Kernel 原因文案必须包含「{fragment}」"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    };

    // run 级 awaiting-outcome(进程退出未结算)
    orch.store
        .set_run_status(run.id, mf_agent::RunStatus::AwaitingOutcome)
        .unwrap();
    wait_for_reason("步骤「构建」等待人工结算");

    // run 级 interrupted(重启恢复)
    orch.store
        .set_run_status(run.id, mf_agent::RunStatus::Interrupted)
        .unwrap();
    wait_for_reason("步骤「构建」运行中断");

    ctx.try_close_project(&root).unwrap();
}
