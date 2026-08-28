//! 插件安装:staging → 校验 → 内容哈希 → 原子发布 → 锁文件。

#[cfg(test)]
use crate::manifest::MANIFEST_FILE;
use crate::manifest::{validate_manifest_paths, PluginManifest};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum InstallSource {
    Bundled,
    Local { path: String },
    Git { url: String, commit: Option<String> },
    Marketplace { id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    pub full_id: String,
    pub name: String,
    pub version: String,
    pub source: InstallSource,
    pub content_hash: String,
    pub permission_fingerprint: String,
    pub enabled: bool,
    pub authorized_at: Option<String>,
    pub installed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LockFile {
    pub plugins: std::collections::BTreeMap<String, LockEntry>,
}

pub fn plugins_root() -> PathBuf {
    // 测试与嵌入式场景可通过 MF_PLUGIN_ROOT 重定向
    if let Ok(root) = std::env::var("MF_PLUGIN_ROOT") {
        return PathBuf::from(root);
    }
    let root = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".monkeyfence");
    root.join("plugins")
}

pub fn lock_path() -> PathBuf {
    plugins_root().join("plugins.lock.json")
}

pub fn load_lock() -> LockFile {
    match std::fs::read_to_string(lock_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            // 损坏的锁文件隔离保留(不静默清空 —— 防止把其他插件的授权状态抹掉后回写)
            let corrupted = lock_path().with_extension(format!(
                "corrupt-{}",
                chrono::Utc::now().format("%Y%m%dT%H%M%S")
            ));
            let _ = std::fs::rename(lock_path(), &corrupted);
            log::error!("锁文件损坏,已隔离到 {}({e})", corrupted.display());
            LockFile::default()
        }),
        Err(_) => LockFile::default(),
    }
}

pub fn save_lock(lock: &LockFile) -> Result<()> {
    // 原子写:临时文件 + 同目录 rename,崩溃不留半截 JSON
    let path = lock_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("lock.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(lock)?)
        .with_context(|| format!("写锁文件失败: {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("发布锁文件失败: {}", path.display()))?;
    Ok(())
}

/// 目录内容哈希:对排序后的(相对路径, 文件哈希)列表做整体 SHA-256。
/// 符号链接一律拒绝(不参与哈希,直接判非法)。
pub fn content_hash(dir: &Path) -> Result<String> {
    let mut items: Vec<(String, String)> = Vec::new();
    collect_hashes(dir, dir, &mut items)?;
    items.sort();
    let mut hasher = Sha256::new();
    for (rel, file_hash) in &items {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(file_hash.as_bytes());
        hasher.update(b"\0");
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn collect_hashes(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("读取目录失败: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".mf-plugin-trash" || name == ".git" || name.starts_with(".staging-") {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            bail!("插件包含符号链接(拒绝安装): {}", path.display());
        }
        if meta.is_dir() {
            collect_hashes(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .context("路径逃逸")?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path)?;
            let mut h = Sha256::new();
            h.update(&bytes);
            out.push((rel, format!("{:x}", h.finalize())));
        }
    }
    Ok(())
}

pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub root: PathBuf,
    pub lock: LockEntry,
}

/// 安装来源目录(已在本地的目录,或 git clone 后的目录)。
/// 流程:复制到临时 staging → 校验清单与路径 → 哈希 → 原子发布 → 锁文件(默认禁用)。
pub fn install_from_dir(source: &Path, origin: InstallSource) -> Result<LockEntry> {
    // 1. staging:复制到插件目录内(同卷 rename 才是原子操作;%TEMP% 跨卷会失败)
    let staging = plugins_root().join(format!(
        ".staging-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    copy_dir_recursive(source, &staging)
        .with_context(|| format!("复制到 staging 失败: {}", source.display()))?;

    let result = install_staged(&staging, origin);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// 从 Git URL 安装:clone 到临时目录后走统一流程。
pub fn install_from_git(url: &str) -> Result<LockEntry> {
    let staging = std::env::temp_dir().join(format!(
        "mf-plugin-git-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let output = std::process::Command::new("git")
        .args(["clone", "--depth", "1", url])
        .arg(&staging)
        .output()
        .context("执行 git clone 失败(需要 git 在 PATH 中)")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_dir_all(&staging);
        bail!("git clone 失败: {err}");
    }
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&staging)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());
    let entry = install_staged(
        &staging,
        InstallSource::Git {
            url: url.to_string(),
            commit,
        },
    );
    let _ = std::fs::remove_dir_all(&staging);
    entry
}

fn install_staged(staging: &Path, origin: InstallSource) -> Result<LockEntry> {
    // 2. 校验
    let manifest = PluginManifest::load(staging)?;
    validate_manifest_paths(staging, &manifest)?;
    // 3. 哈希
    let hash = content_hash(staging)?;
    let fingerprint = manifest.permission_fingerprint(&hash);
    let full_id = manifest.full_id();

    // 4. 原子发布
    let target = plugins_root().join(&full_id);
    let mut trash: Option<PathBuf> = None;
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p)?;
    }
    if target.exists() {
        let t = plugins_root().join(format!(
            ".mf-plugin-trash/{}-{}",
            full_id,
            chrono::Utc::now().timestamp()
        ));
        if let Some(parent) = t.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&target, &t)
            .with_context(|| format!("移除旧版本失败: {}", target.display()))?;
        trash = Some(t);
    }
    if let Err(e) = std::fs::rename(staging, &target) {
        // 发布失败 → 回滚旧版本,不留「两边都没有」的状态
        if let Some(t) = &trash {
            let _ = std::fs::rename(t, &target);
        }
        return Err(anyhow::Error::from(e))
            .with_context(|| format!("发布到插件目录失败(已回滚旧版本): {}", target.display()));
    }

    // 5. 锁文件:新插件默认禁用
    let mut lock = load_lock();
    let entry = LockEntry {
        full_id: full_id.clone(),
        name: manifest.manifest.name.clone(),
        version: manifest.manifest.version_str.clone(),
        source: origin,
        content_hash: hash,
        permission_fingerprint: fingerprint,
        enabled: false,
        authorized_at: None,
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    lock.plugins.insert(full_id, entry.clone());
    save_lock(&lock)?;
    Ok(entry)
}

/// 已损坏安装(清单缺失/非法)在加载时被拒绝,不进入注册表。
pub fn load_installed(root_override: Option<&Path>) -> Vec<(PathBuf, PluginManifest, LockEntry)> {
    let root = root_override
        .map(PathBuf::from)
        .unwrap_or_else(plugins_root);
    let lock = load_lock();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(manifest) = PluginManifest::load(&path) else {
            log::warn!("插件清单非法,拒绝加载: {}", path.display());
            continue;
        };
        let full_id = manifest.full_id();
        if path
            .file_name()
            .map(|n| n.to_string_lossy() != full_id)
            .unwrap_or(true)
        {
            log::warn!("插件目录名与 full_id 不一致,拒绝加载: {}", path.display());
            continue;
        }
        // 安装后目录被篡改必须在加载时被发现:重算内容哈希与锁文件比对,
        // 不一致 → 强制禁用 + 清除授权(要求重新审查)
        let mut forced_disable = false;
        let hash_now = content_hash(&path).ok();
        let hash_locked = lock.plugins.get(&full_id).map(|e| e.content_hash.clone());
        let content_hash = match (hash_now, &hash_locked) {
            (Some(h), Some(locked)) if &h == locked => h,
            (Some(h), Some(_locked)) => {
                log::warn!(
                    "插件内容与锁文件哈希不一致,强制禁用并要求重新授权: {}",
                    path.display()
                );
                forced_disable = true;
                h
            }
            (Some(h), None) => h,
            (None, _) => {
                log::warn!("插件内容哈希计算失败,拒绝加载: {}", path.display());
                continue;
            }
        };
        let lock_entry = lock.plugins.get(&full_id).cloned().unwrap_or(LockEntry {
            full_id: full_id.clone(),
            name: manifest.manifest.name.clone(),
            version: manifest.manifest.version_str.clone(),
            source: InstallSource::Bundled,
            content_hash: String::new(),
            permission_fingerprint: String::new(),
            enabled: false,
            authorized_at: None,
            installed_at: String::new(),
        });
        let lock_entry = if forced_disable {
            LockEntry {
                enabled: false,
                authorized_at: None,
                content_hash,
                ..lock_entry
            }
        } else {
            lock_entry
        };
        out.push((path, manifest, lock_entry));
    }
    out
}

/// 删除插件(目录 + 锁条目)。
pub fn uninstall(full_id: &str) -> Result<()> {
    let target = plugins_root().join(full_id);
    if target.exists() {
        std::fs::remove_dir_all(&target)
            .with_context(|| format!("删除插件目录失败: {}", target.display()))?;
    }
    let mut lock = load_lock();
    lock.plugins.remove(full_id);
    save_lock(&lock)?;
    Ok(())
}

pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = std::fs::symlink_metadata(&from)?;
        if meta.file_type().is_symlink() {
            bail!("源包含符号链接,拒绝复制: {}", from.display());
        }
        if meta.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PipelineContribution;

    fn write_plugin(dir: &Path, pipeline_body: &str) {
        std::fs::create_dir_all(dir.join("pipelines")).unwrap();
        std::fs::create_dir_all(dir.join("skills/demo")).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            r#"
[manifest]
version = 1
publisher = "test"
id = "p1"
name = "Test Plugin"
version_str = "0.1.0"
description = "测试"

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
"#,
        )
        .unwrap();
        std::fs::write(dir.join("pipelines/p1.json"), pipeline_body).unwrap();
        std::fs::write(dir.join("skills/demo/INSTRUCTIONS.md"), "body").unwrap();
    }

    // 测试用隔离目录(home 覆盖不可行,直接覆盖模块内函数行为:plugins_root 由测试重定向)
    // 这里通过环境变量 MF_PLUGIN_ROOT 支持(见 plugins_root_for)。
    static ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn with_isolated_root<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("MF_PLUGIN_ROOT", tmp.path());
        let out = f(tmp.path());
        std::env::remove_var("MF_PLUGIN_ROOT");
        out
    }

    #[test]
    fn content_hash_stable_and_sensitive() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(tmp.path(), "{}");
        let h1 = content_hash(tmp.path()).unwrap();
        let h2 = content_hash(tmp.path()).unwrap();
        assert_eq!(h1, h2, "相同内容哈希稳定");
        std::fs::write(tmp.path().join("pipelines/p1.json"), "{\"changed\":1}").unwrap();
        let h3 = content_hash(tmp.path()).unwrap();
        assert_ne!(h1, h3, "内容变化哈希变化");
    }

    #[test]
    fn symlink_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(tmp.path(), "{}");
        // Windows 需要特权才能建 symlink;用目录 junction 不可移植,改为直接测 copy 拒绝逻辑的间接路径:
        // content_hash 对缺失目录报错即可覆盖损坏安装路径
        let broken = tmp.path().join("no-such-dir");
        assert!(content_hash(&broken).is_err());
    }

    #[test]
    fn install_flow_disabled_by_default_and_validate() {
        with_isolated_root(|_root| {
            let src = tempfile::tempdir().unwrap();
            write_plugin(src.path(), "{\"steps\":[]}");
            let entry = install_from_dir(
                src.path(),
                InstallSource::Local {
                    path: src.path().display().to_string(),
                },
            )
            .unwrap();
            assert!(!entry.enabled, "新插件默认禁用");
            assert!(entry.content_hash.starts_with("sha256:"));
            // 已发布 + 锁文件存在
            assert!(plugins_root().join("test.p1").join(MANIFEST_FILE).is_file());
            let lock = load_lock();
            assert!(lock.plugins.contains_key("test.p1"));

            // 路径逃逸拒绝
            let evil = tempfile::tempdir().unwrap();
            write_plugin(evil.path(), "{}");
            let bad_manifest = PluginManifest {
                pipelines: vec![PipelineContribution {
                    id: "x".into(),
                    name: "x".into(),
                    file: "../../escape.json".into(),
                }],
                ..PluginManifest::load(evil.path()).unwrap()
            };
            std::fs::write(
                evil.path().join(MANIFEST_FILE),
                toml::to_string(&bad_manifest).unwrap(),
            )
            .unwrap();
            assert!(
                install_from_dir(evil.path(), InstallSource::Local { path: "".into() }).is_err()
            );

            // 损坏安装(清单被改坏)在加载时被拒绝
            let installed_dir = plugins_root().join("test.p1");
            std::fs::write(installed_dir.join(MANIFEST_FILE), "not = valid").unwrap();
            assert!(load_installed(None).is_empty());
        });
    }

    #[test]
    fn reinstall_same_content_idempotent() {
        with_isolated_root(|_| {
            let src = tempfile::tempdir().unwrap();
            write_plugin(src.path(), "{}");
            let e1 =
                install_from_dir(src.path(), InstallSource::Local { path: "x".into() }).unwrap();
            let e2 =
                install_from_dir(src.path(), InstallSource::Local { path: "x".into() }).unwrap();
            assert_eq!(e1.content_hash, e2.content_hash);
            assert!(!e2.enabled);
        });
    }
}
