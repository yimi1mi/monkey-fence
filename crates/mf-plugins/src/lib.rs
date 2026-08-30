//! mf-plugins:MonkeyFence 统一插件系统(ADR 0002 / 0003)。
//!
//! - 清单与校验:`manifest`(v2 贡献词汇表)
//! - 安装/锁文件/内容寻址包:`install`
//! - 内置合成插件(CLI Agent / API Provider / 技能):`builtin`
//! - 状态钩子写入:`hooks`
//! - 后台 worker NDJSON 协议:`worker`
//! - 运行时宿主(发现/授权/解析/运行期 pin):`host::PluginHost`
//! - 类型化贡献查找:`contribution_registry`

pub mod builtin;
pub mod builtin_secret_store;
pub mod claude_adapter;
pub mod codex_adapter;
pub mod contribution_registry;
pub mod fs_atomic;
pub mod generic_command_adapter;
pub mod git_worktree_provider;
pub mod hooks;
pub mod host;
pub mod install;
pub mod manifest;
pub mod project_directory_provider;
pub mod worker;
pub mod worker_directory_provider;
pub mod worker_protocol;

pub use host::{PluginHost, PluginPin, ResolvedPlugin};

use install::{InstallSource, LockEntry};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 兼容别名:旧调用方以 `PluginRegistry` 使用宿主。
/// 计划二结束前移除,新代码请直接使用 `PluginHost`。
pub type PluginRegistry = PluginHost;

/// 注册表中的一个插件(内置合成或已安装)。
#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub full_id: String,
    pub manifest: manifest::PluginManifest,
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
    /// 包内容哈希(内置合成插件为空串)。
    pub content_hash: String,
    /// min_app_version 兼容性(计算值,不是常量)。
    pub compatible: bool,
    /// 当前活动 pin 数(任务冻结引用;0 = 可清理)。
    pub active_pins: usize,
    /// 全部贡献计数(按类型)。
    pub agent_types_count: usize,
    pub node_types_count: usize,
    pub ui_schemas_count: usize,
    pub execution_directories_count: usize,
    pub secret_stores_count: usize,
    pub workflow_templates_count: usize,
    pub skills_count: usize,
    pub tools_count: usize,
}

/// 把插件条目的选择状态(启用/授权/指纹)持久化到指定根的锁文件。
pub(crate) fn persist_lock_entry(p: &PluginEntry, root: &Path) {
    if p.builtin {
        return;
    }
    let mut lock = install::load_lock_at(root);
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
    let _ = install::save_lock_at(root, &lock);
}
