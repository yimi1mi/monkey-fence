#![recursion_limit = "256"]

mod agent_workspace;
mod app_ctx;
mod console;
mod diff_view;
mod editor;
mod file_index;
mod file_tree;
mod navigation;
mod pipe_server;
mod project_context;
mod project_overview;
#[cfg(test)]
mod project_overview_tests;
mod quick_open;
mod runtime_host;
mod search;
#[cfg(test)]
mod session_restore_tests;
mod settings;
mod task_composer;
#[cfg(test)]
mod task_composer_tests;
mod task_sidebar;
mod term;
mod theme;
mod vcs_panel;
#[allow(dead_code)]
mod work_items;
mod workspace;
#[cfg(test)]
mod workspace_interaction_tests;

use gpui::prelude::*;
use gpui::{px, size, App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions};
use workspace::Workspace;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // CLI:monkeyfence [项目路径] | monkeyfence --agent-smoke [项目路径]
    if args.iter().any(|a| a == "--agent-smoke") {
        std::process::exit(agent_smoke(args.get(2).cloned()));
    }
    let project = args.get(1).map(std::path::PathBuf::from);

    gpui_platform::application().run(move |cx: &mut App| {
        bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1280.), px(800.0)), cx);
        let project = project.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("MonkeyFence".into()),
                    appears_transparent: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(|cx| {
                    let mut ws = Workspace::new(cx);
                    if let Some(p) = &project {
                        if p.is_dir() {
                            ws.open_folder(p.clone(), cx);
                        } else if p.is_file() {
                            if let Some(parent) = p.parent() {
                                ws.open_folder(parent.to_path_buf(), cx);
                            }
                            ws.open_path(p, cx);
                        }
                    }
                    ws
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

macro_rules! bind_many {
    ($cx:expr, $ctx:expr; $(($key:expr, $action:expr)),+ $(,)?) => {
        $( $cx.bind_keys([KeyBinding::new($key, $action, Some($ctx))]); )+
    };
}

fn bind_keys(cx: &mut App) {
    use editor as ed;
    use quick_open as qo;
    use settings;
    use workspace as ws;

    // 编辑器
    bind_many!(cx, "Editor";
        ("backspace", ed::Backspace),
        ("delete", ed::Delete),
        ("left", ed::Left),
        ("right", ed::Right),
        ("up", ed::Up),
        ("down", ed::Down),
        ("shift-left", ed::SelectLeft),
        ("shift-right", ed::SelectRight),
        ("shift-up", ed::SelectUp),
        ("shift-down", ed::SelectDown),
        ("home", ed::Home),
        ("end", ed::End),
        ("shift-home", ed::SelectHome),
        ("shift-end", ed::SelectEnd),
        ("pageup", ed::PageUp),
        ("pagedown", ed::PageDown),
        ("ctrl-left", ed::WordLeft),
        ("ctrl-right", ed::WordRight),
        ("ctrl-backspace", ed::DeleteWordBackward),
        ("ctrl-delete", ed::DeleteWordForward),
        ("ctrl-a", ed::SelectAll),
        ("ctrl-z", ed::Undo),
        ("ctrl-y", ed::Redo),
        ("ctrl-shift-z", ed::Redo),
        ("ctrl-s", ed::Save),
        ("enter", ed::Newline),
        ("tab", ed::Tab),
        ("shift-tab", ed::Backtab),
        ("ctrl-d", ed::DuplicateLine),
        ("alt-up", ed::MoveLineUp),
        ("alt-down", ed::MoveLineDown),
    );

    // 工作区
    bind_many!(cx, "Workspace";
        ("ctrl-shift-o", ws::OpenFolder),
        ("ctrl-p", ws::QuickOpenFiles),
        ("ctrl-shift-p", ws::CommandPalette),
        ("ctrl-w", ws::CloseTab),
        ("ctrl-tab", ws::NextTab),
        ("ctrl-shift-tab", ws::PrevTab),
        ("ctrl-b", ws::ToggleLeftPanel),
        ("ctrl-shift-e", ws::ShowExplorer),
        ("ctrl-shift-g", ws::ShowVcs),
        ("ctrl-shift-w", ws::ShowBoard),
        ("ctrl-shift-/", ws::ShowAgent),
        ("ctrl-`", ws::ToggleConsole),
        ("ctrl-shift-f", ws::OpenProjectSearch),
        ("ctrl-shift-m", ws::ShowTasks),
        ("ctrl-,", ws::OpenSettings),
    );

    // 设置弹窗
    bind_many!(cx, "Settings";
        ("escape", settings::Dismiss),
    );

    // 快速打开浮层
    bind_many!(cx, "QuickOpen";
        ("enter", qo::ConfirmItem),
        ("escape", qo::Dismiss),
        ("up", qo::SelectPrev),
        ("down", qo::SelectNext),
    );
}

/// 无 GUI 的 v2 冒烟:多步 DAG(mock HTTP Agent 结构化结算)→ 失败进「需要你」
/// → 能力令牌校验 → 人工跳过 → 收敛。同时打印 CLI Agent PATH 检测表。
fn agent_smoke(project: Option<String>) -> i32 {
    use mf_agent::model::*;
    use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
    use mf_agent::pipeline::{PipelineDraft, SessionPolicy, StepDraft};
    use mf_agent::{RuntimeKind, Store};
    use runtime_host::{RuntimeHostImpl, SessionRegistry};

    let root = match project {
        Some(p) => std::path::absolute(&p).unwrap_or_else(|_| std::path::PathBuf::from(p)),
        None => std::env::current_dir().unwrap_or_else(|_| ".".into()),
    };
    println!("[agent-smoke] 项目: {}", root.display());
    let config = mf_agent::Config::load().unwrap_or_default();
    let skills = mf_skills::load_skills(Some(&root));
    let plugins = mf_plugins::PluginRegistry::load(&config, &skills);

    // CLI Agent PATH 检测表(只检测,不安装;不复制凭据)
    println!("[agent-smoke] 内置 CLI Agent 检测:");
    for agent in mf_plugins::builtin::builtin_cli_agents() {
        let found = mf_plugins::builtin::detect_on_path(&agent.command);
        println!(
            "  {:<10} `{}` → {}",
            agent.profile_id,
            agent.command,
            found
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "未检测到(PATH)".into())
        );
    }

    // 目录 + Catalog + RuntimeHost
    let registry = SessionRegistry::new(config.clone());
    let host = RuntimeHostImpl::new(registry.clone());
    let catalog = {
        let profiles = plugins.agent_profiles();
        let mut index = mf_agent::pipeline::ProfileIndex::default();
        let mut specs = std::collections::HashMap::new();
        for p in &profiles {
            let detected = p.runtime != RuntimeKind::Pty
                || p.id == "blank-terminal"
                || mf_plugins::builtin::detect_on_path(&p.command).is_some();
            index.entries.insert(
                p.id.clone(),
                mf_agent::pipeline::ProfileAvailability {
                    installed: true,
                    enabled: true,
                    detected,
                },
            );
            specs.insert(p.id.clone(), p.clone());
        }
        std::sync::Arc::new(parking_lot::RwLock::new(ProfileCatalog { index, specs }))
    };
    let mock_spec = catalog.read().specs.get("mock").cloned().unwrap();
    let _ = mock_spec;
    let db_path = root.join(".mf-agent").join("orchestration.db");
    let store = match Store::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[agent-smoke] 数据库打开失败: {e:#}");
            return 2;
        }
    };
    let limiter = GlobalLimiter::new(4);
    let orch = match Orchestrator::start(
        store,
        root.clone(),
        config,
        host,
        catalog,
        limiter,
        "\\\\.\\pipe\\monkeyfence-smoke".into(),
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[agent-smoke] 调度器启动失败: {e:#}");
            return 2;
        }
    };

    // 1) 建任务 + 手工 DAG:s1 → s2(指示 MOCK_FAIL,演示失败路径)
    let task = orch
        .create_task(
            "agent-smoke: 验证 v2 流水线状态机",
            "结构化结算 → 下游解锁 → 失败进需要你 → 人工跳过 → 收敛",
        )
        .expect("创建任务");
    let draft = PipelineDraft {
        steps: vec![
            StepDraft {
                key: "s1".into(),
                title: "结构化结算演示".into(),
                instructions: "mock Agent 直接结算成功".into(),
                agent_profile: "mock".into(),
                session_policy: SessionPolicy::Fresh,
                deps: vec![],
            },
            StepDraft {
                key: "s2".into(),
                title: "失败进需要你".into(),
                instructions: "MOCK_FAIL".into(),
                agent_profile: "mock".into(),
                session_policy: SessionPolicy::Fresh,
                deps: vec!["s1".into()],
            },
        ],
    };
    orch.save_pipeline(task.id, &draft).expect("保存流水线");
    orch.confirm_and_run(task.id).expect("确认运行");
    println!("[agent-smoke] 任务 #{} 已确认运行(DAG: s1 → s2)", task.id);

    // 2) 事件泵:等待 s1 结算解锁 s2,s2 失败 → needs-you
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut saw_needs_you = false;
    while std::time::Instant::now() < deadline {
        match orch
            .events_rx
            .recv_timeout(std::time::Duration::from_millis(300))
        {
            Ok(ev) => match ev {
                SchedulerEvent::TaskUpdated(t) => {
                    println!("  ~ 任务 #{} → {}", t.id, t.status.label_cn());
                    if t.status == TaskStatus::NeedsYou {
                        saw_needs_you = true;
                    }
                }
                SchedulerEvent::StepUpdated(s) => {
                    println!("  ~ step {} → {}", s.step_key, s.status.label_cn());
                }
                SchedulerEvent::RunUpdated(r) => {
                    println!(
                        "  ~ run #{} → {} (outcome={:?})",
                        r.id,
                        r.status.as_str(),
                        r.outcome
                    );
                }
                SchedulerEvent::Error(e) => println!("  ⚠ {e}"),
                _ => {}
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if saw_needs_you {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    if !saw_needs_you {
        eprintln!("[agent-smoke] 失败:未观察到任务进入 needs-you");
        orch.stop();
        return 1;
    }

    // 3) 能力令牌断言:错误令牌拒绝;已结算 run 重复同结算幂等;冲突结算拒绝
    let runs = orch.runs_of_task(task.id).unwrap();
    let s1_run = runs
        .iter()
        .find(|r| r.outcome.as_deref() == Some("complete"))
        .expect("s1 已结算成功");
    assert_settle_expect(
        "错误令牌拒绝",
        matches!(
            orch.settle_by_token(
                "mft_bogus",
                Settlement::Complete {
                    summary: String::new()
                }
            ),
            Err(SettleError::UnknownToken)
        ),
    );
    assert_settle_expect(
        "相同结算幂等",
        matches!(
            orch.settle_by_token(
                &s1_run.capability_token,
                Settlement::Complete {
                    summary: "again".into()
                }
            ),
            Ok(SettleOutcome::AlreadyApplied)
        ),
    );
    assert_settle_expect(
        "冲突结算拒绝",
        matches!(
            orch.settle_by_token(
                &s1_run.capability_token,
                Settlement::Fail { reason: "x".into() }
            ),
            Err(SettleError::Conflict { .. })
        ),
    );

    // 4) 人工跳过失败节点(必须确认)→ 任务收敛(跳过 = 人工接受不完整,视为成功收敛)
    let steps = orch.task_detail(task.id).unwrap().unwrap().1;
    let s2 = steps.iter().find(|s| s.step_key == "s2").expect("s2");
    assert!(orch.skip_step(s2.id, false).is_err(), "跳过必须人工确认");
    orch.skip_step(s2.id, true).expect("跳过需确认");
    let converged = orch.store.task_view(task.id).unwrap().unwrap();
    println!("[agent-smoke] 收敛:{}", converged.status.label_cn());
    let ok = converged.status == TaskStatus::Succeeded;

    // 5) 产物检查 + 历史可查
    let artifact = root
        .join(".mf-agent")
        .join(format!("step-run-{}.md", s1_run.id));
    let artifact_ok = artifact.is_file();
    println!(
        "[agent-smoke] s1 产物 {}",
        if artifact_ok {
            format!("存在: {}", artifact.display())
        } else {
            "缺失".into()
        }
    );
    let history = orch.runs_of_task(task.id).unwrap().len();
    println!("[agent-smoke] 历史运行数:{history}");
    orch.stop();
    if ok && artifact_ok && history >= 2 {
        println!("[agent-smoke] ✔ 全部通过");
        0
    } else {
        eprintln!("[agent-smoke] ✘ 存在未满足条件(converged={ok}, artifact={artifact_ok}, history={history})");
        1
    }
}

fn assert_settle_expect(what: &str, ok: bool) {
    println!("  [token] {what}:{}", if ok { "✔" } else { "✘" });
    if !ok {
        std::process::exit(1);
    }
}

/// mf-bin 的单元测试集中放在 main.rs(rustc 对超大 gpui 模块内联 #[test]
/// 的宏展开深度计数有怪癖,放在模块文件里会触发 recursion limit)
#[cfg(test)]
mod tests {
    use crate::term::{palette, Screen};

    fn line(s: &Screen, r: usize) -> String {
        (0..s.cols)
            .map(|c| s.cell(r, c).ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn console_ansi_strip_and_control_chars() {
        let mut s = Screen::new(8, 40);
        s.feed(b"\x1b[32mOK\x1b[0m\r\n");
        s.feed(b"50%\r");
        s.feed(b"100%\r\n");
        s.feed(b"abc\x08D\r\n");
        s.feed(b"a\tb\n");
        assert_eq!(line(&s, 0), "OK");
        assert_eq!(line(&s, 1), "100%");
        assert_eq!(line(&s, 2), "abD");
        assert_eq!(line(&s, 3), "a       b");
    }

    #[test]
    fn console_osc_title_captured_not_printed() {
        let mut s = Screen::new(4, 40);
        s.feed(b"\x1b]0;title\x07hello\n");
        assert_eq!(line(&s, 0), "hello");
        assert_eq!(s.title, "title");
    }

    #[test]
    fn console_carriage_return_overwrites() {
        let mut s = Screen::new(4, 40);
        s.feed(b"downloading 50%\rdownloading 99%\n");
        assert_eq!(line(&s, 0), "downloading 99%");
    }

    #[test]
    fn console_clear_screen_tui() {
        let mut s = Screen::new(8, 40);
        s.feed(b"junk everywhere\r\nmore junk");
        s.feed(b"\x1b[2J\x1b[1;1Hready");
        assert_eq!(line(&s, 0), "ready");
        assert_eq!(line(&s, 1), "");
    }

    #[test]
    fn console_sgr_persists_per_cell() {
        let mut s = Screen::new(4, 40);
        s.feed(b"\x1b[1;34mBLUE\x1b[0m ok");
        assert_eq!(s.cell(0, 0).fg, palette(4));
        assert!(s.cell(0, 0).bold);
        assert!(s.cell(0, 5).fg.default);
        assert!(!s.cell(0, 5).bold);
    }

    #[test]
    fn settings_config_roundtrip_with_editor_section() {
        use mf_agent::{Config, ProviderConfig, ProviderKind};
        let mut cfg = Config::default();
        cfg.editor.font_family = "JetBrains Mono".into();
        cfg.editor.font_size = 14.5;
        cfg.roles.insert("worker".into(), "glm".into());
        cfg.providers.insert(
            "glm".into(),
            ProviderConfig {
                kind: ProviderKind::Openai,
                base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
                api_key: "sk-test".into(),
                model: "glm-4.6".into(),
            },
        );
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.editor.font_family, "JetBrains Mono");
        assert_eq!(back.editor.font_size, 14.5);
        assert_eq!(back.provider_for_role("worker").model, "glm-4.6");
        assert_eq!(back.provider_for_role("worker").api_key, "sk-test");
    }

    #[test]
    fn settings_legacy_config_without_editor_section_loads() {
        let legacy = r#"
[roles]
planner = "mock"

[engine]
workers = 3
"#;
        let cfg: mf_agent::Config = toml::from_str(legacy).unwrap();
        assert_eq!(cfg.engine.workers, 3);
        assert_eq!(cfg.editor.font_family, "Consolas");
        assert_eq!(cfg.editor.font_size, 13.0);
    }
}

#[cfg(test)]
mod v2_tests {
    use crate::app_ctx::import_legacy_work_items;
    use crate::pipe_server::{pipe_name_for_current_process, PipeServer};
    use crate::runtime_host::{RuntimeHostImpl, SessionRegistry};
    use mf_agent::model::*;
    use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
    use mf_agent::pipeline::{PipelineDraft, ProfileIndex, SessionPolicy, StepDraft};
    use mf_agent::{RuntimeHost, Store};
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn test_catalog() -> Arc<parking_lot::RwLock<ProfileCatalog>> {
        let mut index = ProfileIndex::default();
        index.entries.insert(
            "mock".into(),
            mf_agent::pipeline::ProfileAvailability {
                installed: true,
                enabled: true,
                detected: true,
            },
        );
        let mut specs = std::collections::HashMap::new();
        specs.insert(
            "mock".to_string(),
            mf_agent::AgentProfileSpec {
                id: "mock".into(),
                display_name: "Mock".into(),
                runtime: mf_agent::RuntimeKind::Http,
                command: String::new(),
                args: vec![],
                env: vec![],
                permission_args: vec![],
                provider: Some(mf_agent::ProviderConfig {
                    kind: mf_agent::ProviderKind::Mock,
                    base_url: String::new(),
                    api_key: String::new(),
                    model: String::new(),
                }),
                icon: None,
                homepage: None,
                hook: None,
            },
        );
        Arc::new(parking_lot::RwLock::new(ProfileCatalog { index, specs }))
    }

    fn start_orch(root: &std::path::Path) -> Arc<Orchestrator> {
        let store = Store::open(&root.join(".mf-agent/orchestration.db")).unwrap();
        Orchestrator::start(
            store,
            root.to_path_buf(),
            mf_agent::Config::default(),
            RuntimeHostImpl::new(SessionRegistry::new(mf_agent::Config::default())),
            test_catalog(),
            GlobalLimiter::new(4),
            pipe_name_for_current_process(),
        )
        .unwrap()
    }

    /// mfctl 命名管道端到端:step.complete / 错误令牌 / agent.state / pipeline.propose。
    #[test]
    fn mfctl_pipe_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let orch = start_orch(tmp.path());
        let task = orch.create_task("管道测试", "").unwrap();
        orch.save_pipeline(
            task.id,
            &PipelineDraft {
                steps: vec![StepDraft {
                    key: "s".into(),
                    title: "s".into(),
                    instructions: String::new(),
                    agent_profile: "mock".into(),
                    session_policy: SessionPolicy::Fresh,
                    deps: vec![],
                }],
            },
        )
        .unwrap();
        orch.confirm_and_run(task.id).unwrap();
        // 等待调度派发拿到 run + token
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let run = loop {
            let runs = orch.runs_of_task(task.id).unwrap();
            if let Some(r) = runs.iter().find(|r| r.status == RunStatus::Running) {
                break r.clone();
            }
            assert!(std::time::Instant::now() < deadline, "等待派发超时");
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let token = run.capability_token.clone();

        let orchestrators: Arc<Mutex<Vec<Arc<Orchestrator>>>> =
            Arc::new(Mutex::new(vec![orch.clone()]));
        let mut server = PipeServer::start(orchestrators).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        // 错误令牌
        let resp = pipe_request(
            &pipe_name_for_current_process(),
            "mft_bogus",
            "step.complete",
            &serde_json::json!({ "summary": "x" }),
        )
        .unwrap();
        assert!(!resp["ok"].as_bool().unwrap(), "错误令牌必须被拒绝: {resp}");

        // 注:mock 运行会自动结算,「活动 run 上的 agent.state」由 review_regress
        // 的 host 事件路径覆盖;此处验证结算后一次性令牌被拒绝。
        // 正确结算(重复提交幂等)
        for i in 0..2 {
            let resp = pipe_request(
                &pipe_name_for_current_process(),
                &token,
                "step.complete",
                &serde_json::json!({ "summary": format!("done-{i}") }),
            )
            .unwrap();
            assert!(resp["ok"].as_bool().unwrap(), "结算失败: {resp}");
        }
        let settled = orch.store.run_by_token(&token).unwrap().unwrap();
        assert_eq!(settled.status, RunStatus::Succeeded);

        // 一次性令牌:结算后状态上报被拒绝
        let resp = pipe_request(
            &pipe_name_for_current_process(),
            &token,
            "agent.state",
            &serde_json::json!({ "state": "done" }),
        )
        .unwrap();
        assert!(
            !resp["ok"].as_bool().unwrap(),
            "已结算令牌不得再上报状态: {resp}"
        );

        // pipeline.propose:协议往返(空草案会返回结构化错误)
        let resp = pipe_request(
            &pipe_name_for_current_process(),
            &token,
            "pipeline.propose",
            &serde_json::json!({ "draft": { "steps": [] } }),
        )
        .unwrap();
        assert!(resp.get("ok").is_some(), "协议响应缺失: {resp}");

        server.stop();
        orch.stop();
    }

    /// work-items.json 兼容导入一次(忽略 vcs_ref;不删除原文件)。
    #[test]
    fn work_items_import_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mf_dir = tmp.path().join(".mf-agent");
        std::fs::create_dir_all(&mf_dir).unwrap();
        std::fs::write(
            mf_dir.join("work-items.json"),
            r#"{
              "version": 1,
              "active_id": "main",
              "items": [
                { "id": "main", "title": "旧工作项A", "workspace": "C:/x", "vcs_ref": "main",
                  "phase": "running", "run_id": null, "comment": "", "unread": false,
                  "created_at": "2026-01-01", "updated_at": "2026-01-02" },
                { "id": "wt-1", "title": "旧工作项B", "workspace": "C:/y", "vcs_ref": "feat",
                  "phase": "done", "run_id": null, "comment": "", "unread": false,
                  "created_at": "2026-01-01", "updated_at": "2026-01-02" }
              ]
            }"#,
        )
        .unwrap();
        let orch = start_orch(tmp.path());
        import_legacy_work_items(&orch, &tmp.path().to_path_buf());
        let tasks = orch.tasks().unwrap();
        assert_eq!(tasks.len(), 2, "两个旧工作项导入为 Task");
        assert!(tasks
            .iter()
            .any(|t| t.title == "旧工作项A" && t.status == TaskStatus::NeedsYou));
        assert!(tasks
            .iter()
            .any(|t| t.title == "旧工作项B" && t.status == TaskStatus::Succeeded));
        let a = tasks.iter().find(|t| t.title == "旧工作项A").unwrap();
        assert!(a.goal.contains("忽略 vcs_ref"), "vcs_ref 应被忽略并注明");
        // 只导入一次
        import_legacy_work_items(&orch, &tmp.path().to_path_buf());
        assert_eq!(orch.tasks().unwrap().len(), 2);
        // 原 JSON 保留未删除
        assert!(mf_dir.join("work-items.json").is_file());
        orch.stop();
    }

    /// 会话持久化:保存 → 读取往返;损坏文件回退空状态;不存在的目录由调用方过滤。
    #[test]
    fn session_roundtrip_and_corrupt_fallback() {
        use crate::app_ctx::{AppCtx, SessionState};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.json");
        let state = SessionState {
            projects: vec!["D:/a".into(), "D:/b".into()],
            foreground: Some("D:/b".into()),
            project_states: Vec::new(),
        };
        AppCtx::save_session_at(&path, &state);
        let back = AppCtx::load_session_at(&path);
        assert_eq!(back.projects.len(), 2);
        assert_eq!(
            back.foreground.as_deref(),
            Some(std::path::Path::new("D:/b"))
        );
        // 原子写不留 tmp 残留
        assert!(!path.with_extension("json.tmp").exists());
        // 损坏 → 空状态
        std::fs::write(&path, "{broken").unwrap();
        assert!(AppCtx::load_session_at(&path).projects.is_empty());
    }

    /// mfctl 命名管道客户端(测试内嵌副本,协议与 mfctl 一致)。
    fn pipe_request(
        name: &str,
        token: &str,
        method: &str,
        params: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            windows_sys::Win32::Storage::FileSystem::CreateFileW(
                wide.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE,
                0,
                std::ptr::null_mut(),
                windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        // 服务端逐实例处理;撞上实例未就绪窗口时短暂重试
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let handle = if handle != INVALID_HANDLE_VALUE {
            handle
        } else {
            let mut h = handle;
            while h == INVALID_HANDLE_VALUE && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
                h = unsafe {
                    windows_sys::Win32::Storage::FileSystem::CreateFileW(
                        wide.as_ptr(),
                        windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                            | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE,
                        0,
                        std::ptr::null_mut(),
                        windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    )
                };
            }
            if h == INVALID_HANDLE_VALUE {
                anyhow::bail!("连接管道失败 name={name}");
            }
            h
        };
        let outcome = (|| {
            let req =
                serde_json::json!({ "id": 1, "token": token, "method": method, "params": params })
                    .to_string();
            let mut out = req.into_bytes();
            out.push(b'\n');
            let mut written = 0u32;
            unsafe {
                if windows_sys::Win32::Storage::FileSystem::WriteFile(
                    handle,
                    out.as_ptr(),
                    out.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                ) == 0
                {
                    anyhow::bail!("写入失败");
                }
            }
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let mut read = 0u32;
                let ok = unsafe {
                    windows_sys::Win32::Storage::FileSystem::ReadFile(
                        handle,
                        chunk.as_mut_ptr(),
                        chunk.len() as u32,
                        &mut read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || read == 0 {
                    anyhow::bail!("读取失败");
                }
                buf.extend_from_slice(&chunk[..read as usize]);
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    return Ok(serde_json::from_slice(&buf[..pos])?);
                }
            }
        })();
        unsafe { CloseHandle(handle) };
        outcome
    }
}
