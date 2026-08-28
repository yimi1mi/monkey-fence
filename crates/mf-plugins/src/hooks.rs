//! 状态钩子写入器:把 MonkeyFence 命名空间条目写入本地 Agent 配置。
//!
//! 纪律:必须经过插件权限审查;只修改 MonkeyFence 命名空间内的条目;
//! 保留用户现有配置;写入前生成可恢复备份;支持可逆移除。

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 展开 ~/ 前缀。
pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

fn backup_path(config: &Path) -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let name = format!(
        "{}.monkeyfence-backup-{ts}",
        config
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    );
    config.with_file_name(name)
}

fn parse_json_or_toml(text: &str) -> Result<Value> {
    if let Ok(v) = serde_json::from_str(text) {
        return Ok(v);
    }
    let t: Value = toml::from_str(text).context("配置既不是 JSON 也不是 TOML")?;
    Ok(t)
}

/// 按扩展名选择写回格式:TOML 配置以 TOML 写回(codex 等 CLI 只认 TOML,
/// JSON 化会毁掉用户配置);未知扩展拒绝写入。
fn serialize_for(path: &Path, v: &Value) -> Result<String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "json" => Ok(serde_json::to_string_pretty(v)?),
        "toml" => toml::to_string(v).context("序列化 TOML 失败(值类型可能不兼容)"),
        other => bail!("不支持的配置扩展名 .{other}(仅支持 .json/.toml)"),
    }
}

/// 安装钩子:在 JSON/TOML 配置的 `namespace` 键下写入 hooks 条目。
/// - 用户已有键一律不动(除 namespace 键下的 monkeyfence 子树)
/// - 写入前备份整个文件
pub fn install_hook(config_path: &str, namespace: &str, command_template: &str) -> Result<PathBuf> {
    let path = expand_home(config_path);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let (mut root, existed) = match std::fs::read_to_string(&path) {
        Ok(text) => (
            parse_json_or_toml(&text).context("解析现有配置失败,拒绝写入")?,
            true,
        ),
        Err(_) => (json!({}), false),
    };
    let backup = if existed {
        let b = backup_path(&path);
        std::fs::copy(&path, &b).with_context(|| format!("生成备份失败: {}", b.display()))?;
        Some(b)
    } else {
        None
    };

    let entry = json!({
        "command_template": command_template,
        "installed_at": chrono::Utc::now().to_rfc3339(),
    });
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("配置根不是对象"))?;
    obj.entry(namespace.to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            anyhow::anyhow!("namespace `{namespace}` 已存在但不是对象,拒绝覆盖用户配置")
        })?
        .insert("agent_state_hook".into(), entry);

    let out = serialize_for(&path, &root)?;
    std::fs::write(&path, out).with_context(|| format!("写入配置失败: {}", path.display()))?;
    Ok(backup.unwrap_or_else(|| path.clone()))
}

/// 移除钩子(可逆):只删除 namespace 下的 monkeyfence 条目;用户配置保持。
pub fn remove_hook(config_path: &str, namespace: &str) -> Result<()> {
    let path = expand_home(config_path);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(()); // 文件不存在 = 已移除
    };
    let mut root = parse_json_or_toml(&text)?;
    let Some(obj) = root.as_object_mut() else {
        return Ok(());
    };
    if let Some(ns) = obj.get_mut(namespace) {
        if let Some(ns_obj) = ns.as_object_mut() {
            ns_obj.remove("agent_state_hook");
            if ns_obj.is_empty() {
                obj.remove(namespace); // 空命名空间一并清理
            }
        }
    }
    // 移除前同样备份(可逆性)
    let backup = backup_path(&path);
    let _ = std::fs::copy(&path, &backup);
    let out = serialize_for(&path, &root)?;
    std::fs::write(&path, out).with_context(|| format!("写回配置失败: {}", path.display()))?;
    Ok(())
}

/// 钩子目标约束:配置必须在用户主目录下,防止第三方插件把钩子写进任意路径。
pub fn validate_hook_target(config_path: &str, namespace: &str) -> Result<PathBuf> {
    let path = expand_home(config_path);
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("无法确定用户主目录"))?;
    let canonical_parent = path
        .parent()
        .and_then(|p| p.exists().then(|| std::fs::canonicalize(p).ok()))
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("钩子配置目录不存在: {}", path.display()))?;
    // 主目录同样 canonicalize:两侧 verbatim 前缀一致,直接可比(大小写不敏感)
    let in_home = {
        let a = canonical_parent.to_string_lossy().to_lowercase();
        let b = std::fs::canonicalize(&home)
            .map(|h| h.to_string_lossy().to_lowercase())
            .unwrap_or_else(|_| home.to_string_lossy().to_lowercase());
        a.starts_with(&b)
    };
    if !in_home {
        bail!("钩子配置必须位于用户主目录内: {}", path.display());
    }
    if namespace != "monkeyfence" {
        bail!("第三方插件钩子命名空间必须是 `monkeyfence`(拒绝: {namespace})");
    }
    Ok(path)
}

/// 权限审查门:未授权能力不得写钩子。
pub fn ensure_hook_allowed(
    capabilities: &crate::manifest::Capabilities,
    authorized: bool,
) -> Result<()> {
    if !capabilities.hooks {
        bail!("插件未声明 hooks 能力,拒绝写入状态钩子");
    }
    if !authorized {
        bail!("插件未授权,拒绝写入状态钩子");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_install_and_remove_preserves_user_config() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("settings.json");
        std::fs::write(&cfg, r#"{"theme": "dark", "custom": {"a": 1}}"#).unwrap();

        let backup = install_hook(
            cfg.to_str().unwrap(),
            "monkeyfence",
            "mfctl agent-state {state}",
        )
        .unwrap();
        assert!(backup.exists(), "写入前生成备份");

        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(after["theme"], "dark", "用户配置保留");
        assert_eq!(after["custom"]["a"], 1, "用户子树保留");
        assert_eq!(
            after["monkeyfence"]["agent_state_hook"]["command_template"],
            "mfctl agent-state {state}"
        );

        // 幂等重装:用户配置仍然保留
        install_hook(
            cfg.to_str().unwrap(),
            "monkeyfence",
            "mfctl agent-state {state}",
        )
        .unwrap();
        let after2: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(after2["theme"], "dark");

        remove_hook(cfg.to_str().unwrap(), "monkeyfence").unwrap();
        let final_: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(final_["theme"], "dark", "移除后用户配置保留");
        assert!(
            final_.get("monkeyfence").is_none(),
            "monkeyfence 命名空间被清理"
        );
    }

    #[test]
    fn refuses_when_namespace_is_user_object() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("settings.json");
        std::fs::write(&cfg, r#"{"monkeyfence": "user-value"}"#).unwrap();
        assert!(install_hook(cfg.to_str().unwrap(), "monkeyfence", "x").is_err());
    }

    #[test]
    fn capability_gate() {
        let caps = crate::manifest::Capabilities {
            hooks: false,
            ..Default::default()
        };
        assert!(ensure_hook_allowed(&caps, true).is_err());
        let caps = crate::manifest::Capabilities {
            hooks: true,
            ..Default::default()
        };
        assert!(ensure_hook_allowed(&caps, true).is_ok());
        assert!(ensure_hook_allowed(&caps, false).is_err());
    }

    #[test]
    fn hook_target_must_be_in_home_and_monkeyfence_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let inside = tmp.path().join("cfg.json");
        std::fs::write(&inside, "{}").unwrap();
        let abs = inside.to_string_lossy().to_string();
        assert!(validate_hook_target(&abs, "monkeyfence").is_ok());
        assert!(
            validate_hook_target(&abs, "evil").is_err(),
            "命名空间必须是 monkeyfence"
        );
        assert!(
            validate_hook_target("C:/Windows/System32/config.json", "monkeyfence").is_err(),
            "主目录之外拒绝"
        );
    }

    #[test]
    fn toml_config_supported() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(&cfg, "model = \"gpt\"\n").unwrap();
        install_hook(
            cfg.to_str().unwrap(),
            "monkeyfence",
            "mfctl agent-state {state}",
        )
        .unwrap();
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("mfctl agent-state"));
        // TOML 必须以 TOML 写回(codex 等只认 TOML;JSON 化会毁掉用户配置)
        assert!(text.contains("model = \"gpt\""), "TOML 格式保留: {text}");
        remove_hook(cfg.to_str().unwrap(), "monkeyfence").unwrap();
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(after.contains("model = \"gpt\""));
        assert!(!after.contains("monkeyfence"), "命名空间应被清理: {after}");
    }
}
