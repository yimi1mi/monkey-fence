//! 内置 Agent 插件(合成插件):本地 CLI Agent + API Provider + 现有技能。
//! CLI 只检测 PATH,不自动安装;不复制本地 Agent 的凭据与配置目录。

use crate::manifest::{
    AgentContribution, AgentHookSpec, Capabilities, ManifestHeader, PluginManifest,
};
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
        },
    ]
}

/// 生成一个内置 CLI Agent 的合成插件清单。
pub fn synthetic_manifest(agent: &BuiltinAgent) -> PluginManifest {
    PluginManifest {
        manifest: ManifestHeader {
            version: 1,
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
            ..Default::default()
        },
        worker: None,
        agents: vec![AgentContribution {
            id: agent.profile_id.clone(),
            name: agent.name.clone(),
            runtime: "pty".into(),
            command: agent.command.clone(),
            args: agent.args.clone(),
            env: Default::default(),
            permission_args: agent.permission_args.clone(),
            homepage: agent.homepage.clone(),
            icon: agent.icon.clone(),
            hook: agent.hook_config.map(|p| AgentHookSpec {
                config_path: p.to_string(),
                namespace: "monkeyfence".into(),
                command_template: "mfctl agent-state {state}".into(),
            }),
        }],
        pipelines: vec![],
        skills: vec![],
        tools: vec![],
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

/// 把 AgentContribution 合成可执行的 AgentProfileSpec。
pub fn profile_spec_from_contribution(
    _plugin_full_id: &str,
    a: &AgentContribution,
) -> AgentProfileSpec {
    AgentProfileSpec {
        id: a.id.clone(),
        display_name: a.name.clone(),
        runtime: match a.runtime.as_str() {
            "pty" => RuntimeKind::Pty,
            "http" => RuntimeKind::Http,
            _ => RuntimeKind::PluginWorker,
        },
        command: a.command.clone(),
        args: a.args.clone(),
        env: a.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        permission_args: a.permission_args.clone(),
        provider: None,
        icon: (!a.icon.is_empty()).then(|| a.icon.clone()),
        homepage: (!a.homepage.is_empty()).then(|| a.homepage.clone()),
        hook: a.hook.as_ref().map(|h| HookSpec {
            config_path: h.config_path.clone(),
            namespace: h.namespace.clone(),
            command_template: h.command_template.clone(),
        }),
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
