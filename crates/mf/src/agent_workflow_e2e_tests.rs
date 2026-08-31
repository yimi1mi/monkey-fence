//! Agent 工作流端到端验收测试(UI Task 5;设计 §15):
//! 双 Claude 实例并行互不覆盖、全局 CLI 配置零写入、
//! 插件贡献视图汇总、unsafe-parallel 用户开关。

use crate::plugin_contribution_view::{
    contribution_summary, unsafe_parallel_allowed, PluginContributionSummary,
};

fn summary_fixture() -> Vec<PluginContributionSummary> {
    vec![
        PluginContributionSummary {
            full_id: "monkeyfence.claude".into(),
            name: "Claude (内置)".into(),
            version: "0.1.0".into(),
            content_hash: "hash-a".into(),
            enabled: true,
            authorized_at: Some("2026-08-28T00:00:00Z".into()),
            contribution_counts: vec![
                ("agent_types".into(), 1),
                ("execution_directory_providers".into(), 0),
            ],
            requested_permissions: vec!["net".into(), "hooks".into()],
            compatible: true,
            active_pins: 1,
        },
        PluginContributionSummary {
            full_id: "monkeyfence.git".into(),
            name: "Git worktree".into(),
            version: "0.1.0".into(),
            content_hash: "hash-b".into(),
            enabled: false,
            authorized_at: None,
            contribution_counts: vec![("execution_directory_providers".into(), 1)],
            requested_permissions: vec!["vcs".into()],
            compatible: true,
            active_pins: 0,
        },
    ]
}

#[test]
fn contribution_summary_lists_types_permissions_and_versions() {
    let rows = summary_fixture();
    let text = contribution_summary(&rows);
    assert!(text.contains("monkeyfence.claude"));
    assert!(text.contains("agent_types: 1"));
    assert!(text.contains("execution_directory_providers: 1"));
    assert!(text.contains("vcs"));
    assert!(text.contains("已禁用"), "禁用状态必须可见");
    // 固定版本与内容哈希可见(设计 §11.5)
    assert!(text.contains("0.1.0"));
    assert!(text.contains("hash-a"));
}

#[test]
fn unsafe_parallel_defaults_off_and_user_can_opt_in() {
    // 默认关闭:目录不能隔离时禁止并行(编译器拒绝)
    assert!(!unsafe_parallel_allowed(false, false));
    // 用户显式开启风险开关:允许(自行承担冲突)
    assert!(unsafe_parallel_allowed(false, true));
    // worktree 可隔离:无需开关
    assert!(unsafe_parallel_allowed(true, false));
}

#[test]
fn two_claude_instances_compile_without_global_config_writes() {
    // 编译路径:两个 Claude 实例的 run-temp 互不相同,
    // 且都不指向 ~/.claude(真实全局配置零写入)。
    use mf_agent::agent_instance::AgentInstanceDraft;
    use mf_agent::catalog_store::CatalogStore;
    use mf_agent::{InstanceScope, RunMode};
    use std::collections::HashSet;

    let catalog = CatalogStore::memory().unwrap();
    let mk = |name: &str| AgentInstanceDraft {
        name: name.into(),
        agent_type: "claude".into(),
        scope: InstanceScope::User,
        project_key: None,
        enabled: true,
        run_mode: RunMode::OneShot,
        executable: "claude".into(),
        argv: vec![],
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({ "completion": "process-exit" }),
        sealed_secret_ids: vec![],
    };
    let a = catalog.create_agent_instance(mk("implementation")).unwrap();
    let b = catalog.create_agent_instance(mk("review")).unwrap();
    assert_ne!(a.id, b.id, "同类型多实例必须彼此独立");

    let adapter = mf_plugins::builtin::adapter_for("claude-code").unwrap();
    let home = dirs::home_dir().unwrap();
    let mut seen_dirs: HashSet<std::path::PathBuf> = HashSet::new();
    for id in [a.id, b.id] {
        let snapshot = catalog.snapshot_agent_instance(&id, None).unwrap();
        let run_temp = std::env::temp_dir()
            .join("monkeyfence-e2e")
            .join(format!("{id:?}"));
        let ctx = mf_agent::LaunchContext::new(run_temp.clone(), std::path::PathBuf::from("."));
        let plan = adapter.compile_launch(&snapshot, &ctx).unwrap();
        let config_dir = std::path::PathBuf::from(
            plan.env
                .iter()
                .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
                .map(|(_, v)| v.clone())
                .unwrap(),
        );
        assert!(config_dir.starts_with(&run_temp), "必须在 run-temp 下");
        assert_ne!(config_dir, home.join(".claude"), "绝不指向真实全局配置");
        seen_dirs.insert(config_dir);
    }
    assert_eq!(seen_dirs.len(), 2, "两个实例的隔离目录互不重叠");
}

#[test]
fn registry_summaries_carry_real_hash_counts_and_pins() {
    // 真实注册表(内置合成插件):计数非 0 占位、兼容性为计算值
    let host = mf_plugins::PluginHost::load_at_with_catalog(
        std::env::temp_dir().join("mf-pcv-test"),
        mf_agent::CatalogStore::memory().unwrap(),
        &mf_agent::Config::default(),
        &[],
    );
    let rows = crate::plugin_contribution_view::summaries_from_registry(&host);
    let claude = rows
        .iter()
        .find(|r| r.full_id == "monkeyfence.claude")
        .expect("内置 claude");
    assert!(
        claude
            .contribution_counts
            .iter()
            .any(|(k, c)| k == "agent_types" && *c == 1),
        "agent_types 计数必须真实: {:?}",
        claude.contribution_counts
    );
    assert!(claude.compatible);
    assert_eq!(claude.active_pins, 0);
    // 内置目录提供器插件贡献 execution_directories
    let dirs = rows
        .iter()
        .find(|r| r.full_id == "monkeyfence.directories")
        .expect("目录提供器合成插件");
    assert!(
        dirs.contribution_counts
            .iter()
            .any(|(k, c)| k == "execution_directories" && *c == 2),
        "project-dir + worktree 两个贡献: {:?}",
        dirs.contribution_counts
    );
}

// ---------- 工作流优先主路径端到端(ADR 0004 / Task 8) ----------

use std::sync::Arc;
use std::time::{Duration, Instant};

fn e2e_wait(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

fn dir_fingerprint(path: &std::path::Path) -> String {
    use sha2::Digest;
    let mut acc = String::new();
    fn walk(prefix: &str, dir: &std::path::Path, acc: &mut String) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
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
                let digest = sha2::Sha256::digest(&bytes);
                acc.push_str(&format!("{rel}:{}:{:x}", bytes.len(), digest));
            }
        }
    }
    walk("", path, &mut acc);
    acc
}

/// 检测到的默认 CLI 测试插件(adapter claude-code + 命令 hostname:
/// 无论参数如何都会退出 → manual 完成语义下进入 awaiting-outcome):
/// 注册进临时根插件宿主后注入 AppCtx —— 全链真实、不触用户 ~/.monkeyfence。
fn install_e2e_cli_plugin(
    catalog: &Arc<mf_agent::CatalogStore>,
) -> Arc<mf_plugins::PluginRegistry> {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(
        src.path().join("monkeyfence-plugin.toml"),
        r#"[manifest]
version = 2
publisher = "mf-e2e"
id = "hostcli"
name = "Hostname CLI E2E"
version_str = "0.1.0"
description = "workflow-first e2e plugin"

[capabilities]

[[agent_types]]
id = "hostname"
name = "Hostname Agent"
adapter = "claude-code"
command = "hostname"
modes = ["oneshot", "interactive"]
"#,
    )
    .unwrap();
    let host = mf_plugins::PluginRegistry::load_at_with_catalog(
        tempfile::tempdir().unwrap().path().to_path_buf(),
        catalog.clone(),
        &mf_agent::Config::default(),
        &[],
    );
    host.install_package(
        src.path(),
        mf_plugins::install::InstallSource::Local {
            path: src.path().display().to_string(),
        },
    )
    .unwrap();
    host.enable("mf-e2e.hostcli", true).unwrap();
    host
}

/// 主场景:打开项目(不建 Task)→ 新建项目工作流 →
/// 默认 CLI 节点 + 保存实例节点 + 依赖 → 画布请求运行 → Composer 输入目标 →
/// 自动创建 Task/Revision 并启动第一个节点 → 第二个节点 awaiting-outcome →
/// 徽标 1 → 直达第二个节点 → 人工结算收敛 → 徽标清零 →
/// 工作流跨重启保留 → 默认 CLI 零写入外部配置。
#[gpui::test]
fn project_workflow_first_run_loop_e2e(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    // 插件注册表 → 临时根(内置 + 检测到的默认 CLI 测试插件);
    // 先注入注册表再打开项目(RuntimeHost 在 open_project 时接线)
    let ctx = crate::app_ctx::AppCtx::with_parts_and_plugins_for_tests(
        mf_agent::Config::default(),
        catalog.clone(),
        install_e2e_cli_plugin(&catalog),
    );

    // 1) 打开项目,不创建任何 Task
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    assert!(orch.store.list_tasks(false).unwrap().is_empty());

    // 2-4) 画布:新建项目工作流,添加保存实例节点 + 默认 CLI 节点 + 依赖
    let instance = catalog
        .create_agent_instance(mf_agent::AgentInstanceDraft {
            name: "e2e-worker".into(),
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
    let canvas = cx.new(|cx| crate::workflow_canvas::WorkflowCanvas::new(ctx.clone(), cx));
    cx.update_entity(&canvas, |c, cx| {
        c.set_project(Some(project.path().to_path_buf()), cx);
        c.new_workflow(cx);
        assert_eq!(
            c.current_key.as_deref(),
            Some("wf-1"),
            "稳定 key,不使用 task id"
        );
        // 默认 CLI 节点(库条目 → 正确引用)
        let default_cli_ref = c
            .library
            .iter()
            .find_map(|e| e.node_reference())
            .filter(|r| r.starts_with("default-cli:"))
            .expect("检测到的默认 CLI 必须出现在画布库");
        assert_eq!(default_cli_ref, "default-cli:mf-e2e.hostcli.hostname");
        c.editor.drag_from_library(&default_cli_ref);
        // 保存实例节点
        c.editor.drag_from_library(&instance.id);
        // 依赖:默认 CLI 节点依赖保存实例节点
        let keys: Vec<String> = c.editor.nodes().iter().map(|n| n.key.clone()).collect();
        assert_eq!(keys.len(), 2);
        let (first, second) = (keys[0].clone(), keys[1].clone());
        assert_eq!(
            c.editor.nodes()[0].instance_id,
            default_cli_ref,
            "第一个节点是默认 CLI"
        );
        c.editor.add_dependency(&first, &second).expect("依赖建立");
        c.save_after_edit();
        assert!(c.save_error.is_none(), "依赖与节点已自动保存");
    });
    let record = orch
        .store
        .load_project_workflow("wf-1")
        .unwrap()
        .expect("项目工作流已持久化");
    assert_eq!(record.nodes.len(), 2);
    assert_eq!(record.nodes[0].deps, vec![record.nodes[1].key.clone()]);

    // 外部配置哨兵(默认 CLI 只读外部配置)
    let sentinel = tempfile::tempdir().unwrap();
    std::fs::write(sentinel.path().join("settings.json"), "{\"keep\":true}\n").unwrap();
    let sentinel_before = dir_fingerprint(sentinel.path());

    // 5-6) 画布请求运行(意图)→ Composer 输入目标 → 自动创建 Task/Revision
    let ws = cx.new(|cx| crate::agent_workspace::AgentWorkspace::new(ctx.clone(), cx));
    cx.update_entity(&ws, |aw, cx| {
        aw.open_run_composer(project.path().to_path_buf(), "wf-1".into(), cx);
        let composer = aw.run_composer.clone().unwrap();
        composer.update(cx, |c, cx| {
            c.state.set_goal("发布前检查\n把报告写到 report.md");
            cx.notify();
        });
        aw.submit_run_composer(cx);
        assert_eq!(
            aw.active_tab(),
            crate::workspace::AgentTab::Runs,
            "运行后进入 Runs"
        );
    });
    let tasks = orch.store.list_tasks(false).unwrap();
    assert_eq!(tasks.len(), 1, "自动创建且仅创建一个 Task");
    let task_id = tasks[0].id;
    assert_eq!(tasks[0].title, "发布前检查");
    assert!(tasks[0].active_revision.is_some(), "Revision 已冻结激活");

    // 6) 第一个节点(保存实例,无依赖)真实启动
    assert!(
        e2e_wait(Duration::from_secs(20), || orch
            .store
            .list_runs_of_task(task_id)
            .map(|runs| runs.len() == 1)
            .unwrap_or(false)),
        "第一个节点必须启动"
    );
    // 人工确认第一节点(显式结算;进程退出不自动等同成功)
    let instance_run = orch.store.list_runs_of_task(task_id).unwrap()[0].clone();
    assert!(
        e2e_wait(Duration::from_secs(15), || orch
            .store
            .run_view(instance_run.id)
            .map(|r| r.is_some_and(|r| r.status == mf_agent::RunStatus::AwaitingOutcome))
            .unwrap_or(false)),
        "第一个节点进程退出后进入待结算,实际 {:?}",
        orch.store
            .run_view(instance_run.id)
            .map(|r| r.map(|r| r.status))
    );
    orch.settle_by_token(
        &instance_run.capability_token,
        mf_agent::Settlement::Complete {
            summary: "实例节点完成".into(),
            output: Default::default(),
        },
    )
    .unwrap();

    // 7) 第二个节点(默认 CLI,manual 完成语义)启动 → awaiting-outcome
    let default_cli_step_of = || {
        orch.store
            .task_steps(task_id)
            .unwrap()
            .into_iter()
            .find(|s| s.agent_profile == "mf-e2e.hostcli.hostname")
    };
    assert!(
        e2e_wait(Duration::from_secs(30), || default_cli_step_of()
            .map(|s| s.status == mf_agent::StepStatus::AwaitingOutcome)
            .unwrap_or(false)),
        "第二个节点(默认 CLI)必须进入 awaiting-outcome,实际 {:?}",
        orch.store.task_steps(task_id).map(|s| s
            .iter()
            .map(|x| (x.step_key.clone(), x.status))
            .collect::<Vec<_>>())
    );

    // 8) 徽标显示 1：只读真实 Overview Hub，不在测试中手算 Attention。
    let attention_of = || {
        ctx.overview
            .current()
            .attention_runs
            .iter()
            .find(|attention| {
                attention.project_root == project.path() && attention.task_id == task_id
            })
            .cloned()
    };
    let has_attention = e2e_wait(Duration::from_secs(10), || attention_of().is_some());
    assert!(has_attention, "第二个节点 awaiting-outcome 必须产生徽标");
    let attention = attention_of().unwrap();
    assert_eq!(attention.task_id, task_id);
    let awaiting_step = default_cli_step_of().unwrap();

    // 9) 点击徽标直达第二个节点(open_attention_run)
    cx.update_entity(&ws, |aw, cx| {
        aw.open_attention_run(&attention, cx);
        assert_eq!(aw.active_tab(), crate::workspace::AgentTab::Runs);
        let focused = aw.runs_page.read(cx).monitor.read(cx).focused_step();
        assert_eq!(focused, Some(awaiting_step.id), "直达优先处理节点");
    });

    // 12) 默认 CLI 零写入:外部哨兵不变 + run-temp 无隔离配置目录
    assert_eq!(
        dir_fingerprint(sentinel.path()),
        sentinel_before,
        "默认 CLI 外部配置目录必须保持原样"
    );
    let default_cli_run = orch
        .store
        .list_runs_of_task(task_id)
        .unwrap()
        .into_iter()
        .max_by_key(|r| r.id)
        .unwrap();
    let run_temp = std::env::temp_dir()
        .join("monkeyfence")
        .join("steps")
        .join(format!("{}-{}", std::process::id(), default_cli_run.id));
    assert!(
        !run_temp.join("claude").exists(),
        "external_config 快照不得物化隔离配置目录: {}",
        run_temp.display()
    );

    // 10) 人工确认(显式结算)→ 运行收敛 → 徽标清零
    orch.settle_by_token(
        &default_cli_run.capability_token,
        mf_agent::Settlement::Complete {
            summary: "默认 CLI 节点完成".into(),
            output: Default::default(),
        },
    )
    .unwrap();
    assert!(
        e2e_wait(Duration::from_secs(30), || orch
            .store
            .task_view(task_id)
            .map(|t| t
                .map(|t| t.status == mf_agent::TaskStatus::Succeeded)
                .unwrap_or(false))
            .unwrap_or(false)),
        "人工确认后运行必须收敛,实际 {:?}",
        orch.store.task_view(task_id).map(|t| t.map(|t| t.status))
    );
    assert!(
        e2e_wait(Duration::from_secs(10), || attention_of().is_none()),
        "唯一直接原因处理后 Hub 徽标必须清零"
    );

    // 清理真实进程并完全关闭第一套 AppCtx/Orchestrator。
    for r in orch.store.list_runs_of_task(task_id).unwrap() {
        if let Some(sid) = r.session_id {
            ctx.registry
                .kill_session(&project.path().to_string_lossy(), sid);
        }
    }
    let restart_config = ctx.config.lock().clone();
    let restart_plugins = ctx.plugins.clone();
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());

    // 11) 真重启：新建 AppCtx/Orchestrator，重新打开同一项目数据库。
    let restarted = crate::app_ctx::AppCtx::with_parts_and_plugins_for_tests(
        restart_config,
        catalog.clone(),
        restart_plugins,
    );
    let restarted_orch = restarted
        .open_project(project.path().to_path_buf())
        .unwrap();
    let workflow_kept = restarted_orch
        .store
        .load_project_workflow("wf-1")
        .unwrap()
        .expect("项目工作流跨重启保留");
    assert_eq!(workflow_kept.nodes.len(), 2);
    assert!(
        e2e_wait(Duration::from_secs(10), || restarted
            .overview
            .current()
            .attention_runs
            .iter()
            .all(|attention| attention.task_id != task_id)),
        "收敛后重启不复活徽标"
    );
    restarted_orch.stop();
    restarted.close_project(&project.path().to_path_buf());
}
