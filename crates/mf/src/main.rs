mod agent_panel;
mod diff_view;
mod editor;
mod file_index;
mod file_tree;
mod quick_open;
mod theme;
mod vcs_panel;
mod workspace;

use gpui::prelude::*;
use gpui::{
    App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
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

fn bind_keys(cx: &mut App) {    use editor as ed;
    use quick_open as qo;
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
    );

    // 快速打开浮层
    bind_many!(cx, "QuickOpen";
        ("enter", qo::ConfirmItem),
        ("escape", qo::Dismiss),
        ("up", qo::SelectPrev),
        ("down", qo::SelectNext),
    );
}

/// 无 GUI 的 agent 编排自测:规划 → 派发 → 执行 → 收敛,打印事件流
fn agent_smoke(project: Option<String>) -> i32 {
    let root = match project {
        Some(p) => std::path::absolute(&p).unwrap_or_else(|_| std::path::PathBuf::from(p)),
        None => std::env::current_dir().unwrap_or_else(|_| ".".into()),
    };
    println!("[agent-smoke] 工作区: {}", root.display());
    let config = mf_agent::Config::load().unwrap_or_default();
    let skills = mf_skills::load_skills(Some(&root));
    println!("[agent-smoke] 提供方: planner={} worker={}", config.provider_for_role("planner").kind.kind_str(), config.provider_for_role("worker").kind.kind_str());
    let db_path = root.join(".mf-agent").join("orchestration.db");
    let engine = match mf_agent::Engine::start(&db_path, root.clone(), config, skills) {
        Ok(e) => std::sync::Arc::new(e),
        Err(e) => {
            eprintln!("[agent-smoke] 引擎启动失败: {e}");
            return 2;
        }
    };
    let run_id = match engine.start_run("agent-smoke: 写一个 .mf-agent/REPORT.md 汇总本次任务流转") {
        Ok(id) => id,
        Err(e) => {
            eprintln!("[agent-smoke] 启动运行失败: {e}");
            return 2;
        }
    };
    println!("[agent-smoke] run #{run_id} 已启动");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while std::time::Instant::now() < deadline {
        match engine.events_rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(ev) => match ev {
                mf_agent::EngineEvent::TaskCreated(t) => {
                    println!("  + 任务 #{} [{}] {}", t.id, t.status.as_str(), t.spec.lines().next().unwrap_or(""));
                }
                mf_agent::EngineEvent::TaskStatus(t) => {
                    println!("  ~ 任务 #{} → {}", t.id, t.status.label_cn());
                }
                mf_agent::EngineEvent::WorkerTool { task_id, tool, summary, .. } => {
                    println!("  🔧 #{} {} {}", task_id, tool, summary.chars().take(80).collect::<String>());
                }
                mf_agent::EngineEvent::WorkerLog { worker, text, .. } => {
                    println!("  💬 [{}] {}", worker, text.lines().next().unwrap_or("").chars().take(100).collect::<String>());
                }
                mf_agent::EngineEvent::QuestionOpened(q) => {
                    println!("  ❓ {}", q.question);
                    let _ = engine.answer_question(q.id, "继续(smoke 自动应答)");
                }
                mf_agent::EngineEvent::EngineError(e) => {
                    println!("  ⚠ {e}");
                }
                mf_agent::EngineEvent::RunFinished(_, msg) => {
                    println!("[agent-smoke] 结束: {msg}");
                    let tasks = engine.tasks_of_run(run_id).unwrap_or_default();
                    let ok = !tasks.is_empty()
                        && tasks.iter().all(|t| t.status == mf_agent::TaskStatus::Completed);
                    for t in &tasks {
                        println!("  #{} {} — {}", t.id, t.status.label_cn(), t.result.as_deref().map(|r| r.lines().next().unwrap_or("")).unwrap_or(""));
                    }
                    engine.stop();
                    return if ok { 0 } else { 1 };
                }
                _ => {}
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    eprintln!("[agent-smoke] 超时未收敛");
    engine.stop();
    1
}
