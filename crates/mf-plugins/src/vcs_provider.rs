//! 版本控制插件契约的内置实现与运行时解析。
//!
//! 设置页只消费 `VcsProviderContribution.settings`；本模块把已启用贡献的
//! adapter + 单实例配置解析为命令环境。这样 Git/P4 的字段、默认值和测试
//! 行为都归插件所有，宿主页面不需要知道具体工具。

use crate::contribution_registry::ContributionRegistry;
use crate::manifest::{
    Capabilities, ManifestHeader, PluginManifest, SettingFieldContribution,
    SettingOptionContribution, VcsProviderContribution,
};
use anyhow::{anyhow, Result};
use mf_agent::Config;
use mf_vcs::git_cli::{GitCli, GitCliConfig};
use mf_vcs::p4::{P4CommandConfig, P4};
use std::path::Path;

pub const BUILTIN_VCS_PLUGIN_ID: &str = "monkeyfence.vcs";
pub const BUILTIN_VCS_VERSION: &str = "0.1.0";

fn boolean_field(
    id: &str,
    label: &str,
    default: bool,
    description: &str,
) -> SettingFieldContribution {
    SettingFieldContribution {
        id: id.into(),
        label: label.into(),
        kind: "boolean".into(),
        default: default.to_string(),
        placeholder: String::new(),
        description: description.into(),
        options: vec![],
    }
}

fn text_field(
    id: &str,
    label: &str,
    kind: &str,
    default: &str,
    placeholder: &str,
    description: &str,
) -> SettingFieldContribution {
    SettingFieldContribution {
        id: id.into(),
        label: label.into(),
        kind: kind.into(),
        default: default.into(),
        placeholder: placeholder.into(),
        description: description.into(),
        options: vec![],
    }
}

pub fn git_contribution() -> VcsProviderContribution {
    VcsProviderContribution {
        id: "git".into(),
        name: "Git".into(),
        adapter: "git-cli".into(),
        description: "仓库读取使用内置 libgit2；外部命令统一使用此实例配置。".into(),
        settings: vec![
            boolean_field(
                "active",
                "启用 Git 集成",
                true,
                "关闭后不再自动探测 Git 仓库。",
            ),
            text_field(
                "executable",
                "Git 可执行文件",
                "path",
                "git",
                "git 或 C:\\Program Files\\Git\\cmd\\git.exe",
                "用于版本检测、还原和应用补丁；不会修改 Git 全局配置。",
            ),
        ],
    }
}

pub fn p4_contribution() -> VcsProviderContribution {
    VcsProviderContribution {
        id: "p4".into(),
        name: "Perforce".into(),
        adapter: "perforce-cli".into(),
        description: "通过 p4 CLI 连接 Helix Core；配置仅保存在 MonkeyFence 单一实例中。".into(),
        settings: vec![
            boolean_field(
                "active",
                "在线 / 启用 Perforce",
                true,
                "关闭后不连接服务器，也不自动探测 P4 workspace。",
            ),
            text_field(
                "executable",
                "P4 可执行文件",
                "path",
                "p4",
                "p4 或 p4.exe 的完整路径",
                "MonkeyFence 启动 p4 命令时使用的程序。",
            ),
            SettingFieldContribution {
                id: "configuration".into(),
                label: "连接配置来源".into(),
                kind: "select".into(),
                default: "p4config".into(),
                placeholder: String::new(),
                description: "P4CONFIG 模式继承项目目录配置；手动模式只注入下方连接变量。".into(),
                options: vec![
                    SettingOptionContribution {
                        value: "p4config".into(),
                        label: "P4CONFIG / 环境".into(),
                    },
                    SettingOptionContribution {
                        value: "manual".into(),
                        label: "手动参数".into(),
                    },
                ],
            },
            text_field(
                "p4config",
                "P4CONFIG 文件名",
                "text",
                "",
                "留空=自动检测 p4config.txt / .p4config",
                "仅 P4CONFIG 模式使用；不会修改系统 P4 环境。",
            ),
            text_field(
                "port",
                "P4PORT",
                "text",
                "",
                "ssl:server:1666",
                "仅手动模式使用。",
            ),
            text_field("user", "P4USER", "text", "", "username", "仅手动模式使用。"),
            text_field(
                "client",
                "P4CLIENT",
                "text",
                "",
                "workspace",
                "仅手动模式使用。",
            ),
            text_field(
                "charset",
                "P4CHARSET",
                "text",
                "",
                "例如 utf8",
                "仅手动模式使用。",
            ),
        ],
    }
}

/// 内置 VCS 包的合成清单；它与其他内置能力一样进入 PluginHost，设置页
/// 只能通过 ContributionRegistry 发现它。
pub fn synthetic_manifest() -> PluginManifest {
    PluginManifest {
        manifest: ManifestHeader {
            version: crate::manifest::MANIFEST_VERSION,
            publisher: "monkeyfence".into(),
            id: "vcs".into(),
            name: "版本控制环境(内置)".into(),
            version_str: BUILTIN_VCS_VERSION.into(),
            min_app_version: String::new(),
            description: "Git 与 Perforce 的项目级命令环境和声明式设置".into(),
            homepage: String::new(),
            icon: String::new(),
        },
        capabilities: Capabilities {
            fs_read: true,
            fs_write: true,
            spawn: true,
            shell: true,
            vcs: true,
            net: true,
            ..Default::default()
        },
        worker: None,
        agent_types: vec![],
        node_types: vec![],
        execution_directory_providers: vec![],
        vcs_providers: vec![git_contribution(), p4_contribution()],
        secret_stores: vec![],
        workflow_templates: vec![],
        skills: vec![],
        tools: vec![],
        ui_schemas: vec![],
    }
}

fn field_default<'a>(provider: &'a VcsProviderContribution, field_id: &str) -> &'a str {
    provider
        .settings
        .iter()
        .find(|field| field.id == field_id)
        .map(|field| field.default.as_str())
        .unwrap_or_default()
}

fn value(
    config: &Config,
    full_id: &str,
    provider: &VcsProviderContribution,
    field_id: &str,
) -> String {
    config.plugin_value(full_id, field_id, field_default(provider, field_id))
}

fn enabled(config: &Config, full_id: &str, provider: &VcsProviderContribution) -> bool {
    value(config, full_id, provider, "active")
        .parse::<bool>()
        .unwrap_or(true)
}

fn p4_config(
    config: &Config,
    full_id: &str,
    provider: &VcsProviderContribution,
) -> P4CommandConfig {
    P4CommandConfig {
        executable: value(config, full_id, provider, "executable"),
        use_p4config: value(config, full_id, provider, "configuration") != "manual",
        p4config: value(config, full_id, provider, "p4config"),
        port: value(config, full_id, provider, "port"),
        user: value(config, full_id, provider, "user"),
        client: value(config, full_id, provider, "client"),
        charset: value(config, full_id, provider, "charset"),
    }
}

#[derive(Clone, Debug, Default)]
pub struct VcsEnvironment {
    pub git: Option<GitCliConfig>,
    pub p4: Option<P4CommandConfig>,
}

impl VcsEnvironment {
    /// 只从当前启用插件贡献解析。未知 adapter 保持可配置但不会被宿主冒充执行。
    pub fn resolve(registry: &ContributionRegistry, config: &Config) -> Self {
        let mut environment = Self::default();
        for (full_id, _, provider) in registry.vcs_providers() {
            if !enabled(config, &full_id, &provider) {
                continue;
            }
            match provider.adapter.as_str() {
                "git-cli" if environment.git.is_none() => {
                    environment.git = Some(GitCliConfig {
                        executable: value(config, &full_id, &provider, "executable"),
                    });
                }
                "perforce-cli" if environment.p4.is_none() => {
                    environment.p4 = Some(p4_config(config, &full_id, &provider));
                }
                _ => {}
            }
        }
        environment
    }

    pub fn git_cli(&self, cwd: impl AsRef<Path>) -> Option<GitCli> {
        self.git.clone().map(|config| GitCli::new(cwd, config))
    }

    pub fn p4(&self, cwd: impl AsRef<Path>) -> Option<P4> {
        self.p4.clone().map(|config| P4::with_config(cwd, config))
    }
}

/// 设置页“测试”按钮的插件运行时入口。
pub fn test_provider(
    registry: &ContributionRegistry,
    config: &Config,
    full_id: &str,
    cwd: &Path,
) -> Result<String> {
    let (_, provider) = registry
        .find_vcs_provider(full_id)
        .ok_or_else(|| anyhow!("VCS 插件贡献不存在或未启用: {full_id}"))?;
    match provider.adapter.as_str() {
        "git-cli" => {
            let cli = GitCli::new(
                cwd,
                GitCliConfig {
                    executable: value(config, full_id, &provider, "executable"),
                },
            );
            Ok(cli.version()?)
        }
        "perforce-cli" => {
            let p4 = P4::with_config(cwd, p4_config(config, full_id, &provider));
            let info = p4.info()?;
            Ok(format!(
                "{} @ {} · {}",
                info.client_name, info.server_name, info.user_name
            ))
        }
        adapter => anyhow::bail!("当前宿主不支持 VCS adapter `{adapter}` 的连接测试"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{install::InstallSource, PluginEntry};
    use std::collections::HashMap;

    fn registry() -> ContributionRegistry {
        let manifest = synthetic_manifest();
        ContributionRegistry::from_enabled(&[PluginEntry {
            full_id: manifest.full_id(),
            manifest,
            root: None,
            source: InstallSource::Bundled,
            content_hash: String::new(),
            permission_fingerprint: String::new(),
            enabled: true,
            authorized_at: Some("now".into()),
            builtin: true,
            detected: HashMap::new(),
        }])
    }

    #[test]
    fn manifest_contributes_git_and_p4_settings() {
        let providers = registry().vcs_providers();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].0, "monkeyfence.vcs.git");
        assert_eq!(providers[1].0, "monkeyfence.vcs.p4");
        assert!(providers[1]
            .2
            .settings
            .iter()
            .any(|field| field.id == "configuration" && field.kind == "select"));
    }

    #[test]
    fn resolves_single_instance_values_without_global_mutation() {
        let registry = registry();
        let mut config = Config::default();
        config.set_plugin_value("monkeyfence.vcs.git", "executable", "custom-git");
        config.set_plugin_value("monkeyfence.vcs.p4", "configuration", "manual");
        config.set_plugin_value("monkeyfence.vcs.p4", "port", "ssl:p4:1666");
        let environment = VcsEnvironment::resolve(&registry, &config);
        assert_eq!(environment.git.unwrap().executable, "custom-git");
        let p4 = environment.p4.unwrap();
        assert!(!p4.use_p4config);
        assert_eq!(p4.port, "ssl:p4:1666");
    }
}
