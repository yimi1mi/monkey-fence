//! Claude Code / Codex 隔离适配器:每次运行独立 `CLAUDE_CONFIG_DIR` /
//! `CODEX_HOME`(run-temp 下),绝不读写真实 `~/.claude`、`~/.codex`。
//! 只物化 Agent Instance 快照声明的配置,不复制用户已有 CLI 主目录。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mf_agent::agent_adapter::{AgentAdapter, LaunchContext};
use mf_agent::agent_instance::AgentInstanceSnapshot;
use mf_agent::secrets::SecretLease;
use mf_agent::RunMode;
use mf_plugins::claude_adapter::ClaudeCodeAdapter;
use mf_plugins::codex_adapter::CodexAdapter;

fn claude_adapter() -> ClaudeCodeAdapter {
    ClaudeCodeAdapter::new()
}

fn codex_adapter() -> CodexAdapter {
    CodexAdapter::new()
}

fn instance(agent_type: &str) -> AgentInstanceSnapshot {
    AgentInstanceSnapshot {
        id: "inst_x".into(),
        name: agent_type.into(),
        agent_type: agent_type.into(),
        version: 1,
        enabled: true,
        run_mode: RunMode::Interactive,
        executable: agent_type.into(),
        argv: vec![],
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({}),
        sealed_secret_ids: vec![],
        external_config: false,
    }
}

fn ctx_at(root: &Path) -> LaunchContext {
    LaunchContext::new(root.to_path_buf(), root.to_path_buf())
}

fn env_value(plan: &mf_agent::LaunchPlan, key: &str) -> Option<String> {
    plan.env
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

#[test]
fn adapters_never_target_real_homes() {
    let root = tempfile::tempdir().unwrap();
    let claude = claude_adapter()
        .compile_launch(&instance("claude-code"), &ctx_at(root.path()))
        .unwrap();
    let codex = codex_adapter()
        .compile_launch(&instance("codex"), &ctx_at(root.path()))
        .unwrap();

    let claude_dir = root.path().join("claude");
    let codex_dir = root.path().join("codex");
    assert_eq!(
        env_value(&claude, "CLAUDE_CONFIG_DIR").map(PathBuf::from),
        Some(claude_dir.clone()),
        "Claude 必须使用 run-temp 下的独立配置目录"
    );
    assert_eq!(
        env_value(&codex, "CODEX_HOME").map(PathBuf::from),
        Some(codex_dir.clone()),
        "Codex 必须使用 run-temp 下的独立主目录"
    );

    // Adapter 编译是纯函数:目录与文件由 Runtime Host 统一物化
    assert!(!claude_dir.exists());
    assert!(!codex_dir.exists());

    // 绝不指向真实全局配置
    let home = dirs::home_dir().unwrap();
    assert_ne!(
        env_value(&claude, "CLAUDE_CONFIG_DIR").map(PathBuf::from),
        Some(home.join(".claude"))
    );
    assert_ne!(
        env_value(&codex, "CODEX_HOME").map(PathBuf::from),
        Some(home.join(".codex"))
    );
}

#[test]
fn materializes_only_snapshot_config_into_isolated_home() {
    let root = tempfile::tempdir().unwrap();
    let mut snapshot = instance("claude-code");
    snapshot.config = serde_json::json!({
        "config_files": {
            "settings.json": { "permissions": { "allow": ["Bash(ls*)"] } }
        }
    });
    let plan = claude_adapter()
        .compile_launch(&snapshot, &ctx_at(root.path()))
        .unwrap();

    let settings = PathBuf::from("claude").join("settings.json");
    let spec = plan
        .temp_files
        .iter()
        .find(|file| file.path == settings)
        .expect("settings.json 必须由 LaunchPlan 声明");
    let text = String::from_utf8(spec.contents.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["permissions"]["allow"][0], "Bash(ls*)");

    // Codex 同样物化(TOML 由值字符串承载)
    let mut snapshot = instance("codex");
    snapshot.config = serde_json::json!({
        "config_files": { "config.toml": "model = \"gpt-5\"\n" }
    });
    let codex_plan = codex_adapter()
        .compile_launch(&snapshot, &ctx_at(root.path()))
        .unwrap();
    let toml = PathBuf::from("codex").join("config.toml");
    assert!(codex_plan.temp_files.iter().any(|file| {
        file.path == toml && String::from_utf8_lossy(&file.contents).contains("gpt-5")
    }));

    // 不复制用户真实 CLI 主目录:隔离目录只包含快照声明的文件
    assert!(!root.path().join("claude").exists());
    assert!(!root.path().join("codex").exists());
}

#[test]
fn config_paths_cannot_escape_isolated_home() {
    let root = tempfile::tempdir().unwrap();
    let mut snapshot = instance("claude-code");
    snapshot.config = serde_json::json!({
        "config_files": { "../evil.json": { "x": 1 } }
    });
    let err = claude_adapter()
        .compile_launch(&snapshot, &ctx_at(root.path()))
        .unwrap_err();
    assert!(err.to_string().contains("逃逸") || err.to_string().contains(".."));

    let mut snapshot = instance("codex");
    snapshot.config = serde_json::json!({
        "config_files": { "C:/Windows/evil.toml": "x = 1" }
    });
    assert!(codex_adapter()
        .compile_launch(&snapshot, &ctx_at(root.path()))
        .is_err());
}

#[test]
fn secrets_go_through_redacted_env_only() {
    let root = tempfile::tempdir().unwrap();
    let mut snapshot = instance("claude-code");
    snapshot.config = serde_json::json!({
        "secret_env": { "ANTHROPIC_API_KEY": "api-key" }
    });
    snapshot.sealed_secret_ids = vec!["api-key".into()];
    let mut context = ctx_at(root.path());
    let mut secrets = HashMap::new();
    secrets.insert(
        "api-key".to_string(),
        Arc::new(SecretLease::new("api-key", b"sk-ant-secret-99".to_vec())),
    );
    context.secrets = secrets;

    let plan = claude_adapter()
        .compile_launch(&snapshot, &context)
        .unwrap();
    assert!(plan
        .secret_env
        .iter()
        .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.as_slice() == b"sk-ant-secret-99"));
    let debug = format!("{plan:?}");
    assert!(!debug.contains("sk-ant-secret-99"), "Debug 泄露: {debug}");

    // Codex 同理
    let mut snapshot = instance("codex");
    snapshot.config = serde_json::json!({
        "secret_env": { "OPENAI_API_KEY": "api-key" }
    });
    snapshot.sealed_secret_ids = vec!["api-key".into()];
    let plan = codex_adapter().compile_launch(&snapshot, &context).unwrap();
    assert!(plan
        .secret_env
        .iter()
        .any(|(k, v)| k == "OPENAI_API_KEY" && v.as_slice() == b"sk-ant-secret-99"));
}

#[test]
fn shell_still_requires_permission_in_isolated_adapters() {
    let root = tempfile::tempdir().unwrap();
    let mut snapshot = instance("claude-code");
    snapshot.execution_contract = serde_json::json!({ "use_shell": true });
    assert!(claude_adapter()
        .compile_launch(&snapshot, &ctx_at(root.path()))
        .is_err());
}

#[test]
fn validate_and_observe_delegate_to_contract() {
    let snapshot = instance("codex");
    assert!(codex_adapter().validate(&snapshot).is_empty());
    let mut bad = snapshot.clone();
    bad.execution_contract = serde_json::json!({ "input": "nope" });
    assert!(!codex_adapter().validate(&bad).is_empty());

    use mf_agent::agent_adapter::{CompletionObservation, ProcessObservation};
    let obs = ProcessObservation {
        exited: true,
        exit_code: Some(0),
        stdout_tail: String::new(),
        result_file: None,
    };
    assert_eq!(
        claude_adapter().observe(&snapshot, &obs),
        CompletionObservation::Completed
    );
}

#[test]
fn builtin_registry_routes_isolated_adapters() {
    assert_eq!(
        mf_plugins::builtin::adapter_for("claude-code")
            .unwrap()
            .id(),
        "claude-code"
    );
    assert_eq!(
        mf_plugins::builtin::adapter_for("codex").unwrap().id(),
        "codex"
    );
    assert!(mf_plugins::builtin::adapter_for("unknown-adapter").is_none());
}
