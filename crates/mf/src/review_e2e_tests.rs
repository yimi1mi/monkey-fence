//! 复审阻塞项 15:真实 AppCtx/Orchestrator 端到端。
//!
//! 全链走真实生产路径:插件宿主(内置贡献)→ 目录库实例 + Secret →
//! 模板分配(assign_workflow 编译+pin)→ confirm_and_run → 真实 PTY
//! 子进程(cmd.exe)→ 显式结算 → worktree 汇合回项目目录 → 任务收敛;
//! Default CLI 只读外部配置(CLAUDE_CONFIG_DIR 前后哈希不变)。

use crate::app_ctx::AppCtx;
use mf_agent::model::{RunStatus, TaskStatus};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

fn dir_hash(path: &Path) -> String {
    // 目录内容稳定指纹(相对路径 + 文件字节)
    let mut acc = String::new();
    fn walk(prefix: &str, path: &Path, acc: &mut String) {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .map(|it| it.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if entry.path().is_dir() {
                walk(&rel, &entry.path(), acc);
            } else {
                let bytes = std::fs::read(entry.path()).unwrap_or_default();
                acc.push_str(&format!("{rel}:{}:", bytes.len()));
            }
        }
    }
    walk("", path, &mut acc);
    acc
}

fn git_repo_with_commit(root: &Path) {
    let git = mf_vcs::git::Git::init(root).unwrap();
    std::fs::write(root.join("README.md"), "seed\n").unwrap();
    git.stage(&[PathBuf::from("README.md")]).unwrap();
    git.commit("seed").unwrap();
}

fn e2e_instance_draft(executable: &str, argv: &[&str]) -> mf_agent::AgentInstanceDraft {
    mf_agent::AgentInstanceDraft {
        name: "e2e-worker".into(),
        agent_type: "claude".into(),
        scope: mf_agent::InstanceScope::User,
        project_key: None,
        enabled: true,
        run_mode: mf_agent::RunMode::OneShot,
        executable: executable.into(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({
            "input": "stdin",
            "completion": "process-exit"
        }),
        sealed_secret_ids: vec![],
    }
}

#[test]
fn full_workflow_e2e_real_process_secret_pin_merge() {
    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog = mf_agent::CatalogStore::open(&catalog_dir.path().join("catalog.db")).unwrap();
    let ctx = AppCtx::with_parts_opt(mf_agent::Config::default(), catalog.clone(), false);
    // 确定性 Secret 主密钥(此环境 keyring 在测试进程内不可靠;先于 open_project)
    ctx.set_secret_master_key_for_tests([42u8; 32]);

    // 项目 = 真实 Git 仓库(worktree 隔离提供器 + 汇合)
    let project = tempfile::tempdir().unwrap();
    git_repo_with_commit(project.path());
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();

    // 1) Secret 密封 + 实例引用(只存引用)
    let secret_id = ctx.seal_secret("MY_TOKEN", "sk-e2e-secret-value").unwrap();
    let mut draft = e2e_instance_draft(
        &std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
        &["/C", "set MY_TOKEN>token.txt"],
    );
    draft.config = serde_json::json!({ "secret_env": { "MY_TOKEN": secret_id } });
    draft.sealed_secret_ids = vec![secret_id.clone()];
    let instance = ctx.catalog_store.create_agent_instance(draft).unwrap();

    // 2) 模板分配(编译 + 插件 pin + Revision 冻结)
    let task = orch
        .create_task("E2E 发布检查", "把检查产物落到项目目录")
        .unwrap();
    let version = ctx
        .catalog_store
        .save_template(&mf_agent::workflow::WorkflowTemplateDraft {
            key: "e2e-release".into(),
            name: "E2E 发布检查".into(),
            task_local: false,
            nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
                key: "check".into(),
                title: "检查".into(),
                instructions: "输出环境变量到 token.txt".into(),
                agent_instance_id: instance.id.clone(),
                deps: vec![],
            }],
        })
        .unwrap();
    ctx.assign_workflow(project.path(), task.id, version.version_id, false)
        .unwrap();
    // pin 已按任务 run_key 固定
    let pins = catalog.list_plugin_pins().unwrap();
    assert!(
        pins.iter()
            .any(|p| p.run_key == format!("task-{}", task.id)),
        "工作流分配必须 pin 插件: {pins:?}"
    );

    // 3) 运行:真实 PTY 子进程(worktree 目录内执行)
    orch.confirm_and_run(task.id).unwrap();
    let token = {
        let deadline = Duration::from_secs(20);
        assert!(
            wait_until(deadline, || orch
                .store
                .list_runs_of_task(task.id)
                .map(|runs| !runs.is_empty())
                .unwrap_or(false)),
            "等待真实派发超时"
        );
        orch.store
            .list_runs_of_task(task.id)
            .unwrap()
            .into_iter()
            .max_by_key(|r| r.id)
            .unwrap()
            .capability_token
    };

    // 4) 等真实进程退出(Exited 事件 → awaiting-outcome)再结算:
    //    结算触发汇合,文件必须已落盘
    let settle_deadline = Duration::from_secs(30);
    assert!(
        wait_until(settle_deadline, || orch
            .store
            .list_runs_of_task(task.id)
            .unwrap()
            .first()
            .map(|r| matches!(r.status, RunStatus::AwaitingOutcome | RunStatus::Failed))
            .unwrap_or(false)),
        "等待真实进程退出超时,状态 {:?}",
        orch.store
            .list_runs_of_task(task.id)
            .unwrap()
            .first()
            .map(|r| r.status)
    );
    for r in orch.store.list_runs_of_task(task.id).unwrap() {
        eprintln!(
            "PROBE pre-settle status={:?} payload={:?}",
            r.status, r.outcome_payload
        );
    }
    for r in orch.store.list_runs_of_task(task.id).unwrap() {
        eprintln!(
            "PROBE run status={:?} outcome={:?} payload={:?}",
            r.status, r.outcome, r.outcome_payload
        );
    }
    let outcome = orch
        .settle_by_token(
            &token,
            mf_agent::Settlement::Complete {
                summary: "E2E 检查完成".into(),
            },
        )
        .unwrap();
    assert_ne!(outcome, mf_agent::SettleOutcome::AlreadyApplied);

    assert!(
        wait_until(Duration::from_secs(30), || orch
            .store
            .task_view(task.id)
            .unwrap()
            .map(|t| t.status == TaskStatus::Succeeded)
            .unwrap_or(false)),
        "任务应收敛成功,实际 {:?}",
        orch.store.task_view(task.id).unwrap().map(|t| t.status)
    );

    // 汇合结果:Secret 经真实进程环境变量写进 token.txt 并合并回项目目录
    let merged = project.path().join("token.txt");
    assert!(
        wait_until(Duration::from_secs(10), || merged.is_file()),
        "worktree 变更应汇合回项目目录"
    );
    let content = std::fs::read_to_string(&merged).unwrap();
    assert!(
        content.contains("sk-e2e-secret-value"),
        "Secret 必须经环境变量送达真实进程: {content}"
    );
    // 租约释放:worktree 目录清理
    assert!(
        wait_until(Duration::from_secs(10), || {
            !merged.parent().unwrap().join(".worktrees").exists()
                || std::fs::read_dir(merged.parent().unwrap().join(".worktrees"))
                    .map(|mut it| it.next().is_none())
                    .unwrap_or(true)
        }),
        "worktree 应在汇合后释放"
    );
    // 任务成功后插件 pin 释放
    assert!(
        wait_until(Duration::from_secs(10), || !catalog
            .list_plugin_pins()
            .unwrap()
            .iter()
            .any(|p| p.run_key == format!("task-{}", task.id))),
        "任务成功后应释放插件 pin"
    );

    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

#[test]
fn default_cli_leaves_external_config_untouched() {
    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog = mf_agent::CatalogStore::open(&catalog_dir.path().join("catalog.db")).unwrap();
    let ctx = AppCtx::with_parts_opt(mf_agent::Config::default(), catalog, false);
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    let task = orch.create_task("默认 CLI", "只读外部配置").unwrap();

    // 外部配置目录(哨兵文件)+ 前置哈希
    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(config_dir.path().join("settings.json"), "{\"keep\":true}\n").unwrap();
    let before = dir_hash(config_dir.path());

    // Default CLI 离散会话:external_config = true(只读外部已有配置)
    let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    let snapshot = mf_agent::AgentInstanceSnapshot {
        id: "default-cli-e2e".into(),
        name: "默认 CLI E2E".into(),
        agent_type: "claude".into(),
        version: 0,
        enabled: true,
        run_mode: mf_agent::RunMode::Interactive,
        executable: cmd,
        argv: vec!["/C".into(), "exit".into(), "0".into()],
        env: vec![(
            "CLAUDE_CONFIG_DIR".into(),
            config_dir.path().to_string_lossy().to_string(),
        )],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({ "completion": "manual" }),
        sealed_secret_ids: vec![],
    };
    ctx.create_ad_hoc_session(
        project.path(),
        task.id,
        &snapshot,
        mf_agent::RunMode::Interactive,
        true,
    )
    .unwrap();

    // 外部配置目录内容不变(只读,绝不写入)
    std::thread::sleep(Duration::from_millis(500));
    let after = dir_hash(config_dir.path());
    assert_eq!(before, after, "外部配置目录必须保持原样");

    // 会话注册在 display ID 键下(真实进程)
    let sessions = orch.store.list_sessions().unwrap();
    assert!(
        sessions
            .iter()
            .any(|s| s.status != mf_agent::SessionStatus::Idle),
        "离散会话应已启动"
    );
    // 清理:杀掉进程(展示会话键)
    for s in orch.store.list_sessions().unwrap() {
        if !matches!(
            s.status,
            mf_agent::SessionStatus::Dead | mf_agent::SessionStatus::Hidden
        ) {
            ctx.registry
                .kill_session(&project.path().to_string_lossy(), s.id);
        }
    }
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

// ---------- GPUI 实体挂载(四入口 + RunMonitor) ----------

#[gpui::test]
fn workspace_entities_mount_and_run_monitor_projects(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog = mf_agent::CatalogStore::open(&catalog_dir.path().join("catalog.db")).unwrap();
    let ctx = AppCtx::with_parts_opt(mf_agent::Config::default(), catalog, false);
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    let task = orch.create_task("实体挂载", "g").unwrap();

    // `+` 菜单四入口(终端/默认 CLI/实例/临时实例)来自真实插件贡献
    let sidebar = cx.new(|cx| crate::task_sidebar::TaskSidebar::new(ctx.clone(), cx));
    let menu = cx.read_entity(&sidebar, |s: &crate::task_sidebar::TaskSidebar, _| {
        s.build_menu()
    });
    let kinds: Vec<&str> = menu
        .iter()
        .map(|e| match e.kind {
            crate::task_cli_menu::MenuKind::Terminal => "terminal",
            crate::task_cli_menu::MenuKind::DefaultCli => "default-cli",
            crate::task_cli_menu::MenuKind::AgentInstance => "instance",
            crate::task_cli_menu::MenuKind::TemporaryInstance => "temporary",
        })
        .collect();
    for required in ["terminal", "default-cli", "temporary"] {
        assert!(
            kinds.contains(&required),
            "+ 菜单缺少 {required}: {kinds:?}"
        );
    }

    // RunMonitor 实体:任务选择 → 投影加载(真实 Step 投影)
    let monitor = cx.new(|cx| crate::run_monitor::RunMonitor::new(ctx.clone(), cx));
    cx.update_entity(&monitor, |m: &mut crate::run_monitor::RunMonitor, cx| {
        m.set_task(Some((project.path().to_path_buf(), task.id)), cx)
    });
    let details = cx.read_entity(&monitor, |m: &crate::run_monitor::RunMonitor, _| {
        m.snapshot_node_count()
    });
    // 任务无工作流 → 0 节点(但 set_task 路径真实执行)
    assert_eq!(details, 0);

    // 实例页与画布实体可构造(挂载在 AgentWorkspace 的同款实体)
    let _instances =
        cx.new(|cx| crate::agent_instances_view::AgentInstancesPage::new(ctx.clone(), cx));
    let _canvas = cx.new(|cx| crate::workflow_canvas::WorkflowCanvas::new(ctx.clone(), cx));

    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}
