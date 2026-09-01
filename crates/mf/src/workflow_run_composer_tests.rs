//! 工作流运行 Composer 与 `AppCtx::run_project_workflow`(ADR 0004 / Task 4):
//! 空 goal 拒绝、成功路径创建 Task/Revision 并开始调度、编译失败不留孤儿
//! Task、单/多节点同一 API、goal 进入 prompt 构造链。

use crate::app_ctx::{AppCtx, WorkflowRunTarget};
use crate::workflow_run_composer::WorkflowRunComposerState;
use mf_agent::workflow::{ProjectWorkflowDraft, WorkflowNodeDraft};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

fn node(key: &str, deps: &[&str]) -> WorkflowNodeDraft {
    WorkflowNodeDraft {
        key: key.into(),
        title: key.into(),
        instructions: format!("做 {key}"),
        agent_instance_id: String::new(), // setup 注入真实实例
        deps: deps.iter().map(|s| s.to_string()).collect(),
    }
}

/// 可真实运行的环境:cmd.exe 实例(进程退出即完成)+ 项目工作流。
fn setup_with_workflow(nodes: Vec<WorkflowNodeDraft>) -> (Arc<AppCtx>, tempfile::TempDir) {
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = AppCtx::with_catalog_for_tests(catalog.clone());
    let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    let instance = catalog
        .create_agent_instance(mf_agent::AgentInstanceDraft {
            name: "e2e-worker".into(),
            agent_type: "claude".into(),
            scope: mf_agent::InstanceScope::User,
            project_key: None,
            enabled: true,
            run_mode: mf_agent::RunMode::OneShot,
            executable: cmd,
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
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    orch.store
        .save_project_workflow(&ProjectWorkflowDraft {
            key: "wf-e2e".into(),
            name: "E2E 工作流".into(),
            nodes: nodes
                .into_iter()
                .map(|mut n| {
                    n.agent_instance_id = instance.id.clone();
                    n
                })
                .collect(),
            allow_unsafe_parallel: false,
        })
        .unwrap();
    (ctx, project)
}

#[test]
fn empty_goal_cannot_submit() {
    let mut state = WorkflowRunComposerState::new(
        PathBuf::from("D:/proj"),
        "proj".into(),
        "wf".into(),
        "工作流".into(),
        false,
        1,
    );
    assert!(!state.can_submit(), "空 goal 不能提交");
    let err = state
        .submit(|_, _, _| panic!("空 goal 不得触达运行服务"))
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("目标"));
    state.set_goal("  ");
    assert!(!state.can_submit(), "空白 goal 同样不能提交");
    state.set_goal("修复登录超时");
    assert!(state.can_submit());
}

#[test]
fn submit_success_and_error_manage_state() {
    let mut state = WorkflowRunComposerState::new(
        PathBuf::from("D:/proj"),
        "proj".into(),
        "wf".into(),
        "工作流".into(),
        false,
        2,
    );
    state.set_goal("目标");
    let target = state
        .submit(|root, key, goal| {
            assert_eq!(root, &PathBuf::from("D:/proj"));
            assert_eq!(key, "wf");
            assert_eq!(goal, "目标");
            Ok(WorkflowRunTarget {
                project_root: root.clone(),
                workflow_key: key.to_string(),
                task_id: 7,
                revision_id: 3,
            })
        })
        .unwrap();
    assert_eq!(target.task_id, 7);
    assert!(!state.is_submitting());

    state.set_goal("再次运行");
    let _ = state
        .submit(|_, _, _| Err(anyhow::anyhow!("编译失败")))
        .err()
        .unwrap();
    assert_eq!(state.error(), Some("编译失败"), "错误必须进入状态机展示");
    assert!(!state.is_submitting());
    state.set_goal("重新输入清除错误");
    assert_eq!(state.error(), None, "再次编辑清除错误");
}

#[test]
fn run_project_workflow_creates_task_revision_and_starts_scheduling() {
    let (ctx, project) = setup_with_workflow(vec![node("a", &[])]);
    let before = ctx
        .orchestrator_of(project.path())
        .unwrap()
        .store
        .list_tasks(false)
        .unwrap()
        .len();
    let goal = "发布前检查\n补充:把报告写到 report.md";
    let target = ctx
        .run_project_workflow(project.path(), "wf-e2e", goal)
        .expect("运行项目工作流");
    assert_eq!(target.workflow_key, "wf-e2e");
    assert_eq!(target.project_root, project.path().to_path_buf());
    let orch = ctx.orchestrator_of(project.path()).unwrap();
    // Task 创建:标题取第一非空行,完整 goal 保留
    let task = orch.store.task_view(target.task_id).unwrap().unwrap();
    assert_eq!(task.title, "发布前检查");
    assert_eq!(task.goal, goal);
    // Revision 冻结且已激活,调度启动(真实派发)
    assert!(target.revision_id > 0);
    assert!(
        wait_until(Duration::from_secs(20), || orch
            .store
            .list_runs_of_task(target.task_id)
            .map(|runs| !runs.is_empty())
            .unwrap_or(false)),
        "等待真实派发超时"
    );
    // goal 进入节点 prompt 构造链(build_workflow_prompt 引用 task.goal)
    let snapshot = orch
        .store
        .revision_snapshot(target.revision_id)
        .unwrap()
        .unwrap();
    let prompt = mf_agent::orchestrator::build_workflow_prompt(
        &task,
        &snapshot.nodes[0],
        "token-x",
        &Default::default(),
    );
    assert!(
        prompt.contains(goal),
        "工作流目标必须进入节点 prompt: {prompt}"
    );
    assert!(prompt.contains("做 a"), "节点工作说明进入 prompt");
    assert_eq!(orch.store.list_tasks(false).unwrap().len(), before + 1);
    // 不把项目工作流自动保存成全局模板
    let templates = ctx.catalog_store.list_templates(false).unwrap();
    assert!(
        templates.iter().all(|t| t.key != "wf-e2e"),
        "项目工作流不得自动晋升全局模板: {templates:?}"
    );
    // 清理真实进程并停止
    for r in orch.store.list_runs_of_task(target.task_id).unwrap() {
        if let Some(sid) = r.session_id {
            if let Some(session) = orch.store.session_view(sid).unwrap() {
                ctx.registry.kill_session(&session.public_handle);
            }
        }
    }
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

#[test]
fn compile_failure_leaves_no_new_task() {
    // T1b 起,结构非法(未知依赖/重复键)在保存时即 fail-closed;
    // 这里用「循环」:结构校验通过、编译期仍必须失败。
    let (ctx, project) = setup_with_workflow(vec![node("a", &["b"]), node("b", &["a"])]);
    let orch = ctx.orchestrator_of(project.path()).unwrap();
    let before = orch.store.list_tasks(false).unwrap().len();
    let err = ctx
        .run_project_workflow(project.path(), "wf-e2e", "运行一个坏掉的工作流")
        .err()
        .expect("循环依赖必须编译失败");
    assert!(
        format!("{err:#}").contains("编译失败"),
        "错误必须来自编译: {err:#}"
    );
    assert_eq!(
        orch.store.list_tasks(false).unwrap().len(),
        before,
        "编译失败不得留下孤儿 Task"
    );
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

#[test]
fn single_and_multi_node_workflows_use_the_same_api() {
    // 多节点串行:同一 API 一次调用
    let (ctx, project) = setup_with_workflow(vec![node("a", &[]), node("b", &["a"])]);
    let target = ctx
        .run_project_workflow(project.path(), "wf-e2e", "两步工作流")
        .expect("多节点同一 API 运行");
    let orch = ctx.orchestrator_of(project.path()).unwrap();
    let snapshot = orch
        .store
        .revision_snapshot(target.revision_id)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.nodes.len(), 2);
    let steps = orch.store.task_steps(target.task_id).unwrap();
    assert_eq!(steps.len(), 2, "两个节点都投影为 Step");
    for r in orch.store.list_runs_of_task(target.task_id).unwrap() {
        if let Some(sid) = r.session_id {
            if let Some(session) = orch.store.session_view(sid).unwrap() {
                ctx.registry.kill_session(&session.public_handle);
            }
        }
    }
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

#[test]
fn missing_workflow_or_project_are_stable_errors() {
    let (ctx, project) = setup_with_workflow(vec![node("a", &[])]);
    let err = ctx
        .run_project_workflow(project.path(), "ghost-wf", "目标")
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("不存在"));
    let err = ctx
        .run_project_workflow(Path::new("D:/not-opened"), "wf-e2e", "目标")
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("项目未打开"));
    let orch = ctx.orchestrator_of(project.path()).unwrap();
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}
