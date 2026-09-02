//! CLI 发现与 canonical 去重(T4b,Issue #40;spec §9.4)。
//!
//! discovery 只搜宿主允许的 PATH 与已登记受管目录;candidate 只接受
//! **命令名**(拒绝路径分隔符/绝对路径——浏览器输入不能变成搜索路径)。
//! canonicalize 解析入口 link/shim 的最终 target;相同 canonical
//! executable 去重为一个 Installation,入口形态与来源目录一并记录。
//! 文件系统探测经 `FsProbe` 注入(测试可换 fake;生产真实 FS)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// candidate 校验问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CandidateProblem {
    #[error("candidate `{0}` 含路径分隔符:命令名之外输入不得成为搜索路径")]
    PathLike(String),
    #[error("candidate 为空")]
    Empty,
}

/// 校验 candidate:只接受裸命令名(字母数字与 - _ . @ / + 组成,无
/// 分隔符、非绝对路径)。
pub fn validate_candidate(name: &str) -> Result<(), CandidateProblem> {
    if name.trim().is_empty() {
        return Err(CandidateProblem::Empty);
    }
    let looks_path_like = name.contains('/')
        || name.contains('\\')
        || Path::new(name).is_absolute()
        || name.starts_with('.');
    if looks_path_like {
        return Err(CandidateProblem::PathLike(name.to_string()));
    }
    Ok(())
}

/// 文件系统探测缝隙(生产真实 FS;测试 fake)。
pub trait FsProbe {
    /// 目录下是否存在可执行文件 `name`(含 Windows PATHEXT 扩展)。
    fn find_executable(&self, dir: &Path, name: &str) -> Option<PathBuf>;
    /// canonicalize(解析 symlink/junction/shim 的最终 target)。
    fn canonicalize(&self, path: &Path) -> Option<PathBuf>;
    /// 文件 SHA256(可执行身份)。
    fn file_identity(&self, path: &Path) -> Option<String>;
}

/// 真实文件系统探测。
pub struct RealFsProbe;

impl FsProbe for RealFsProbe {
    fn find_executable(&self, dir: &Path, name: &str) -> Option<PathBuf> {
        if let Some(direct) = try_exact(dir, name) {
            return Some(direct);
        }
        if cfg!(windows) {
            for ext in [".exe", ".cmd", ".bat", ".com"] {
                if let Some(hit) = try_exact(dir, &format!("{name}{ext}")) {
                    return Some(hit);
                }
            }
        }
        None
    }

    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }

    fn file_identity(&self, path: &Path) -> Option<String> {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path).ok()?;
        Some(format!("{:x}", Sha256::digest(bytes)))
    }
}

fn try_exact(dir: &Path, name: &str) -> Option<PathBuf> {
    let candidate = dir.join(name);
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// 入口形态(§9.4:canonicalize 并记录入口 link/shim 与最终 target)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// 直接文件。
    Direct,
    /// symlink/junction(合法可发现;target 被替换 → 启动前拒绝,由
    /// identity 重核保证)。
    Link,
}

/// 去重后的 Installation(同一 canonical executable 只有一条)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredInstallation {
    /// canonical executable(最终 target 绝对路径)。
    pub canonical_path: PathBuf,
    /// target 可执行身份(SHA256)。
    pub executable_identity: String,
    /// 外部(PATH)或受管(受管根内)。
    pub kind: InstallationKind,
    /// 全部入口(不同目录的 shim/symlink/direct 指向同一 canonical)。
    pub entries: Vec<DiscoveredEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallationKind {
    External,
    Managed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredEntry {
    pub entry_path: PathBuf,
    pub kind: EntryKind,
}

/// 执行 discovery:按序扫描 `scan_roots`(PATH 目录 + 受管根),同一
/// candidate 的多个命中按 canonical 去重合并;不同 candidate 命中同一
/// canonical(常见 shim)也合并为一个 Installation。
pub fn discover(
    probe: &dyn FsProbe,
    scan_roots: &[PathBuf],
    managed_roots: &[PathBuf],
    candidates: &[String],
) -> Result<Vec<DiscoveredInstallation>> {
    for name in candidates {
        validate_candidate(name)?;
    }
    let mut by_canonical: BTreeMap<PathBuf, DiscoveredInstallation> = BTreeMap::new();
    for name in candidates {
        for root in scan_roots {
            let Some(entry_path) = probe.find_executable(root, name) else {
                continue;
            };
            let Some(canonical_path) = probe.canonicalize(&entry_path) else {
                continue;
            };
            let Some(identity) = probe.file_identity(&canonical_path) else {
                continue;
            };
            let kind = EntryKind::from(&entry_path, &canonical_path);
            let installation_kind = if managed_roots.iter().any(|m| canonical_path.starts_with(m)) {
                InstallationKind::Managed
            } else {
                InstallationKind::External
            };
            let entry = DiscoveredEntry { entry_path, kind };
            by_canonical
                .entry(canonical_path.clone())
                .or_insert_with(|| DiscoveredInstallation {
                    canonical_path: canonical_path.clone(),
                    executable_identity: identity,
                    kind: installation_kind,
                    entries: Vec::new(),
                })
                .entries
                .push(entry);
        }
    }
    Ok(by_canonical.into_values().collect())
}

impl EntryKind {
    fn from(entry: &Path, canonical: &Path) -> Self {
        if entry == canonical {
            EntryKind::Direct
        } else {
            EntryKind::Link
        }
    }
}
