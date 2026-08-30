//! Workflow Canvas 保存链(I8/I9):save_draft 返回 Result,
//! 保存失败必须中止 compile/assign/confirm(绝不继续读取旧 Store
//! 草稿并运行);同内容保存不刷新 updated_at(不产生重复 Revision/pin)。

/// I9 回归:画布清空后确认运行 —— save_draft 必须以错误中止,
/// 绝不能继续读取 Store 里的旧草稿并运行。
#[gpui::test]
fn canvas_confirm_aborts_when_save_fails_instead_of_running_stale_draft(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::AppContext as _;
    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog = mf_agent::CatalogStore::open(&catalog_dir.path().join("catalog.db")).unwrap();
    let ctx = crate::app_ctx::AppCtx::with_parts_opt(mf_agent::Config::default(), catalog, false);
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    let task = orch.create_task("保存失败中止", "g").unwrap();

    // 直接在 Store 放一份合法草稿(两个节点;实例用内置 generic-command)
    let instance = ctx
        .catalog_store
        .create_agent_instance(mf_agent::agent_instance::AgentInstanceDraft {
            name: "worker".into(),
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
        .unwrap();
    let draft = mf_agent::workflow::WorkflowTemplateDraft {
        key: format!("task-{}", task.id),
        name: "本地".into(),
        task_local: true,
        nodes: vec![
            mf_agent::workflow::WorkflowNodeDraft {
                key: "a".into(),
                title: "A".into(),
                instructions: "做 A".into(),
                agent_instance_id: instance.id.clone(),
                deps: vec![],
            },
            mf_agent::workflow::WorkflowNodeDraft {
                key: "b".into(),
                title: "B".into(),
                instructions: "做 B".into(),
                agent_instance_id: instance.id.clone(),
                deps: vec!["a".into()],
            },
        ],
    };
    orch.store
        .save_task_workflow(&project.path().to_string_lossy(), task.id, &draft, false)
        .unwrap();

    // 画布:选中任务加载草稿 → 清空编辑器(保存必然失败:空工作流)
    let canvas = cx.new(|cx| crate::workflow_canvas::WorkflowCanvas::new(ctx.clone(), cx));
    cx.update_entity(
        &canvas,
        |c: &mut crate::workflow_canvas::WorkflowCanvas, cx| {
            c.set_selected_task(Some((project.path().to_path_buf(), task.id)), cx);
        },
    );
    cx.update_entity(
        &canvas,
        |c: &mut crate::workflow_canvas::WorkflowCanvas, cx| {
            c.editor.load_nodes(Vec::new());
            c.confirm_and_run_task_local(cx);
        },
    );
    let (status, still_draft_status) = cx
        .read_entity(&canvas, |c: &crate::workflow_canvas::WorkflowCanvas, _| {
            (c.status.clone(), c.status.clone())
        });
    let _ = still_draft_status;
    assert!(
        status.contains("空工作流") || status.contains("保存失败"),
        "保存失败必须体现在状态栏: {status}"
    );
    // 绝不能继续读取旧 Store 草稿并运行
    let t = orch.store.task_view(task.id).unwrap().unwrap();
    assert_ne!(
        t.status,
        mf_agent::model::TaskStatus::Running,
        "保存失败必须中止确认运行(实际 {:?})",
        t.status
    );
    assert!(
        orch.store.list_revision_ids(task.id).unwrap().is_empty(),
        "保存失败不得冻结 Revision(不得读取旧草稿继续)"
    );

    // 恢复画布内容后确认照常运行(中止不破坏正常路径)
    cx.update_entity(
        &canvas,
        |c: &mut crate::workflow_canvas::WorkflowCanvas, cx| {
            c.editor.load_nodes(
                draft
                    .nodes
                    .iter()
                    .map(|n| crate::workflow_editor::EditorNode {
                        key: n.key.clone(),
                        title: n.title.clone(),
                        instance_id: n.agent_instance_id.clone(),
                        deps: n.deps.clone(),
                        instructions: n.instructions.clone(),
                    })
                    .collect(),
            );
            c.confirm_and_run_task_local(cx);
        },
    );
    let t = orch.store.task_view(task.id).unwrap().unwrap();
    assert_eq!(
        t.status,
        mf_agent::model::TaskStatus::Running,
        "恢复后确认运行必须照常工作"
    );
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}
