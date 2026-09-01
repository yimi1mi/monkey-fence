//! mfctl 管道客户端契约测试(T0c,Issue #14):
//! 真实 `mfctl.exe` 子进程 ↔ 真实 `PipeServer`(单一事实源,#[path]
//! 直接复用 mf 包的服务端实现)↔ 真实 Orchestrator/Store。
//!
//! 冻结的外部行为:
//! - `MF_RUN_TOKEN`/`MF_PIPE` 环境变量注入路径(真实 Agent 场景)
//!   与 `--token`/`--pipe` 显式传参路径;
//! - step complete/fail 的 Settlement 与结构化输出(--output-json
//!   进入 Handoff.output)、幂等重提交;
//! - agent-state 上报与结算后拒绝;
//! - 错误路径(缺管道/缺令牌/令牌无效)的退出码与诊断;
//! - stdout/stderr 不回显能力令牌原文。

// 单一事实源:直接编译 mf 包的生产服务端(mf 是 bin-only crate,
// 无法作为库依赖;#[path] include 避免复制协议实现)。
#[allow(dead_code)]
#[path = "../../mf/src/pipe_server.rs"]
mod pipe_server;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fixture {
    orch: Arc<mf_agent::orchestrator::Orchestrator>,
    run: mf_agent::model::RunView,
    task_id: i64,
    _registered: pipe_server::contract_tests::RegisteredRun,
    _serial: parking_lot::MutexGuard<'static, ()>,
}

/// 复用 include 进来的服务端契约模块的**共享** PipeServer
///(同一测试二进制内管道名唯一,FIRST_PIPE_INSTANCE 禁止二次 start)
/// 与串行锁；run/Orchestrator 夹具也直接复用服务端契约模块，防止
/// 客户端与服务端测试的建模方式发生漂移。
fn with_fixture(test: impl FnOnce(&Fixture, &str, &str)) {
    let serial = pipe_server::contract_tests::serial_guard();
    let list = pipe_server::contract_tests::shared_pipe_server();
    let registered = pipe_server::contract_tests::registered_run_in(Some(list));
    let fixture = Fixture {
        orch: registered.orch.clone(),
        run: registered.run.clone(),
        task_id: registered.task_id,
        _registered: registered,
        _serial: serial,
    };
    let pipe = pipe_server::pipe_name_for_current_process();
    let token = fixture.run.capability_token.clone();
    test(&fixture, &pipe, &token);
}

fn mfctl_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mfctl"))
}

/// 子进程超时必须 kill + wait,避免管道回归时整个测试套件挂死。
/// 超时诊断不打印 Command Debug,因为 argv 可能携带能力令牌。
fn output_with_timeout(command: &mut std::process::Command) -> std::process::Output {
    use std::process::Stdio;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("启动 mfctl 失败");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().expect("回收 mfctl 输出失败"),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("mfctl 子进程超过 10s,已终止并回收");
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("查询 mfctl 子进程状态失败: {error}");
            }
        }
    }
}

fn redact(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "[MF_RUN_TOKEN]")
    }
}

/// 以环境变量注入运行真实 mfctl(真实 Agent 场景:CLI 从 env 读取)。
fn mfctl_env(pipe: &str, token: &str, args: &[&str]) -> (String, String, Option<i32>) {
    let mut command = std::process::Command::new(mfctl_exe());
    command
        .args(args)
        .env("MF_PIPE", pipe)
        .env("MF_RUN_TOKEN", token)
        .env_remove("MFCTL_HINT");
    let out = output_with_timeout(&mut command);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

fn run_outcome(fx: &Fixture) -> Option<String> {
    fx.orch
        .store
        .run_view(fx.run.id)
        .unwrap()
        .and_then(|r| r.outcome)
}

fn wait_outcome(fx: &Fixture) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(outcome) = run_outcome(fx) {
            return Some(outcome);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    None
}

/// 契约:Agent 从环境变量读取令牌与管道,step complete 以中文 summary
/// 与 --output-json 结构化输出结算;输出进入 Handoff 载荷。
#[test]
fn mfctl_env_step_complete_settles_with_structured_output() {
    with_fixture(|fx, pipe, token| {
        let (stdout, stderr, code) = mfctl_env(
            pipe,
            token,
            &[
                "step",
                "complete",
                "--summary",
                "完成-中文摘要-42",
                "--output-json",
                "{\"report_path\":\"报告-42.md\"}",
            ],
        );
        assert!(!stdout.contains(token), "stdout 不得回显令牌");
        assert!(!stderr.contains(token), "stderr 不得回显令牌");
        let safe_stdout = redact(&stdout, token);
        let safe_stderr = redact(&stderr, token);
        assert_eq!(
            code,
            Some(0),
            "mfctl 必须成功退出: {safe_stdout}{safe_stderr}"
        );
        assert!(stdout.contains("Step 已结算"), "成功输出: {safe_stdout}");
        assert_eq!(wait_outcome(fx).as_deref(), Some("complete"));
        // 结构化输出进入 Handoff(结算同事务落库,下游精确引用);
        // 完整比较稳定 schema,不用 substring 掩盖字段漂移。
        let handoffs = fx.orch.store.list_handoff_rows(fx.task_id).unwrap();
        assert_eq!(handoffs.len(), 1, "应只生成一条 Handoff");
        let row = &handoffs[0];
        assert_eq!(row.step_id, Some(fx.run.step_id));
        assert_eq!(row.run_id, Some(fx.run.id));
        let handoff_json = serde_json::to_string(&row.handoff).unwrap();
        assert!(
            !handoff_json.contains(token),
            "Handoff 不得包含真实 capability token"
        );
        assert_eq!(
            row.handoff,
            mf_agent::Handoff {
                status: "complete".into(),
                summary: "完成-中文摘要-42".into(),
                changed_files: vec![],
                artifacts: vec![],
                verification: None,
                blockers: vec![],
                recommendations: vec![],
                output: serde_json::json!({ "report_path": "报告-42.md" }),
                raw_log_ref: Some(format!("agent-run:{}", fx.run.id)),
            }
        );
        let payload = fx
            .orch
            .store
            .run_view(fx.run.id)
            .unwrap()
            .unwrap()
            .outcome_payload
            .clone()
            .unwrap_or_default();
        assert!(!payload.contains(token), "载荷不得包含令牌原文");
        assert_eq!(payload, "完成-中文摘要-42", "summary 必须精确持久化");
    });
}

/// 契约:显式 --token/--pipe 传参与 env 注入等价;step fail 结算失败。
#[test]
fn mfctl_flag_step_fail_settles_failure() {
    with_fixture(|fx, pipe, token| {
        let mut command = std::process::Command::new(mfctl_exe());
        command
            .args(["step", "fail", "--reason", "失败原因-中文"])
            .args(["--token", token])
            .args(["--pipe", pipe]);
        let out = output_with_timeout(&mut command);
        assert_eq!(out.status.code(), Some(0));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stdout.contains(token) && !stderr.contains(token));
        assert!(
            stdout.contains("Step 已结算"),
            "失败结算也是显式结算: {}",
            redact(&stdout, token)
        );
        assert_eq!(wait_outcome(fx).as_deref(), Some("fail"));
    });
}

/// 契约:同向结算重提交幂等(mfctl 层透传服务端幂等语义)。
#[test]
fn mfctl_repeat_complete_is_idempotent() {
    with_fixture(|_fx, pipe, token| {
        let first = mfctl_env(pipe, token, &["step", "complete", "--summary", "首次"]);
        assert!(!first.0.contains(token) && !first.1.contains(token));
        assert_eq!(first.2, Some(0));
        let second = mfctl_env(pipe, token, &["step", "complete", "--summary", "重复"]);
        assert!(!second.0.contains(token) && !second.1.contains(token));
        assert_eq!(
            second.2,
            Some(0),
            "重复同向结算必须成功: {}",
            redact(&second.1, token)
        );
        assert!(
            second.0.contains("幂等"),
            "幂等输出: {}",
            redact(&second.0, token)
        );
    });
}

/// 契约:agent-state 子命令上报状态;结算后一次性令牌拒绝再上报。
#[test]
fn mfctl_agent_state_reports_then_rejects_after_settlement() {
    with_fixture(|_fx, pipe, token| {
        let (stdout, stderr, code) = mfctl_env(pipe, token, &["agent-state", "working"]);
        assert!(!stdout.contains(token) && !stderr.contains(token));
        assert_eq!(code, Some(0));
        assert!(stdout.contains("状态已上报"), "上报输出: {stdout}");

        let settle = mfctl_env(pipe, token, &["step", "complete", "--summary", "结算"]);
        assert!(!settle.0.contains(token) && !settle.1.contains(token));
        assert_eq!(settle.2, Some(0));

        let late = mfctl_env(pipe, token, &["agent-state", "done"]);
        assert!(!late.0.contains(token) && !late.1.contains(token));
        assert_eq!(late.2, Some(2), "结算后上报必须失败退出");
        assert!(
            late.1.contains("已结算"),
            "诊断: {}",
            redact(&late.1, token)
        );
    });
}

/// 契约:缺管道/缺令牌/令牌无效的错误路径 —— 退出码 2 与明确诊断,
/// 诊断文本不泄露令牌。
#[test]
fn mfctl_error_paths_exit_two_with_diagnostics() {
    with_fixture(|_fx, pipe, _token| {
        // 无 MF_PIPE(清空环境)
        let mut no_pipe_command = std::process::Command::new(mfctl_exe());
        no_pipe_command
            .args(["step", "complete", "--summary", "s"])
            .env_remove("MF_PIPE")
            .env_remove("MF_RUN_TOKEN");
        let no_pipe = output_with_timeout(&mut no_pipe_command);
        assert_eq!(no_pipe.status.code(), Some(2));
        let text = String::from_utf8_lossy(&no_pipe.stderr);
        assert!(text.contains("MF_PIPE"), "缺管道诊断: {text}");

        // 有管道无令牌
        let mut no_token_command = std::process::Command::new(mfctl_exe());
        no_token_command
            .args(["step", "complete", "--summary", "s"])
            .env("MF_PIPE", pipe)
            .env_remove("MF_RUN_TOKEN");
        let no_token = output_with_timeout(&mut no_token_command);
        assert_eq!(no_token.status.code(), Some(2));
        let text = String::from_utf8_lossy(&no_token.stderr);
        assert!(text.contains("MF_RUN_TOKEN"), "缺令牌诊断: {text}");

        // 令牌无效
        let bad = mfctl_env(
            pipe,
            "mft-not-a-real-token",
            &["step", "complete", "--summary", "s"],
        );
        assert_eq!(bad.2, Some(2));
        assert!(bad.1.contains("能力令牌无效"), "无效令牌诊断: {}", bad.1);
    });
}

/// 契约:用法错误(未知子命令)退出码 2 并打印用法。
#[test]
fn mfctl_unknown_usage_exits_two() {
    let mut command = std::process::Command::new(mfctl_exe());
    command
        .args(["definitely", "not", "a", "command"])
        .env("MF_PIPE", "\\\\.\\pipe\\unused")
        .env("MF_RUN_TOKEN", "unused");
    let out = output_with_timeout(&mut command);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("用法"),
        "用法提示"
    );
}
