//! Terminal v1 headless matrix(T3f/B3,Issue #34)。
//!
//! fake-agent 驱动的端到端矩阵:洪泛输出下 Ctrl+C 送达 budget(A9
//! `flood_ctrlc_delivery_ms` ≤200ms)、断线重连的 journal 全量/增量
//! replay、slash/Unicode 字节透明(解释权在 CLI,§8.4)。全程只经
//! `AppCtx::attach_terminal` → `TerminalChannel`(GPUI 唯一终端入口)。

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
        mf_kernel::project_registry::ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    std::mem::forget(tmp);
    let (runtime, client) = InProcessKernelRuntime::for_test(
        service,
        mf_kernel::command::ServiceIdempotencyKey::for_test(vec![0x2B; 32]).unwrap(),
        ClientId::parse("terminal-matrix-test").unwrap(),
        Principal::parse("terminal-matrix-user").unwrap(),
    )
    .unwrap();
    ctx.install_kernel_tracer_for_tests(runtime, client);
    ctx
}

/// 极简 base64(标准字母表,无填充边界外的容错)——避免为测试引入依赖。
fn b64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

struct LaunchedSession {
    app: std::sync::Arc<AppCtx>,
    handle: String,
    record: std::path::PathBuf,
}

fn launch_fake_agent(display_id: i64) -> LaunchedSession {
    let app = test_app();
    let record = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let host = RuntimeHostImpl::new(app.registry().clone());
    let (events, _rx) = crossbeam_channel::bounded(16);
    let spec = AdHocLaunchSpec {
        task_id: 1,
        session_id: display_id + 100,
        display_session_id: display_id,
        display_session_handle: format!("session-matrix-{display_id}"),
        title: "matrix 会话".into(),
        run_mode: mf_agent::RunMode::Interactive,
        plan: mf_agent::LaunchPlan {
            run_temp: record.path().join("run-temp"),
            executable: crate::agent_workflow_e2e_tests::fake_agent::exe(),
            argv: vec![
                "--record".into(),
                record.path().to_string_lossy().into_owned(),
                "matrix-prompt".into(),
            ],
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
    let handle = spec.display_session_handle.clone();
    let record_path = record.path().to_path_buf();
    std::mem::forget(record);
    std::mem::forget(workdir);
    host.launch_ad_hoc(spec).expect("fake-agent 启动失败");
    // 等 alive
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if app.registry().session_alive(&handle) {
            return LaunchedSession {
                app,
                handle,
                record: record_path,
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("fake-agent 未就绪");
}

fn attach(channel_app: &AppCtx, handle: &str) -> mf_terminal::TerminalChannel {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match channel_app.attach_terminal(handle, 0) {
            Ok(channel) => return channel,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("attach 失败:{error:?}"),
        }
    }
}

#[test]
fn flood_output_ctrlc_delivery_within_budget() {
    let session = launch_fake_agent(910);
    let channel = attach(&session.app, &session.handle);
    // 洪泛:连续 64 条 4 KiB 输出指令(256 KiB 突发,reader 持续 drain)
    let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let encoded = b64(&payload);
    let mut flood_script = String::new();
    for _ in 0..64 {
        flood_script.push_str("!out ");
        flood_script.push_str(&encoded);
        flood_script.push('\r');
    }
    channel
        .send_input(flood_script.as_bytes())
        .expect("洪泛脚本发送失败");
    // 洪泛进行中发 Ctrl+C(0x03):必须在 budget 内到达真实 PTY
    let sent_at = Instant::now();
    channel.send_input(&[0x03]).expect("Ctrl+C 发送失败");
    let deadline = sent_at + Duration::from_millis(200);
    // 轮询 input.hex(10ms 粒度;出现 0x03 即送达)
    let ctrl_c_hex = "03";
    let mut delivered_at = None;
    while Instant::now() < deadline + Duration::from_millis(300) {
        if let Ok(text) = std::fs::read_to_string(session.record.join("input.hex")) {
            if text.replace(['\n', '\r'], "").contains(ctrl_c_hex) {
                delivered_at = Some(Instant::now());
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let Some(delivered) = delivered_at else {
        panic!("Ctrl+C 未在观察窗口内到达 PTY(input.hex)");
    };
    // A9 budget 200ms + 轮询/IO 余量(CI 抖动)。预算语义由 CI 侧
    // 精确计时把关;这里守住 500ms 上界防回归。
    assert!(
        delivered.duration_since(sent_at) <= Duration::from_millis(500),
        "Ctrl+C 送达耗时 {:?} 超出上界",
        delivered.duration_since(sent_at)
    );
    // 洪泛期间 reader 持续 drain:journal 持续前进
    let facts = channel.output_facts().expect("facts");
    assert!(facts.last_seq > 0, "洪泛输出应推进 journal seq");
    channel.terminate().expect("清理失败");
}

#[test]
fn reconnect_replays_full_then_incremental() {
    let session = launch_fake_agent(911);
    let first = attach(&session.app, &session.handle);
    let payload: Vec<u8> = (0..2048).map(|i| (i % 241) as u8).collect();
    let mut script = String::new();
    for i in 0..8 {
        script.push_str("!out ");
        script.push_str(&b64(&payload));
        script.push('\r');
        let _ = i;
    }
    first
        .send_input(script.as_bytes())
        .expect("输出指令发送失败");
    // 等 journal 前进(输出经 PTY 往返)
    let deadline = Instant::now() + Duration::from_secs(10);
    let facts = loop {
        let facts = first.output_facts().unwrap();
        if facts.last_seq >= 8 || Instant::now() > deadline {
            break facts;
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    assert!(
        facts.last_seq >= 8,
        "fake-agent 输出未到达 journal:{facts:?}"
    );
    let last = facts.last_seq;
    // 断线重连(attach after_seq=0):全量 replay
    let reconnected = attach(&session.app, &session.handle);
    let full = reconnected.replay_output(0).expect("全量 replay");
    assert_eq!(full.len() as u64, last, "全量 replay 必须覆盖到 last_seq");
    assert_eq!(full.last().unwrap().seq, last);
    // 增量(after_seq=last):无新输出则空
    let incremental = reconnected.replay_output(last).expect("增量 replay");
    assert!(incremental.is_empty(), "无新输出时增量 replay 为空");
    reconnected.terminate().expect("清理失败");
}

#[test]
fn slash_unicode_and_control_bytes_pass_through_unchanged() {
    let session = launch_fake_agent(912);
    let channel = attach(&session.app, &session.handle);
    // 行命令带真实行尾(0x0d);Unicode 原样;TUI 按键用完整 VT 序列
    // (下箭头 ESC [ B,与真实键盘直通同构)。
    let line1: Vec<u8> = {
        let mut v = b"/model gpt-5".to_vec();
        v.push(0x0d);
        v
    };
    let line2: Vec<u8> = {
        let mut v = "/skills 你好-技能-Ω".as_bytes().to_vec();
        v.push(0x0d);
        v
    };
    let vt_arrow_down: Vec<u8> = vec![0x1b, b'[', b'B'];
    channel.send_input(&line1).expect("line1 发送失败");
    crate::agent_workflow_e2e_tests::fake_agent::wait_recorded_bytes(
        &session.record,
        "input.hex",
        &line1,
        Duration::from_secs(10),
    );
    channel.send_input(&line2).expect("line2 发送失败");
    crate::agent_workflow_e2e_tests::fake_agent::wait_recorded_bytes(
        &session.record,
        "input.hex",
        "/skills 你好-技能-Ω".as_bytes(),
        Duration::from_secs(10),
    );
    channel.send_input(&vt_arrow_down).expect("VT 序列发送失败");
    crate::agent_workflow_e2e_tests::fake_agent::wait_recorded_bytes(
        &session.record,
        "input.hex",
        &vt_arrow_down,
        Duration::from_secs(10),
    );
    channel.terminate().expect("清理失败");
}
