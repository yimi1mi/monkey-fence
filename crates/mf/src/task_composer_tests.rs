//! Task Composer 的自动化测试(显式项目归属 + 未读幂等)。
//! 独立文件:task_composer 模块的 GPUI 宏链很深,内联 #[test] 展开超递归预算。

use crate::project_context::ActivationTarget;
use crate::task_composer::TaskComposerState;
use mf_agent::config::Config;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::runtime::{AdHocLaunchSpec, LaunchSpec, RuntimeEvent, RuntimeHost};
use mf_agent::store::Store;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

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

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mf-composer-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fake_projects() -> Vec<(PathBuf, String)> {
    vec![
        (PathBuf::from(r"C:\proj\a"), "a".into()),
        (PathBuf::from(r"C:\proj\b"), "b".into()),
    ]
}

#[test]
fn cannot_submit_without_project() {
    let mut c = TaskComposerState::new(Vec::new(), None);
    c.set_title("t");
    assert!(!c.can_submit(), "无当前项目时不能提交");
}

#[test]
fn no_current_project_does_not_fall_back_to_first_project() {
    let mut c = TaskComposerState::new(fake_projects(), None);
    c.set_title("t");

    assert_eq!(c.selected_project(), None, "项目归属必须由用户显式选择");
    assert!(!c.can_submit(), "不能静默使用项目列表第一项");

    c.select_next_project();
    assert_eq!(
        c.selected_project(),
        Some(&PathBuf::from(r"C:\proj\a")),
        "显式选择后才允许使用第一项"
    );
    assert!(c.can_submit());
}

#[test]
fn cannot_submit_without_title_or_goal() {
    let mut c = TaskComposerState::new(fake_projects(), None);
    assert!(!c.can_submit());
    c.select_next_project();
    c.set_title("t");
    assert!(c.can_submit(), "goal 默认跟随 title");
    let mut c2 = TaskComposerState::new(fake_projects(), None);
    c2.select_next_project();
    c2.set_title("t");
    c2.set_goal("");
    assert!(!c2.can_submit(), "goal 清空后不能提交");
}

#[test]
fn goal_follows_title_until_edited() {
    let mut c = TaskComposerState::new(fake_projects(), None);
    c.set_title("第一个目标");
    assert_eq!(c.goal(), "第一个目标");
    c.set_goal("自定义目标");
    c.set_title("改标题");
    assert_eq!(c.goal(), "自定义目标", "手工编辑后不再跟随");
}

#[test]
fn default_project_is_current() {
    let c = TaskComposerState::new(fake_projects(), Some(&PathBuf::from(r"C:\proj\b")));
    assert_eq!(c.selected_project(), Some(&PathBuf::from(r"C:\proj\b")));
}

/// 两项目存在时,当前任务属于 A 也不影响 Composer 显式选择 B;
/// 任务只写入 B 的数据库,并返回 B + 新 task id 的 ActivationTarget。
#[test]
fn submit_creates_in_explicitly_selected_project_b() {
    let dir_a = scratch("a");
    let dir_b = scratch("b");
    let orch_a = start_orch(&dir_a);
    let orch_b = start_orch(&dir_b);
    // A 中已有任务(模拟用户当前停在 A)
    orch_a.create_task("A 任务", "A 目标").unwrap();

    let projs = vec![(dir_a.clone(), "a".into()), (dir_b.clone(), "b".into())];
    let mut c = TaskComposerState::new(projs, Some(&dir_a));
    c.select_next_project(); // a → b(显式选择,不受当前任务属于 A 影响)
    c.set_title("B 的新任务");
    assert!(c.can_submit());
    let target = c
        .submit(|root| {
            if root == &dir_a {
                Some(orch_a.clone())
            } else if root == &dir_b {
                Some(orch_b.clone())
            } else {
                None
            }
        })
        .expect("提交成功");

    match &target {
        ActivationTarget::Task { project, task_id } => {
            let expected_b = crate::project_context::normalize_project_path(&dir_b).0;
            assert_eq!(*project, expected_b, "创建成功后激活 B");
            assert!(
                orch_b.task_detail(*task_id).unwrap().is_some(),
                "任务写入 B"
            );
            assert_eq!(orch_a.tasks().unwrap().len(), 1, "A 数据库不受影响");
            assert_eq!(orch_b.tasks().unwrap().len(), 1);
        }
        other => panic!("期望 Task 激活,得到 {other:?}"),
    }
}

/// 打开 Task 清除 unread;重复调用幂等。
#[test]
fn mark_task_read_is_idempotent() {
    let dir = scratch("readtask");
    let orch = start_orch(&dir);
    let t = orch.create_task("t", "g").unwrap();
    orch.store.set_task_unread(t.id, true).unwrap();
    orch.mark_task_read(t.id).unwrap();
    assert!(!orch.tasks().unwrap()[0].unread);
    orch.mark_task_read(t.id).unwrap();
    orch.mark_task_read(t.id).unwrap();
    assert!(!orch.tasks().unwrap()[0].unread, "重复调用幂等");
}

/// 打开 Agent 卡片清除 session unread;重复调用幂等。
#[test]
fn mark_session_read_is_idempotent() {
    let dir = scratch("readsess");
    let orch = start_orch(&dir);
    let s = orch
        .store
        .create_session(None, "http", "mock", "s")
        .unwrap();
    orch.store.set_session_unread(s.id, true).unwrap();
    while orch.events_rx.try_recv().is_ok() {}
    orch.mark_session_read(s.id).unwrap();
    let event = orch
        .events_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("mark_session_read 必须发布 SessionUpdated");
    assert!(matches!(
        event,
        mf_agent::model::SchedulerEvent::SessionUpdated(ref updated)
            if updated.id == s.id && !updated.unread
    ));
    orch.mark_session_read(s.id).unwrap();
    assert!(!orch.sessions().unwrap()[0].unread, "重复调用幂等");
}
