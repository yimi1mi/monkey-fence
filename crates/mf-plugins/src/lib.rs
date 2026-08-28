//! mf-plugins:MonkeyFence 统一插件系统(ADR 0002)。
//!
//! - 清单与校验:`manifest`
//! - 安装/锁文件/哈希:`install`
//! - 内置合成插件(CLI Agent / API Provider / 技能):`builtin`
//! - 状态钩子写入:`hooks`
//! - 后台 worker NDJSON 协议:`worker`
//! - 运行时注册表:`PluginRegistry`(本文件)

pub mod builtin;
pub mod hooks;
pub mod install;
pub mod manifest;
pub mod worker;

use anyhow::{bail, Result};
use install::{InstallSource, LockEntry};
use manifest::PluginManifest;
use mf_agent::pipeline::PipelineDraft;
use mf_agent::runtime::AgentProfileSpec;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// 注册表中的一个插件(内置合成或已安装)。
#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub full_id: String,
    pub manifest: PluginManifest,
    pub root: Option<PathBuf>,
    pub source: InstallSource,
    pub content_hash: String,
    pub permission_fingerprint: String,
    pub enabled: bool,
    pub authorized_at: Option<String>,
    pub builtin: bool,
    /// 检测结果缓存(profile_id → 是否检测到),由 refresh_detection 刷新。
    pub detected: HashMap<String, bool>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginSummary {
    pub full_id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub homepage: String,
    pub source_kind: String,
    pub enabled: bool,
    pub builtin: bool,
    pub authorized_at: Option<String>,
    pub agents: Vec<String>,
    pub has_worker: bool,
    pub capabilities: manifest::Capabilities,
}

pub struct PluginRegistry {
    plugins: RwLock<Vec<PluginEntry>>,
    /// 用户覆盖的 agent 命令/参数(设置页编辑)。
    agent_overrides: RwLock<HashMap<String, AgentProfileSpec>>,
    /// 插件流水线模板(id → (模板名, draft))。
    templates: RwLock<Vec<(String, String, PipelineDraft)>>,
}

impl PluginRegistry {
    /// 扫描插件目录 + 生成内置合成插件。
    pub fn load(config: &mf_agent::Config, skills: &[mf_skills::Skill]) -> Arc<PluginRegistry> {
        let mut plugins: Vec<PluginEntry> = Vec::new();

        // 内置 CLI Agent(始终"已安装",默认启用,但 detected 由 PATH 决定)
        for agent in builtin::builtin_cli_agents() {
            let m = builtin::synthetic_manifest(&agent);
            plugins.push(PluginEntry {
                content_hash: String::new(),
                permission_fingerprint: m.permission_fingerprint("builtin"),
                full_id: m.full_id(),
                manifest: m,
                root: None,
                source: InstallSource::Bundled,
                enabled: true,
                authorized_at: Some(chrono::Utc::now().to_rfc3339()),
                builtin: true,
                detected: HashMap::new(),
            });
        }

        // API Provider → 内置合成 profile(openai 兼容 / anthropic / mock)
        {
            let mut m = PluginManifest {
                manifest: manifest::ManifestHeader {
                    version: 1,
                    publisher: "monkeyfence".into(),
                    id: "api-providers".into(),
                    name: "API 智能体(内置)".into(),
                    version_str: "0.1.0".into(),
                    min_app_version: String::new(),
                    description:
                        "来自 ~/.monkeyfence/config.toml 的 OpenAI 兼容 / Anthropic / mock 提供方"
                            .into(),
                    homepage: String::new(),
                    icon: String::new(),
                },
                capabilities: manifest::Capabilities {
                    net: true,
                    ..Default::default()
                },
                worker: None,
                agents: config
                    .providers
                    .iter()
                    .map(|(name, _p)| manifest::AgentContribution {
                        id: name.clone(),
                        name: name.clone(),
                        runtime: "http".into(),
                        command: String::new(),
                        args: vec![],
                        env: Default::default(),
                        permission_args: vec![],
                        homepage: String::new(),
                        icon: String::new(),
                        hook: None,
                    })
                    .collect(),
                pipelines: vec![],
                skills: vec![],
                tools: vec![],
            };
            m.manifest.id = "api-providers".into();
            plugins.push(PluginEntry {
                content_hash: String::new(),
                permission_fingerprint: m.permission_fingerprint("builtin"),
                full_id: m.full_id(),
                manifest: m,
                root: None,
                source: InstallSource::Bundled,
                enabled: true,
                authorized_at: Some(chrono::Utc::now().to_rfc3339()),
                builtin: true,
                detected: config.providers.keys().map(|k| (k.clone(), true)).collect(),
            });
        }

        // 现有技能 → 兼容合成插件
        {
            let m = PluginManifest {
                manifest: manifest::ManifestHeader {
                    version: 1,
                    publisher: "monkeyfence".into(),
                    id: "skills".into(),
                    name: "技能(兼容合成插件)".into(),
                    version_str: "0.1.0".into(),
                    min_app_version: String::new(),
                    description: "项目与全局技能目录中的现有技能".into(),
                    homepage: String::new(),
                    icon: String::new(),
                },
                capabilities: manifest::Capabilities::default(),
                worker: None,
                agents: vec![],
                pipelines: vec![],
                skills: skills
                    .iter()
                    .map(|s| manifest::SkillContribution {
                        path: s.source.display().to_string(),
                    })
                    .collect(),
                tools: vec![],
            };
            plugins.push(PluginEntry {
                content_hash: String::new(),
                permission_fingerprint: m.permission_fingerprint("builtin"),
                full_id: m.full_id(),
                manifest: m,
                root: None,
                source: InstallSource::Bundled,
                enabled: true,
                authorized_at: Some(chrono::Utc::now().to_rfc3339()),
                builtin: true,
                detected: HashMap::new(),
            });
        }

        // 已安装的第三方插件(默认禁用,直到用户授权)
        for (root, m, lock) in install::load_installed(None) {
            plugins.push(PluginEntry {
                full_id: m.full_id(),
                manifest: m,
                root: Some(root),
                source: lock.source,
                content_hash: lock.content_hash,
                permission_fingerprint: lock.permission_fingerprint,
                enabled: lock.enabled,
                authorized_at: lock.authorized_at,
                builtin: false,
                detected: HashMap::new(),
            });
        }

        let reg = Arc::new(PluginRegistry {
            plugins: RwLock::new(plugins),
            agent_overrides: RwLock::new(HashMap::new()),
            templates: RwLock::new(Vec::new()),
        });
        reg.refresh_detection();
        reg.reload_templates();
        reg
    }

    /// 刷新 CLI Agent PATH 检测。
    pub fn refresh_detection(&self) {
        let mut plugins = self.plugins.write();
        for p in plugins.iter_mut() {
            for a in &p.manifest.agents {
                let detected = if a.runtime == "pty" && !a.command.is_empty() {
                    builtin::detect_on_path(&a.command).is_some()
                } else if a.runtime == "http" {
                    true
                } else {
                    false // plugin-worker:未实现前视为不可用
                };
                p.detected.insert(a.id.clone(), detected);
            }
        }
    }

    /// 从插件目录加载流水线模板。
    pub fn reload_templates(&self) {
        let mut out = Vec::new();
        let plugins = self.plugins.read();
        for p in plugins.iter() {
            let Some(root) = &p.root else { continue };
            for t in &p.manifest.pipelines {
                let full = root.join(&t.file);
                let Ok(text) = std::fs::read_to_string(&full) else {
                    log::warn!("流水线模板缺失: {}", full.display());
                    continue;
                };
                match serde_json::from_str::<PipelineDraft>(&text) {
                    Ok(d) => out.push((format!("{}:{}", p.full_id, t.id), t.name.clone(), d)),
                    Err(e) => log::warn!("流水线模板解析失败 {}: {e}", full.display()),
                }
            }
        }
        *self.templates.write() = out;
    }

    pub fn templates(&self) -> Vec<(String, String, PipelineDraft)> {
        self.templates.read().clone()
    }

    pub fn summaries(&self) -> Vec<PluginSummary> {
        let plugins = self.plugins.read();
        plugins
            .iter()
            .map(|p| PluginSummary {
                full_id: p.full_id.clone(),
                name: p.manifest.manifest.name.clone(),
                version: p.manifest.manifest.version_str.clone(),
                description: p.manifest.manifest.description.clone(),
                homepage: p.manifest.manifest.homepage.clone(),
                source_kind: match &p.source {
                    InstallSource::Bundled => "bundled".into(),
                    InstallSource::Local { .. } => "local".into(),
                    InstallSource::Git { .. } => "git".into(),
                    InstallSource::Marketplace { .. } => "marketplace".into(),
                },
                enabled: p.enabled,
                builtin: p.builtin,
                authorized_at: p.authorized_at.clone(),
                agents: p.manifest.agents.iter().map(|a| a.id.clone()).collect(),
                has_worker: p.manifest.worker.is_some(),
                capabilities: p.manifest.capabilities.clone(),
            })
            .collect()
    }

    /// 启用(= 授权)。要求当前权限指纹与授权指纹一致;变化则要求重新授权。
    pub fn enable(&self, full_id: &str, reauthorize: bool) -> Result<()> {
        let mut plugins = self.plugins.write();
        let p = plugins
            .iter_mut()
            .find(|p| p.full_id == full_id)
            .ok_or_else(|| anyhow::anyhow!("插件不存在: {full_id}"))?;
        let current = p.manifest.permission_fingerprint(&p.content_hash);
        if !p.builtin {
            let authorized = p.authorized_at.is_some();
            if authorized && p.permission_fingerprint != current && !reauthorize {
                bail!("插件权限/内容发生变化,需要重新授权才能启用");
            }
            p.permission_fingerprint = current;
            if reauthorize || p.authorized_at.is_none() {
                p.authorized_at = Some(chrono::Utc::now().to_rfc3339());
            }
            persist_lock_entry(p);
        }
        p.enabled = true;
        Ok(())
    }

    pub fn disable(&self, full_id: &str) -> Result<()> {
        let mut plugins = self.plugins.write();
        let p = plugins
            .iter_mut()
            .find(|p| p.full_id == full_id)
            .ok_or_else(|| anyhow::anyhow!("插件不存在: {full_id}"))?;
        if p.builtin {
            bail!("内置合成插件不可禁用");
        }
        p.enabled = false;
        persist_lock_entry(p);
        Ok(())
    }

    /// 用户在设置页覆盖 agent 命令/参数/环境。
    pub fn set_agent_override(&self, spec: AgentProfileSpec) {
        self.agent_overrides.write().insert(spec.id.clone(), spec);
    }

    pub fn clear_agent_override(&self, profile_id: &str) {
        self.agent_overrides.write().remove(profile_id);
    }

    /// 全部可执行 Agent Profile(仅来自启用插件;含覆盖与检测状态)。
    pub fn agent_profiles(&self) -> Vec<AgentProfileSpec> {
        let plugins = self.plugins.read();
        let overrides = self.agent_overrides.read();
        let mut out = Vec::new();
        out.push(builtin::blank_terminal_profile());
        for p in plugins.iter().filter(|p| p.enabled) {
            for a in &p.manifest.agents {
                let mut spec = builtin::profile_spec_from_contribution(&p.full_id, a);
                if let Some(over) = overrides.get(&a.id) {
                    spec = over.clone();
                }
                out.push(spec);
            }
        }
        out
    }

    /// 插件是否允许运行 worker(禁用/未授权插件不得运行)。
    pub fn worker_allowed(&self, full_id: &str) -> Result<manifest::WorkerSpec> {
        let plugins = self.plugins.read();
        let p = plugins
            .iter()
            .find(|p| p.full_id == full_id)
            .ok_or_else(|| anyhow::anyhow!("插件不存在: {full_id}"))?;
        if !p.enabled {
            bail!("插件已禁用,不得运行 worker");
        }
        if !p.builtin && p.authorized_at.is_none() {
            bail!("插件未授权,不得运行 worker");
        }
        p.manifest
            .worker
            .clone()
            .ok_or_else(|| anyhow::anyhow!("插件没有声明 worker"))
    }
}

fn persist_lock_entry(p: &PluginEntry) {
    if p.builtin {
        return;
    }
    let mut lock = install::load_lock();
    lock.plugins.insert(
        p.full_id.clone(),
        LockEntry {
            full_id: p.full_id.clone(),
            name: p.manifest.manifest.name.clone(),
            version: p.manifest.manifest.version_str.clone(),
            source: p.source.clone(),
            content_hash: p.content_hash.clone(),
            permission_fingerprint: p.permission_fingerprint.clone(),
            enabled: p.enabled,
            authorized_at: p.authorized_at.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    let _ = install::save_lock(&lock);
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::*;

    fn make_registry_with_plugin(
        m: PluginManifest,
        root: Option<PathBuf>,
        enabled: bool,
        authorized: bool,
    ) -> PluginRegistry {
        PluginRegistry {
            plugins: RwLock::new(vec![PluginEntry {
                full_id: m.full_id(),
                content_hash: "h".into(),
                permission_fingerprint: m.permission_fingerprint("h"),
                manifest: m,
                root,
                source: InstallSource::Local { path: "x".into() },
                enabled,
                authorized_at: authorized.then(|| chrono::Utc::now().to_rfc3339()),
                builtin: false,
                detected: HashMap::new(),
            }]),
            agent_overrides: RwLock::new(HashMap::new()),
            templates: RwLock::new(Vec::new()),
        }
    }

    fn worker_manifest() -> PluginManifest {
        PluginManifest {
            manifest: ManifestHeader {
                version: 1,
                publisher: "t".into(),
                id: "w".into(),
                name: "W".into(),
                version_str: "0.1".into(),
                min_app_version: String::new(),
                description: String::new(),
                homepage: String::new(),
                icon: String::new(),
            },
            capabilities: Capabilities::default(),
            worker: Some(WorkerSpec {
                command: "w.exe".into(),
                args: vec![],
            }),
            agents: vec![],
            pipelines: vec![],
            skills: vec![],
            tools: vec![],
        }
    }

    #[test]
    fn disabled_plugin_cannot_run_worker() {
        let reg = make_registry_with_plugin(worker_manifest(), None, false, true);
        assert!(
            reg.worker_allowed("t.w").is_err(),
            "禁用插件不得运行 worker"
        );
        let reg = make_registry_with_plugin(worker_manifest(), None, true, false);
        assert!(
            reg.worker_allowed("t.w").is_err(),
            "未授权插件不得运行 worker"
        );
        let reg = make_registry_with_plugin(worker_manifest(), None, true, true);
        assert!(reg.worker_allowed("t.w").is_ok());
    }

    #[test]
    fn permission_change_requires_reauth() {
        let reg = make_registry_with_plugin(worker_manifest(), None, true, true);
        // 相同指纹:直接启用 OK
        assert!(reg.enable("t.w", false).is_ok());
        // 修改插件(worker 命令变化)→ 指纹变化
        let mut plugins = reg.plugins.write();
        let p = plugins.first_mut().unwrap();
        p.manifest.worker = Some(WorkerSpec {
            command: "evil.exe".into(),
            args: vec![],
        });
        drop(plugins);
        assert!(
            reg.enable("t.w", false).is_err(),
            "权限/内容变化必须要求重新授权"
        );
        assert!(reg.enable("t.w", true).is_ok(), "显式重新授权后可启用");
    }

    #[test]
    fn builtin_agents_registered_and_detected() {
        let config = mf_agent::Config::default();
        let reg = PluginRegistry::load(&config, &[]);
        let profiles = reg.agent_profiles();
        let ids: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
        for expected in [
            "codex",
            "claude",
            "opencode",
            "cursor",
            "kimi",
            "mock",
            "blank-terminal",
        ] {
            assert!(ids.contains(&expected), "缺少 profile {expected}");
        }
        // mock(API)始终检测为可用;CLI 至少命令结构正确
        let mock = profiles.iter().find(|p| p.id == "mock").unwrap();
        assert_eq!(mock.runtime, mf_agent::runtime::RuntimeKind::Http);
    }

    #[test]
    fn agent_override_applies() {
        let config = mf_agent::Config::default();
        let reg = PluginRegistry::load(&config, &[]);
        let mut spec = reg
            .agent_profiles()
            .into_iter()
            .find(|p| p.id == "codex")
            .unwrap();
        spec.args = vec!["--custom".into()];
        reg.set_agent_override(spec);
        let over = reg
            .agent_profiles()
            .into_iter()
            .find(|p| p.id == "codex")
            .unwrap();
        assert!(over.args.contains(&"--custom".to_string()));
        reg.clear_agent_override("codex");
        let restored = reg
            .agent_profiles()
            .into_iter()
            .find(|p| p.id == "codex")
            .unwrap();
        assert!(!restored.args.contains(&"--custom".to_string()));
    }
}
