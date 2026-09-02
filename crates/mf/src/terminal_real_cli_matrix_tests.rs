//! 真实 CLI headless 矩阵(T10c/T11 本机 gated,Issue #61)。
//!
//! 本机存在真实 codex/claude 时运行(CI 或他人环境无 CLI 时自动跳过):
//! ① `--version` 快速退出经完整管线(AppCtx→launch→PTY→journal→exit);
//! ② codex TUI 模式 banner 输出流 + 真实 PTY resize + terminate 收口。
//! IME(Microsoft Pinyin composition)需要真实浏览器人工交互——本矩阵
//! 无法覆盖,保持人工验收项(§8.9)。

use std::time::{Duration, Instant};

use crate::app_ctx::AppCtx;
use crate::runtime_host::RuntimeHostImpl;
use mf_agent::runtime::{AdHocLaunchSpec, RuntimeEvent, RuntimeHost as _};
use mf_kernel::handles::{ClientId, Principal};
use mf_kernel::kernel::InProcessKernelRuntime;

fn test_app() -> std::sync::Arc<AppCtx> {
    let ctx = AppCtx::with_catalog_for_tests(mf_agent::CatalogStore::memory().unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let service =
        mf_kernel::project_registry::ServiceStore::open(&tmp.path().join("service-v1.db"))
            .unwrap();
    std::mem::forget(tmp);
    let (runtime, client) = InProcessKernelRuntime::for_test(
        service,
        mf_kernel::command::ServiceIdempotencyKey::for_test(vec![0x2C; 32]).unwrap(),
        ClientId::parse("real-cli-matrix").unwrap(),
        Principal::parse("real-cli-user").unwrap(),
    )
    .unwrap();
    ctx.install_kernel_tracer_for_tests(runtime, client);
    ctx
}

fn real_cli(name: &str) -> Option<std::path::PathBuf> {
    let path = match name {
        "codex" => std::path::PathBuf::from(
            r"C:\Users\hongjinmin\AppData\Roaming\npm\codex.cmd",
        ),
        "claude" => std::path::PathBuf::from(r"C:\Users\hongjinmin\.local\bin\claude.exe"),
        _ => return None,
    };
    path.exists().then_some(path)
}


/// 存活期内抓取 journal 输出(退出后注册表摘除;竞态窗口内轮询)。
fn capture_while_alive(
    app: &AppCtx,
    handle: &str,
    deadline_secs: u64,
) -> Option<(mf_terminal::TerminalChannel, String)> {
    // 持续跟踪到会话摘除:每次成功 attach 就刷新最后一次 replay 快照
    // (快速退出场景的完整输出只短暂存在,单次抓取会落在中间态)。
    let deadline = Instant::now() + Duration::from_secs(deadline_secs);
    let mut last: Option<(mf_terminal::TerminalChannel, String)> = None;
    loop {
        if let Ok(channel) = app.attach_terminal(handle, 0) {
            if let Ok(replay) = channel.replay_output(0) {
                if !replay.is_empty() {
                    let text: Vec<u8> = replay.iter().flat_map(|c| c.bytes.to_vec()).collect();
                    last = Some((channel, String::from_utf8_lossy(&text).into_owned()));
                }
            }
        }
        if !app.registry().session_alive(handle) {
            break;
        }
        assert!(Instant::now() < deadline, "会话未在时限内结束/产出");
        std::thread::sleep(Duration::from_millis(3));
    }
    last
}
/// 启动 spec(行命令形式;快速退出场景用 completion=ProcessExit)。
fn quick_spec(exe: std::path::PathBuf, display_id: i64, argv: Vec<String>) -> (AdHocLaunchSpec, tempfile::TempDir, tempfile::TempDir) {
    let record = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let (events, _rx) = crossbeam_channel::bounded(64);
    let spec = AdHocLaunchSpec {
        task_id: 1,
        session_id: display_id + 100,
        display_session_id: display_id,
        display_session_handle: format!("session-real-{display_id}"),
        title: format!("真实 CLI {display_id}").into(),
        run_mode: mf_agent::RunMode::Interactive,
        plan: mf_agent::LaunchPlan {
            run_temp: record.path().join("run-temp"),
            executable: exe,
            argv,
            env: vec![],
            secret_env: vec![],
            cwd: None,
            temp_files: vec![],
            input: mf_agent::InputInjection::Argv(String::new()),
            completion: mf_agent::CompletionDetector::ProcessExit,
            uses_shell: false,
        },
        run_temp: record.path().join("run-temp"),
        workdir: workdir.path().to_path_buf(),
        events,
    };
    (spec, record, workdir)
}

/// ① `--version` 快速退出:输出经 redactor→journal,exit 通知;quit 后
/// journal 收到全部 stdout(final seq > 0)。
#[test]
fn real_cli_version_probe_flows_through_pipeline() {
    let Some(exe) = real_cli("codex") else {
        eprintln!("skip: 本机无 codex CLI");
        return;
    };
    let app = test_app();
    let host = RuntimeHostImpl::new(app.registry().clone());
    let (spec, _record, _workdir) = quick_spec(exe, 950, vec!["--version".into()]);
    let handle = spec.display_session_handle.clone();
    host.launch_ad_hoc(spec).expect("真实 codex --version 启动");
    // 存活期内抓 journal 输出(退出后注册表摘除)
    let (channel, text) = capture_while_alive(&app, &handle, 30)
        .expect("codex --version 输出未到达 journal(30s)");
    assert!(
        text.contains("codex-cli") || text.contains("codex"),
        "版本输出可见:{text}"
    );
    // 快速退出:exit ≠ settlement,会话自然摘除
    let deadline = Instant::now() + Duration::from_secs(30);
    while app.registry().session_alive(&handle) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!app.registry().session_alive(&handle), "--version 应自然退出");
    drop(channel);
}

#[test]
fn real_claude_version_probe_flows_through_pipeline() {
    let Some(exe) = real_cli("claude") else {
        eprintln!("skip: 本机无 claude CLI");
        return;
    };
    let app = test_app();
    let host = RuntimeHostImpl::new(app.registry().clone());
    let (spec, _record, _workdir) = quick_spec(exe, 951, vec!["--version".into()]);
    let handle = spec.display_session_handle.clone();
    host.launch_ad_hoc(spec).expect("真实 claude --version 启动");
    let (_channel, text) = capture_while_alive(&app, &handle, 30)
        .expect("claude --version 输出未到达 journal(30s)");
    assert!(
        text.contains("Claude Code") || text.contains("claude"),
        "claude 版本输出可见:{text}"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while app.registry().session_alive(&handle) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!app.registry().session_alive(&handle), "--version 应自然退出");
}

/// ② codex TUI banner:输出流进 journal、**真实 PTY resize**、terminate
/// 收口(进程树清理)。不验证 IME(人工项)与 Ctrl+C 语义(凭据相关)。
#[test]
fn real_codex_tui_banner_resize_and_terminate() {
    let Some(exe) = real_cli("codex") else {
        eprintln!("skip: 本机无 codex CLI");
        return;
    };
    let app = test_app();
    let host = RuntimeHostImpl::new(app.registry().clone());
    let (spec, _record, _workdir) = quick_spec(exe, 952, vec![]);
    let handle = spec.display_session_handle.clone();
    host.launch_ad_hoc(spec).expect("真实 codex TUI 启动");

    // banner 输出到达 journal(未登录也会输出提示/界面字节)
    // TUI 会话持续存活:banner 到达即取 channel(不等退出)
    let deadline = Instant::now() + Duration::from_secs(20);
    let channel = loop {
        if let Ok(channel) = app.attach_terminal(&handle, 0) {
            if channel.output_facts().map(|f| f.last_seq >= 1).unwrap_or(false) {
                break channel;
            }
        }
        assert!(Instant::now() < deadline, "codex TUI banner 未在 20s 内到达 journal");
        std::thread::sleep(Duration::from_millis(20));
    };
    // 真实 resize(ConPTY):不报错;TUI 继续输出(journal 前进或保持)
    channel.resize(120, 40).expect("真实 PTY resize 失败");
    std::thread::sleep(Duration::from_millis(800));
    let facts_after_resize = channel.output_facts().unwrap();
    assert!(facts_after_resize.last_seq >= 1, "resize 后 journal 仍有内容");
    // terminate:进程树收口(Job Object;无人值守不留孤儿 CLI)
    channel.terminate().expect("terminate 失败");
    let deadline = Instant::now() + Duration::from_secs(15);
    while app.registry().session_alive(&handle) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !app.registry().session_alive(&handle),
        "terminate 后真实 codex 进程树应收口"
    );
}
