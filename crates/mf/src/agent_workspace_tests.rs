//! AgentWorkspace 顶层结构(ADR 0004 / Task 6):
//! 只投影「工作流 / 运行」两个页签;无提醒时 Agent 入口进入 Workflows;
//! 有 Needs You 卡片时进入 Runs;选择 Task 不改写当前页签;
//! 设置页持有嵌入式实例配置页。

use crate::agent_workspace::AgentWorkspace;
use crate::project_overview::{AgentCardOverview, AttentionBucket, ProjectOverviewSnapshot};
use std::sync::Arc;

fn card(bucket: AttentionBucket) -> AgentCardOverview {
    AgentCardOverview {
        project_root: std::path::PathBuf::from("D:/proj"),
        project_name: "proj".into(),
        session: mf_agent::model::SessionView {
            id: 1,
            session_key: None,
            runtime: "pty".into(),
            agent_profile: "claude".into(),
            title: "s".into(),
            status: mf_agent::SessionStatus::Working,
            last_instruction: None,
            last_reply: None,
            unread: false,
            created_at: String::new(),
            updated_at: String::new(),
        },
        run: None,
        task_id: Some(1),
        task_title: Some("t".into()),
        profile_display: "Claude".into(),
        tail: vec![],
        alive: true,
        bucket,
        is_http: false,
    }
}

fn snapshot_with(cards: Vec<AgentCardOverview>) -> Arc<ProjectOverviewSnapshot> {
    snapshot_with_attention(cards, Vec::new())
}

fn snapshot_with_attention(
    cards: Vec<AgentCardOverview>,
    attention_runs: Vec<crate::project_overview::WorkflowRunAttention>,
) -> Arc<ProjectOverviewSnapshot> {
    Arc::new(ProjectOverviewSnapshot {
        revision: 1,
        projects: vec![],
        agent_cards: cards,
        global_active_runs: 0,
        templates: vec![],
        attention_run_count: attention_runs.len(),
        attention_runs,
    })
}

/// AgentTab 枚举只含两个变体:非通配 match 保证新增变体时编译失败。
#[test]
fn agent_tab_projects_exactly_workflows_and_runs() {
    let labels: Vec<&str> = [
        crate::workspace::AgentTab::Workflows,
        crate::workspace::AgentTab::Runs,
    ]
    .iter()
    .map(|tab| match tab {
        crate::workspace::AgentTab::Workflows => "工作流",
        crate::workspace::AgentTab::Runs => "运行",
    })
    .collect();
    assert_eq!(labels, vec!["工作流", "运行"], "顶层只保留两个页签");
}

#[gpui::test]
fn agent_entry_without_attention_goes_to_workflows(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let ws = cx.new(|cx| AgentWorkspace::new(ctx.clone(), cx));
    // 无提醒快照
    cx.update_entity(&ws, |aw: &mut AgentWorkspace, cx| {
        aw.set_overview(snapshot_with(vec![card(AttentionBucket::Working)]), cx);
        aw.enter_from_activity(cx);
        assert_eq!(
            aw.active_tab(),
            crate::workspace::AgentTab::Workflows,
            "无提醒时进入 Workflows(首次默认)"
        );
    });
    // 用户切到 Runs 后再点入口:保持上次页(无提醒)
    cx.update_entity(&ws, |aw: &mut AgentWorkspace, cx| {
        aw.show_tab(crate::workspace::AgentTab::Runs, cx);
        aw.enter_from_activity(cx);
        assert_eq!(
            aw.active_tab(),
            crate::workspace::AgentTab::Runs,
            "无提醒保持上次页"
        );
    });
}

#[gpui::test]
fn agent_entry_with_attention_goes_to_runs(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let ws = cx.new(|cx| AgentWorkspace::new(ctx.clone(), cx));
    let attention = crate::project_overview::WorkflowRunAttention {
        project_root: std::path::PathBuf::from("D:/proj"),
        task_id: 9,
        task_title: "需要你".into(),
        reason_count: 1,
        focus_step_id: Some(4),
    };
    cx.update_entity(&ws, |aw: &mut AgentWorkspace, cx| {
        aw.show_tab(crate::workspace::AgentTab::Workflows, cx);
        // Task 7 起入口按运行级「需要你」计数判定(卡片桶不再是口径)
        aw.set_overview(
            snapshot_with_attention(vec![card(AttentionBucket::Working)], vec![attention]),
            cx,
        );
        aw.enter_from_activity(cx);
        assert_eq!(
            aw.active_tab(),
            crate::workspace::AgentTab::Runs,
            "有 Needs You 运行时必须进入 Runs"
        );
    });
}

#[gpui::test]
fn multiple_attention_entry_forces_needs_you_filter(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let ctx =
        crate::app_ctx::AppCtx::with_catalog_for_tests(mf_agent::CatalogStore::memory().unwrap());
    let ws = cx.new(|cx| AgentWorkspace::new(ctx, cx));
    let make_attention = |task_id| crate::project_overview::WorkflowRunAttention {
        project_root: std::path::PathBuf::from(format!("D:/p-{task_id}")),
        task_id,
        task_title: format!("运行 {task_id}"),
        reason_count: 1,
        focus_step_id: None,
    };
    cx.update_entity(&ws, |aw, cx| {
        aw.runs_page.update(cx, |page, cx| {
            page.set_filter(crate::workflow_runs_page::RunFilter::RecentlyCompleted, cx)
        });
        aw.set_overview(
            snapshot_with_attention(vec![], vec![make_attention(1), make_attention(2)]),
            cx,
        );
        aw.enter_from_activity(cx);
        assert_eq!(aw.active_tab(), crate::workspace::AgentTab::Runs);
        assert_eq!(
            aw.runs_page.read(cx).filter,
            crate::workflow_runs_page::RunFilter::NeedsYou,
            "多个提醒也必须进入需要你过滤"
        );
    });
}

#[gpui::test]
fn selecting_task_does_not_rewrite_agent_tab(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let project = tempfile::tempdir().unwrap();
    ctx.open_project(project.path().to_path_buf()).unwrap();
    let ws = cx.new(|cx| AgentWorkspace::new(ctx.clone(), cx));
    cx.update_entity(&ws, |aw: &mut AgentWorkspace, cx| {
        aw.show_tab(crate::workspace::AgentTab::Runs, cx);
        // 选择 Task(Project 与 Task 分别传递)
        aw.set_context(
            Some(project.path().to_path_buf()),
            Some((project.path().to_path_buf(), 7)),
            cx,
        );
        assert_eq!(
            aw.active_tab(),
            crate::workspace::AgentTab::Runs,
            "选择 Task 不得改写用户当前页签"
        );
        // 没有 Task 时工作流页仍可用(画布已收到项目)
        aw.set_context(Some(project.path().to_path_buf()), None, cx);
        let canvas_project = aw.workflow_page.read(cx).project_root.clone();
        assert_eq!(
            canvas_project,
            Some(project.path().to_path_buf()),
            "画布接收当前 Project(与 Task 无关)"
        );
    });
}

#[gpui::test]
fn settings_holds_embedded_instances_page(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    // 设置 → 智能体:嵌入式实例配置页(创建一次;关闭设置不丢工作流状态)
    let settings = cx.new(|cx| crate::settings::SettingsView::new_with_app(ctx.clone(), None, cx));
    cx.read_entity(&settings, |s: &crate::settings::SettingsView, cx| {
        let page = s
            .agent_instances
            .as_ref()
            .expect("设置页必须持有嵌入式实例页");
        assert!(page.read(cx).embedded, "嵌入模式:去页头、不占 size_full");
    });
    // Agent 工作区不再持有实例配置页(结构上只有 画布 + 运行 + Composer)
    let ws = cx.new(|cx| AgentWorkspace::new(ctx.clone(), cx));
    let _ = &ws;
}

#[gpui::test]
fn run_requested_opens_composer_and_submit_activates_task(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    // 可运行工作流(cmd.exe 实例,进程退出即完成)
    let instance = ctx
        .catalog_store
        .create_agent_instance(mf_agent::AgentInstanceDraft {
            name: "w".into(),
            agent_type: "claude".into(),
            scope: mf_agent::InstanceScope::User,
            project_key: None,
            enabled: true,
            run_mode: mf_agent::RunMode::OneShot,
            executable: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
            argv: vec!["/C".into(), "exit".into(), "0".into()],
            env: vec![],
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({
                "input": "argv",
                "completion": "process-exit"
            }),
            sealed_secret_ids: vec![],
        })
        .unwrap();
    orch.store
        .save_project_workflow(&mf_agent::ProjectWorkflowDraft {
            key: "wf-1".into(),
            name: "E2E".into(),
            nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
                key: "a".into(),
                title: "A".into(),
                instructions: String::new(),
                agent_instance_id: instance.id.clone(),
                deps: vec![],
            }],
            allow_unsafe_parallel: false,
        })
        .unwrap();

    let ws = cx.new(|cx| AgentWorkspace::new(ctx.clone(), cx));
    // 画布事件 → Composer(意图,不直接运行)
    cx.update_entity(&ws, |aw: &mut AgentWorkspace, cx| {
        aw.open_run_composer(project.path().to_path_buf(), "wf-1".into(), cx);
        assert!(aw.run_composer_open(), "RunRequested 打开 Composer");
        assert_eq!(aw.active_tab(), crate::workspace::AgentTab::Workflows);
    });
    // 输入目标并提交:创建 Task/Revision 并切到 Runs
    cx.update_entity(&ws, |aw: &mut AgentWorkspace, cx| {
        let composer = aw.run_composer.clone().unwrap();
        composer.update(cx, |c, cx| {
            c.state.set_goal("从工作流直接运行");
            cx.notify();
        });
        aw.submit_run_composer(cx);
        assert!(!aw.run_composer_open(), "提交成功后关闭 Composer");
        assert_eq!(
            aw.active_tab(),
            crate::workspace::AgentTab::Runs,
            "成功后切到 Runs"
        );
    });
    // Task 真实创建且开始调度
    let tasks = orch.store.list_tasks(false).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "从工作流直接运行");
    assert!(tasks[0].active_revision.is_some(), "Revision 已冻结并激活");
    // 清理真实进程
    for r in orch.store.list_runs_of_task(tasks[0].id).unwrap() {
        if let Some(sid) = r.session_id {
            ctx.registry
                .kill_session(&project.path().to_string_lossy(), sid);
        }
    }
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}
