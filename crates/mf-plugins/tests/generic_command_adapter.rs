//! Generic Command Adapter 契约:executable + argv 直启(不经 Shell)、
//! Secret 只进 secret_env/redactions(全链路脱敏)、输入注入与完成检测、
//! Shell 模式必须权限门控(设计 §6.1 / §8)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mf_agent::agent_adapter::{
    AgentAdapter, CompletionDetector, CompletionObservation, InputInjection, LaunchContext,
    LaunchPlan,
};
use mf_agent::agent_instance::AgentInstanceSnapshot;
use mf_agent::secrets::SecretLease;
use mf_agent::RunMode;
use mf_plugins::generic_command_adapter::GenericCommandAdapter;

fn adapter() -> GenericCommandAdapter {
    GenericCommandAdapter::new()
}

fn snapshot_with_args(argv: &[&str]) -> AgentInstanceSnapshot {
    AgentInstanceSnapshot {
        id: "inst_x".into(),
        name: "generic".into(),
        agent_type: "generic-command".into(),
        version: 1,
        enabled: true,
        run_mode: RunMode::OneShot,
        executable: "agent.exe".into(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env: vec![("LANG".into(), "C".into())],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({
            "input": "argv",
            "completion": "process-exit"
        }),
        sealed_secret_ids: vec![],
        external_config: false,
    }
}

fn ctx() -> LaunchContext {
    LaunchContext::new(PathBuf::from("C:/tmp/run"), PathBuf::from("C:/tmp/work"))
}

#[test]
fn generic_adapter_preserves_argument_boundaries() {
    let plan = adapter()
        .compile_launch(&snapshot_with_args(&["--prompt", "a; rm -rf x"]), &ctx())
        .unwrap();
    assert_eq!(plan.executable, PathBuf::from("agent.exe"));
    assert_eq!(plan.argv[1], "a; rm -rf x");
    assert!(!plan.uses_shell);
}

#[test]
fn plan_debug_never_contains_secret_values() {
    let mut snapshot = snapshot_with_args(&[]);
    snapshot.config = serde_json::json!({
        "secret_env": { "MY_TOKEN": "api-key" }
    });
    snapshot.sealed_secret_ids = vec!["api-key".into()];
    let mut context = ctx();
    let mut secrets = HashMap::new();
    secrets.insert(
        "api-key".to_string(),
        Arc::new(SecretLease::new("api-key", b"sk-plaintext-42".to_vec())),
    );
    context.secrets = secrets;

    let plan = adapter().compile_launch(&snapshot, &context).unwrap();
    // 非敏感 env 原样传递
    assert!(plan.env.contains(&("LANG".to_string(), "C".to_string())));
    // Secret 进入 secret_env 与 redactions,供启动与日志脱敏
    assert!(plan
        .secret_env
        .iter()
        .any(|(k, v)| k == "MY_TOKEN" && v.as_slice() == b"sk-plaintext-42"));
    assert!(plan.redaction_values().contains(&"sk-plaintext-42"));
    // Debug 输出全链路脱敏
    let debug = format!("{plan:?}");
    assert!(
        !debug.contains("sk-plaintext-42"),
        "LaunchPlan Debug 泄露: {debug}"
    );
}

#[test]
fn missing_secret_blocks_launch() {
    let mut snapshot = snapshot_with_args(&[]);
    snapshot.config = serde_json::json!({
        "secret_env": { "MY_TOKEN": "api-key" }
    });
    snapshot.sealed_secret_ids = vec!["api-key".into()];
    // ctx 未提供明文(未解封)→ 必须阻止启动,而不是静默丢变量
    let err = adapter().compile_launch(&snapshot, &ctx()).unwrap_err();
    assert!(err.to_string().contains("api-key"));
}

#[test]
fn argv_stdin_and_prompt_file_injection() {
    let mut snapshot = snapshot_with_args(&["--flag"]);
    let mut context = ctx();
    context.prompt = Some("do the work".into());

    // Argv:prompt 追加为尾随参数
    let plan = adapter().compile_launch(&snapshot, &context).unwrap();
    assert_eq!(
        plan.argv,
        vec!["--flag".to_string(), "do the work".to_string()]
    );
    assert!(matches!(plan.input, InputInjection::Argv(_)));

    // Stdin:prompt 进入 stdin 字节流
    snapshot.execution_contract = serde_json::json!({ "input": "stdin" });
    let plan = adapter().compile_launch(&snapshot, &context).unwrap();
    assert!(matches!(plan.input, InputInjection::Stdin(_)));
    assert_eq!(plan.argv, vec!["--flag".to_string()]);

    // PromptFile:prompt 写入 run temp 下的文件
    snapshot.execution_contract = serde_json::json!({ "input": "prompt-file" });
    let plan = adapter().compile_launch(&snapshot, &context).unwrap();
    match &plan.input {
        InputInjection::PromptFile(path) => {
            assert_eq!(path, &PathBuf::from("C:/tmp/run").join("prompt.txt"));
            let spec = plan
                .temp_files
                .iter()
                .find(|f| f.path == PathBuf::from("prompt.txt"))
                .expect("prompt 文件必须同时出现在 temp_files");
            assert_eq!(spec.contents, b"do the work");
        }
        other => panic!("应为 PromptFile,得到 {other:?}"),
    }
}

#[test]
fn completion_detector_modes() {
    let mut snapshot = snapshot_with_args(&[]);

    snapshot.execution_contract =
        serde_json::json!({ "completion": "stdout-marker", "stdout_marker": "DONE##" });
    let plan = adapter().compile_launch(&snapshot, &ctx()).unwrap();
    assert_eq!(
        plan.completion,
        CompletionDetector::StdoutMarker("DONE##".into())
    );

    snapshot.execution_contract =
        serde_json::json!({ "completion": "result-file", "result_file": "result.json" });
    let plan = adapter().compile_launch(&snapshot, &ctx()).unwrap();
    assert_eq!(
        plan.completion,
        CompletionDetector::ResultFile(PathBuf::from("C:/tmp/run").join("result.json"))
    );

    snapshot.execution_contract = serde_json::json!({ "completion": "manual" });
    let plan = adapter().compile_launch(&snapshot, &ctx()).unwrap();
    assert_eq!(plan.completion, CompletionDetector::Manual);

    snapshot.execution_contract =
        serde_json::json!({ "completion": "result-file", "result_file": "../escape.json" });
    assert!(adapter().compile_launch(&snapshot, &ctx()).is_err());
}

#[test]
fn shell_mode_requires_plugin_permission() {
    let mut snapshot = snapshot_with_args(&[]);
    snapshot.execution_contract = serde_json::json!({ "use_shell": true });

    // 无 shell 授权 → 拒绝编译
    let err = adapter().compile_launch(&snapshot, &ctx()).unwrap_err();
    assert!(
        err.to_string().contains("shell"),
        "应提及 shell 权限: {err}"
    );

    // 授权后允许
    let mut context = ctx();
    context.grants_shell = true;
    let plan = adapter().compile_launch(&snapshot, &context).unwrap();
    assert!(plan.uses_shell);
}

#[test]
fn isolated_config_files_rejected_without_support() {
    // Generic Command 不支持进程级隔离配置:请求 config_files 必须报错,
    // 而不是静默改写真实 CLI 全局配置(设计 §3 非目标)
    let mut snapshot = snapshot_with_args(&[]);
    snapshot.config = serde_json::json!({
        "config_files": { "settings.json": { "a": 1 } }
    });
    let err = adapter().compile_launch(&snapshot, &ctx()).unwrap_err();
    assert!(err.to_string().contains("隔离"), "应说明隔离不支持: {err}");
}

#[test]
fn validate_rejects_unknown_contract_modes() {
    let mut snapshot = snapshot_with_args(&[]);
    snapshot.execution_contract = serde_json::json!({ "input": "carrier-pigeon" });
    let errors = adapter().validate(&snapshot);
    assert!(!errors.is_empty());

    snapshot.execution_contract = serde_json::json!({ "completion": "vibes" });
    assert!(!adapter().validate(&snapshot).is_empty());

    // 合法契约无错误
    snapshot.execution_contract = serde_json::json!({});
    assert!(adapter().validate(&snapshot).is_empty());
}

#[test]
fn observe_and_extract_handoff() {
    use mf_agent::agent_adapter::ProcessObservation;

    let mut snapshot = snapshot_with_args(&[]);
    snapshot.execution_contract =
        serde_json::json!({ "completion": "stdout-marker", "stdout_marker": "DONE##" });

    let obs = ProcessObservation {
        exited: false,
        exit_code: None,
        stdout_tail: "working...\nDONE##".into(),
        result_file: None,
    };
    assert_eq!(
        adapter().observe(&snapshot, &obs),
        CompletionObservation::Completed
    );
    let obs = ProcessObservation {
        exited: false,
        exit_code: None,
        stdout_tail: "working...".into(),
        result_file: None,
    };
    assert_eq!(
        adapter().observe(&snapshot, &obs),
        CompletionObservation::Running
    );

    // result-file 模式:文件内容 JSON → Handoff
    snapshot.execution_contract = serde_json::json!({ "completion": "result-file" });
    let obs = ProcessObservation {
        exited: true,
        exit_code: Some(0),
        stdout_tail: String::new(),
        result_file: Some(br#"{"summary":"done","output":{"report":"r.md"}}"#.to_vec()),
    };
    assert_eq!(
        adapter().observe(&snapshot, &obs),
        CompletionObservation::Completed
    );
    let handoff = adapter().extract_handoff(&obs).unwrap();
    assert_eq!(handoff.summary, "done");
    assert_eq!(handoff.output["report"], "r.md");
}

#[test]
fn cwd_defaults_to_workdir() {
    let plan: LaunchPlan = adapter()
        .compile_launch(&snapshot_with_args(&[]), &ctx())
        .unwrap();
    assert_eq!(plan.cwd, Some(PathBuf::from("C:/tmp/work")));
}

// ---------- 外部配置只读意图(Default CLI) ----------

#[test]
fn external_config_intent_skips_isolation_and_rejects_config_files() {
    use mf_plugins::claude_adapter::ClaudeCodeAdapter;

    // claude 适配器:external_config = true 时不改写 CLAUDE_CONFIG_DIR
    let mut ctx = ctx();
    ctx.prompt = Some("hi".into());
    ctx.external_config = true;
    let claude = ClaudeCodeAdapter::new();
    let plan = claude
        .compile_launch(&snapshot_with_args(&["-p"]), &ctx)
        .unwrap();
    assert!(
        !plan.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
        "外部配置意图下不得注入隔离配置目录: {:?}",
        plan.env
    );
    assert!(
        plan.temp_files.is_empty(),
        "外部配置意图下不得物化任何配置文件"
    );

    // 显式声明 config_files 与只读意图冲突:拒绝,绝不写入外部配置
    let mut snapshot = snapshot_with_args(&[]);
    snapshot.config = serde_json::json!({
        "config_files": { "settings.json": "{\"x\":1}" }
    });
    let err = claude.compile_launch(&snapshot, &ctx).unwrap_err();
    assert!(format!("{err:#}").contains("config_files"), "{err:#}");

    // 默认(false)保持既有隔离语义
    let isolated = claude
        .compile_launch(&snapshot_with_args(&["-p"]), &ctx_with_prompt(false))
        .unwrap();
    assert!(isolated.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
}

fn ctx_with_prompt(external: bool) -> LaunchContext {
    let mut ctx = ctx();
    ctx.prompt = Some("hi".into());
    ctx.external_config = external;
    ctx
}

// ---------- PromptFile 输入注入 ----------

#[test]
fn prompt_file_injection_yields_trusted_absolute_path() {
    let mut snapshot = snapshot_with_args(&["run"]);
    snapshot.execution_contract = serde_json::json!({
        "input": "prompt-file",
        "completion": "process-exit"
    });
    let mut ctx = ctx();
    ctx.prompt = Some("长提示内容".into());
    let plan = adapter().compile_launch(&snapshot, &ctx).unwrap();

    match &plan.input {
        mf_agent::InputInjection::PromptFile(path) => {
            assert!(
                path.is_absolute(),
                "PromptFile 必须给出可信绝对路径: {}",
                path.display()
            );
            assert_eq!(path, &ctx.run_temp.join("prompt.txt"));
            // 内容同时进 temp_files,由 Runtime Host 在 run_temp 下物化
            assert!(plan
                .temp_files
                .iter()
                .any(|f| f.path == Path::new("prompt.txt")));
        }
        other => panic!("期望 PromptFile 注入,实际 {other:?}"),
    }
    // argv 不带提示文本(由文件承载)
    assert!(!plan.argv.iter().any(|a| a.contains("长提示内容")));
}
