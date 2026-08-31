//! 插件根清单 `monkeyfence-plugin.toml` 的类型与校验(v2 贡献词汇表)。
//!
//! v2 移除 v1 的 `agents` 字段(不做兼容别名),改用 Agent Type /
//! Node Type / Execution Directory / VCS Provider / Secret Store /
//! Workflow Template / UI Schema 等统一贡献词汇(ADR 0002 / 0003)。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const MANIFEST_FILE: &str = "monkeyfence-plugin.toml";
pub const MANIFEST_VERSION: i64 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub manifest: ManifestHeader,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub worker: Option<WorkerSpec>,
    #[serde(default)]
    pub agent_types: Vec<AgentTypeContribution>,
    #[serde(default)]
    pub node_types: Vec<NodeTypeContribution>,
    #[serde(default)]
    pub execution_directory_providers: Vec<ExecutionDirectoryContribution>,
    #[serde(default)]
    pub vcs_providers: Vec<VcsProviderContribution>,
    #[serde(default)]
    pub secret_stores: Vec<SecretStoreContribution>,
    #[serde(default)]
    pub workflow_templates: Vec<WorkflowTemplateContribution>,
    #[serde(default)]
    pub skills: Vec<SkillContribution>,
    #[serde(default)]
    pub tools: Vec<ToolContribution>,
    #[serde(default)]
    pub ui_schemas: Vec<UiSchemaContribution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestHeader {
    pub version: i64,
    pub publisher: String,
    pub id: String,
    pub name: String,
    pub version_str: String,
    #[serde(default)]
    pub min_app_version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub icon: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub fs_read: bool,
    #[serde(default)]
    pub fs_write: bool,
    #[serde(default)]
    pub net: bool,
    #[serde(default)]
    pub spawn: bool,
    #[serde(default)]
    pub shell: bool,
    #[serde(default)]
    pub secrets: bool,
    #[serde(default)]
    pub vcs: bool,
    #[serde(default)]
    pub background_worker: bool,
    #[serde(default)]
    pub hooks: bool,
}

/// 能力声明 → 稳定字符串(用于授权指纹)。
impl Capabilities {
    pub fn fingerprint_part(&self) -> String {
        format!(
            "fs_read={} fs_write={} net={} spawn={} shell={} secrets={} vcs={} \
             background_worker={} hooks={}",
            self.fs_read,
            self.fs_write,
            self.net,
            self.spawn,
            self.shell,
            self.secrets,
            self.vcs,
            self.background_worker,
            self.hooks
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Agent Type:插件贡献的 CLI 执行类型(配置 Schema、检测方式、运行模式与适配器契约)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTypeContribution {
    pub id: String,
    pub name: String,
    /// 适配器契约标识(claude-code / codex / generic-command / http / plugin-worker ...)。
    pub adapter: String,
    /// 相对插件根目录的配置 Schema 文件路径;空表示无自定义配置。
    #[serde(default)]
    pub config_schema: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub detect_commands: Vec<String>,
    /// 支持的运行模式(oneshot / interactive ...)。
    #[serde(default)]
    pub modes: Vec<String>,
    #[serde(default)]
    pub supports_isolated_config: bool,
}

/// Node Type:工作流节点类型(第一版仅 agent / join,由内核识别;其余由插件扩展)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTypeContribution {
    pub id: String,
    pub name: String,
    /// 节点运行语义(agent | join ...)。
    pub kind: String,
    /// 相对插件根目录的节点属性 Schema;空表示无。
    #[serde(default)]
    pub config_schema: String,
    #[serde(default)]
    pub description: String,
}

/// Execution Directory Provider:为 Agent Run 提供路径租约的策略。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionDirectoryContribution {
    pub id: String,
    pub name: String,
    /// 策略实现标识(project-dir | worktree ...)。
    pub kind: String,
    #[serde(default)]
    pub supports_parallel: bool,
    /// 提供器是否提供独占隔离租约(worktree 类 = true;共享项目目录 = false)。
    /// Workflow Compiler 据此判定并行安全(unsafe-parallel 默认拒绝)。
    #[serde(default)]
    pub isolates: bool,
    #[serde(default)]
    pub description: String,
}

/// VCS Provider:版本控制环境与设置表单均由插件贡献。宿主只识别稳定的
/// adapter 契约，并按 `settings` 声明渲染，不写死 Git/P4 字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcsProviderContribution {
    pub id: String,
    pub name: String,
    /// 运行时适配器契约(git-cli | perforce-cli | plugin-worker ...)。
    pub adapter: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub settings: Vec<SettingFieldContribution>,
}

/// 声明式设置字段。`kind` 首版支持 text/path/boolean/select。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingFieldContribution {
    pub id: String,
    pub label: String,
    #[serde(default = "default_setting_kind")]
    pub kind: String,
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub options: Vec<SettingOptionContribution>,
}

fn default_setting_kind() -> String {
    "text".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingOptionContribution {
    pub value: String,
    pub label: String,
}

/// Secret Store:加密 Secret 的存储实现。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretStoreContribution {
    pub id: String,
    pub name: String,
    /// 后端实现标识(os-credential ...)。
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub description: String,
}

/// Workflow Template:可复用的 DAG 模板文件贡献。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTemplateContribution {
    pub id: String,
    pub name: String,
    /// 相对插件根目录的 JSON 文件路径(PipelineDraft)。
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillContribution {
    /// 相对插件根目录的技能目录。
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolContribution {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// 声明式 UI 贡献:插件只提供 Schema 文件,由宿主统一渲染(不得注入 GPUI)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiSchemaContribution {
    pub id: String,
    /// UI 目标区域(settings-form | node-properties | badge | action ...)。
    pub surface: String,
    /// 相对插件根目录的 Schema 文件路径(JSON)。
    pub file: String,
}

impl PluginManifest {
    pub fn parse(text: &str) -> Result<PluginManifest> {
        let m: PluginManifest = toml::from_str(text).context("monkeyfence-plugin.toml 解析失败")?;
        m.validate()?;
        Ok(m)
    }

    pub fn load(dir: &Path) -> Result<PluginManifest> {
        let path = dir.join(MANIFEST_FILE);
        if !path.is_file() {
            bail!("缺少插件清单 {}", MANIFEST_FILE);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("读取清单失败: {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn full_id(&self) -> String {
        format!("{}.{}", self.manifest.publisher, self.manifest.id)
    }

    /// 结构校验:清单版本、命名、各类贡献 id 在类别内唯一、适配器非空。
    pub fn validate(&self) -> Result<()> {
        if self.manifest.version != MANIFEST_VERSION {
            bail!(
                "manifest version {} 不受支持(当前支持 {MANIFEST_VERSION})",
                self.manifest.version
            );
        }
        for field in [&self.manifest.publisher, &self.manifest.id] {
            if field.is_empty()
                || !field
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            {
                bail!("publisher/id 非法: `{field}`(仅允许字母数字与 - _ .)");
            }
        }
        if self.manifest.name.trim().is_empty() {
            bail!("插件 name 不能为空");
        }
        fn ensure_unique(name: &str, ids: Vec<&str>) -> Result<()> {
            let total = ids.len();
            let mut sorted = ids;
            sorted.sort();
            sorted.dedup();
            if sorted.len() != total {
                bail!("{name} 贡献 id 重复");
            }
            Ok(())
        }
        ensure_unique(
            "agent_types",
            self.agent_types.iter().map(|a| a.id.as_str()).collect(),
        )?;
        ensure_unique(
            "node_types",
            self.node_types.iter().map(|a| a.id.as_str()).collect(),
        )?;
        ensure_unique(
            "execution_directory_providers",
            self.execution_directory_providers
                .iter()
                .map(|a| a.id.as_str())
                .collect(),
        )?;
        ensure_unique(
            "vcs_providers",
            self.vcs_providers.iter().map(|a| a.id.as_str()).collect(),
        )?;
        ensure_unique(
            "secret_stores",
            self.secret_stores.iter().map(|a| a.id.as_str()).collect(),
        )?;
        ensure_unique(
            "workflow_templates",
            self.workflow_templates
                .iter()
                .map(|a| a.id.as_str())
                .collect(),
        )?;
        ensure_unique("tools", self.tools.iter().map(|a| a.id.as_str()).collect())?;
        ensure_unique(
            "ui_schemas",
            self.ui_schemas.iter().map(|a| a.id.as_str()).collect(),
        )?;
        for a in &self.agent_types {
            if a.adapter.trim().is_empty() {
                bail!("agent_type `{}` 缺少 adapter 契约标识", a.id);
            }
        }
        for provider in &self.vcs_providers {
            if provider.adapter.trim().is_empty() {
                bail!("vcs_provider `{}` 缺少 adapter 契约标识", provider.id);
            }
            ensure_unique(
                &format!("vcs_provider `{}` settings", provider.id),
                provider
                    .settings
                    .iter()
                    .map(|field| field.id.as_str())
                    .collect(),
            )?;
            for field in &provider.settings {
                if !matches!(field.kind.as_str(), "text" | "path" | "boolean" | "select") {
                    bail!(
                        "vcs_provider `{}` 字段 `{}` kind 不支持: {}",
                        provider.id,
                        field.id,
                        field.kind
                    );
                }
                if field.kind == "select" && field.options.is_empty() {
                    bail!(
                        "vcs_provider `{}` select 字段 `{}` 缺少 options",
                        provider.id,
                        field.id
                    );
                }
            }
        }
        for t in &self.ui_schemas {
            if t.surface.trim().is_empty() {
                bail!("ui_schema `{}` 缺少 surface", t.id);
            }
        }
        if !self.manifest.min_app_version.is_empty() {
            // 简单 semver 比较(仅数字段)
            let ok = version_gte(&env_version(), &self.manifest.min_app_version);
            if !ok {
                bail!(
                    "需要 MonkeyFence >= {},当前 {}",
                    self.manifest.min_app_version,
                    env_version()
                );
            }
        }
        Ok(())
    }

    /// 权限相关指纹:能力 + worker + 描述变化都要求重新授权。
    pub fn permission_fingerprint(&self, content_hash: &str) -> String {
        let mut parts: Vec<String> = vec![
            self.capabilities.fingerprint_part(),
            format!("desc={}", self.manifest.description),
        ];
        if let Some(w) = &self.worker {
            parts.push(format!("worker={} {:?}", w.command, w.args));
        }
        parts.push(format!("content={content_hash}"));
        let mut hasher = Sha256::new();
        hasher.update(parts.join("\n").as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

/// min_app_version 兼容性(空要求 = 兼容;供摘要/插件页使用)。
pub fn compatible_with_app(min_app_version: &str) -> bool {
    min_app_version.is_empty() || version_gte(&env_version(), min_app_version)
}

fn env_version() -> String {
    option_env!("CARGO_PKG_VERSION")
        .unwrap_or("0.1.0")
        .to_string()
}

fn version_gte(current: &str, required: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(|c| c == '.' || c == '-')
            .filter_map(|s| s.parse::<u64>().ok())
            .collect()
    };
    let c = parse(current);
    let r = parse(required);
    for i in 0..r.len().max(c.len()) {
        let a = c.get(i).copied().unwrap_or(0);
        let b = r.get(i).copied().unwrap_or(0);
        if a > b {
            return true;
        }
        if a < b {
            return false;
        }
    }
    true
}

use sha2::{Digest, Sha256};

/// 相对路径校验:拒绝绝对路径、`..` 逃逸与符号链接逃逸。
pub fn safe_relative_path(root: &Path, rel: &str) -> Result<std::path::PathBuf> {
    let rel_path = Path::new(rel);
    if rel.is_empty() {
        bail!("相对路径为空");
    }
    if rel_path.is_absolute() {
        bail!("贡献路径必须是相对路径: {rel}");
    }
    for comp in rel_path.components() {
        match comp {
            std::path::Component::ParentDir => bail!("贡献路径不允许 `..`: {rel}"),
            std::path::Component::Normal(_) => {}
            _ => bail!("贡献路径非法: {rel}"),
        }
    }
    let full = root.join(rel_path);
    // 符号链接逃逸:逐级检查
    let mut cur = root.to_path_buf();
    for comp in rel_path.components() {
        cur.push(comp);
        let meta = std::fs::symlink_metadata(&cur)
            .with_context(|| format!("路径不存在: {}", cur.display()))?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&cur)?;
            if target.is_absolute() {
                bail!(
                    "符号链接指向绝对路径(逃逸): {} -> {}",
                    cur.display(),
                    target.display()
                );
            }
        }
    }
    let canonical_root = std::fs::canonicalize(root)?;
    let canonical = std::fs::canonicalize(&full)?;
    if !canonical.starts_with(&canonical_root) {
        bail!("路径逃逸: {rel}");
    }
    Ok(full)
}

/// 校验清单引用的所有相对路径存在且不逃逸(空字符串跳过)。
pub fn validate_manifest_paths(root: &Path, m: &PluginManifest) -> Result<()> {
    for a in &m.agent_types {
        if !a.config_schema.is_empty() {
            safe_relative_path(root, &a.config_schema)?;
        }
    }
    for n in &m.node_types {
        if !n.config_schema.is_empty() {
            safe_relative_path(root, &n.config_schema)?;
        }
    }
    for t in &m.workflow_templates {
        safe_relative_path(root, &t.file)?;
    }
    for s in &m.skills {
        safe_relative_path(root, &s.path)?;
    }
    for u in &m.ui_schemas {
        safe_relative_path(root, &u.file)?;
    }
    if let Some(w) = &m.worker {
        safe_relative_path(root, &w.command).or_else(|_| {
            // worker 命令允许是相对可执行名(在插件根内)或绝对路径
            if Path::new(&w.command).is_absolute() {
                Ok(PathBuf::from(&w.command))
            } else {
                bail!("worker command 非法: {}", w.command)
            }
        })?;
    }
    if !m.manifest.icon.is_empty() {
        safe_relative_path(root, &m.manifest.icon)?;
    }
    Ok(())
}

use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[manifest]
version = 2
publisher = "zhipu"
id = "demo"
name = "Demo Plugin"
version_str = "0.1.0"
min_app_version = "0.1.0"
description = "演示"

[capabilities]
net = true

[[agent_types]]
id = "demo"
name = "Demo"
adapter = "generic-command"
command = "demo"
detect_commands = ["demo"]
modes = ["interactive"]

[[workflow_templates]]
id = "p1"
name = "管道"
file = "pipelines/p1.json"

[[skills]]
path = "skills/demo"
"#;

    #[test]
    fn parse_and_validate_ok() {
        let m = PluginManifest::parse(VALID).unwrap();
        assert_eq!(m.full_id(), "zhipu.demo");
        assert_eq!(m.agent_types.len(), 1);
        assert!(m.capabilities.net);
        assert!(!m.capabilities.fs_write);
        assert!(!m.capabilities.shell);
    }

    #[test]
    fn rejects_bad_manifest() {
        assert!(PluginManifest::parse(
            "[manifest]\nversion = 99\npublisher=\"a\"\nid=\"b\"\nname=\"c\"\nversion_str=\"1\""
        )
        .is_err());
        assert!(PluginManifest::parse(
            VALID
                .replace("publisher = \"zhipu\"", "publisher = \"../evil\"")
                .as_str()
        )
        .is_err());
        // v1 清单(agents 字段)不再受支持
        assert!(PluginManifest::parse(
            VALID
                .replace("version = 2", "version = 1")
                .replace("adapter = \"generic-command\"", "runtime = \"pty\"")
                .as_str()
        )
        .is_err());
        // adapter 缺失
        assert!(
            PluginManifest::parse(VALID.replace("adapter = \"generic-command\"", "").as_str())
                .is_err()
        );
    }

    #[test]
    fn rejects_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let m = PluginManifest::parse(VALID).unwrap();
        let bad = PluginManifest {
            workflow_templates: vec![WorkflowTemplateContribution {
                id: "p".into(),
                name: "p".into(),
                file: "../outside.json".into(),
            }],
            ..m.clone()
        };
        assert!(validate_manifest_paths(tmp.path(), &bad).is_err());
        assert!(safe_relative_path(tmp.path(), "C:/Windows/system32").is_err());
        std::fs::create_dir_all(tmp.path().join("ok")).unwrap();
        std::fs::write(tmp.path().join("ok/pipeline.json"), "{}").unwrap();
        assert!(safe_relative_path(tmp.path(), "ok/pipeline.json").is_ok());
    }

    #[test]
    fn fingerprint_changes_with_capabilities() {
        let m1 = PluginManifest::parse(VALID).unwrap();
        let m2 = PluginManifest {
            capabilities: Capabilities {
                fs_write: true,
                ..m1.capabilities.clone()
            },
            ..m1.clone()
        };
        assert_ne!(
            m1.permission_fingerprint("h"),
            m2.permission_fingerprint("h")
        );
        let m3 = PluginManifest {
            capabilities: Capabilities {
                vcs: true,
                background_worker: true,
                ..m1.capabilities.clone()
            },
            ..m1.clone()
        };
        assert_ne!(
            m1.permission_fingerprint("h"),
            m3.permission_fingerprint("h")
        );
        // 内容哈希变化也改变指纹
        assert_ne!(
            m1.permission_fingerprint("h1"),
            m1.permission_fingerprint("h2")
        );
    }
}
