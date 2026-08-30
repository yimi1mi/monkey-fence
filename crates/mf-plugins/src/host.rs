//! Plugin Host:插件发现、安装、授权、内容寻址解析与运行期 pin(ADR 0002 / 0003)。
//!
//! - 包按内容哈希存放(`packages/<sha256>/`),发布后不可变;
//! - `resolve` 按 (full_id, version, hash) 精确取包并重验哈希;
//! - 活动 Revision 通过 `pin_for_run` 固定包版本,插件更新不替换活动 pin;
//! - pin 以 CatalogStore 为事实源,内存引用计数是可重建投影。

use crate::install::{self, InstallSource};
use crate::manifest::PluginManifest;
use crate::persist_lock_entry;
use crate::PluginEntry;
use anyhow::{bail, Context as _, Result};
use mf_agent::pipeline::PipelineDraft;
use mf_agent::runtime::AgentProfileSpec;
use mf_agent::{CatalogStore, PluginPinRecord};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::contribution_registry::ContributionRegistry;

/// 按 (full_id, version, content_hash) 解析出的不可变包视图。
#[derive(Debug, Clone)]
pub struct ResolvedPlugin {
    pub full_id: String,
    pub version: String,
    pub content_hash: String,
    pub root: PathBuf,
    pub manifest: PluginManifest,
}

/// 一次运行的插件固定记录;`release_run_pins` 之前保证对应包不被清理。
#[derive(Debug, Clone)]
pub struct PluginPin {
    pub run_key: String,
    pub full_id: String,
    pub version: String,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
struct PackageRecord {
    root: PathBuf,
    manifest: PluginManifest,
}

/// 内置合成目录插件身份(进程内 worktree 实现的唯一归属)。
pub const BUILTIN_DIRECTORIES_PLUGIN_ID: &str = "monkeyfence.directories";
/// 内置合成目录插件版本(与合成清单 version_str 一致)。
pub const BUILTIN_DIRECTORIES_VERSION: &str = "0.1.0";

/// 目录提供器工厂:内置进程内实现或第三方 worker 进程驱动。
#[derive(Clone, Debug)]
pub enum DirectoryProviderFactory {
    /// 内置合成插件的 worktree 进程内实现(GitWorktreeProvider)。
    BuiltinWorktree,
    /// 第三方:worker 进程驱动(命令相对插件包根或绝对路径)。
    Worker {
        command: String,
        args: Vec<String>,
        plugin_root: PathBuf,
    },
}

/// 按「完整贡献 ID + 版本 + 内容哈希」解析出的目录提供器。
#[derive(Clone, Debug)]
pub struct DirectoryProviderResolution {
    /// 完整贡献 ID(plugin_full_id.contribution_id)。
    pub full_contribution_id: String,
    /// 策略实现标识(清单声明)。
    pub kind: String,
    pub isolates: bool,
    pub supports_parallel: bool,
    pub factory: DirectoryProviderFactory,
    /// 解析出的插件包 pin(F10:生产构造 WorkerDirectoryProvider 的
    /// 必需输入;完整贡献 ID + 精确版本 + 内容哈希)。
    pub pin: mf_agent::workflow::PluginSourcePin,
}

pub struct PluginHost {
    /// 插件根目录(包与锁文件都位于其下)。
    root: PathBuf,
    catalog: Arc<CatalogStore>,
    plugins: RwLock<Vec<PluginEntry>>,
    /// 内置 profile(命令/参数/钩子全保真),加载时按固定顺序构建。
    builtin_profiles: RwLock<Vec<AgentProfileSpec>>,
    /// 用户覆盖的 agent 命令/参数(设置页编辑)。
    agent_overrides: RwLock<HashMap<String, AgentProfileSpec>>,
    /// 插件流水线模板(id → (模板名, draft))。
    templates: RwLock<Vec<(String, String, PipelineDraft)>>,
    /// 内容寻址包索引:content_hash → 包记录(全部已安装版本,含未选择的历史版本)。
    packages: RwLock<HashMap<String, PackageRecord>>,
    /// 活动 pin:run_key → 该运行的 pin 列表(内存引用计数)。
    pins: RwLock<HashMap<String, Vec<PluginPin>>>,
    /// content_hash → 活动引用数;0 或缺失 = 可清理。
    pin_counts: RwLock<HashMap<String, usize>>,
}

impl PluginHost {
    /// 空宿主(指定插件根;不加载内置/已安装,用于测试与嵌入场景)。
    pub fn empty_at(root: PathBuf) -> Arc<PluginHost> {
        Self::empty_at_with_catalog(
            root,
            CatalogStore::memory().expect("内存目录库初始化不应失败"),
        )
    }

    pub fn empty_at_with_catalog(root: PathBuf, catalog: Arc<CatalogStore>) -> Arc<PluginHost> {
        let (pins, pin_counts) = Self::load_pin_state(&catalog);
        Arc::new(PluginHost {
            root,
            catalog,
            plugins: RwLock::new(Vec::new()),
            builtin_profiles: RwLock::new(Vec::new()),
            agent_overrides: RwLock::new(HashMap::new()),
            templates: RwLock::new(Vec::new()),
            packages: RwLock::new(HashMap::new()),
            pins: RwLock::new(pins),
            pin_counts: RwLock::new(pin_counts),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 扫描插件目录 + 生成内置合成插件。
    pub fn load(config: &mf_agent::Config, skills: &[mf_skills::Skill]) -> Arc<PluginHost> {
        Self::load_with_catalog(
            CatalogStore::memory().expect("内存目录库初始化不应失败"),
            config,
            skills,
        )
    }

    pub fn load_with_catalog(
        catalog: Arc<CatalogStore>,
        config: &mf_agent::Config,
        skills: &[mf_skills::Skill],
    ) -> Arc<PluginHost> {
        Self::load_at_with_catalog(install::plugins_root(), catalog, config, skills)
    }

    pub fn load_at(
        root: PathBuf,
        config: &mf_agent::Config,
        skills: &[mf_skills::Skill],
    ) -> Arc<PluginHost> {
        Self::load_at_with_catalog(
            root,
            CatalogStore::memory().expect("内存目录库初始化不应失败"),
            config,
            skills,
        )
    }

    pub fn load_at_with_catalog(
        root: PathBuf,
        catalog: Arc<CatalogStore>,
        config: &mf_agent::Config,
        skills: &[mf_skills::Skill],
    ) -> Arc<PluginHost> {
        let mut plugins: Vec<PluginEntry> = Vec::new();

        // 内置 CLI Agent(始终"已安装",默认启用,但 detected 由 PATH 决定)
        for agent in crate::builtin::builtin_cli_agents() {
            let m = crate::builtin::synthetic_manifest(&agent);
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

        // API Provider → 内置合成 Agent Type(openai 兼容 / anthropic / mock)
        {
            let m = PluginManifest {
                manifest: crate::manifest::ManifestHeader {
                    version: crate::manifest::MANIFEST_VERSION,
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
                capabilities: crate::manifest::Capabilities {
                    net: true,
                    ..Default::default()
                },
                worker: None,
                agent_types: config
                    .providers
                    .iter()
                    .map(|(name, _p)| crate::manifest::AgentTypeContribution {
                        id: name.clone(),
                        name: name.clone(),
                        adapter: "http".into(),
                        config_schema: String::new(),
                        command: String::new(),
                        detect_commands: vec![],
                        modes: vec!["oneshot".into()],
                        supports_isolated_config: false,
                    })
                    .collect(),
                node_types: vec![],
                execution_directory_providers: vec![],
                secret_stores: vec![],
                workflow_templates: vec![],
                skills: vec![],
                tools: vec![],
                ui_schemas: vec![],
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
                detected: config.providers.keys().map(|k| (k.clone(), true)).collect(),
            });
        }

        // 执行目录提供器(内置合成:项目目录 + Git worktree 隔离)
        {
            let m = PluginManifest {
                manifest: crate::manifest::ManifestHeader {
                    version: crate::manifest::MANIFEST_VERSION,
                    publisher: "monkeyfence".into(),
                    id: "directories".into(),
                    name: "执行目录提供器(内置)".into(),
                    version_str: "0.1.0".into(),
                    min_app_version: String::new(),
                    description: "项目目录共享租约与 Git worktree 隔离租约".into(),
                    homepage: String::new(),
                    icon: String::new(),
                },
                worker: None,
                agent_types: vec![],
                node_types: vec![],
                // worktree 提供器读写工作区并操作集成 refs:
                // 能力声明由清单承载,解析方(app_ctx)据此做 pinned 校验
                capabilities: crate::manifest::Capabilities {
                    fs_read: true,
                    fs_write: true,
                    vcs: true,
                    ..Default::default()
                },
                execution_directory_providers: vec![
                    crate::project_directory_provider::contribution(),
                    crate::git_worktree_provider::contribution(),
                ],
                secret_stores: vec![],
                workflow_templates: vec![],
                skills: vec![],
                tools: vec![],
                ui_schemas: vec![],
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

        // 现有技能 → 兼容合成插件
        {
            let m = PluginManifest {
                manifest: crate::manifest::ManifestHeader {
                    version: crate::manifest::MANIFEST_VERSION,
                    publisher: "monkeyfence".into(),
                    id: "skills".into(),
                    name: "技能(兼容合成插件)".into(),
                    version_str: "0.1.0".into(),
                    min_app_version: String::new(),
                    description: "项目与全局技能目录中的现有技能".into(),
                    homepage: String::new(),
                    icon: String::new(),
                },
                capabilities: crate::manifest::Capabilities::default(),
                worker: None,
                agent_types: vec![],
                node_types: vec![],
                execution_directory_providers: vec![],
                secret_stores: vec![],
                workflow_templates: vec![],
                skills: skills
                    .iter()
                    .map(|s| crate::manifest::SkillContribution {
                        path: s.source.display().to_string(),
                    })
                    .collect(),
                tools: vec![],
                ui_schemas: vec![],
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
        let packages = Self::scan_packages(&root);
        for (pkg_root, m, lock) in install::load_installed_at(&root) {
            let _ = &pkg_root;
            plugins.push(PluginEntry {
                full_id: m.full_id(),
                manifest: m,
                root: Some(pkg_root),
                source: lock.source,
                content_hash: lock.content_hash,
                permission_fingerprint: lock.permission_fingerprint,
                enabled: lock.enabled,
                authorized_at: lock.authorized_at,
                builtin: false,
                detected: HashMap::new(),
            });
        }

        // 内置 profile:CLI Agent(全保真)→ API Provider(按 id 解析配置)
        let builtin_profiles: Vec<AgentProfileSpec> = crate::builtin::builtin_cli_agents()
            .iter()
            .map(crate::builtin::profile_spec_from_builtin)
            .chain(
                config
                    .providers
                    .keys()
                    .map(|n| crate::builtin::http_profile(n)),
            )
            .collect();

        let (pins, pin_counts) = Self::load_pin_state(&catalog);
        let host = Arc::new(PluginHost {
            root,
            catalog,
            plugins: RwLock::new(plugins),
            builtin_profiles: RwLock::new(builtin_profiles),
            agent_overrides: RwLock::new(HashMap::new()),
            templates: RwLock::new(Vec::new()),
            packages: RwLock::new(packages),
            pins: RwLock::new(pins),
            pin_counts: RwLock::new(pin_counts),
        });
        host.refresh_detection();
        host.reload_templates();
        host
    }

    fn load_pin_state(
        catalog: &CatalogStore,
    ) -> (HashMap<String, Vec<PluginPin>>, HashMap<String, usize>) {
        let records = match catalog.list_plugin_pins() {
            Ok(records) => records,
            Err(error) => {
                log::error!("读取持久化插件 pin 失败: {error:#}");
                Vec::new()
            }
        };
        let mut pins: HashMap<String, Vec<PluginPin>> = HashMap::new();
        let mut counts = HashMap::new();
        for record in records {
            let pin = PluginPin {
                run_key: record.run_key.clone(),
                full_id: record.full_id,
                version: record.version,
                content_hash: record.content_hash.clone(),
            };
            pins.entry(record.run_key).or_default().push(pin);
            *counts.entry(record.content_hash).or_insert(0) += 1;
        }
        (pins, counts)
    }

    fn refresh_pin_state(&self) -> Result<()> {
        let records = self.catalog.list_plugin_pins()?;
        let mut pins: HashMap<String, Vec<PluginPin>> = HashMap::new();
        let mut counts = HashMap::new();
        for record in records {
            let pin = PluginPin {
                run_key: record.run_key.clone(),
                full_id: record.full_id,
                version: record.version,
                content_hash: record.content_hash.clone(),
            };
            pins.entry(record.run_key).or_default().push(pin);
            *counts.entry(record.content_hash).or_insert(0) += 1;
        }
        *self.pins.write() = pins;
        *self.pin_counts.write() = counts;
        Ok(())
    }

    /// 扫描内容寻址包目录,构建 hash → 包记录索引(不验证哈希;`resolve` 时验证)。
    fn scan_packages(root: &Path) -> HashMap<String, PackageRecord> {
        let mut out = HashMap::new();
        let Ok(entries) = std::fs::read_dir(install::packages_root_at(root)) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if dir_name.len() != 64 || !dir_name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let Ok(manifest) = PluginManifest::load(&path) else {
                log::warn!("包清单非法,跳过索引: {}", path.display());
                continue;
            };
            out.insert(
                format!("sha256:{dir_name}"),
                PackageRecord {
                    root: path,
                    manifest,
                },
            );
        }
        out
    }

    /// 刷新 Agent Type 检测:detect_commands 走 PATH;http 适配器始终可用;
    /// plugin-worker 适配器在实现前视为不可用。
    pub fn refresh_detection(&self) {
        let mut plugins = self.plugins.write();
        for p in plugins.iter_mut() {
            for a in &p.manifest.agent_types {
                let detected = if !a.detect_commands.is_empty() {
                    a.detect_commands
                        .iter()
                        .any(|c| crate::builtin::detect_on_path(c).is_some())
                } else if a.adapter == "http" {
                    true
                } else {
                    false
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
            for t in &p.manifest.workflow_templates {
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

    pub fn summaries(&self) -> Vec<crate::PluginSummary> {
        let plugins = self.plugins.read();
        plugins
            .iter()
            .map(|p| crate::PluginSummary {
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
                agents: p
                    .manifest
                    .agent_types
                    .iter()
                    .map(|a| a.id.clone())
                    .collect(),
                has_worker: p.manifest.worker.is_some(),
                capabilities: p.manifest.capabilities.clone(),
                content_hash: p.content_hash.clone(),
                compatible: crate::manifest::compatible_with_app(
                    &p.manifest.manifest.min_app_version,
                ),
                active_pins: self
                    .catalog
                    .plugin_pin_count_of_plugin(&p.full_id, &p.content_hash)
                    .unwrap_or(0),
                agent_types_count: p.manifest.agent_types.len(),
                node_types_count: p.manifest.node_types.len(),
                ui_schemas_count: p.manifest.ui_schemas.len(),
                execution_directories_count: p.manifest.execution_directory_providers.len(),
                secret_stores_count: p.manifest.secret_stores.len(),
                workflow_templates_count: p.manifest.workflow_templates.len(),
                skills_count: p.manifest.skills.len(),
                tools_count: p.manifest.tools.len(),
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
            persist_lock_entry(p, &self.root);
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
        persist_lock_entry(p, &self.root);
        Ok(())
    }

    /// 用户在设置页覆盖 agent 命令/参数/环境。
    pub fn set_agent_override(&self, spec: AgentProfileSpec) {
        self.agent_overrides.write().insert(spec.id.clone(), spec);
    }

    pub fn clear_agent_override(&self, profile_id: &str) {
        self.agent_overrides.write().remove(profile_id);
    }

    /// 全部可执行 Agent Profile:内置(全保真)+ 启用的第三方插件 Agent Type;
    /// 最后统一应用用户覆盖。
    pub fn agent_profiles(&self) -> Vec<AgentProfileSpec> {
        let overrides = self.agent_overrides.read();
        let mut out: Vec<AgentProfileSpec> = Vec::new();
        out.push(crate::builtin::blank_terminal_profile());
        out.extend(self.builtin_profiles.read().iter().cloned());
        {
            let plugins = self.plugins.read();
            for p in plugins.iter().filter(|p| p.enabled && !p.builtin) {
                for a in &p.manifest.agent_types {
                    out.push(crate::builtin::profile_spec_from_contribution(
                        &p.full_id, a,
                    ));
                }
            }
        }
        for spec in out.iter_mut() {
            if let Some(over) = overrides.get(&spec.id) {
                *spec = over.clone();
            }
        }
        out
    }

    /// 插件是否允许运行 worker(禁用/未授权插件不得运行)。
    pub fn worker_allowed(&self, full_id: &str) -> Result<crate::manifest::WorkerSpec> {
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

    // ---------- 内容寻址安装 / 解析 / pin ----------

    /// 安装来源目录为内容寻址包(staging → 校验 → 哈希 → `packages/<hex>/`),
    /// 返回该包的精确解析视图。同内容重装幂等;新版本不替换旧包目录。
    pub fn install_package(&self, source: &Path, origin: InstallSource) -> Result<ResolvedPlugin> {
        let entry = install::install_from_dir_at(&self.root, source, origin)?;
        // 刷新内存状态:包索引 + 已安装插件列表(保留内置)
        *self.packages.write() = Self::scan_packages(&self.root);
        self.reload_installed_locked();
        self.resolve(&entry.full_id, &entry.version, &entry.content_hash)
    }

    /// 只重载已安装第三方插件(保留内置条目)。
    fn reload_installed_locked(&self) {
        let mut plugins = self.plugins.write();
        plugins.retain(|p| p.builtin);
        for (root, m, lock) in install::load_installed_at(&self.root) {
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
        drop(plugins);
        self.refresh_detection();
        self.reload_templates();
    }

    /// 按 (full_id, version, content_hash) 精确解析包:
    /// 哈希必须命中内容寻址目录、重算哈希一致(防篡改)、身份匹配。
    pub fn resolve(&self, full_id: &str, version: &str, hash: &str) -> Result<ResolvedPlugin> {
        let hit = self.packages.read().get(hash).cloned();
        let record: Option<PackageRecord> = match hit {
            Some(r) => Some(r),
            None => {
                // 索引未命中:重扫一次(其他进程可能已安装),仍无则报错
                let mut packages = self.packages.write();
                *packages = Self::scan_packages(&self.root);
                packages.get(hash).cloned()
            }
        };
        let record = record
            .ok_or_else(|| anyhow::anyhow!("插件包不存在: {full_id} @ {hash}(可能已被清理)"))?;
        if record.manifest.full_id() != full_id {
            bail!(
                "插件包身份不匹配: 请求 {full_id},实际 {}",
                record.manifest.full_id()
            );
        }
        if record.manifest.manifest.version_str != version {
            bail!(
                "插件包版本不匹配: 请求 {version},实际 {}",
                record.manifest.manifest.version_str
            );
        }
        let hash_now = install::content_hash(&record.root)
            .with_context(|| format!("重算包哈希失败: {}", record.root.display()))?;
        if hash_now != hash {
            bail!(
                "插件包内容与哈希不一致(篡改): {} != {hash}",
                record.root.display()
            );
        }
        Ok(ResolvedPlugin {
            full_id: full_id.to_string(),
            version: version.to_string(),
            content_hash: hash.to_string(),
            root: record.root.clone(),
            manifest: record.manifest,
        })
    }

    /// 为一次运行固定插件版本:活动 pin 期间该哈希的包不可被清理,
    /// 插件更新安装新包不会替换 pin 指向的旧包。
    pub fn pin_for_run(&self, run_key: &str, plugin: &ResolvedPlugin) -> Result<PluginPin> {
        // pin 前再次确认包存在且完整
        self.resolve(&plugin.full_id, &plugin.version, &plugin.content_hash)?;
        let pin = PluginPin {
            run_key: run_key.to_string(),
            full_id: plugin.full_id.clone(),
            version: plugin.version.clone(),
            content_hash: plugin.content_hash.clone(),
        };
        self.catalog.record_plugin_pin(&PluginPinRecord {
            run_key: pin.run_key.clone(),
            full_id: pin.full_id.clone(),
            version: pin.version.clone(),
            content_hash: pin.content_hash.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        })?;
        self.refresh_pin_state()?;
        Ok(pin)
    }

    /// 按 pin 解析其固定的包(不随最新安装漂移)。
    pub fn resolve_pin(&self, pin: &PluginPin) -> Result<ResolvedPlugin> {
        self.resolve(&pin.full_id, &pin.version, &pin.content_hash)
    }

    /// 释放一次运行的全部 pin(幂等);引用归零的哈希才可被清理。
    pub fn release_run_pins(&self, run_key: &str) -> Result<()> {
        self.catalog.remove_plugin_pins_for_run(run_key)?;
        self.refresh_pin_state()
    }

    /// 按插件包身份 pin(工作流冻结用):
    /// - 非空 content_hash:走内容寻址包解析(pin 期间不可清理/卸载);
    /// - 空 content_hash:内置合成插件(不可卸载,无 packages 记录),
    ///   校验插件在位后仅记录 pin 引用。
    pub fn pin_source_for_run(
        &self,
        run_key: &str,
        full_id: &str,
        version: &str,
        content_hash: &str,
    ) -> Result<()> {
        if !content_hash.is_empty() {
            let resolved = self.resolve(full_id, version, content_hash)?;
            self.pin_for_run(run_key, &resolved)?;
            return Ok(());
        }
        self.ensure_builtin_plugin(full_id, version)?;
        self.catalog.record_plugin_pin(&PluginPinRecord {
            run_key: run_key.to_string(),
            full_id: full_id.to_string(),
            version: version.to_string(),
            content_hash: content_hash.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        })?;
        self.refresh_pin_state()?;
        Ok(())
    }

    // ---------- 目录提供器解析(I7)----------

    /// 按完整 pinned 身份(完整贡献 ID + 版本 + 内容哈希)解析目录
    /// 提供器(I7):
    /// - 空哈希 = 内置合成插件:身份必须**精确**等于内置 worktree 贡献
    ///   (monkeyfence.directories.worktree)才映射到进程内实现;
    ///   任何其他内置贡献(即便同名)都没有进程内实现 → 拒绝;
    /// - 非空哈希 = 第三方:不得冒充内置身份;内容寻址包解析校验
    ///   版本/哈希/防篡改;贡献 kind 非空;能力校验 fs_read+fs_write
    ///   (隔离类还需 vcs);插件启用且已授权;必须声明 worker。
    pub fn resolve_directory_provider(
        &self,
        full_contribution_id: &str,
        version: &str,
        content_hash: &str,
    ) -> Result<DirectoryProviderResolution> {
        let (plugin_full_id, contribution_id) = full_contribution_id
            .rsplit_once('.')
            .ok_or_else(|| anyhow::anyhow!("完整贡献 ID 非法(缺贡献段): {full_contribution_id}"))?;
        if content_hash.is_empty() {
            // 内置合成插件:精确身份 → 进程内 worktree 工厂
            anyhow::ensure!(
                plugin_full_id == BUILTIN_DIRECTORIES_PLUGIN_ID,
                "内置空哈希身份只属于 {BUILTIN_DIRECTORIES_PLUGIN_ID}(请求 {plugin_full_id})"
            );
            self.ensure_builtin_plugin(plugin_full_id, version)?;
            let contribution = {
                let plugins = self.plugins.read();
                let plugin = plugins
                    .iter()
                    .find(|p| p.builtin && p.full_id == plugin_full_id)
                    .ok_or_else(|| anyhow::anyhow!("内置插件不存在: {plugin_full_id}"))?;
                plugin
                    .manifest
                    .execution_directory_providers
                    .iter()
                    .find(|c| c.id == contribution_id)
                    .cloned()
            }
            .ok_or_else(|| anyhow::anyhow!("内置插件不贡献目录提供器 `{contribution_id}`"))?;
            anyhow::ensure!(
                contribution.id == "worktree"
                    && contribution.kind == "worktree"
                    && contribution.isolates
                    && contribution.supports_parallel,
                "内置目录贡献 `{contribution_id}` 与进程内 worktree 身份不符(借名拒绝)"
            );
            return Ok(DirectoryProviderResolution {
                full_contribution_id: full_contribution_id.to_string(),
                kind: contribution.kind.clone(),
                isolates: contribution.isolates,
                supports_parallel: contribution.supports_parallel,
                factory: DirectoryProviderFactory::BuiltinWorktree,
                pin: mf_agent::workflow::PluginSourcePin {
                    full_id: plugin_full_id.to_string(),
                    version: version.to_string(),
                    content_hash: content_hash.to_string(),
                },
            });
        }
        // 第三方:内容寻址包解析(版本/哈希/防篡改),身份校验在内
        anyhow::ensure!(
            plugin_full_id != BUILTIN_DIRECTORIES_PLUGIN_ID,
            "第三方内容不得冒充内置目录插件身份({plugin_full_id})"
        );
        let resolved = self.resolve(plugin_full_id, version, content_hash)?;
        let contribution = resolved
            .manifest
            .execution_directory_providers
            .iter()
            .find(|c| c.id == contribution_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "插件包 {}@{} 不贡献目录提供器 `{contribution_id}`",
                    plugin_full_id,
                    version
                )
            })?;
        anyhow::ensure!(
            !contribution.kind.trim().is_empty(),
            "目录提供器 `{full_contribution_id}` 未声明 kind"
        );
        // 启用 + 授权 + 能力(隔离类需要 vcs)
        // 启用/授权按插件身份(升级后最新安装条目);内容真实性由
        // resolve() 的内容寻址哈希校验保证(旧 pin 指向旧包仍可解析)
        let (enabled, authorized) = {
            let plugins = self.plugins.read();
            plugins
                .iter()
                .find(|p| p.full_id == plugin_full_id && !p.builtin)
                .map(|p| (p.enabled, p.authorized_at.is_some()))
                .ok_or_else(|| anyhow::anyhow!("插件 {plugin_full_id} 未安装"))?
        };
        anyhow::ensure!(enabled, "插件已禁用,不得解析目录提供器: {plugin_full_id}");
        anyhow::ensure!(
            authorized,
            "插件未授权,不得解析目录提供器: {plugin_full_id}"
        );
        let caps = &resolved.manifest.capabilities;
        anyhow::ensure!(
            caps.fs_read && caps.fs_write,
            "目录提供器能力不足:需要 fs_read+fs_write(声明 fs_read={} fs_write={})",
            caps.fs_read,
            caps.fs_write
        );
        if contribution.isolates {
            anyhow::ensure!(
                caps.vcs,
                "隔离类目录提供器(kind={})需要 vcs 能力",
                contribution.kind
            );
        }
        let worker = resolved.manifest.worker.clone().ok_or_else(|| {
            anyhow::anyhow!("第三方目录提供器必须声明 worker(插件 {plugin_full_id})")
        })?;
        Ok(DirectoryProviderResolution {
            full_contribution_id: full_contribution_id.to_string(),
            kind: contribution.kind.clone(),
            isolates: contribution.isolates,
            supports_parallel: contribution.supports_parallel,
            factory: DirectoryProviderFactory::Worker {
                command: worker.command,
                args: worker.args,
                plugin_root: resolved.root.clone(),
            },
            pin: mf_agent::workflow::PluginSourcePin {
                full_id: plugin_full_id.to_string(),
                version: version.to_string(),
                content_hash: content_hash.to_string(),
            },
        })
    }

    /// 校验插件包身份可解析(工作流派发前):内容哈希不一致/插件缺失时报错。
    pub fn resolve_source_pin(
        &self,
        full_id: &str,
        version: &str,
        content_hash: &str,
    ) -> Result<()> {
        if content_hash.is_empty() {
            return self.ensure_builtin_plugin(full_id, version);
        }
        self.resolve(full_id, version, content_hash)?;
        Ok(())
    }

    fn ensure_builtin_plugin(&self, full_id: &str, version: &str) -> Result<()> {
        let found = self.plugins.read().iter().any(|p| {
            p.builtin && p.full_id == full_id && p.manifest.manifest.version_str == version
        });
        anyhow::ensure!(found, "内置插件 {full_id} @ {version} 不存在或版本不符");
        Ok(())
    }

    /// 某哈希包的当前活动引用数(0 = 可清理)。
    pub fn active_pin_count(&self, hash: &str) -> usize {
        self.pin_counts.read().get(hash).copied().unwrap_or(0)
    }

    pub fn uninstall(&self, full_id: &str) -> Result<()> {
        if self
            .plugins
            .read()
            .iter()
            .any(|plugin| plugin.full_id == full_id && plugin.builtin)
        {
            bail!("内置合成插件不可卸载");
        }
        let hashes: Vec<String> = self
            .packages
            .read()
            .iter()
            .filter(|(_, package)| package.manifest.full_id() == full_id)
            .map(|(hash, _)| hash.clone())
            .collect();
        if hashes.is_empty() {
            bail!("插件不存在: {full_id}");
        }
        for hash in &hashes {
            if self.catalog.plugin_pin_count(hash)? > 0 {
                bail!("插件仍被活动运行固定,无法卸载: {full_id}");
            }
        }
        install::uninstall_at(&self.root, full_id)?;
        *self.packages.write() = Self::scan_packages(&self.root);
        self.reload_installed_locked();
        Ok(())
    }

    // ---------- 贡献查找 ----------

    /// 插件包根目录(内置合成插件为 None)。
    pub fn plugin_root_of(&self, full_id: &str) -> Option<PathBuf> {
        self.plugins
            .read()
            .iter()
            .find(|p| p.full_id == full_id)
            .and_then(|p| p.root.clone())
    }

    /// 当前启用插件的类型化贡献索引(按完整贡献 ID 查找)。
    pub fn contributions(&self) -> ContributionRegistry {
        ContributionRegistry::from_enabled(&self.plugins.read())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Capabilities, ManifestHeader, PluginManifest as M, WorkerSpec};
    use std::collections::HashMap as Map;

    fn make_host_with_plugin(
        m: M,
        root: Option<PathBuf>,
        enabled: bool,
        authorized: bool,
    ) -> PluginHost {
        // root 用一次性临时目录:enable/disable 会向 root 写锁文件,
        // 不得污染真实插件目录或测试 CWD(keep 后由测试进程生命周期托管)
        let tmp_root = tempfile::tempdir().unwrap().keep();
        PluginHost {
            root: tmp_root,
            catalog: CatalogStore::memory().expect("内存目录库初始化不应失败"),
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
                detected: Map::new(),
            }]),
            builtin_profiles: RwLock::new(Vec::new()),
            agent_overrides: RwLock::new(Map::new()),
            templates: RwLock::new(Vec::new()),
            packages: RwLock::new(Map::new()),
            pins: RwLock::new(Map::new()),
            pin_counts: RwLock::new(Map::new()),
        }
    }

    fn worker_manifest() -> M {
        M {
            manifest: ManifestHeader {
                version: crate::manifest::MANIFEST_VERSION,
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
            agent_types: vec![],
            node_types: vec![],
            execution_directory_providers: vec![],
            secret_stores: vec![],
            workflow_templates: vec![],
            skills: vec![],
            tools: vec![],
            ui_schemas: vec![],
        }
    }

    #[test]
    fn disabled_plugin_cannot_run_worker() {
        let host = make_host_with_plugin(worker_manifest(), None, false, true);
        assert!(
            host.worker_allowed("t.w").is_err(),
            "禁用插件不得运行 worker"
        );
        let host = make_host_with_plugin(worker_manifest(), None, true, false);
        assert!(
            host.worker_allowed("t.w").is_err(),
            "未授权插件不得运行 worker"
        );
        let host = make_host_with_plugin(worker_manifest(), None, true, true);
        assert!(host.worker_allowed("t.w").is_ok());
    }

    #[test]
    fn permission_change_requires_reauth() {
        let host = make_host_with_plugin(worker_manifest(), None, true, true);
        // 相同指纹:直接启用 OK
        assert!(host.enable("t.w", false).is_ok());
        // 修改插件(worker 命令变化)→ 指纹变化
        {
            let mut plugins = host.plugins.write();
            let p = plugins.first_mut().unwrap();
            p.manifest.worker = Some(WorkerSpec {
                command: "evil.exe".into(),
                args: vec![],
            });
        }
        assert!(
            host.enable("t.w", false).is_err(),
            "权限/内容变化必须要求重新授权"
        );
        assert!(host.enable("t.w", true).is_ok(), "显式重新授权后可启用");
    }

    #[test]
    fn builtin_agents_registered_and_detected() {
        let config = mf_agent::Config::default();
        let host = PluginHost::load(&config, &[]);
        let profiles = host.agent_profiles();
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
        let host = PluginHost::load(&config, &[]);
        let mut spec = host
            .agent_profiles()
            .into_iter()
            .find(|p| p.id == "codex")
            .unwrap();
        spec.args = vec!["--custom".into()];
        host.set_agent_override(spec);
        let over = host
            .agent_profiles()
            .into_iter()
            .find(|p| p.id == "codex")
            .unwrap();
        assert!(over.args.contains(&"--custom".to_string()));
        host.clear_agent_override("codex");
        let restored = host
            .agent_profiles()
            .into_iter()
            .find(|p| p.id == "codex")
            .unwrap();
        assert!(!restored.args.contains(&"--custom".to_string()));
    }
}
