//! Task Composer 的自动化测试(显式项目归属 + 未读幂等)。
//! 独立文件:task_composer 模块的 GPUI 宏链很深,内联 #[test] 展开超递归预算。

use crate::project_context::ActivationTarget;
use crate::task_composer::{TaskComposerState, WorkflowChoice};
use mf_agent::config::Config;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::runtime::{AdHocLaunchSpec, LaunchSpec, RuntimeEvent, RuntimeHost};
use mf_agent::store::Store;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

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

// ---------- 工作流分配(UI Task 3)----------

#[test]
fn composer_defaults_to_task_local_workflow() {
    let mut state = TaskComposerState::new(vec![(PathBuf::from("/p"), "P".into())], None);
    assert_eq!(state.workflow_choice(), &WorkflowChoice::TaskLocal);
    state.select_workflow("review-template");
    assert_eq!(
        state.workflow_choice(),
        &WorkflowChoice::Template("review-template".into())
    );
    state.select_workflow("");
    assert_eq!(state.workflow_choice(), &WorkflowChoice::TaskLocal);
}

#[test]
fn workflow_options_list_global_templates_only() {
    let options = TaskComposerState::workflow_options(&[
        ("全局A".into(), false),
        ("task-1 草稿".into(), true),
        ("全局B".into(), false),
    ]);
    assert_eq!(options, vec!["全局A".to_string(), "全局B".to_string()]);
}

// ---------- 工作流选择:渲染选项 + 持久化 ----------

#[test]
fn workflow_choice_renders_task_local_and_templates() {
    let mut c = TaskComposerState::new(fake_projects(), None);
    assert_eq!(c.workflow_choice(), &WorkflowChoice::TaskLocal);
    assert_eq!(c.workflow_choice_label(), "工作流:任务本地(新建,默认私有)");
    c.set_templates(vec![("t-release".into(), "发布检查".into())]);
    c.select_workflow("t-release");
    assert_eq!(
        c.workflow_choice(),
        &WorkflowChoice::Template("t-release".into())
    );
    assert_eq!(c.workflow_choice_label(), "工作流:模板 发布检查");
    c.cycle_workflow();
    assert_eq!(c.workflow_choice(), &WorkflowChoice::TaskLocal);
    c.cycle_workflow();
    assert_eq!(
        c.workflow_choice(),
        &WorkflowChoice::Template("t-release".into())
    );
}

#[test]
fn submit_assigns_selected_template_to_new_task() {
    let dir = scratch("assign");
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let db_dir = dir.join(".mf-agent");
    std::fs::create_dir_all(&db_dir).unwrap();
    let orch = Orchestrator::start_with(
        mf_agent::Store::open(&db_dir.join("orchestration.db")).unwrap(),
        dir.clone(),
        Config::default(),
        Arc::new(NoopHost),
        Arc::new(RwLock::new(ProfileCatalog::default())),
        GlobalLimiter::new(4),
        "test-pipe".into(),
        Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
        mf_agent::orchestrator::WorkflowKernel::new(catalog.clone()),
    )
    .unwrap();
    // 模板引用一个可用实例
    let instance = catalog
        .create_agent_instance(mf_agent::AgentInstanceDraft {
            name: "worker".into(),
            agent_type: "generic-command".into(),
            scope: mf_agent::InstanceScope::User,
            project_key: None,
            enabled: true,
            run_mode: mf_agent::RunMode::OneShot,
            executable: "agent.exe".into(),
            argv: vec![],
            env: vec![],
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({ "completion": "process-exit" }),
            sealed_secret_ids: vec![],
        })
        .unwrap();
    let version = catalog
        .save_template(&mf_agent::workflow::WorkflowTemplateDraft {
            key: "t-release".into(),
            name: "发布检查".into(),
            task_local: false,
            nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
                key: "a".into(),
                title: "A".into(),
                instructions: "做 A".into(),
                agent_instance_id: instance.id.clone(),
                deps: vec![],
            }],
        })
        .unwrap();

    let mut c = TaskComposerState::new(vec![(dir.clone(), "scratch".into())], Some(&dir));
    c.set_title("发布任务");
    c.set_templates(vec![("t-release".into(), "发布检查".into())]);
    c.select_workflow("t-release");

    // 生产同款分配闭包:模板 key → 当前版本 → assign_workflow(编译+pin+Revision)
    let catalog_for_assign = catalog.clone();
    let mut assigned: Vec<(PathBuf, i64)> = Vec::new();
    let target = c
        .submit_with_workflow(
            |root| (root == &dir).then(|| orch.clone()),
            |root, task_id, choice| {
                let WorkflowChoice::Template(key) = choice else {
                    return Ok(());
                };
                let current = catalog_for_assign
                    .template_versions(key)?
                    .into_iter()
                    .next_back()
                    .ok_or_else(|| anyhow::anyhow!("模板 {key} 不存在"))?;
                let _ = &dir;
                let mut plugins = std::collections::HashMap::new();
                plugins.insert(
                    "generic-command".to_string(),
                    mf_agent::workflow::PluginSourcePin {
                        full_id: "monkeyfence.generic-command".into(),
                        version: "0.1.0".into(),
                        content_hash: String::new(),
                        contribution_id: String::new(),
                    },
                );
                orch.assign_workflow(task_id, &current, &plugins, false)?;
                assigned.push((root.to_path_buf(), task_id));
                Ok(())
            },
        )
        .unwrap();
    let _ = orch.stop();
    assert!(matches!(target, ActivationTarget::Task { task_id: 1, .. }));
    assert_eq!(assigned.len(), 1, "模板选择必须触发分配");
    // 分配结果:任务有了带快照的 draft Revision(确认运行时激活)
    let orch2 = start_orch(&dir);
    let steps = orch2.store.task_steps(1).unwrap();
    assert_eq!(steps.len(), 1, "Step 投影必须随分配落库");
    assert!(
        orch2
            .store
            .revision_snapshot(steps[0].revision_id)
            .unwrap()
            .is_some(),
        "分配必须冻结工作流快照"
    );
    orch2.stop();
}

#[test]
fn submit_with_task_local_choice_skips_assignment() {
    let dir = scratch("local");
    let mut c = TaskComposerState::new(vec![(dir.clone(), "scratch".into())], Some(&dir));
    c.set_title("本地工作流任务");
    let mut assigned = 0;
    let dir_orch = start_orch(&dir);
    c.submit_with_workflow(
        |root| (root == &dir).then(|| dir_orch.clone()),
        |_root, _task, _choice| {
            assigned += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(assigned, 0, "任务本地选择不触发模板分配");
}

#[test]
fn composer_assign_failure_leaves_no_draft_task() {
    // 预编译/分配失败 → 回滚:项目库不得留下无人认领的 Draft 任务
    let dir = scratch("rollback");
    let orch = start_orch(&dir);
    let mut c = TaskComposerState::new(vec![(dir.clone(), "scratch".into())], Some(&dir));
    c.set_title("会失败的任务");
    c.set_templates(vec![("ghost-template".into(), "不存在的模板".into())]);
    c.select_workflow("ghost-template");

    let err = c
        .submit_with_workflow(
            |root| (root == &dir).then(|| orch.clone()),
            |_root, _task, _choice| anyhow::bail!("模板不存在(注入失败)"),
        )
        .unwrap_err();
    assert!(err.to_string().contains("注入失败"), "{err:#}");
    assert!(
        orch.tasks().unwrap().is_empty(),
        "分配失败不得留下 Draft 任务"
    );
    orch.stop();
}

// ---------- I9:全局模板 Composer 的并行风险开关(持久化,默认 false) ----------

#[test]
fn composer_unsafe_parallel_defaults_false_and_persisted_on_submit() {
    use crate::task_composer::TaskComposerState;
    let dir = tempfile::tempdir().unwrap();
    let projects = vec![(dir.path().to_path_buf(), "P".to_string())];
    let mut state = TaskComposerState::new(projects.clone(), Some(&dir.path().to_path_buf()));
    state.set_title("t");
    state.set_goal("g");
    state.set_templates(vec![("tpl".into(), "模板".into())]);
    state.select_workflow("tpl");
    // 默认关闭(非隔离目录下并行编译默认拒绝)
    assert!(!state.allow_unsafe_parallel());
    assert!(state.unsafe_parallel_label().contains("关闭"));

    // 开启后随提交持久化到项目 Store(task_assign_settings)
    state.toggle_unsafe_parallel();
    assert!(state.allow_unsafe_parallel());
    assert!(state.unsafe_parallel_label().contains("开启"));

    let created = state
        .submit_with_workflow(
            |root| {
                Some(
                    mf_agent::Orchestrator::start(
                        Store::open(&root.join("composer.db")).unwrap(),
                        root.clone(),
                        Config::default(),
                        Arc::new(NoopHost),
                        Arc::new(RwLock::new(ProfileCatalog::default())),
                        GlobalLimiter::new(4),
                        "pipe".into(),
                        Arc::new(
                            mf_agent::execution_directory::ProjectDirectoryProvider::default(),
                        ),
                    )
                    .unwrap(),
                )
            },
            |root, task_id, _choice| {
                // 与 sidebar 生产闭包同型:提交时持久化用户显式选择
                let store = Store::open(&root.join("composer.db")).unwrap();
                store
                    .set_task_assign_unsafe_parallel(&root.to_string_lossy(), task_id, true)
                    .unwrap();
                assert!(store
                    .task_assign_unsafe_parallel(&root.to_string_lossy(), task_id)
                    .unwrap());
                Ok(())
            },
        )
        .unwrap();
    // 持久化读取:默认 false,无记录的任务不继承他人选择
    let store = Store::open(&dir.path().join("composer.db")).unwrap();
    let task_id = match created {
        crate::project_context::ActivationTarget::Task { task_id, .. } => task_id,
        _ => panic!("应激活任务"),
    };
    assert!(!store
        .task_assign_unsafe_parallel(&dir.path().to_string_lossy(), task_id + 999)
        .unwrap());
}

#[test]
fn task_assign_unsafe_parallel_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("assign.db");
    let store = mf_agent::Store::open(&db).unwrap();
    let key = dir.path().to_string_lossy().to_string();
    assert!(!store.task_assign_unsafe_parallel(&key, 5).unwrap());
    store
        .set_task_assign_unsafe_parallel(&key, 5, true)
        .unwrap();
    // 重新打开(重启恢复)后仍可读
    let reopened = mf_agent::Store::open(&db).unwrap();
    assert!(reopened.task_assign_unsafe_parallel(&key, 5).unwrap());
    // 关闭后再次持久化为 false
    reopened
        .set_task_assign_unsafe_parallel(&key, 5, false)
        .unwrap();
    assert!(!mf_agent::Store::open(&db)
        .unwrap()
        .task_assign_unsafe_parallel(&key, 5)
        .unwrap());
}
