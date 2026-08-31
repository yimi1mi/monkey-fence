//! Workflow Canvas 项目工作流编辑器(ADR 0004 / Task 5):
//! 无 Task 只有 Project 也能建/编工作流;跨项目列表隔离;
//! 从全局模板创建后互不联动;默认 CLI 与保存实例产生正确引用;
//! 保存失败(dirty)阻止运行;环/自依赖/未知依赖仍被拒绝。

use crate::workflow_canvas::{AgentLibraryEntry, WorkflowCanvas};

fn cmd_instance(ctx: &std::sync::Arc<crate::app_ctx::AppCtx>, name: &str) -> String {
    ctx.catalog_store
        .create_agent_instance(mf_agent::AgentInstanceDraft {
            name: name.into(),
            agent_type: "opencode".into(),
            scope: mf_agent::InstanceScope::User,
            project_key: None,
            enabled: true,
            run_mode: mf_agent::RunMode::OneShot,
            executable: "cmd.exe".into(),
            argv: vec!["/K".into()],
            env: vec![],
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({ "completion": "process-exit" }),
            sealed_secret_ids: vec![],
        })
        .unwrap()
        .id
}

/// 无 Task、只有 Project:可创建、编辑、保存、请求运行。
#[gpui::test]
fn canvas_edits_project_workflows_without_any_task(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    let instance_id = cmd_instance(&ctx, "worker");

    let canvas = cx.new(|cx| WorkflowCanvas::new(ctx.clone(), cx));
    // 只传项目(没有选中任何 Task)
    cx.update_entity(&canvas, |c: &mut WorkflowCanvas, cx| {
        c.set_project(Some(project.path().to_path_buf()), cx);
    });
    cx.update_entity(&canvas, |c: &mut WorkflowCanvas, cx| {
        c.new_workflow(cx);
        assert_eq!(
            c.current_key.as_deref(),
            Some("wf-1"),
            "新建生成稳定 key,不用 task id"
        );
        // 从 Agent 库添加节点(保存实例引用)→ 自动保存
        c.editor.drag_from_library(&instance_id);
        c.save_after_edit();
        assert!(c.save_error.is_none(), "自动保存必须成功");
    });
    let record = orch
        .store
        .load_project_workflow("wf-1")
        .unwrap()
        .expect("节点加入后自动落库");
    assert_eq!(record.nodes.len(), 1);
    assert_eq!(record.nodes[0].agent_instance_id, instance_id);
    assert_eq!(record.name, "工作流 1");

    // 重命名(原子编辑:保存)
    cx.update_entity(&canvas, |c: &mut WorkflowCanvas, cx| {
        c.rename_workflow("发布检查", cx);
    });
    let record = orch.store.load_project_workflow("wf-1").unwrap().unwrap();
    assert_eq!(record.name, "发布检查");

    // 运行意图:保存成功后允许
    cx.update_entity(&canvas, |c: &mut WorkflowCanvas, cx| {
        assert!(c.request_run(cx).is_ok(), "干净工作流可以请求运行");
    });
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

/// A 项目的工作流不出现在 B 项目列表;各自编辑互不串扰。
#[gpui::test]
fn project_workflow_lists_are_isolated_per_project(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let project_a = tempfile::tempdir().unwrap();
    let project_b = tempfile::tempdir().unwrap();
    let orch_a = ctx.open_project(project_a.path().to_path_buf()).unwrap();
    let orch_b = ctx.open_project(project_b.path().to_path_buf()).unwrap();
    let instance_id = cmd_instance(&ctx, "worker");
    orch_a
        .store
        .save_project_workflow(&mf_agent::ProjectWorkflowDraft {
            key: "wf-a-only".into(),
            name: "A 专属".into(),
            nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
                key: "a".into(),
                title: "A".into(),
                instructions: String::new(),
                agent_instance_id: instance_id.clone(),
                deps: vec![],
            }],
            allow_unsafe_parallel: false,
        })
        .unwrap();

    let canvas = cx.new(|cx| WorkflowCanvas::new(ctx.clone(), cx));
    // 切到 B:B 列表为空,看不到 A 的工作流
    cx.update_entity(&canvas, |c: &mut WorkflowCanvas, cx| {
        c.set_project(Some(project_b.path().to_path_buf()), cx);
    });
    let b_list = cx.read_entity(&canvas, |c: &WorkflowCanvas, _| {
        c.workflows
            .iter()
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>()
    });
    assert!(
        !b_list.contains(&"wf-a-only".to_string()),
        "A 的工作流不得出现在 B 列表: {b_list:?}"
    );
    // 切回 A:出现且可加载
    cx.update_entity(&canvas, |c: &mut WorkflowCanvas, cx| {
        c.set_project(Some(project_a.path().to_path_buf()), cx);
        c.load_workflow("wf-a-only");
        assert_eq!(c.editor.nodes().len(), 1);
    });
    orch_a.stop();
    orch_b.stop();
    ctx.close_project(&project_a.path().to_path_buf());
    ctx.close_project(&project_b.path().to_path_buf());
}

#[gpui::test]
fn workflow_list_read_error_preserves_editor_and_blocks_new_writes(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    let instance_id = cmd_instance(&ctx, "worker");
    let make = |key: &str, name: &str| mf_agent::ProjectWorkflowDraft {
        key: key.into(),
        name: name.into(),
        nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
            key: "a".into(),
            title: "A".into(),
            instructions: String::new(),
            agent_instance_id: instance_id.clone(),
            deps: vec![],
        }],
        allow_unsafe_parallel: false,
    };
    orch.store
        .save_project_workflow(&make("wf-1", "当前"))
        .unwrap();
    orch.store
        .save_project_workflow(&make("wf-bad", "损坏"))
        .unwrap();

    let canvas = cx.new(|cx| WorkflowCanvas::new(ctx.clone(), cx));
    cx.update_entity(&canvas, |c, cx| {
        c.set_project(Some(project.path().to_path_buf()), cx);
        c.load_workflow("wf-1");
        assert_eq!(c.editor.nodes().len(), 1);
    });
    orch.store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE project_workflows SET graph_json = '{bad' WHERE workflow_key = 'wf-bad'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    cx.update_entity(&canvas, |c, cx| {
        c.reload_workflows();
        assert_eq!(c.current_key.as_deref(), Some("wf-1"));
        assert_eq!(c.editor.nodes().len(), 1, "读取错误不得清空当前编辑态");
        assert!(c.save_error.is_some(), "读取错误必须阻止后续写入/运行");
        c.new_workflow(cx);
        assert_eq!(
            c.current_key.as_deref(),
            Some("wf-1"),
            "存储异常时不得新建同 key 覆盖存量"
        );
    });
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

/// 从全局模板创建:复制当前版本;之后编辑项目工作流不回写模板。
#[gpui::test]
fn create_from_template_copies_and_does_not_track(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog.clone());
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    let instance_id = cmd_instance(&ctx, "worker");

    // 全局模板(当前版本 1 个节点)
    let version = catalog
        .save_template(&mf_agent::workflow::WorkflowTemplateDraft {
            key: "tpl-release".into(),
            name: "发布模板".into(),
            task_local: false,
            nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
                key: "check".into(),
                title: "检查".into(),
                instructions: "模板原版".into(),
                agent_instance_id: instance_id.clone(),
                deps: vec![],
            }],
        })
        .unwrap();

    let canvas = cx.new(|cx| WorkflowCanvas::new(ctx.clone(), cx));
    cx.update_entity(&canvas, |c: &mut WorkflowCanvas, cx| {
        c.set_project(Some(project.path().to_path_buf()), cx);
        c.create_from_template("tpl-release", cx);
        assert_eq!(c.editor.nodes().len(), 1, "复制模板当前版本节点");
        // 编辑项目工作流(改标题+加节点)并保存
        c.editor.set_selected_title("检查 v2");
        c.editor.drag_from_library(&instance_id);
        c.save_after_edit();
    });
    let created = orch
        .store
        .list_project_workflows()
        .unwrap()
        .into_iter()
        .find(|r| r.name == "发布模板")
        .expect("从模板创建的项目工作流已落库");
    assert_eq!(created.nodes.len(), 2, "项目工作流副本已独立编辑");
    // 模板不被修改:仍是 1 个节点、说明未变
    let tpl_now = catalog
        .template_version(version.version_id)
        .unwrap()
        .unwrap();
    assert_eq!(tpl_now.nodes.len(), 1);
    assert_eq!(tpl_now.nodes[0].instructions, "模板原版");
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

/// 默认 CLI 与保存实例都产生正确节点引用(default-cli:<完整贡献 ID>)。
#[gpui::test]
fn library_entries_produce_correct_node_references(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let default_cli = AgentLibraryEntry::DefaultCli {
        full_contribution_id: "test.plugin.agent".into(),
        name: "Agent".into(),
    };
    let instance = AgentLibraryEntry::Instance {
        id: "inst_x".into(),
        name: "我的配置".into(),
    };
    assert_eq!(
        default_cli.node_reference().unwrap(),
        "default-cli:test.plugin.agent",
        "默认 CLI 引用必须是完整贡献 ID"
    );
    assert_eq!(instance.node_reference().unwrap(), "inst_x");
    assert!(AgentLibraryEntry::ManageSettings.node_reference().is_none());
    assert_eq!(
        default_cli.display_name(),
        "Agent",
        "显示用户名,不是 contribution id"
    );

    // 画布:库条目含保存实例分组与管理入口
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let instance_id = cmd_instance(&ctx, "worker");
    let canvas = cx.new(|cx| WorkflowCanvas::new(ctx.clone(), cx));
    let entries = cx.read_entity(&canvas, |c: &WorkflowCanvas, _| c.library.clone());
    assert!(
        entries
            .iter()
            .any(|e| matches!(e, AgentLibraryEntry::Instance { id, .. } if *id == instance_id)),
        "保存配置进入库: {entries:?}"
    );
    assert!(
        entries.last() == Some(&AgentLibraryEntry::ManageSettings),
        "管理智能体配置入口在末尾: {entries:?}"
    );
    // 未检测到的 CLI 不出现在画布库(内置 CLI 在测试环境通常未检测;
    // 若某机器装了 codex,断言仍成立 —— 这里只断言不存在虚构类型)
    assert!(
        !entries.iter().any(
            |e| matches!(e, AgentLibraryEntry::DefaultCli { full_contribution_id, .. }
            if full_contribution_id == "ghost.plugin.agent")
        ),
        "未检测/未知 CLI 不进入画布库"
    );
}

/// 保存失败(dirty)必须阻止运行:绝不能运行旧 Store 内容。
#[gpui::test]
fn save_failure_blocks_run_instead_of_running_stale_store(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    let instance_id = cmd_instance(&ctx, "worker");
    orch.store
        .save_project_workflow(&mf_agent::ProjectWorkflowDraft {
            key: "wf-stale".into(),
            name: "旧内容".into(),
            nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
                key: "a".into(),
                title: "A".into(),
                instructions: String::new(),
                agent_instance_id: instance_id.clone(),
                deps: vec![],
            }],
            allow_unsafe_parallel: false,
        })
        .unwrap();

    let canvas = cx.new(|cx| WorkflowCanvas::new(ctx.clone(), cx));
    cx.update_entity(&canvas, |c: &mut WorkflowCanvas, cx| {
        c.set_project(Some(project.path().to_path_buf()), cx);
        c.load_workflow("wf-stale");
        // 清空已持久化的工作流 → 保存被 Store 拒绝(至少一个节点)
        c.editor.load_nodes(Vec::new());
        c.save_after_edit();
        assert!(c.save_error.is_some(), "保存失败必须保留 dirty 状态");
        // Run 必须被拒绝
        let err = c.request_run(cx).err().expect("dirty 状态必须阻止运行");
        assert!(
            format!("{err:#}").contains("未保存"),
            "错误必须指明未保存修改: {err:#}"
        );
    });
    // Store 旧内容仍在,但没有任何 Revision 被冻结/运行
    assert!(orch.store.list_revision_ids(1).unwrap().is_empty());
    let tasks = orch.store.list_tasks(false).unwrap();
    assert!(tasks.is_empty(), "不得创建任何 Task");
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

/// 环、自依赖与未知依赖仍被拒绝(编辑器层)。
#[test]
fn editor_still_rejects_cycles_self_and_unknown_deps() {
    use crate::workflow_editor::WorkflowEditorState;
    let mut editor = WorkflowEditorState::load(&crate::workflow_editor::MemoryPrefs::default());
    editor.drag_from_library("inst-a");
    editor.drag_from_library("inst-a");
    let keys: Vec<String> = editor.nodes().iter().map(|n| n.key.clone()).collect();
    let (a, b) = (keys[0].clone(), keys[1].clone());
    assert!(editor.add_dependency(&a, &a).is_err(), "自依赖拒绝");
    assert!(editor.add_dependency(&a, "ghost").is_err(), "未知依赖拒绝");
    assert!(editor.add_dependency(&b, &a).is_ok());
    assert!(
        editor.add_dependency(&a, &b).is_err(),
        "环拒绝(b→a 后 a→b 成环)"
    );
}
