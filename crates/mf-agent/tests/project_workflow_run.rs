//! 项目工作流 → 运行(Task 4,mf-agent 侧):
//! ProjectWorkflowRecord 投影为临时模板版本后走同一 Compiler/assign 路径;
//! goal 进入节点 prompt 构造链;单节点工作流即"单 Agent 场景"。

mod common;

use common::*;
use mf_agent::orchestrator::WorkflowKernel;
use mf_agent::workflow::{ProjectWorkflowDraft, WorkflowNodeDraft};

/// ProjectWorkflowRecord 存量 → 临时 WorkflowTemplateVersion → assign:
/// 冻结快照节点与项目工作流一致,digest 与草稿公式一致。
#[test]
fn project_workflow_record_projects_into_assign_path() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let store = &fx.orch.store;
    let record = store
        .save_project_workflow(&ProjectWorkflowDraft {
            key: "wf-rel".into(),
            name: "发布检查".into(),
            nodes: vec![node("a", &[], "做 A", &fx.instance_id)],
            allow_unsafe_parallel: false,
        })
        .unwrap();

    // 投影(与 mf 层 run_project_workflow 同构):临时模板版本
    let version = mf_agent::workflow::WorkflowTemplateVersion {
        version_id: 0,
        template_key: format!("project-workflow/{}", record.key),
        version: 1,
        nodes: record.nodes.clone(),
        created_at: String::new(),
    };
    let task = fx
        .orch
        .create_task("发布前检查", "完整目标\n第二行")
        .unwrap();
    let rev = fx
        .orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .expect("项目工作流必须能走正式 Compiler/assign 路径");
    let snapshot = fx.orch.store.revision_snapshot(rev.id).unwrap().unwrap();
    assert_eq!(snapshot.template_key, "project-workflow/wf-rel");
    assert_eq!(snapshot.nodes.len(), 1);
    assert_eq!(snapshot.nodes[0].key, "a");
    assert_eq!(snapshot.nodes[0].instance.id, fx.instance_id);

    // goal 进入节点 prompt 构造链
    let prompt = mf_agent::orchestrator::build_workflow_prompt(
        &task,
        &snapshot.nodes[0],
        "tok",
        &Default::default(),
    );
    assert!(
        prompt.contains("完整目标"),
        "Task.goal 注入 prompt: {prompt}"
    );
    assert!(prompt.contains("做 A"));

    // 运行后恢复:confirm_and_run 派发真实 fixture 会话
    fx.orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(std::time::Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .iter()
        .any(|(s, _)| s.node_key == "a")));
    fx.orch.stop();
}

/// 单节点项目工作流 = 单 Agent 场景(决策 4:不实现第二套执行路径)。
#[test]
fn single_node_project_workflow_runs_through_same_pipeline() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let record = fx
        .orch
        .store
        .save_project_workflow(&ProjectWorkflowDraft {
            key: "wf-single".into(),
            name: "单步".into(),
            nodes: vec![node("only", &[], "只做这一步", &fx.instance_id)],
            allow_unsafe_parallel: false,
        })
        .unwrap();
    let version = mf_agent::workflow::WorkflowTemplateVersion {
        version_id: 0,
        template_key: "project-workflow/wf-single".into(),
        version: 1,
        nodes: record.nodes.clone(),
        created_at: String::new(),
    };
    let task = fx.orch.create_task("单步", "g").unwrap();
    let rev = fx
        .orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap();
    fx.orch.confirm_and_run(task.id).unwrap();
    let steps = fx.orch.store.task_steps(task.id).unwrap();
    assert_eq!(steps.len(), 1, "单节点投影为单 Step,同一调度路径");
    let _ = rev;
    assert!(wait_until(std::time::Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .iter()
        .any(|(s, _)| s.node_key == "only")));
    fx.orch.stop();
}

/// 编译失败时由调用方回滚 Task(mf 层负责);这里验证 assign 失败
/// 不产生 Revision,且回滚后 Store 无残留 Draft。
#[test]
fn assign_failure_rolls_back_cleanly_for_caller() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = mf_agent::workflow::WorkflowTemplateVersion {
        version_id: 0,
        template_key: "project-workflow/wf-bad".into(),
        version: 1,
        nodes: vec![
            node("a", &[], "做 A", &fx.instance_id),
            WorkflowNodeDraft {
                key: "b".into(),
                title: "B".into(),
                instructions: "做 B".into(),
                agent_instance_id: fx.instance_id.clone(),
                deps: vec!["ghost".into()],
            },
        ],
        created_at: String::new(),
    };
    let task = fx.orch.create_task("坏工作流", "g").unwrap();
    assert!(fx
        .orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .is_err());
    assert!(
        fx.orch.store.list_revision_ids(task.id).unwrap().is_empty(),
        "编译失败不得留下 Revision"
    );
    // 调用方(mf 层)随后 discard_task;这里验证 discard 可行且彻底
    fx.orch.discard_task(task.id).unwrap();
    assert!(fx.orch.store.task_view(task.id).unwrap().is_none());
    fx.orch.stop();
}

#[test]
fn discarding_assigned_draft_releases_revision_pins() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let task = fx.orch.create_task("待回滚", "g").unwrap();
    let version = fx.template(
        "wf-pin-rollback",
        vec![node("a", &[], "做 A", &fx.instance_id)],
    );
    let revision = fx
        .orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap();
    let expected = mf_agent::orchestrator::workflow_pin_key(&fx.orch.root, task.id, revision.id);
    assert!(
        fx.pins
            .pinned
            .lock()
            .iter()
            .any(|(key, _)| key == &expected),
        "前置:Revision 必须已经固定插件 pin"
    );

    fx.orch.discard_task(task.id).unwrap();

    assert!(
        fx.pins.released.lock().iter().any(|key| key == &expected),
        "删除无 Run 的 Draft Task 必须释放其 Revision pin"
    );
    assert!(fx.orch.store.task_view(task.id).unwrap().is_none());
    fx.orch.stop();
}

/// WorkflowKernel 默认(instance_resolver = None)时项目工作流里保存的
/// 普通实例引用解析行为与既有模板路径一致(目录库回退)。
#[test]
fn kernel_without_resolver_keeps_catalog_instance_resolution() {
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let kernel = WorkflowKernel::new(catalog.clone());
    assert!(kernel.instance_resolver.is_none());
    let err = kernel.resolve_instance("not-an-instance").err().unwrap();
    assert!(format!("{err:#}").contains("not-an-instance"));
}
