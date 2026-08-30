//! RunMonitor 确认交互(GPUI 实体级):危险动作第一次点击只设置
//! 待确认意图、不执行;显式确认后恰好执行一次;重复确认/无意图
//! 确认为 no-op。

#[gpui::test]
fn run_monitor_confirm_gate_first_click_holds_and_confirm_executes_once(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::AppContext as _;
    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog = mf_agent::CatalogStore::open(&catalog_dir.path().join("catalog.db")).unwrap();
    let ctx = crate::app_ctx::AppCtx::with_parts_opt(mf_agent::Config::default(), catalog, false);
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    let task = orch.create_task("确认门", "g").unwrap();

    // task-local 单节点草稿(opencode 类型 + cmd.exe /K 常驻)→ 确认运行
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
        nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
            key: "a".into(),
            title: "A".into(),
            instructions: "做 A".into(),
            agent_instance_id: instance.id.clone(),
            deps: vec![],
        }],
    };
    let root_key = project.path().to_string_lossy().to_string();
    orch.store
        .save_task_workflow(&root_key, task.id, &draft, false)
        .unwrap();
    let index = crate::adapter_launch::workflow_plugin_index(&ctx.plugins);
    orch.assign_and_confirm_task_local(task.id, &index).unwrap();

    // 等待 run 真实运行 + 租约持有
    let run_id = loop {
        let runs = orch.store.list_runs_of_task(task.id).unwrap();
        if let Some(r) = runs
            .iter()
            .find(|r| r.status == mf_agent::model::RunStatus::Running)
        {
            break r.id;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    };
    assert!(wait_until_std(std::time::Duration::from_secs(5), || {
        orch.store
            .list_execution_leases(task.id)
            .map(|ls| ls.iter().any(|l| l.status == "held"))
            .unwrap_or(false)
    }));

    let monitor = cx.new(|cx| crate::run_monitor::RunMonitor::new(ctx.clone(), cx));
    cx.update_entity(&monitor, |m: &mut crate::run_monitor::RunMonitor, cx| {
        m.set_task(Some((project.path().to_path_buf(), task.id)), cx)
    });

    // 找到带 Cancel 动作的节点索引
    let cancel_idx = cx.read_entity(&monitor, |m: &crate::run_monitor::RunMonitor, _| {
        m.node_details_for_test()
            .iter()
            .position(|d| d.actions.contains(&crate::run_monitor::RunAction::Cancel))
            .expect("运行中的节点必须提供 Cancel 动作")
    });

    // 第一次点击 Cancel:只设置待确认意图,不执行
    cx.update_entity(&monitor, |m: &mut crate::run_monitor::RunMonitor, cx| {
        m.run_action(cancel_idx, crate::run_monitor::RunAction::Cancel, cx)
    });
    let (pending, status) = cx.read_entity(&monitor, |m: &crate::run_monitor::RunMonitor, _| {
        (m.has_pending_confirm(), m.status_text().to_string())
    });
    assert!(pending, "第一次点击必须进入待确认状态");
    assert!(status.contains("确认取消"), "提示必须说明后果: {status}");
    // 未执行:run 仍在运行、租约仍持有
    let run = orch.store.run_view(run_id).unwrap().unwrap();
    assert_eq!(
        run.status,
        mf_agent::model::RunStatus::Running,
        "第一次点击不得执行取消"
    );
    assert!(
        orch.store
            .list_execution_leases(task.id)
            .unwrap()
            .iter()
            .any(|l| l.status == "held"),
        "第一次点击不得释放执行租约"
    );

    // 重复点击 Cancel:仍是待确认,不执行
    cx.update_entity(&monitor, |m: &mut crate::run_monitor::RunMonitor, cx| {
        m.run_action(cancel_idx, crate::run_monitor::RunAction::Cancel, cx)
    });
    assert!(
        cx.read_entity(&monitor, |m: &crate::run_monitor::RunMonitor, _| {
            m.has_pending_confirm()
        })
    );
    assert_eq!(
        orch.store.run_view(run_id).unwrap().unwrap().status,
        mf_agent::model::RunStatus::Running,
        "重复点击不得执行"
    );

    // 显式确认:恰好执行一次(进程树终止确认 → run 取消 + 租约释放)
    cx.update_entity(&monitor, |m: &mut crate::run_monitor::RunMonitor, cx| {
        m.confirm_pending(cx)
    });
    assert!(
        wait_until_std(std::time::Duration::from_secs(10), || {
            orch.store
                .run_view(run_id)
                .map(|r| {
                    r.map(|r| r.status == mf_agent::model::RunStatus::Cancelled)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        }),
        "确认后必须取消运行,实际 {:?}",
        orch.store.run_view(run_id).unwrap().map(|r| r.status)
    );
    assert!(
        wait_until_std(std::time::Duration::from_secs(5), || {
            !orch
                .store
                .list_execution_leases(task.id)
                .unwrap()
                .iter()
                .any(|l| l.status == "held")
        }),
        "确认取消后必须释放执行租约"
    );
    assert!(
        !cx.read_entity(&monitor, |m: &crate::run_monitor::RunMonitor, _| {
            m.has_pending_confirm()
        }),
        "确认后待确认意图必须清空"
    );

    // 再次确认(无待确认意图):no-op,不产生新的取消/错误
    cx.update_entity(&monitor, |m: &mut crate::run_monitor::RunMonitor, cx| {
        m.confirm_pending(cx)
    });
    assert_eq!(
        orch.store.run_view(run_id).unwrap().unwrap().status,
        mf_agent::model::RunStatus::Cancelled,
        "重复确认必须是 no-op(幂等)"
    );

    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

fn wait_until_std(timeout: std::time::Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    cond()
}
