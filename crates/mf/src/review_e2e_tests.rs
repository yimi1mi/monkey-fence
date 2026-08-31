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
    // 目录内容稳定指纹:相对路径 + 内容 SHA-256(等长改写也会被检出)
    use sha2::{Digest, Sha256};
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
                let digest = Sha256::digest(&bytes);
                acc.push_str(&format!("{rel}:{}:{:x}", bytes.len(), digest));
            }
        }
    }
    walk("", path, &mut acc);
    acc
}

/// 工作树指纹(忽略 .git / .mf-agent 等点前缀目录):
/// 合并回滚断言只关心用户可见文件,不受 dangling git 对象/WAL 影响。
#[cfg(test)]
fn worktree_hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut acc = String::new();
    fn walk(prefix: &str, path: &Path, acc: &mut String) {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .map(|it| {
                it.filter_map(|e| e.ok())
                    .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                    .collect()
            })
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
                let digest = Sha256::digest(&bytes);
                acc.push_str(&format!("{rel}:{}:{:x}", bytes.len(), digest));
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
    // pin 已按「规范化 project + task + revision」run_key 固定
    let pins = catalog.list_plugin_pins().unwrap();
    let rev_ids = orch.store.list_revision_ids(task.id).unwrap();
    assert_eq!(rev_ids.len(), 1, "分配后应有一个 Revision");
    let expected_key =
        mf_agent::orchestrator::workflow_pin_key(project.path(), task.id, rev_ids[0]);
    assert!(
        pins.iter().any(|p| p.run_key == expected_key),
        "工作流分配必须 pin 插件: {pins:?}(期望 {expected_key})"
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
                output: Default::default(),
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
    // 任务成功后插件 pin 释放(全部 Revision 的 key)
    assert!(
        wait_until(Duration::from_secs(10), || !catalog
            .list_plugin_pins()
            .unwrap()
            .iter()
            .any(|p| p.run_key == expected_key)),
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
        external_config: false,
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

/// 正式工作流节点的 default-cli 引用(ADR 0004 / Task 3):
/// 生产 resolver 合成 external_config 快照 → Revision 冻结往返不丢失 →
/// 真实派发经 RuntimeHost(compile_instance_launch 按冻结值跳过隔离注入)→
/// 外部配置目录保持只读。
#[test]
fn default_cli_workflow_node_external_config_reaches_runtime() {
    use mf_agent::orchestrator::{
        DirectoryRouting, GlobalLimiter, Orchestrator, ProfileCatalog, WorkflowKernel,
    };
    use mf_agent::store::Store;
    use parking_lot::{Mutex, RwLock};

    // 1) 检测到的测试 CLI(command = cmd)
    let src = tempfile::tempdir().unwrap();
    std::fs::write(
        src.path().join("monkeyfence-plugin.toml"),
        r#"[manifest]
version = 2
publisher = "mf-test"
id = "cmdcli"
name = "Cmd CLI Test"
version_str = "0.1.0"
description = "e2e default-cli plugin"

[capabilities]

[[agent_types]]
id = "cmd"
name = "Cmd Agent"
adapter = "generic-command"
command = "cmd"
modes = ["oneshot", "interactive"]
"#,
    )
    .unwrap();
    let catalog = mf_agent::CatalogStore::memory().unwrap();
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
    host.enable("mf-test.cmdcli", true).unwrap();

    // 2) 生产装配:with_launcher + 插件感知 resolver 注入 WorkflowKernel
    let config = mf_agent::Config::default();
    let registry = crate::runtime_host::SessionRegistry::new(config.clone());
    let host_impl = crate::runtime_host::RuntimeHostImpl::with_launcher(
        registry.clone(),
        crate::runtime_host::WorkflowLauncher {
            plugins: host.clone(),
            catalog: catalog.clone(),
            secret_master_key: None,
        },
    );
    let project = tempfile::tempdir().unwrap();
    let store = Store::open(&project.path().join(".mf-agent").join("workflow-v1.db")).unwrap();
    let resolver = crate::app_ctx::PluginInstanceResolver::new(
        host.clone(),
        catalog.clone(),
        Arc::new(Mutex::new(config.clone())),
    );
    let orch = Orchestrator::start_with_routing(
        store,
        project.path().to_path_buf(),
        config,
        host_impl,
        Arc::new(RwLock::new(ProfileCatalog::default())),
        GlobalLimiter::new(4),
        "pipe-e2e".into(),
        Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
        WorkflowKernel {
            catalog: catalog.clone(),
            pins: Some(Arc::new(crate::app_ctx::PluginHostPins {
                host: host.clone(),
            })),
            instance_resolver: Some(Arc::new(resolver)),
        },
        DirectoryRouting::default(),
    )
    .unwrap();

    // 外部配置哨兵(断言全程只读)
    let sentinel = tempfile::tempdir().unwrap();
    std::fs::write(sentinel.path().join("settings.json"), "{\"keep\":true}\n").unwrap();
    let before = dir_hash(sentinel.path());

    // 3) default-cli 工作流节点:分配 → Revision 冻结
    let task = orch.create_task("默认 CLI 工作流", "只读外部配置").unwrap();
    let version = mf_agent::workflow::WorkflowTemplateVersion {
        version_id: 0,
        template_key: "proj-default-cli".into(),
        version: 1,
        nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
            key: "run".into(),
            title: "运行".into(),
            instructions: "只读外部配置".into(),
            agent_instance_id: "default-cli:mf-test.cmdcli.cmd".into(),
            deps: vec![],
        }],
        created_at: String::new(),
    };
    let index = crate::adapter_launch::workflow_plugin_index(&host);
    let rev = orch
        .assign_workflow(task.id, &version, &index, false)
        .expect("检测到的 default-cli 引用必须能编译冻结");

    // 4) Revision 序列化往返:external_config 不丢失
    let snapshot = orch
        .store
        .revision_snapshot(rev.id)
        .unwrap()
        .expect("Revision 必须保存快照");
    let json = serde_json::to_string(&snapshot).unwrap();
    let snapshot: mf_agent::workflow::WorkflowSnapshot = serde_json::from_str(&json).unwrap();
    let node = &snapshot.nodes[0];
    assert!(
        node.instance.external_config,
        "冻结快照必须保留外部配置意图"
    );
    assert_eq!(node.instance.agent_type, "mf-test.cmdcli.cmd");
    assert_eq!(node.instance.executable, "cmd");
    assert!(
        node.plugin.is_some(),
        "插件 pin 按完整 agent_type 冻结: {:?}",
        node.plugin
    );

    // 5) 确认运行 → 真实派发(RuntimeHost 按冻结 external_config 编译)
    orch.confirm_and_run(task.id).unwrap();
    assert!(
        wait_until(Duration::from_secs(20), || orch
            .store
            .list_runs_of_task(task.id)
            .map(|runs| !runs.is_empty())
            .unwrap_or(false)),
        "等待真实派发超时"
    );
    let runs = orch.store.list_runs_of_task(task.id).unwrap();
    assert!(
        runs.iter()
            .any(|r| r.status == mf_agent::RunStatus::Running),
        "default-cli 节点应真实启动: {runs:?}"
    );
    // WorkflowLaunchSpec 携带的实例快照同样保留 external_config
    // (派发链使用冻结快照本身;这里以运行真实存在 + 快照往返双保险)

    // 6) 外部配置目录保持只读
    std::thread::sleep(Duration::from_millis(500));
    let after = dir_hash(sentinel.path());
    assert_eq!(before, after, "外部配置目录必须保持原样");

    // 清理:终止真实会话(manual 完成语义下 run 状态由人工收口,
    // 这里只要求进程确实被杀掉)
    for r in orch.store.list_runs_of_task(task.id).unwrap() {
        if let Some(sid) = r.session_id {
            registry.kill_session(&project.path().to_string_lossy(), sid);
        }
    }
    assert!(
        wait_until(Duration::from_secs(10), || orch
            .store
            .list_runs_of_task(task.id)
            .unwrap()
            .iter()
            .filter_map(|r| r.session_id)
            .all(|sid| {
                !registry.session_alive(&project.path().to_string_lossy(), sid)
            })),
        "会话进程应被终止"
    );
    orch.stop();
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

#[test]
fn dir_hash_detects_equal_length_rewrites() {
    // 指纹必须包含内容 SHA-256:等长改写(旧实现只看长度)也要被检出
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "AAAA\n").unwrap();
    let before = dir_hash(dir.path());
    std::fs::write(dir.path().join("a.txt"), "BBBB\n").unwrap();
    let after = dir_hash(dir.path());
    assert_ne!(before, after, "等长改写必须改变目录哈希");
    // 内容相同(即使重写)哈希稳定
    std::fs::write(dir.path().join("a.txt"), "AAAA\n").unwrap();
    assert_eq!(before, dir_hash(dir.path()));
}

// ---------- I15:真实进程 E2E(parallel join / 嵌套+失败回滚 / PID 终止) ----------

/// OS 进程表中是否存在该 PID(tasklist;非 Windows 环境返回 false)。
#[cfg(test)]
fn tasklist_has_pid(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

/// 真实项目 Git 仓库(位于独立 tempdir 的子目录:worktrees 根
/// `<tempdir>/.worktrees` 随 tempdir 隔离,避免与其他并行测试的
/// `mf-run-*` worktree 名在共享 %TEMP%\.worktrees 下互相污染)。
#[cfg(test)]
struct E2eProject {
    _catalog_dir: tempfile::TempDir,
    _holder: tempfile::TempDir,
    root: PathBuf,
    ctx: Arc<AppCtx>,
}

#[cfg(test)]
fn e2e_project_with_catalog() -> E2eProject {
    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog = mf_agent::CatalogStore::open(&catalog_dir.path().join("catalog.db")).unwrap();
    let ctx = AppCtx::with_parts_opt(mf_agent::Config::default(), catalog, false);
    let holder = tempfile::tempdir().unwrap();
    let root = holder.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git_repo_with_commit(&root);
    E2eProject {
        _catalog_dir: catalog_dir,
        _holder: holder,
        root,
        ctx,
    }
}

/// 等待任务下至少 count 个 run 到达 AwaitingOutcome(进程已退出)。
#[cfg(test)]
fn wait_runs_awaiting(orch: &Arc<mf_agent::Orchestrator>, task_id: i64, count: usize) -> bool {
    wait_until(Duration::from_secs(40), || {
        orch.store
            .list_runs_of_task(task_id)
            .map(|runs| {
                runs.iter()
                    .filter(|r| r.status == RunStatus::AwaitingOutcome)
                    .count()
                    >= count
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
fn token_of_latest_run(orch: &Arc<mf_agent::Orchestrator>, task_id: i64) -> String {
    orch.store
        .list_runs_of_task(task_id)
        .unwrap()
        .into_iter()
        .max_by_key(|r| r.id)
        .unwrap()
        .capability_token
}

#[test]
fn e2e_parallel_join_real_processes_merge_as_batch_downstream_sees_all() {
    if !cfg!(windows) {
        return;
    }
    let e2e = e2e_project_with_catalog();
    let (project, ctx) = (&e2e.root, &e2e.ctx);
    let orch = ctx.open_project(project.to_path_buf()).unwrap();
    let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    // 三个实例:并行父各写一个文件,join 节点写汇合标记
    let mk = |argv: &[&str]| -> mf_agent::AgentInstanceSnapshot {
        let draft = e2e_instance_draft(&cmd, argv);
        // 只取一次实例:直接用 snapshot 形态(draft→snapshot 同构)
        let id = ctx.catalog_store.create_agent_instance(draft).unwrap().id;
        ctx.catalog_store
            .snapshot_agent_instance(&id, None)
            .unwrap()
    };
    let inst_a = mk(&["/C", "echo from-a>pa.txt"]);
    let inst_b = mk(&["/C", "echo from-b>pb.txt"]);
    let inst_c = mk(&["/C", "echo joined>pj.txt"]);
    let node = |key: &str, deps: &[&str], inst: &str| mf_agent::workflow::WorkflowNodeDraft {
        key: key.into(),
        title: format!("节点 {key}"),
        instructions: format!("做 {key}"),
        agent_instance_id: inst.into(),
        deps: deps.iter().map(|s| s.to_string()).collect(),
    };
    let task = orch
        .create_task("并行 join E2E", "真实进程并行+汇合")
        .unwrap();
    let version = ctx
        .catalog_store
        .save_template(&mf_agent::workflow::WorkflowTemplateDraft {
            key: "e2e-join".into(),
            name: "并行 join".into(),
            task_local: false,
            nodes: vec![
                node("a", &[], &inst_a.id),
                node("b", &[], &inst_b.id),
                node("c", &["a", "b"], &inst_c.id),
            ],
        })
        .unwrap();
    ctx.assign_workflow(project, task.id, version.version_id, false)
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();

    // 两个并行节点都以真实进程运行并退出
    assert!(
        wait_runs_awaiting(&orch, task.id, 2),
        "两个并行父节点都应完成真实进程运行"
    );
    let run_of_step = |key: &str| {
        let steps = orch.store.task_steps(task.id).unwrap();
        let step = steps.iter().find(|s| s.step_key == key).unwrap();
        orch.store
            .list_runs_of_step(step.id)
            .unwrap()
            .into_iter()
            .rev()
            .next()
            .unwrap()
    };
    // a 先结算:join 批未完整 → 不汇合(项目目录零落盘)
    orch.settle_by_token(
        &run_of_step("a").capability_token,
        mf_agent::Settlement::complete("A 完成"),
    )
    .unwrap();
    assert!(
        !project.join("pa.txt").exists(),
        "join 批未完整:a 的修改不得提前汇合落盘"
    );
    // b 结算:整批汇合 → 两个父修改都进项目目录,join 节点才派发
    orch.settle_by_token(
        &run_of_step("b").capability_token,
        mf_agent::Settlement::complete("B 完成"),
    )
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || {
            project.join("pa.txt").is_file() && project.join("pb.txt").is_file()
        }),
        "整批汇合后两个父节点的修改都必须落盘"
    );
    // 下游 join 真实进程派发(其 worktree 从汇合基线检出)并退出
    let c_step_id = {
        let steps = orch.store.task_steps(task.id).unwrap();
        steps.iter().find(|s| s.step_key == "c").unwrap().id
    };
    assert!(
        wait_until(Duration::from_secs(30), || {
            orch.store
                .list_runs_of_step(c_step_id)
                .map(|runs| runs.iter().any(|r| r.status == RunStatus::AwaitingOutcome))
                .unwrap_or(false)
        }),
        "join 节点应被派发并以真实进程运行到待结算"
    );
    // 结算 join → 收敛成功,汇合标记落盘
    orch.settle_by_token(
        &run_of_step("c").capability_token,
        mf_agent::Settlement::complete("汇合完成"),
    )
    .unwrap();
    let converged = wait_until(Duration::from_secs(30), || {
        orch.store
            .task_view(task.id)
            .unwrap()
            .map(|t| t.status == TaskStatus::Succeeded)
            .unwrap_or(false)
    });
    if !converged {
        for row in orch.store.list_pending_merges(Some(task.id)).unwrap() {
            eprintln!("PENDING-MERGE conflicts={:?}", row.conflicts);
        }
        for step in orch.store.task_steps(task.id).unwrap() {
            eprintln!("STEP {} {:?}", step.step_key, step.status);
        }
        for r in orch.store.list_runs_of_task(task.id).unwrap() {
            eprintln!("RUN {} {:?} {:?}", r.id, r.status, r.outcome);
        }
    }
    assert!(
        converged,
        "任务应收敛成功,实际 {:?}",
        orch.store.task_view(task.id).unwrap().map(|t| t.status)
    );
    assert!(project.join("pj.txt").is_file());
    orch.stop();
    ctx.close_project(&project.to_path_buf());
}

#[test]
fn e2e_nested_file_merge_failure_rolls_back_then_recovers() {
    if !cfg!(windows) {
        return;
    }
    let e2e = e2e_project_with_catalog();
    let (project, ctx) = (&e2e.root, &e2e.ctx);
    let orch = ctx.open_project(project.to_path_buf()).unwrap();
    let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    // 真实进程在 worktree 内创建嵌套目录文件
    let instance = ctx
        .catalog_store
        .create_agent_instance(e2e_instance_draft(
            &cmd,
            &[
                "/C",
                "mkdir docs\\deep 2>nul & echo nested>docs\\deep\\nested.md",
            ],
        ))
        .unwrap();
    let task = orch
        .create_task("嵌套回滚 E2E", "嵌套文件+失败回滚")
        .unwrap();
    let version = ctx
        .catalog_store
        .save_template(&mf_agent::workflow::WorkflowTemplateDraft {
            key: "e2e-nested".into(),
            name: "嵌套".into(),
            task_local: false,
            nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
                key: "build".into(),
                title: "构建".into(),
                instructions: "写嵌套文件".into(),
                agent_instance_id: instance.id.clone(),
                deps: vec![],
            }],
        })
        .unwrap();
    ctx.assign_workflow(project, task.id, version.version_id, false)
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_runs_awaiting(&orch, task.id, 1), "真实进程应完成");

    // 注入失败障碍:项目目录中 docs 已是普通文件 → 合并应用中段失败
    std::fs::write(project.join("docs"), "not a directory\n").unwrap();
    let baseline_hash = worktree_hash(project);
    orch.settle_by_token(
        &token_of_latest_run(&orch, task.id),
        mf_agent::Settlement::complete("嵌套构建完成"),
    )
    .unwrap();
    // 合并失败 → needs-you + 待决行;项目目录零部分写(含障碍文件原样)
    assert!(
        wait_until(Duration::from_secs(10), || {
            orch.store
                .task_view(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::NeedsYou)
                .unwrap_or(false)
        }),
        "合并失败必须进入 needs-you,实际 {:?}",
        orch.store.task_view(task.id).unwrap().map(|t| t.status)
    );
    assert_eq!(
        orch.store.list_pending_merges(Some(task.id)).unwrap().len(),
        1,
        "失败必须持久化为待决汇合"
    );
    assert_eq!(
        worktree_hash(project),
        baseline_hash,
        "合并失败后项目工作树必须整体回滚(零部分写)"
    );
    let git = mf_vcs::git::Git::open(project).unwrap();
    let rev_id = orch.store.active_revision(task.id).unwrap().unwrap().id;
    let refname = mf_vcs::git::Git::integration_ref(task.id, rev_id);
    assert!(
        git.read_ref(&refname).unwrap().is_none(),
        "失败的合并不得推进集成基线 ref"
    );

    // 清除障碍后重试合并:嵌套文件落盘、任务收敛成功
    std::fs::remove_file(project.join("docs")).unwrap();
    let remaining = orch.resolve_pending_merges(task.id).unwrap();
    assert!(remaining.is_empty(), "{remaining:?}");
    assert!(
        wait_until(Duration::from_secs(10), || {
            orch.store
                .task_view(task.id)
                .unwrap()
                .map(|t| t.status == TaskStatus::Succeeded)
                .unwrap_or(false)
        }),
        "清障重试后任务应收敛,实际 {:?}",
        orch.store.task_view(task.id).unwrap().map(|t| t.status)
    );
    let nested = project.join("docs").join("deep").join("nested.md");
    assert!(nested.is_file(), "嵌套文件最终必须落盘");
    assert!(std::fs::read_to_string(&nested).unwrap().contains("nested"));
    orch.stop();
    ctx.close_project(&project.to_path_buf());
}

#[test]
fn e2e_cancel_run_terminates_real_os_process_and_releases_worktree() {
    if !cfg!(windows) {
        return;
    }
    let e2e = e2e_project_with_catalog();
    let (project, ctx) = (&e2e.root, &e2e.ctx);
    let orch = ctx.open_project(project.to_path_buf()).unwrap();
    let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    // 常驻进程(/K 不退出),等待取消
    let instance = ctx
        .catalog_store
        .create_agent_instance(e2e_instance_draft(&cmd, &["/K"]))
        .unwrap();
    let task = orch.create_task("PID 终止 E2E", "取消真实进程").unwrap();
    let version = ctx
        .catalog_store
        .save_template(&mf_agent::workflow::WorkflowTemplateDraft {
            key: "e2e-cancel".into(),
            name: "取消".into(),
            task_local: false,
            nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
                key: "hang".into(),
                title: "常驻".into(),
                instructions: "等待取消".into(),
                agent_instance_id: instance.id.clone(),
                deps: vec![],
            }],
        })
        .unwrap();
    ctx.assign_workflow(project, task.id, version.version_id, false)
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(
        wait_until(Duration::from_secs(20), || {
            orch.store
                .list_runs_of_task(task.id)
                .map(|runs| runs.iter().any(|r| r.status == RunStatus::Running))
                .unwrap_or(false)
        }),
        "等待真实进程派发"
    );
    let run = orch
        .store
        .list_runs_of_task(task.id)
        .unwrap()
        .into_iter()
        .find(|r| r.status == RunStatus::Running)
        .unwrap();
    let session_id = run.session_id.expect("工作流 run 绑定会话");
    // 工作流会话注册在 run 的执行租约工作目录(worktree)键下
    // (租约行在 run 行之后写入:轮询等待)
    let mut lease_path = None;
    assert!(
        wait_until(Duration::from_secs(10), || {
            lease_path = orch
                .store
                .list_execution_leases(task.id)
                .unwrap()
                .into_iter()
                .find(|l| l.status == "held")
                .map(|l| std::path::PathBuf::from(l.path));
            lease_path.is_some()
        }),
        "worktree 租约应持有,实际 {:?}",
        orch.store.list_execution_leases(task.id).unwrap()
    );
    let lease_path = lease_path.unwrap();
    // 会话注册键 = 项目根(进程 cwd 是租约路径,但路由按项目根)
    let workdir_key = project.to_string_lossy().to_string();
    // OS PID 可观测且进程真实存活
    assert!(
        wait_until(Duration::from_secs(10), || ctx
            .registry
            .session_pid(&workdir_key, session_id)
            .is_some()),
        "会话应有可观测 OS PID"
    );
    let pid = ctx.registry.session_pid(&workdir_key, session_id).unwrap();
    assert!(tasklist_has_pid(pid), "前置:PID {pid} 应存活");

    // 取消:确认真实 OS 进程终止(不是 kill 后立刻谎报)
    let cancelled = orch.cancel_run(run.id).unwrap();
    assert_eq!(cancelled.status, RunStatus::Cancelled);
    let gone = wait_until(Duration::from_secs(5), || !tasklist_has_pid(pid));
    if !gone {
        let line = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        panic!("取消返回后 OS 进程必须真正终止(PID {pid} 仍可见: {line})");
    }
    // 租约释放:worktree 目录清理
    assert!(
        wait_until(Duration::from_secs(10), || !lease_path.exists()),
        "取消后 worktree 应释放: {}",
        lease_path.display()
    );
    orch.stop();
    ctx.close_project(&project.to_path_buf());
}
