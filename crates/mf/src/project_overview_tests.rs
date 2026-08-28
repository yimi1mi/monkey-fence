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
    fn launch(&self, _spec: LaunchSpec, _events: crossbeam_channel::Sender<(i64, RuntimeEvent)>) {}
    fn launch_ad_hoc(&self, _spec: AdHocLaunchSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn send_prompt(&self, _p: &str, _r: i64, _s: i64, _t: &str) {}
    fn stop_run(&self, _p: &str, _r: i64) {}
    fn kill_session(&self, _p: &str, _s: i64) {}
    fn kill_ad_hoc(&self, _p: &str, _s: i64) {}
    fn answer_question(&self, _p: &str, _r: i64, _a: &str) {}
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
    }))
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
