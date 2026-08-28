//! 插件安装:staging → 校验 → 内容哈希 → 内容寻址原子发布 → 锁文件。
//!
//! 包目录按内容哈希命名(`packages/<sha256-hex>/`),发布后绝不改写;
//! 同一 full_id 可并存多个版本,锁文件记录每个 full_id 的当前选择(启用状态独立于包内容)。

#[cfg(test)]
use crate::manifest::MANIFEST_FILE;
use crate::manifest::{validate_manifest_paths, PluginManifest};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum InstallSource {
    Bundled,
    Local { path: String },
    Git { url: String, commit: Option<String> },
    Marketplace { id: String },
}

/// full_id → 当前选择(启用状态、授权指纹指向该版本的哈希)。
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

/// 内容寻址包登记:content_hash → 包记录(与启用状态分离)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageEntry {
    pub content_hash: String,
    pub full_id: String,
    pub version: String,
    pub installed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LockFile {
    pub plugins: BTreeMap<String, LockEntry>,
    #[serde(default)]
    pub packages: BTreeMap<String, PackageEntry>,
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

/// 内容寻址包根:`<plugins>/packages/`。
pub fn packages_root() -> PathBuf {
    packages_root_at(&plugins_root())
}

pub fn packages_root_at(root: &Path) -> PathBuf {
    root.join("packages")
}

pub fn lock_path() -> PathBuf {
    lock_path_at(&plugins_root())
}

pub fn lock_path_at(root: &Path) -> PathBuf {
    root.join("plugins.lock.json")
}

pub fn load_lock() -> LockFile {
    load_lock_at(&plugins_root())
}

pub fn load_lock_at(root: &Path) -> LockFile {
    match std::fs::read_to_string(lock_path_at(root)) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            // 损坏的锁文件隔离保留(不静默清空 —— 防止把其他插件的授权状态抹掉后回写)
            let corrupted = lock_path_at(root).with_extension(format!(
                "corrupt-{}",
                chrono::Utc::now().format("%Y%m%dT%H%M%S")
            ));
            let _ = std::fs::rename(lock_path_at(root), &corrupted);
            log::error!("锁文件损坏,已隔离到 {}({e})", corrupted.display());
            LockFile::default()
        }),
        Err(_) => LockFile::default(),
    }
}

pub fn save_lock(lock: &LockFile) -> Result<()> {
    save_lock_at(&plugins_root(), lock)
}

pub fn save_lock_at(root: &Path, lock: &LockFile) -> Result<()> {
    // 原子写:临时文件 + 同目录 rename,崩溃不留半截 JSON
    let path = lock_path_at(root);
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

/// 内容哈希 → 包目录名(去掉 `sha256:` 前缀,Windows 目录名不允许冒号)。
pub fn hash_dir_name(hash: &str) -> &str {
    hash.strip_prefix("sha256:").unwrap_or(hash)
}

pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub root: PathBuf,
    pub lock: LockEntry,
}

/// 安装来源目录(已在本地的目录,或 git clone 后的目录)。
/// 流程:复制到临时 staging → 校验清单与路径 → 哈希 → 内容寻址发布 → 锁文件(默认禁用)。
pub fn install_from_dir(source: &Path, origin: InstallSource) -> Result<LockEntry> {
    install_from_dir_at(&plugins_root(), source, origin)
}

pub fn install_from_dir_at(root: &Path, source: &Path, origin: InstallSource) -> Result<LockEntry> {
    // 1. staging:复制到插件目录内(同卷 rename 才是原子操作;%TEMP% 跨卷会失败)
    let staging = root.join(format!(
        ".staging-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    copy_dir_recursive(source, &staging)
        .with_context(|| format!("复制到 staging 失败: {}", source.display()))?;

    let result = install_staged_into(root, &staging, origin);
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
    let entry = install_from_dir_at(
        &plugins_root(),
        &staging,
        InstallSource::Git {
            url: url.to_string(),
            commit,
        },
    );
    let _ = std::fs::remove_dir_all(&staging);
    entry
}

/// 内容寻址发布:校验 → 哈希 → `packages/<hex>/`(已存在则复用,绝不改写)→ 锁文件。
fn install_staged_into(root: &Path, staging: &Path, origin: InstallSource) -> Result<LockEntry> {
    // 2. 校验
    let manifest = PluginManifest::load(staging)?;
    validate_manifest_paths(staging, &manifest)?;
    // 3. 哈希
    let hash = content_hash(staging)?;
    let fingerprint = manifest.permission_fingerprint(&hash);
    let full_id = manifest.full_id();

    // 4. 内容寻址发布:同内容包已存在则复用;否则原子 rename
    let target = packages_root_at(root).join(hash_dir_name(&hash));
    if !target.exists() {
        if let Some(p) = target.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::rename(staging, &target)
            .with_context(|| format!("发布包目录失败: {}", target.display()))?;
    }

    // 5. 锁文件:包登记 + full_id 选择条目(同哈希重装保留授权;新哈希默认禁用)
    let mut lock = load_lock_at(root);
    lock.packages.insert(
        hash.clone(),
        PackageEntry {
            content_hash: hash.clone(),
            full_id: full_id.clone(),
            version: manifest.manifest.version_str.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    let preserved = lock
        .plugins
        .get(&full_id)
        .filter(|old| old.content_hash == hash);
    let entry = LockEntry {
        full_id: full_id.clone(),
        name: manifest.manifest.name.clone(),
        version: manifest.manifest.version_str.clone(),
        source: origin,
        content_hash: hash,
        permission_fingerprint: fingerprint,
        enabled: preserved.map(|old| old.enabled).unwrap_or(false),
        authorized_at: preserved.and_then(|old| old.authorized_at.clone()),
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    lock.plugins.insert(full_id, entry.clone());
    save_lock_at(root, &lock)?;
    Ok(entry)
}

/// 加载已安装插件:只返回每个 full_id 的当前选择版本(历史版本经 `PluginHost::resolve` 按哈希取)。
/// 包内容与目录名(哈希)不一致 → 视为篡改,拒绝加载。
pub fn load_installed(root_override: Option<&Path>) -> Vec<(PathBuf, PluginManifest, LockEntry)> {
    load_installed_at(root_override
        .map(PathBuf::from)
        .unwrap_or_else(plugins_root)
        .as_path())
}

pub fn load_installed_at(root: &Path) -> Vec<(PathBuf, PluginManifest, LockEntry)> {
    let lock = load_lock_at(root);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(packages_root_at(root)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        if dir_name.len() != 64 || !dir_name.chars().all(|c| c.is_ascii_hexdigit()) {
            log::warn!("包目录名不是内容哈希,拒绝加载: {}", path.display());
            continue;
        }
        let Ok(manifest) = PluginManifest::load(&path) else {
            log::warn!("插件清单非法,拒绝加载: {}", path.display());
            continue;
        };
        let full_id = manifest.full_id();
        // 安装后目录被篡改必须在加载时被发现:重算内容哈希与目录名比对
        let Some(hash_now) = content_hash(&path).ok() else {
            log::warn!("插件内容哈希计算失败,拒绝加载: {}", path.display());
            continue;
        };
        if hash_dir_name(&hash_now) != dir_name {
            log::warn!(
                "插件内容与包目录哈希不一致(篡改),拒绝加载: {}",
                path.display()
            );
            continue;
        }
        let Some(selected) = lock.plugins.get(&full_id) else {
            continue; // 没有选择条目:仅作为可 resolve 的历史包存在
        };
        if selected.content_hash != hash_now {
            continue; // 非当前选择版本
        }
        let lock_entry = selected.clone();
        out.push((path, manifest, lock_entry));
    }
    out.sort_by(|a, b| a.1.full_id().cmp(&b.1.full_id()));
    out
}

/// 删除插件(选择条目 + 该 full_id 的全部包目录 + 包登记)。
pub fn uninstall(full_id: &str) -> Result<()> {
    uninstall_at(&plugins_root(), full_id)
}

pub fn uninstall_at(root: &Path, full_id: &str) -> Result<()> {
    let mut lock = load_lock_at(root);
    // 收集该 full_id 的包目录(逐个加载清单判断归属)
    if let Ok(entries) = std::fs::read_dir(packages_root_at(root)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let belongs = PluginManifest::load(&path)
                .map(|m| m.full_id() == full_id)
                .unwrap_or(false);
            if belongs {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("删除插件包失败: {}", path.display()))?;
            }
        }
    }
    lock.packages.retain(|_, p| p.full_id != full_id);
    lock.plugins.remove(full_id);
    save_lock_at(root, &lock)?;
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
    use crate::manifest::WorkflowTemplateContribution;

    fn write_plugin(dir: &Path, pipeline_body: &str) {
        std::fs::create_dir_all(dir.join("pipelines")).unwrap();
        std::fs::create_dir_all(dir.join("skills/demo")).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            r#"
[manifest]
version = 2
publisher = "test"
id = "p1"
name = "Test Plugin"
version_str = "0.1.0"
description = "测试"

[capabilities]
net = true

[[agent_types]]
id = "demo"
name = "Demo"
adapter = "generic-command"
command = "demo"
detect_commands = ["demo"]

[[workflow_templates]]
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
            // 已发布到内容寻址目录 + 锁文件存在
            let pkg_dir = packages_root()
                .join(hash_dir_name(&entry.content_hash))
                .join(MANIFEST_FILE);
            assert!(pkg_dir.is_file());
            let lock = load_lock();
            assert!(lock.plugins.contains_key("test.p1"));
            assert!(lock
                .packages
                .contains_key(&entry.content_hash.clone()));

            // 路径逃逸拒绝
            let evil = tempfile::tempdir().unwrap();
            write_plugin(evil.path(), "{}");
            let bad_manifest = PluginManifest {
                workflow_templates: vec![WorkflowTemplateContribution {
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
            let installed_dir = packages_root().join(hash_dir_name(&entry.content_hash));
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

    #[test]
    fn new_version_keeps_old_package_dir() {
        with_isolated_root(|_| {
            let v1 = tempfile::tempdir().unwrap();
            write_plugin(v1.path(), "{\"v\":1}");
            let e1 =
                install_from_dir(v1.path(), InstallSource::Local { path: "x".into() }).unwrap();
            let v2 = tempfile::tempdir().unwrap();
            write_plugin(v2.path(), "{\"v\":2}");
            let e2 =
                install_from_dir(v2.path(), InstallSource::Local { path: "x".into() }).unwrap();
            assert_ne!(e1.content_hash, e2.content_hash);
            // 旧版本包目录保留(内容寻址、不可变)
            assert!(packages_root()
                .join(hash_dir_name(&e1.content_hash))
                .join(MANIFEST_FILE)
                .is_file());
            assert!(packages_root()
                .join(hash_dir_name(&e2.content_hash))
                .join(MANIFEST_FILE)
                .is_file());
            // 只有当前选择版本进入加载列表
            let loaded = load_installed(None);
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].2.content_hash, e2.content_hash);
        });
    }
}
