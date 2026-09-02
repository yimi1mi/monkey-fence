//! TerminalChannel shim 端到端(Issue #28):GPUI 侧唯一的终端写入口是
//! `AppCtx::attach_terminal` → `TerminalChannel`;输入字节经 kernel
//! 委托真实 SessionRegistry PTY 管线到达 fake-agent,终止经通道收口,
//! 未知会话 fail-closed。等价于旧 `send_prompt_raw`/`kill_session`
//! 直调路径的行为,经 shim 不回归。

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
        mf_kernel::command::ServiceIdempotencyKey::for_test(vec![0x2A; 32]).unwrap(),
        ClientId::parse("terminal-shim-test").unwrap(),
        Principal::parse("terminal-shim-user").unwrap(),
    )
    .unwrap();
    ctx.install_kernel_tracer_for_tests(runtime, client);
    ctx
}

fn fake_spec(
    events: crossbeam_channel::Sender<(i64, RuntimeEvent)>,
    exe: &std::path::Path,
    record: &std::path::Path,
    workdir: &std::path::Path,
    display_id: i64,
) -> AdHocLaunchSpec {
    use mf_agent::InputInjection;
    AdHocLaunchSpec {
        task_id: 1,
        session_id: display_id + 100,
        display_session_id: display_id,
        display_session_handle: format!("session-shim-{display_id}"),
        title: "shim 测试会话".into(),
        run_mode: mf_agent::RunMode::Interactive,
        plan: mf_agent::LaunchPlan {
            run_temp: record.join("run-temp"),
            executable: exe.to_path_buf(),
            argv: vec![
                "--record".into(),
                record.to_string_lossy().into_owned(),
                "shim-prompt".into(),
            ],
            env: vec![],
            secret_env: vec![],
            cwd: None,
            temp_files: vec![],
            input: InputInjection::Argv(String::new()),
            completion: mf_agent::CompletionDetector::ProcessExit,
            uses_shell: false,
        },
        run_temp: record.join("run-temp"),
        workdir: workdir.to_path_buf(),
        events,
    }
}

#[test]
fn attach_terminal_routes_input_and_terminate_through_kernel_shim() {
    let app = test_app();
    let record = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let host = RuntimeHostImpl::new(app.registry().clone());
    let (events, _rx) = crossbeam_channel::bounded(16);
    let spec = fake_spec(
        events,
        &crate::agent_workflow_e2e_tests::fake_agent::exe(),
        record.path(),
        workdir.path(),
        900,
    );
    let session_handle = spec.display_session_handle.clone();
    host.launch_ad_hoc(spec).expect("离散会话启动失败");

    // 会话存活后经 kernel shim 取通道(不再直接触碰 SessionRegistry)。
    let deadline = Instant::now() + Duration::from_secs(10);
    let channel = loop {
        match app.attach_terminal(&session_handle, 0) {
            Ok(channel) => break channel,
            Err(mf_kernel::kernel::KernelProblem::ServiceUnavailable(error)) => {
                panic!("宿主未装配(装配链破坏):{error}");
            }
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(error) => panic!("存活会话 attach 失败:{error:?}"),
        }
    };
    assert_eq!(channel.session().as_str(), session_handle);
    assert!(channel.is_alive());

    // 输入字节原样到达真实 PTY(fake-agent 记录 input.hex)。
    channel
        .send_input("/model shim-Ω\r".as_bytes())
        .expect("shim 输入发送失败");
    // wait_recorded_bytes 超时在内部 panic(与既有 e2e 同语义)。
    crate::agent_workflow_e2e_tests::fake_agent::wait_recorded_bytes(
        record.path(),
        "input.hex",
        "/model shim-Ω".as_bytes(),
        Duration::from_secs(10),
    );

    // 终止经通道;进程树收口。
    channel.terminate().expect("shim 终止失败");
    let deadline = Instant::now() + Duration::from_secs(10);
    while channel.is_alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(!channel.is_alive(), "terminate 后会话应结束");
}

#[test]
fn attach_terminal_rejects_unknown_session_fail_closed() {
    let app = test_app();
    let problem = app
        .attach_terminal("session-shim-不存在的会话", 0)
        .unwrap_err();
    assert_eq!(
        problem,
        mf_kernel::kernel::KernelProblem::ResourceNotFound,
        "未知会话必须 fail-closed"
    );
}
