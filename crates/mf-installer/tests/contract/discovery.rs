//! T4b 契约(Issue #40;spec §9.4):discovery 去重、candidate 硬化、
//! adoption 判定与 identity 重核。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mf_installer::adoption::{evaluate_adoption, verify_launch_identity, TrustedArtifact};
use mf_installer::discovery::{
    discover, validate_candidate, DiscoveredInstallation, FsProbe, InstallationKind,
};

/// 内存文件系统 fake:预先登记 (entry → canonical, identity)。
struct FakeFs {
    /// entry 路径 → canonical 路径。
    links: BTreeMap<PathBuf, PathBuf>,
    /// canonical → identity。
    identities: BTreeMap<PathBuf, String>,
    /// 可执行文件集合(entry 或 canonical 本身)。
    files: Vec<PathBuf>,
}

impl FakeFs {
    fn new() -> Self {
        Self {
            links: BTreeMap::new(),
            identities: BTreeMap::new(),
            files: Vec::new(),
        }
    }

    fn add_direct(&mut self, canonical: &Path, identity: &str) {
        self.files.push(canonical.to_path_buf());
        self.identities
            .insert(canonical.to_path_buf(), identity.to_string());
    }

    fn add_link(&mut self, entry: &Path, target: &Path) {
        self.files.push(entry.to_path_buf());
        self.links.insert(entry.to_path_buf(), target.to_path_buf());
    }

    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        self.links.get(path).cloned().or_else(|| {
            if self.files.iter().any(|f| f == path) {
                Some(path.to_path_buf())
            } else {
                None
            }
        })
    }
}

impl FsProbe for FakeFs {
    fn find_executable(&self, dir: &Path, name: &str) -> Option<PathBuf> {
        let direct = dir.join(name);
        if self.files.contains(&direct) {
            return Some(direct);
        }
        // Windows 扩展形态
        for ext in [".exe", ".cmd"] {
            let candidate = dir.join(format!("{name}{ext}"));
            if self.files.contains(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        let mut current = path.to_path_buf();
        // 迭代解析多层 shim/link
        for _ in 0..8 {
            match self.links.get(&current) {
                Some(next) => current = next.clone(),
                None => break,
            }
        }
        if self.files.contains(&current) {
            Some(current)
        } else {
            None
        }
    }

    fn file_identity(&self, path: &Path) -> Option<String> {
        self.identities.get(path).cloned()
    }
}

fn installation(
    canonical: &Path,
    identity: &str,
    kind: InstallationKind,
) -> DiscoveredInstallation {
    DiscoveredInstallation {
        canonical_path: canonical.to_path_buf(),
        executable_identity: identity.to_string(),
        kind,
        entries: vec![],
    }
}

#[test]
fn pathlike_candidates_are_rejected_hard() {
    // 浏览器输入不能变成搜索路径
    for bad in [
        "../escape",
        "C:/Windows/system32/cmd.exe",
        "/usr/bin/env",
        "./local",
        "dir\\sub\\cmd",
    ] {
        assert!(
            validate_candidate(bad).is_err(),
            "类路径 candidate 必须拒绝:{bad}"
        );
    }
    for good in ["codex", "claude-code", "aider.chat", "opencode@latest"] {
        assert!(validate_candidate(good).is_ok(), "命令名应接受:{good}");
    }
    assert!(validate_candidate("").is_err());
    assert!(validate_candidate("   ").is_err());
}

#[test]
fn duplicate_installations_dedupe_by_canonical_target() {
    let managed = PathBuf::from("/managed");
    let mut fs = FakeFs::new();
    // 受管真身
    fs.add_direct(&managed.join("codex.exe"), "hash-aaa");
    // PATH 里两个不同 shim 指向同一受管真身 + 一个直接副本(同 hash)
    fs.add_link(
        Path::new("/usr/local/bin/codex"),
        &managed.join("codex.exe"),
    );
    fs.add_link(
        Path::new("/opt/homebrew/bin/codex"),
        &managed.join("codex.exe"),
    );
    fs.add_direct(Path::new("/tools/codex.exe"), "hash-aaa");

    let roots = vec![
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/tools"),
        // 受管根同样进入扫描(direct 入口 + shim 入口并存)
        managed.clone(),
    ];
    let installations = discover(&fs, &roots, &[managed.clone()], &["codex".into()]).unwrap();
    // 两个 canonical(managed 真身 + 同 hash 副本)→ 两条 Installation;
    // shim 合并进真身的 entries
    assert_eq!(installations.len(), 2, "按 canonical 去重");
    let main = installations
        .iter()
        .find(|i| i.canonical_path == managed.join("codex.exe"))
        .expect("受管真身");
    assert_eq!(main.kind, InstallationKind::Managed);
    assert_eq!(
        main.entries.len(),
        3,
        "direct + 两个 shim 入口合并:{:?}",
        main.entries
    );
    let copy = installations
        .iter()
        .find(|i| i.canonical_path == Path::new("/tools/codex.exe"))
        .expect("外部副本");
    assert_eq!(copy.kind, InstallationKind::External);
}

#[test]
fn symlink_target_swap_rejects_launch_identity() {
    // §9.7:Revision 冻结 identity;target 被替换 → 拒绝静默运行
    let managed = PathBuf::from("/managed");
    let mut fs = FakeFs::new();
    fs.add_direct(&managed.join("cli.exe"), "hash-original");
    fs.add_link(Path::new("/usr/local/bin/cli"), &managed.join("cli.exe"));
    let roots = vec![PathBuf::from("/usr/local/bin")];
    let frozen = discover(&fs, &roots, &[managed], &["cli".into()])
        .unwrap()
        .pop()
        .unwrap();
    // 正常启动
    verify_launch_identity(&frozen, "hash-original").unwrap();
    // target 被替换(hash 改变)
    let mut swapped = fs;
    swapped
        .identities
        .insert(managed_path(&swapped, "cli.exe"), "hash-evil".into());
    let now = discover(
        &swapped,
        &roots,
        &[PathBuf::from("/managed")],
        &["cli".into()],
    )
    .unwrap()
    .pop()
    .unwrap();
    assert!(verify_launch_identity(&now, "hash-original").is_err());
}

fn managed_path(fs: &FakeFs, name: &str) -> PathBuf {
    fs.identities
        .keys()
        .find(|p| p.ends_with(name))
        .cloned()
        .expect("managed file")
}

#[test]
fn adoption_requires_exact_trusted_hash() {
    let external = installation(
        Path::new("/usr/local/bin/tool"),
        "hash-actual",
        InstallationKind::External,
    );
    let trusted = TrustedArtifact {
        agent_type_id: "codex".into(),
        installer_id: "npm-global".into(),
        executable_sha256: "hash-actual".into(),
    };
    match evaluate_adoption(&external, &trusted) {
        mf_installer::adoption::AdoptionDecision::Adoptable(receipt) => {
            assert_eq!(receipt.agent_type_id, "codex");
            assert_eq!(
                receipt.canonical_executable,
                Path::new("/usr/local/bin/tool")
            );
        }
        other => panic!("完全匹配必须可 adopt:{other:?}"),
    }
    // 不匹配 → 只能重装
    let untrusted = TrustedArtifact {
        executable_sha256: "hash-different".into(),
        ..trusted.clone()
    };
    assert!(matches!(
        evaluate_adoption(&external, &untrusted),
        mf_installer::adoption::AdoptionDecision::ReinstallRequired { .. }
    ));
    // 受管安装不走 adoption
    let m = installation(
        Path::new("/managed/tool"),
        "hash-actual",
        InstallationKind::Managed,
    );
    assert!(matches!(
        evaluate_adoption(&m, &trusted),
        mf_installer::adoption::AdoptionDecision::ReinstallRequired { .. }
    ));
}
