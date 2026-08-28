//! 插件根清单 `monkeyfence-plugin.toml` 的类型与校验。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const MANIFEST_FILE: &str = "monkeyfence-plugin.toml";
pub const MANIFEST_VERSION: i64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub manifest: ManifestHeader,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub worker: Option<WorkerSpec>,
    #[serde(default)]
    pub agents: Vec<AgentContribution>,
    #[serde(default)]
    pub pipelines: Vec<PipelineContribution>,
    #[serde(default)]
    pub skills: Vec<SkillContribution>,
    #[serde(default)]
    pub tools: Vec<ToolContribution>,
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
    pub hooks: bool,
}

/// 能力声明 → 稳定字符串(用于授权指纹)。
impl Capabilities {
    pub fn fingerprint_part(&self) -> String {
        format!(
            "fs_read={} fs_write={} net={} spawn={} hooks={}",
            self.fs_read, self.fs_write, self.net, self.spawn, self.hooks
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContribution {
    pub id: String,
    pub name: String,
    /// pty | http | plugin-worker
    pub runtime: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub permission_args: Vec<String>,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub hook: Option<AgentHookSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHookSpec {
    /// 相对用户主目录(~/...)或绝对路径。
    pub config_path: String,
    /// MonkeyFence 命名空间键名。
    pub namespace: String,
    /// 状态回调命令模板,{state} 会被替换为 working/waiting/blocked/done。
    pub command_template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineContribution {
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

    /// 结构校验:清单版本、命名、贡献 id 唯一。
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
        let mut agent_ids: Vec<&str> = self.agents.iter().map(|a| a.id.as_str()).collect();
        agent_ids.sort();
        agent_ids.dedup();
        if agent_ids.len() != self.agents.len() {
            bail!("agents 贡献 id 重复");
        }
        for a in &self.agents {
            match a.runtime.as_str() {
                "pty" | "http" | "plugin-worker" => {}
                other => bail!("agent `{}` runtime 非法: {other}", a.id),
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

    /// 权限相关指纹:能力 + worker + 钩子 + 描述变化都要求重新授权。
    pub fn permission_fingerprint(&self, content_hash: &str) -> String {
        let mut parts: Vec<String> = vec![
            self.capabilities.fingerprint_part(),
            format!("desc={}", self.manifest.description),
        ];
        if let Some(w) = &self.worker {
            parts.push(format!("worker={} {:?}", w.command, w.args));
        }
        for a in &self.agents {
            if let Some(h) = &a.hook {
                parts.push(format!(
                    "hook={} {} {}",
                    a.id, h.config_path, h.command_template
                ));
            }
        }
        parts.push(format!("content={content_hash}"));
        let mut hasher = Sha256::new();
        hasher.update(parts.join("\n").as_bytes());
        format!("{:x}", hasher.finalize())
    }
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

/// 校验清单引用的所有相对路径存在且不逃逸。
pub fn validate_manifest_paths(root: &Path, m: &PluginManifest) -> Result<()> {
    for p in &m.pipelines {
        safe_relative_path(root, &p.file)?;
    }
    for s in &m.skills {
        safe_relative_path(root, &s.path)?;
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
version = 1
publisher = "zhipu"
id = "demo"
name = "Demo Plugin"
version_str = "0.1.0"
min_app_version = "0.1.0"
description = "演示"

[capabilities]
net = true

[[agents]]
id = "demo"
name = "Demo"
runtime = "pty"
command = "demo"

[[pipelines]]
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
        assert_eq!(m.agents.len(), 1);
        assert!(m.capabilities.net);
        assert!(!m.capabilities.fs_write);
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
        // runtime 非法
        assert!(PluginManifest::parse(
            VALID
                .replace("runtime = \"pty\"", "runtime = \"telepathy\"")
                .as_str()
        )
        .is_err());
    }

    #[test]
    fn rejects_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let m = PluginManifest::parse(VALID).unwrap();
        let bad = PluginManifest {
            pipelines: vec![PipelineContribution {
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
        // 内容哈希变化也改变指纹
        assert_ne!(
            m1.permission_fingerprint("h1"),
            m1.permission_fingerprint("h2")
        );
    }
}
