//! Git worktree Provider(Orchestration Task 5):隔离创建、
//! 拓扑顺序确定性合并、冲突 → NeedsUser(不覆盖)、非 Git 回退、清理。

use mf_agent::execution_directory::{
    ensure_lease_under_root, ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use mf_plugins::git_worktree_provider::{deterministic_merge_order, GitWorktreeProvider};
use std::path::PathBuf;

// ---------- 真实仓库 Fixture ----------

struct RepoFixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl RepoFixture {
    fn with_base_files() -> RepoFixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        let sig = git2::Signature::now("mf", "mf@test").unwrap();
        std::fs::write(root.join("shared.txt"), "base\n").unwrap();
        std::fs::write(root.join("a.txt"), "aaa\n").unwrap();
        std::fs::write(root.join("b.txt"), "bbb\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
        RepoFixture { _dir: dir, root }
    }

    fn provider(&self) -> GitWorktreeProvider {
        GitWorktreeProvider::new(self.root.clone()).unwrap()
    }

    fn ctx(&self, task: i64, step: &str, attempt: u32) -> LeaseContext {
        LeaseContext {
            task_id: task,
            step_id: 0,
            attempt,
            project_root: self.root.clone(),
            step_key: step.into(),
        }
    }
}

fn worktree_path(root: &std::path::Path, name: &str) -> PathBuf {
    root.parent().unwrap().join(".worktrees").join(name)
}

// ---------- 隔离创建 ----------

#[test]
fn acquire_creates_isolated_named_worktrees_with_base_files() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();

    let l1 = provider.acquire(&fx.ctx(1, "build", 1)).unwrap();
    let l2 = provider.acquire(&fx.ctx(1, "docs", 1)).unwrap();

    assert!(l1.isolated && l2.isolated);
    // git 返回的路径可能带 8.3 短名/大小写差异,规范化后比较
    assert_eq!(
        l1.path.canonicalize().unwrap(),
        worktree_path(&fx.root, "mf-run-1-build-1")
            .canonicalize()
            .unwrap(),
        "命名必须是 mf-run-<task>-<step>-<attempt>"
    );
    assert_ne!(l1.path, l2.path);
    // worktree 从基线创建:基线文件存在且内容一致(autocrlf 归一)
    let base_text = |p: PathBuf| std::fs::read_to_string(p).unwrap().replace("\r\n", "\n");
    assert_eq!(base_text(l1.path.join("shared.txt")), "base\n");
    // 路径必须位于 .worktrees 之下(两侧都规范化,规避 8.3 短名差异)
    let wt_root = fx
        .root
        .parent()
        .unwrap()
        .join(".worktrees")
        .canonicalize()
        .unwrap();
    ensure_lease_under_root(&wt_root, &l1.path.canonicalize().unwrap()).unwrap();

    // 重复获取同名 → 幂等返回同一目录(已存在)
    let again = provider.acquire(&fx.ctx(1, "build", 1)).unwrap();
    assert_eq!(again.path, l1.path);
}

#[test]
fn release_removes_worktree_directory() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    let lease = provider.acquire(&fx.ctx(2, "build", 1)).unwrap();
    assert!(lease.path.join("shared.txt").exists());

    provider.release(&lease).unwrap();
    assert!(!lease.path.exists(), "释放后目录应被清理");

    // 清理后可以再次创建同名
    let again = provider.acquire(&fx.ctx(2, "build", 2)).unwrap();
    assert!(again.path.join("shared.txt").exists());
    provider.release(&again).unwrap();
}

// ---------- 确定性合并 ----------

#[test]
fn ordered_merge_applies_disjoint_changes_to_project() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    let l1 = provider.acquire(&fx.ctx(3, "build", 1)).unwrap();
    let l2 = provider.acquire(&fx.ctx(3, "docs", 1)).unwrap();

    std::fs::write(l1.path.join("a.txt"), "changed-by-build\n").unwrap();
    std::fs::write(l2.path.join("b.txt"), "changed-by-docs\n").unwrap();

    let outcome = provider.merge(&[l2.clone(), l1.clone()]).unwrap();
    assert_eq!(outcome, MergeOutcome::Merged);
    // 项目目录同时获得两个 worktree 的修改(Windows autocrlf 可能写 CRLF)
    let norm = |p: PathBuf| std::fs::read_to_string(p).unwrap().replace("\r\n", "\n");
    assert_eq!(norm(fx.root.join("a.txt")), "changed-by-build\n");
    assert_eq!(norm(fx.root.join("b.txt")), "changed-by-docs\n");
}

#[test]
fn conflicting_join_returns_needs_user_without_overwrite() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    let l1 = provider.acquire(&fx.ctx(4, "build", 1)).unwrap();
    let l2 = provider.acquire(&fx.ctx(4, "docs", 1)).unwrap();

    std::fs::write(l1.path.join("shared.txt"), "from-build\n").unwrap();
    std::fs::write(l2.path.join("shared.txt"), "from-docs\n").unwrap();

    let outcome = provider.merge(&[l1, l2]).unwrap();
    match outcome {
        MergeOutcome::NeedsUser { conflicts } => {
            assert!(
                conflicts.iter().any(|c| c.contains("shared.txt")),
                "冲突列表应包含 shared.txt: {conflicts:?}"
            );
        }
        other => panic!("应为 NeedsUser,得到 {other:?}"),
    }
    // 主工作区不被覆盖:基线内容原样保留
    assert_eq!(
        std::fs::read_to_string(fx.root.join("shared.txt")).unwrap(),
        "base\n"
    );
}

#[test]
fn merge_order_is_topological_then_step_key() {
    let lease = |key: &str, deps: &[&str]| ExecutionLease {
        id: format!("lease-{key}"),
        path: PathBuf::from("."),
        isolated: true,
        provider: "worktree".into(),
        metadata: serde_json::json!({ "step_key": key, "deps": deps }),
    };
    let mut leases = vec![
        lease("package", &["build", "docs"]),
        lease("docs", &[]),
        lease("audit", &[]),
        lease("build", &["audit"]),
    ];
    deterministic_merge_order(&mut leases);
    let keys: Vec<&str> = leases
        .iter()
        .map(|l| l.metadata["step_key"].as_str().unwrap())
        .collect();
    // 拓扑:audit → build → (docs 与 package);同层按稳定键:docs < package
    assert_eq!(keys, vec!["audit", "build", "docs", "package"]);
}

// ---------- 非 Git 回退 ----------

#[test]
fn non_git_root_falls_back_to_shared_project_directory() {
    let dir = tempfile::tempdir().unwrap();
    let provider = GitWorktreeProvider::new(dir.path().to_path_buf()).unwrap();
    let lease = provider
        .acquire(&LeaseContext {
            task_id: 9,
            step_id: 0,
            attempt: 1,
            project_root: dir.path().to_path_buf(),
            step_key: "solo".into(),
        })
        .unwrap();
    assert!(!lease.isolated, "非 Git 根不隔离,需风险开关才能并行");
    assert_eq!(lease.path, dir.path());
    assert!(matches!(
        provider.merge(&[lease.clone()]).unwrap(),
        MergeOutcome::NotRequired
    ));
    provider.release(&lease).unwrap();
}
