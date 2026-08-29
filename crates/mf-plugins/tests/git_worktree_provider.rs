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
        self.ctx_rev(task, step, attempt, 1, &[])
    }

    fn ctx_rev(
        &self,
        task: i64,
        step: &str,
        attempt: u32,
        revision: i64,
        deps: &[&str],
    ) -> LeaseContext {
        LeaseContext {
            task_id: task,
            step_id: 0,
            revision_id: revision,
            attempt,
            project_root: self.root.clone(),
            step_key: step.into(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
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
            revision_id: 1,
            attempt: 1,
            project_root: dir.path().to_path_buf(),
            step_key: "solo".into(),
            deps: vec![],
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

// ---------- Task/Revision 集成基线(串行下游 / 并行序无关 / 冲突零覆盖) ----------

fn norm(p: std::path::PathBuf) -> String {
    std::fs::read_to_string(p).unwrap().replace("\r\n", "\n")
}

#[test]
fn serial_downstream_worktree_sees_upstream_merged_changes() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    // 串行链 build → package(同 task/rev;package 依赖 build)
    let up = provider
        .acquire(&fx.ctx_rev(11, "build", 1, 1, &[]))
        .unwrap();
    std::fs::write(up.path.join("a.txt"), "from-build\n").unwrap();
    assert_eq!(provider.merge(&[up.clone()]).unwrap(), MergeOutcome::Merged);
    assert_eq!(norm(fx.root.join("a.txt")), "from-build\n");
    provider.release(&up).unwrap();

    // 下游 acquire:必须从汇合结果(集成基线)检出,看得见上游修改
    let down = provider
        .acquire(&fx.ctx_rev(11, "package", 1, 1, &["build"]))
        .unwrap();
    assert_eq!(
        norm(down.path.join("a.txt")),
        "from-build\n",
        "串行下游 worktree 必须包含上游已汇合的修改"
    );
    // 下游继续修改并汇合:项目目录拿到叠加结果
    std::fs::write(down.path.join("b.txt"), "from-package\n").unwrap();
    assert_eq!(
        provider.merge(&[down.clone()]).unwrap(),
        MergeOutcome::Merged
    );
    assert_eq!(norm(fx.root.join("a.txt")), "from-build\n");
    assert_eq!(norm(fx.root.join("b.txt")), "from-package\n");
    provider.release(&down).unwrap();

    // 任务终态:集成基线 ref 清理(hidden ref 不残留)
    let git = mf_vcs::git::Git::open(&fx.root).unwrap();
    assert_eq!(
        git.delete_integration_refs(11).unwrap(),
        1,
        "任务终态应清理集成基线 ref"
    );
}

#[test]
fn parallel_disjoint_merge_is_order_independent() {
    let run = |merge_order: &[&str]| {
        let fx = RepoFixture::with_base_files();
        let provider = fx.provider();
        let build = provider
            .acquire(&fx.ctx_rev(12, "build", 1, 1, &[]))
            .unwrap();
        let docs = provider
            .acquire(&fx.ctx_rev(12, "docs", 1, 1, &[]))
            .unwrap();
        std::fs::write(build.path.join("a.txt"), "by-build\n").unwrap();
        std::fs::write(build.path.join("new-build.txt"), "new\n").unwrap();
        std::fs::write(docs.path.join("b.txt"), "by-docs\n").unwrap();
        let by_key = |k: &str| {
            if k == "build" {
                build.clone()
            } else {
                docs.clone()
            }
        };
        let leases: Vec<_> = merge_order.iter().map(|k| by_key(k)).collect();
        assert_eq!(provider.merge(&leases).unwrap(), MergeOutcome::Merged);
        let _ = provider.release(&build);
        let _ = provider.release(&docs);
        (
            norm(fx.root.join("a.txt")),
            norm(fx.root.join("b.txt")),
            norm(fx.root.join("new-build.txt")),
        )
    };
    let ab = run(&["build", "docs"]);
    let ba = run(&["docs", "build"]);
    assert_eq!(
        ab, ba,
        "无重叠并行合并必须与顺序无关(实际 {ab:?} vs {ba:?})"
    );
    assert_eq!(ab.0, "by-build\n");
    assert_eq!(ab.1, "by-docs\n");
    assert_eq!(ab.2, "new\n");
}

#[test]
fn conflict_batch_precheck_leaves_project_dir_untouched() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    let build = provider
        .acquire(&fx.ctx_rev(13, "build", 1, 1, &[]))
        .unwrap();
    let docs = provider
        .acquire(&fx.ctx_rev(13, "docs", 1, 1, &[]))
        .unwrap();
    // build 同时改了独占文件 + 冲突文件;docs 改冲突文件
    std::fs::write(build.path.join("a.txt"), "by-build\n").unwrap();
    std::fs::write(build.path.join("shared.txt"), "from-build\n").unwrap();
    let docs_baseline_bytes = std::fs::read(docs.path.join("shared.txt")).unwrap();
    std::fs::write(docs.path.join("shared.txt"), "from-docs\n").unwrap();

    match provider.merge(&[build.clone(), docs.clone()]).unwrap() {
        MergeOutcome::NeedsUser { conflicts } => {
            assert!(
                conflicts.iter().any(|c| c.contains("shared.txt")),
                "{conflicts:?}"
            );
        }
        other => panic!("应为 NeedsUser,得到 {other:?}"),
    }
    // 冲突前置预检:项目目录零部分写(独占文件也不得提前落盘)
    assert_eq!(norm(fx.root.join("shared.txt")), "base\n");
    assert_eq!(norm(fx.root.join("a.txt")), "aaa\n");

    // 冲突解决:docs 放弃对 shared.txt 的修改(恢复基线字节)后整批重试,
    // 汇合成功且 build 的修改全部落地
    std::fs::write(docs.path.join("shared.txt"), docs_baseline_bytes).unwrap();
    assert_eq!(
        provider.merge(&[build.clone(), docs.clone()]).unwrap(),
        MergeOutcome::Merged
    );
    assert_eq!(norm(fx.root.join("a.txt")), "by-build\n");
    assert_eq!(norm(fx.root.join("shared.txt")), "from-build\n");
    let _ = provider.release(&build);
    let _ = provider.release(&docs);
}

#[test]
fn acquire_records_deps_and_baseline_in_lease_metadata() {
    // 拓扑合并顺序的前提:acquire 把上游 deps 与基线 oid 落进租约元数据;
    // deterministic_merge_order 按 metadata.deps 排序(见上方纯函数测试)
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    let lease = provider
        .acquire(&fx.ctx_rev(14, "package", 1, 1, &["build", "docs"]))
        .unwrap();
    let deps: Vec<&str> = lease.metadata["deps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(deps, vec!["build", "docs"], "deps 必须进租约元数据");
    let baseline = lease.metadata["baseline"].as_str().unwrap().to_string();
    assert_eq!(baseline.len(), 40, "基线 oid(40 hex)必须记录: {baseline}");
    let mut ordered = vec![lease.clone()];
    let mut other = ExecutionLease {
        id: "x".into(),
        path: lease.path.clone(),
        isolated: true,
        provider: "worktree".into(),
        metadata: serde_json::json!({ "step_key": "build", "deps": [] }),
    };
    // 与真实租约混排:build(无依赖)排在 package 之前
    std::mem::swap(&mut other, &mut ordered[0]);
    ordered.push(lease.clone());
    deterministic_merge_order(&mut ordered);
    let keys: Vec<&str> = ordered
        .iter()
        .map(|l| l.metadata["step_key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["build", "package"]);
    provider.release(&lease).unwrap();
}

// ---------- 并行兄弟先后结算:后完成者不得静默覆盖已汇合修改 ----------

#[test]
fn sequential_sibling_same_file_conflicts_without_overwrite() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    let a = provider
        .acquire(&fx.ctx_rev(21, "build", 1, 1, &[]))
        .unwrap();
    let b = provider
        .acquire(&fx.ctx_rev(21, "docs", 1, 1, &[]))
        .unwrap();
    // 两个兄弟都改 shared.txt(各自基线都是 T0)
    std::fs::write(a.path.join("shared.txt"), "from-build\n").unwrap();
    std::fs::write(b.path.join("shared.txt"), "from-docs\n").unwrap();

    // 先完成者单独汇合:成功,基线推进到 T1
    assert_eq!(provider.merge(&[a.clone()]).unwrap(), MergeOutcome::Merged);
    assert_eq!(norm(fx.root.join("shared.txt")), "from-build\n");

    // 后完成者单独汇合:与"基线→当前集成 ref"之间已汇合的修改重叠
    // → NeedsUser,项目目录零覆盖(仍是先完成者的版本)
    match provider.merge(&[b.clone()]).unwrap() {
        MergeOutcome::NeedsUser { conflicts } => {
            assert!(
                conflicts
                    .iter()
                    .any(|c| c.contains("shared.txt") && c.contains("已汇合")),
                "冲突必须指向已汇合变更: {conflicts:?}"
            );
        }
        other => panic!("应为 NeedsUser,得到 {other:?}(静默覆盖是缺陷)"),
    }
    assert_eq!(
        norm(fx.root.join("shared.txt")),
        "from-build\n",
        "冲突时项目目录保持先完成者的已汇合版本"
    );
    let _ = provider.release(&a);
    let _ = provider.release(&b);
}

// ---------- 原子合并:嵌套路径先建树、失败整体回滚(C2 回归) ----------

#[test]
fn nested_new_file_merges_and_downstream_worktree_sees_it() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    let lease = provider
        .acquire(&fx.ctx_rev(31, "build", 1, 1, &[]))
        .unwrap();
    std::fs::write(lease.path.join("a.txt"), "by-build\n").unwrap();
    // 嵌套目录中的新文件(非根级单段路径)
    std::fs::create_dir_all(lease.path.join("docs").join("guide")).unwrap();
    std::fs::write(
        lease.path.join("docs").join("guide").join("deep.md"),
        "# deep\n",
    )
    .unwrap();
    let outcome = provider.merge(&[lease.clone()]).unwrap();
    assert_eq!(outcome, MergeOutcome::Merged, "嵌套新文件必须能合并");
    assert_eq!(
        norm(fx.root.join("docs").join("guide").join("deep.md")),
        "# deep\n",
        "项目目录必须落盘嵌套新文件"
    );
    // 集成基线 ref 的树包含嵌套路径:下游 acquire 从汇合结果检出可见
    let down = provider
        .acquire(&fx.ctx_rev(31, "package", 1, 1, &["build"]))
        .unwrap();
    assert_eq!(
        norm(down.path.join("docs").join("guide").join("deep.md")),
        "# deep\n",
        "下游 worktree 必须看到嵌套新文件(基线树已包含)"
    );
    let _ = provider.release(&lease);
    let _ = provider.release(&down);
}

#[test]
fn nested_deleted_file_updates_baseline_tree() {
    let fx = RepoFixture::with_base_files();
    // 基线里先有嵌套文件
    {
        let repo = git2::Repository::open(&fx.root).unwrap();
        let sig = git2::Signature::now("mf", "mf@test").unwrap();
        std::fs::create_dir_all(fx.root.join("docs").join("guide")).unwrap();
        std::fs::write(fx.root.join("docs").join("guide").join("old.md"), "old\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "nested base", &tree, &[&parent])
            .unwrap();
    }
    let provider = fx.provider();
    let lease = provider
        .acquire(&fx.ctx_rev(33, "build", 1, 1, &[]))
        .unwrap();
    std::fs::remove_file(lease.path.join("docs").join("guide").join("old.md")).unwrap();
    assert_eq!(
        provider.merge(&[lease.clone()]).unwrap(),
        MergeOutcome::Merged,
        "删除嵌套文件必须能合并"
    );
    assert!(
        !fx.root.join("docs").join("guide").join("old.md").exists(),
        "项目目录中的嵌套文件应被删除"
    );
    // 基线树也不再有该文件
    let git = mf_vcs::git::Git::open(&fx.root).unwrap();
    let refname = mf_vcs::git::Git::integration_ref(33, 1);
    let oid = git.read_ref(&refname).unwrap().unwrap();
    let repo = git2::Repository::open(&fx.root).unwrap();
    let commit = repo.find_commit(oid).unwrap();
    assert!(
        commit
            .tree()
            .unwrap()
            .get_path(std::path::Path::new("docs/guide/old.md"))
            .is_err(),
        "集成基线树不得再包含已删除的嵌套文件"
    );
    let _ = provider.release(&lease);
}

#[test]
fn merge_failure_mid_apply_rolls_back_project_dir_and_ref() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    let lease = provider
        .acquire(&fx.ctx_rev(32, "build", 1, 1, &[]))
        .unwrap();
    // 变更集:根级文件修改 + docs/ 下新文件。
    // 预置障碍:项目目录中 `docs` 已是普通文件 → 应用到 docs/new.md 时
    // create_dir_all 必然失败(应用中段注入失败)
    std::fs::write(lease.path.join("a.txt"), "by-build\n").unwrap();
    std::fs::create_dir_all(lease.path.join("docs")).unwrap();
    std::fs::write(lease.path.join("docs").join("new.md"), "new\n").unwrap();
    std::fs::write(fx.root.join("docs"), "not a directory\n").unwrap();

    let git = mf_vcs::git::Git::open(&fx.root).unwrap();
    let refname = mf_vcs::git::Git::integration_ref(32, 1);
    let ref_before = git.read_ref(&refname).unwrap();
    let err = provider.merge(&[lease.clone()]).unwrap_err();
    assert!(
        err.to_string().contains("目录") || format!("{err:#}").contains("dir"),
        "失败原因应指向目录创建: {err:#}"
    );
    // 失败必须整体回滚:项目目录与集成 ref 都回到合并前(零部分写)
    assert_eq!(
        norm(fx.root.join("a.txt")),
        "aaa\n",
        "已复制的文件必须回滚,不得留部分写"
    );
    assert_eq!(
        git.read_ref(&refname).unwrap(),
        ref_before,
        "失败时集成 ref 不得推进"
    );
    // 障碍清除后重试:整批成功
    std::fs::remove_file(fx.root.join("docs")).unwrap();
    assert_eq!(
        provider.merge(&[lease.clone()]).unwrap(),
        MergeOutcome::Merged
    );
    assert_eq!(norm(fx.root.join("a.txt")), "by-build\n");
    assert_eq!(norm(fx.root.join("docs").join("new.md")), "new\n");
    let _ = provider.release(&lease);
}

#[test]
fn sequential_sibling_disjoint_changes_stack_in_baseline() {
    let fx = RepoFixture::with_base_files();
    let provider = fx.provider();
    let a = provider
        .acquire(&fx.ctx_rev(22, "build", 1, 1, &[]))
        .unwrap();
    let b = provider
        .acquire(&fx.ctx_rev(22, "docs", 1, 1, &[]))
        .unwrap();
    std::fs::write(a.path.join("a.txt"), "by-build\n").unwrap();
    std::fs::write(b.path.join("b.txt"), "by-docs\n").unwrap();

    assert_eq!(provider.merge(&[a.clone()]).unwrap(), MergeOutcome::Merged);
    // 不重叠的兄弟:后完成者正常汇合,两者修改都保留,
    // 且集成基线同时包含两者(下游从汇合结果检出可见全部)
    assert_eq!(provider.merge(&[b.clone()]).unwrap(), MergeOutcome::Merged);
    assert_eq!(norm(fx.root.join("a.txt")), "by-build\n");
    assert_eq!(norm(fx.root.join("b.txt")), "by-docs\n");
    let downstream = provider
        .acquire(&fx.ctx_rev(22, "package", 1, 1, &["build", "docs"]))
        .unwrap();
    assert_eq!(norm(downstream.path.join("a.txt")), "by-build\n");
    assert_eq!(norm(downstream.path.join("b.txt")), "by-docs\n");
    let _ = provider.release(&a);
    let _ = provider.release(&b);
    let _ = provider.release(&downstream);
}
