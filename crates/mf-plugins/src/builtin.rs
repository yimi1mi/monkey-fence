//! 内置 Agent 插件(合成插件):本地 CLI Agent + API Provider + 现有技能。
//! CLI 只检测 PATH,不自动安装;不复制本地 Agent 的凭据与配置目录。
//!
//! v2 清单只声明 Agent Type 契约(adapter/检测/运行模式);
//! 内置 profile 的完整命令/参数/钩子由 `profile_spec_from_builtin` 直接合成。

use crate::manifest::{AgentTypeContribution, Capabilities, ManifestHeader, PluginManifest};
use mf_agent::runtime::{AgentProfileSpec, HookSpec, RuntimeKind};
use std::path::PathBuf;

pub struct BuiltinAgent {
    pub profile_id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub permission_args: Vec<String>,
    pub homepage: String,
    pub icon: String,
    /// 状态钩子写入的本地 Agent 配置(JSON)。
    pub hook_config: Option<&'static str>,
    /// 一键安装:仅收录官方包管理器来源(npm 包已逐一核实);
    /// None = 走官方安装页(独立安装器/未核实包,不做自动执行)。
    pub install: Option<InstallSpec>,
}

/// 一键安装规格(program 需在 PATH)。
#[derive(Debug, Clone)]
pub struct InstallSpec {
    pub program: String,
    pub args: Vec<String>,
    /// 安装命令的人类可读形式(复制用)。
    pub display: String,
}

fn pip_install(package: &str) -> InstallSpec {
    InstallSpec {
        program: "python".into(),
        args: vec!["-m".into(), "pip".into(), "install".into(), package.into()],
        display: format!("python -m pip install {package}"),
    }
}

fn npm_install(package: &str) -> InstallSpec {
    InstallSpec {
        program: "npm".into(),
        args: vec!["install".into(), "-g".into(), package.into()],
        display: format!("npm install -g {package}"),
    }
}

/// 首批内置 CLI Agent 插件。
pub fn builtin_cli_agents() -> Vec<BuiltinAgent> {
    vec![
        BuiltinAgent {
            profile_id: "codex".into(),
            name: "Codex".into(),
            command: "codex".into(),
            args: vec![],
            permission_args: vec!["--dangerously-bypass-approvals-and-sandbox".into()],
            homepage: "https://developers.openai.com/codex/cli".into(),
            icon: "◉".into(),
            hook_config: Some("~/.codex/config.toml"),
            install: Some(npm_install("@openai/codex")),
        },
        BuiltinAgent {
            profile_id: "claude".into(),
            name: "Claude".into(),
            command: "claude".into(),
            args: vec![],
            permission_args: vec!["--dangerously-skip-permissions".into()],
            homepage: "https://claude.com/claude-code".into(),
            icon: "✳".into(),
            hook_config: Some("~/.claude/settings.json"),
            install: Some(npm_install("@anthropic-ai/claude-code")),
        },
        BuiltinAgent {
            profile_id: "opencode".into(),
            name: "OpenCode".into(),
            command: "opencode".into(),
            args: vec![],
            permission_args: vec![],
            homepage: "https://opencode.ai".into(),
            icon: "⌘".into(),
            hook_config: None,
            install: Some(npm_install("opencode-ai")),
        },
        BuiltinAgent {
            profile_id: "cursor".into(),
            name: "Cursor".into(),
            command: "cursor-agent".into(),
            args: vec![],
            permission_args: vec!["--full-auto".into()],
            homepage: "https://cursor.com/docs/agent/cli".into(),
            icon: "▲".into(),
            hook_config: None,
            // npm 上的 cursor-agent 包非官方(第三方仓库);走官方安装页
            install: None,
        },
        BuiltinAgent {
            profile_id: "gemini".into(),
            name: "Gemini CLI".into(),
            command: "gemini".into(),
            args: vec![],
            permission_args: vec!["--yolo".into()],
            homepage: "https://github.com/google-gemini/gemini-cli".into(),
            icon: "✧".into(),
            hook_config: None,
            install: Some(npm_install("@google/gemini-cli")),
        },
        BuiltinAgent {
            profile_id: "copilot".into(),
            name: "GitHub Copilot".into(),
            command: "copilot".into(),
            args: vec![],
            permission_args: vec![],
            homepage: "https://githubnext.com/projects/copilot-cli/".into(),
            icon: "⊶".into(),
            hook_config: None,
            install: Some(npm_install("@github/copilot")),
        },
        BuiltinAgent {
            profile_id: "qwen".into(),
            name: "Qwen Code".into(),
            command: "qwen".into(),
            args: vec![],
            permission_args: vec!["--yolo".into()],
            homepage: "https://github.com/QwenLM/qwen-code".into(),
            icon: "◈".into(),
            hook_config: None,
            install: Some(npm_install("@qwen-code/qwen-code")),
        },
        BuiltinAgent {
            profile_id: "iflow".into(),
            name: "iFlow CLI".into(),
            command: "iflow".into(),
            args: vec![],
            permission_args: vec![],
            homepage: "https://github.com/iflow-ai/iflow-cli".into(),
            icon: "≋".into(),
            hook_config: None,
            install: Some(npm_install("@iflow-ai/iflow-cli")),
        },
        BuiltinAgent {
            profile_id: "aider".into(),
            name: "Aider".into(),
            command: "aider".into(),
            args: vec![],
            permission_args: vec!["--yes-always".into()],
            homepage: "https://aider.chat".into(),
            icon: "⌥".into(),
            hook_config: None,
            install: Some(pip_install("aider-chat")),
        },
        BuiltinAgent {
            profile_id: "amp".into(),
            name: "Amp".into(),
            command: "amp".into(),
            args: vec![],
            permission_args: vec![],
            homepage: "https://ampcode.com".into(),
            icon: "⚡".into(),
            hook_config: None,
            // npm @sourcegraph/amp 无仓库字段,未核实 → 官方页
            install: None,
        },
        BuiltinAgent {
            profile_id: "kimi".into(),
            name: "Kimi".into(),
            command: "kimi".into(),
            args: vec![],
            permission_args: vec![],
            homepage: "https://kimi.com".into(),
            icon: "◐".into(),
            hook_config: None,
            // Moonshot 官方为独立安装器;npm 未核实官方包,不自动安装
            install: None,
        },
    ]
}

/// 供设置页展示:内置 Agent 的安装规格(克隆返回)。
pub fn install_spec_of(profile_id: &str) -> Option<InstallSpec> {
    builtin_cli_agents()
        .iter()
        .find(|a| a.profile_id == profile_id)
        .and_then(|a| a.install.clone())
}

/// 内置 CLI 的 Agent Adapter 契约标识。
/// Claude Code 与 Codex 有专属适配器;其余走通用命令适配器。
fn adapter_of(profile_id: &str) -> &'static str {
    match profile_id {
        "claude" => "claude-code",
        "codex" => "codex",
        _ => "generic-command",
    }
}

/// 是否支持进程级隔离配置(不改写真实 CLI 全局配置的前提)。
fn supports_isolated_config(profile_id: &str) -> bool {
    matches!(profile_id, "claude" | "codex")
}

/// 生成一个内置 CLI Agent 的合成插件清单(v2)。
pub fn synthetic_manifest(agent: &BuiltinAgent) -> PluginManifest {
    PluginManifest {
        manifest: ManifestHeader {
            version: crate::manifest::MANIFEST_VERSION,
            publisher: "monkeyfence".into(),
            id: agent.profile_id.clone(),
            name: format!("{} (内置)", agent.name),
            version_str: "0.1.0".into(),
            min_app_version: String::new(),
            description: format!("内置 {} CLI Agent(使用本机已有登录配置)", agent.name),
            homepage: agent.homepage.clone(),
            icon: String::new(),
        },
        capabilities: Capabilities {
            net: true,
            hooks: true,
            ..Default::default()
        },
        worker: None,
        agent_types: vec![AgentTypeContribution {
            id: agent.profile_id.clone(),
            name: agent.name.clone(),
            adapter: adapter_of(&agent.profile_id).into(),
            config_schema: String::new(),
            command: agent.command.clone(),
            detect_commands: vec![agent.command.clone()],
            modes: vec!["interactive".into(), "oneshot".into()],
            supports_isolated_config: supports_isolated_config(&agent.profile_id),
        }],
        node_types: vec![],
        execution_directory_providers: vec![],
        secret_stores: vec![],
        workflow_templates: vec![],
        skills: vec![],
        tools: vec![],
        ui_schemas: vec![],
    }
}

/// PATH 检测:返回命令的绝对路径(Windows 会尝试 PATHEXT 扩展)。
pub fn detect_on_path(command: &str) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    let path_env = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path_env) {
        // 先试原名(可能已带扩展),再按 PATHEXT 补全
        if dir.join(command).is_file() {
            return Some(dir.join(command));
        }
        for ext in &exts {
            let candidate = dir.join(format!("{command}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 内置 BuiltinAgent → 完整保真的 AgentProfileSpec(命令/参数/状态钩子不丢失)。
pub fn profile_spec_from_builtin(agent: &BuiltinAgent) -> AgentProfileSpec {
    AgentProfileSpec {
        id: agent.profile_id.clone(),
        display_name: agent.name.clone(),
        runtime: RuntimeKind::Pty,
        command: agent.command.clone(),
        args: agent.args.clone(),
        env: vec![],
        permission_args: agent.permission_args.clone(),
        provider: None,
        icon: (!agent.icon.is_empty()).then(|| agent.icon.clone()),
        homepage: (!agent.homepage.is_empty()).then(|| agent.homepage.clone()),
        hook: agent.hook_config.map(|p| HookSpec {
            config_path: p.to_string(),
            namespace: "monkeyfence".into(),
            command_template: "mfctl agent-state {state}".into(),
        }),
    }
}

/// 内置 API Provider 的合成 profile(与 v1 行为一致:provider 配置由运行时按 id 解析)。
pub fn http_profile(name: &str) -> AgentProfileSpec {
    AgentProfileSpec {
        id: name.to_string(),
        display_name: name.to_string(),
        runtime: RuntimeKind::Http,
        command: String::new(),
        args: vec![],
        env: vec![],
        permission_args: vec![],
        provider: None,
        icon: None,
        homepage: None,
        hook: None,
    }
}

/// 把第三方插件的 AgentTypeContribution 合成基础 AgentProfileSpec。
/// 完整执行契约(参数/环境/Secret)由 Agent Instance 配置在启动时注入,
/// 这里只提供适配器路由所需的最小信息。
pub fn profile_spec_from_contribution(
    _plugin_full_id: &str,
    a: &AgentTypeContribution,
) -> AgentProfileSpec {
    AgentProfileSpec {
        id: a.id.clone(),
        display_name: a.name.clone(),
        runtime: match a.adapter.as_str() {
            "http" => RuntimeKind::Http,
            "plugin-worker" => RuntimeKind::PluginWorker,
            // claude-code / codex / generic-command 等 CLI 适配器
            _ => RuntimeKind::Pty,
        },
        command: a.command.clone(),
        args: vec![],
        env: vec![],
        permission_args: vec![],
        provider: None,
        icon: None,
        homepage: None,
        hook: None,
    }
}

/// API Provider(OpenAI 兼容 / Anthropic / mock)合成 Agent Profile。
pub fn profile_spec_from_provider(name: &str, p: &mf_agent::ProviderConfig) -> AgentProfileSpec {
    AgentProfileSpec {
        id: name.to_string(),
        display_name: format!("{name} (API)"),
        runtime: RuntimeKind::Http,
        command: String::new(),
        args: vec![],
        env: vec![],
        permission_args: vec![],
        provider: Some(p.clone()),
        icon: Some("☁".into()),
        homepage: None,
        hook: None,
    }
}

/// 空白终端(用户手动开 shell 的 Agent)。
pub fn blank_terminal_profile() -> AgentProfileSpec {
    AgentProfileSpec {
        id: "blank-terminal".into(),
        display_name: "空白终端".into(),
        runtime: RuntimeKind::Pty,
        command: default_shell(),
        args: vec![],
        env: vec![],
        permission_args: vec![],
        provider: None,
        icon: Some("▮".into()),
        homepage: None,
        hook: None,
    }
}

pub fn default_shell() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_agents_present() {
        let agents = builtin_cli_agents();
        let ids: Vec<&str> = agents.iter().map(|a| a.profile_id.as_str()).collect();
        for expected in ["codex", "claude", "opencode", "cursor", "kimi"] {
            assert!(ids.contains(&expected), "缺少内置 Agent {expected}");
        }
    }

    #[test]
    fn install_specs_only_official_packages() {
        let agents = builtin_cli_agents();
        // 已核实的官方 npm 包
        let auto: Vec<&str> = agents
            .iter()
            .filter(|a| a.install.is_some())
            .map(|a| a.profile_id.as_str())
            .collect();
        let mut auto = auto;
        auto.sort();
        assert_eq!(
            auto,
            vec!["aider", "claude", "codex", "copilot", "gemini", "iflow", "opencode", "qwen"]
        );
        assert!(install_spec_of("codex")
            .unwrap()
            .display
            .contains("@openai/codex"));
        assert!(install_spec_of("aider")
            .unwrap()
            .display
            .contains("pip install aider-chat"));
        // cursor/kimi/amp 不提供自动安装(npm 包非官方/未核实)
        assert!(install_spec_of("cursor").is_none());
        assert!(install_spec_of("kimi").is_none());
        assert!(install_spec_of("amp").is_none());
    }

    #[test]
    fn synthetic_manifests_valid() {
        for a in builtin_cli_agents() {
            let m = synthetic_manifest(&a);
            assert!(m.validate().is_ok(), "{} 清单非法", a.profile_id);
            assert_eq!(m.full_id(), format!("monkeyfence.{}", a.profile_id));
        }
    }

    #[test]
    fn path_detection_finds_cmd() {
        // cmd.exe 一定在 PATH 上(Windows 测试环境)
        assert!(detect_on_path("cmd").is_some() || detect_on_path("cmd.exe").is_some());
        assert!(detect_on_path("definitely-not-a-real-command-xyz").is_none());
    }

    #[test]
    fn path_detection_rejects_empty() {
        assert!(detect_on_path("").is_none());
    }
}
