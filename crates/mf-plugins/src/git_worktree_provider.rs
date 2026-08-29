//! Git worktree Execution Directory Provider(设计 §6.3 / §9.4 / ADR 0003)。
//!
//! - 集成基线:每个 Task/Revision 维护 hidden ref
//!   `refs/mf/integration/task-<t>-rev-<r>`(初值 = 仓库 HEAD)。
//!   acquire 从基线检出 worktree;merge 把变更落回项目目录后把基线
//!   推进到汇合结果 —— 串行下游从汇合结果建租约,天然看见上游修改。
//! - merge:按"拓扑依赖序 + 稳定节点键"排序;第一遍批量预检所有租约
//!   相对各自主基线的变更集,任一文件被多个租约修改 → `NeedsUser`,
//!   项目目录零部分写;第二遍先在内存 index 构建完整汇合树(嵌套路径
//!   递归构造子树),原子推进基线 ref,再以撤销日志回滚式应用到项目
//!   目录 —— 应用失败时项目目录与集成 ref 整体回到合并前。
//! - release:校验路径位于 `.worktrees` 之下后删除 worktree
//!   (绝不触碰仓库根或仓库元数据)。非 Git 根回退共享目录(不隔离)。

use anyhow::{Context as _, Result};
use mf_agent::execution_directory::{
    ensure_lease_under_root, ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use mf_vcs::git::Git;
use std::collections::HashSet;

/// 提供器 ID(清单 `execution_directory_providers[].id`)。
pub const PROVIDER_ID: &str = "worktree";

pub struct GitWorktreeProvider {
    repo_root: PathBuf,
    /// `.worktrees` 根;非 Git 根为 None(回退共享目录)。
    worktrees_root: Option<PathBuf>,
}

impl GitWorktreeProvider {
    pub fn new(repo_root: PathBuf) -> Result<GitWorktreeProvider> {
        if Git::is_repo(&repo_root) {
            let git = Git::open(&repo_root)?;
            Ok(GitWorktreeProvider {
                repo_root,
                worktrees_root: Some(git.worktree_root()?),
            })
        } else {
            Ok(GitWorktreeProvider {
                repo_root,
                worktrees_root: None,
            })
        }
    }

    fn worktree_name(ctx: &LeaseContext) -> String {
        format!("mf-run-{}-{}-{}", ctx.task_id, ctx.step_key, ctx.attempt)
    }

    /// Task/Revision 集成基线提交(无基线 ref 时为当前 HEAD)。
    fn integration_baseline(&self, ctx: &LeaseContext) -> Result<git2::Oid> {
        let git = Git::open(&self.repo_root)?;
        let refname = Git::integration_ref(ctx.task_id, ctx.revision_id);
        if let Some(oid) = git.read_ref(&refname)? {
            return Ok(oid);
        }
        let repo = git2::Repository::open(&self.repo_root)?;
        let head = repo.head()?.peel_to_commit()?.id();
        Ok(head)
    }
}

impl ExecutionDirectoryProvider for GitWorktreeProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn isolates(&self) -> bool {
        // 非 Git 根回退共享目录(不隔离);Git 根提供独占 worktree
        self.worktrees_root.is_some()
    }

    fn acquire(&self, ctx: &LeaseContext) -> Result<ExecutionLease> {
        let Some(worktrees_root) = &self.worktrees_root else {
            // 非 Git 根:无法隔离,回退共享项目目录(并行需用户风险开关)
            return Ok(ExecutionLease {
                id: format!("shared-{}-{}", ctx.task_id, ctx.step_key),
                path: ctx.project_root.clone(),
                isolated: false,
                provider: PROVIDER_ID.into(),
                metadata: serde_json::json!({ "fallback": "project-dir" }),
            });
        };
        let name = Self::worktree_name(ctx);
        let path = worktrees_root.join(&name);
        ensure_lease_under_root(worktrees_root, &path)?;
        // 基线 = Task/Revision 集成基线(上游已汇合的修改对下游可见)
        let baseline = self.integration_baseline(ctx)?;
        if !path.exists() {
            let git = Git::open(&self.repo_root)?;
            git.worktree_create_at(&name, Some(baseline))
                .with_context(|| format!("创建隔离 worktree `{name}` 失败"))?;
        }
        Ok(ExecutionLease {
            id: format!("wt-{name}"),
            path,
            isolated: true,
            provider: PROVIDER_ID.into(),
            metadata: serde_json::json!({
                "worktree": name,
                "task_id": ctx.task_id,
                "revision_id": ctx.revision_id,
                "step_key": ctx.step_key,
                "attempt": ctx.attempt,
                "deps": ctx.deps,
                "baseline": baseline.to_string(),
            }),
        })
    }

    fn merge(&self, leases: &[ExecutionLease]) -> Result<MergeOutcome> {
        if self.worktrees_root.is_none() {
            return Ok(MergeOutcome::NotRequired);
        }
        let mut worktree_leases: Vec<ExecutionLease> = leases
            .iter()
            .filter(|l| l.isolated && l.provider == PROVIDER_ID)
            .cloned()
            .collect();
        if worktree_leases.is_empty() {
            return Ok(MergeOutcome::NotRequired);
        }
        deterministic_merge_order(&mut worktree_leases);

        let repo = git2::Repository::open(&self.repo_root)
            .with_context(|| format!("打开仓库失败: {}", self.repo_root.display()))?;
        let head_tree_id = repo.head()?.peel_to_commit()?.tree_id();
        let refname = merge_integration_ref(&worktree_leases)?;
        let git_probe = Git::open(&self.repo_root)?;
        // 当前集成 ref 树(已含兄弟节点已汇合的修改;无 ref 时为 None)
        let integrated_tree: Option<git2::Oid> = git_probe
            .read_ref(&refname)?
            .and_then(|oid| repo.find_commit(oid).ok())
            .map(|c| c.tree_id());

        // 1. 第一遍:批量预检 —— 各租约相对"各自基线"的变更文件集
        //    (不持有 Diff —— git2 的 Diff 借用其所属 Repository)。
        //    两类重叠都 → NeedsUser,此时项目目录零写入:
        //    a. 批内两个租约改了同一文件;
        //    b. 租约改的文件在"它的基线 → 当前集成 ref"之间已被其他
        //       已汇合租约修改 —— 并行兄弟先后结算时,后完成者不得
        //       静默覆盖先完成者已汇合的修改。
        let mut changed_by_lease: Vec<(String, HashSet<String>, git2::Oid)> = Vec::new();
        for lease in &worktree_leases {
            let baseline_tree = lease_baseline_tree(&repo, lease, head_tree_id);
            let files = changed_files(&repo, &lease.path, baseline_tree)?;
            let label = lease
                .metadata
                .get("step_key")
                .and_then(|v| v.as_str())
                .unwrap_or(&lease.id)
                .to_string();
            changed_by_lease.push((label, files, baseline_tree));
        }
        let mut conflicts: Vec<String> = Vec::new();
        for i in 0..changed_by_lease.len() {
            for j in (i + 1)..changed_by_lease.len() {
                let (a, files_a, _) = &changed_by_lease[i];
                let (b, files_b, _) = &changed_by_lease[j];
                for file in files_a.intersection(files_b) {
                    conflicts.push(format!("{file}(修改者: {a} 与 {b})"));
                }
            }
        }
        // 已汇合的外部变更:基线树 → 当前集成 ref 树之间的差异路径
        if let Some(current) = integrated_tree {
            for (label, files, baseline) in &changed_by_lease {
                if *baseline == current {
                    continue; // 基线即最新(本租约自身汇合后的幂等重试)
                }
                let merged_since = tree_diff_paths(&repo, *baseline, current);
                for file in files.intersection(&merged_since) {
                    conflicts.push(format!("{file}(修改者: {label} 与 已汇合的其他节点)"));
                }
            }
        }
        // c. 用户本地漂移:项目工作目录相对「应用基底树」的未提交修改
        //    (含未跟踪文件)与批内变更路径重叠 → NeedsUser,项目目录
        //    零写入,绝不覆盖用户字节;不同路径的漂移不受影响。
        let apply_base = integrated_tree
            .or_else(|| changed_by_lease.first().map(|(_, _, t)| *t))
            .unwrap_or(head_tree_id);
        let user_drift = project_dir_drift_paths(&repo, apply_base)?;
        if !user_drift.is_empty() {
            for (label, files, _) in &changed_by_lease {
                for file in files.intersection(&user_drift) {
                    conflicts.push(format!(
                        "{file}(项目工作目录存在未提交的本地修改,与 {label} 的汇合变更重叠)"
                    ));
                }
            }
        }
        if !conflicts.is_empty() {
            conflicts.sort();
            conflicts.dedup();
            return Ok(MergeOutcome::NeedsUser { conflicts });
        }

        // 2. 逐租约收集变更(路径、删除标记、源内容)。内容在触碰项目
        //    目录之前全部读出 —— 任一源文件读取失败时零写入。
        let mut changes: Vec<MergeChange> = Vec::new();
        for lease in &worktree_leases {
            let baseline_tree = lease_baseline_tree(&repo, lease, head_tree_id);
            for (path, deleted) in changed_files_with_status(&repo, &lease.path, baseline_tree)? {
                let content = if deleted {
                    None
                } else {
                    let src = lease.path.join(&path);
                    Some(
                        std::fs::read(&src)
                            .with_context(|| format!("读取合并源文件失败: {}", src.display()))?,
                    )
                };
                changes.push(MergeChange {
                    path,
                    deleted,
                    content,
                });
            }
        }

        // 3. 先在内存 index 上构建完整汇合树并验证(嵌套路径由
        //    write_tree_to 递归构造子树;TreeBuilder::insert 只接受单段
        //    路径,直接喂 `a/b/c` 会失败)。全部成功之前不推进 ref、
        //    不写项目目录。
        let base_tree = integrated_tree
            .or_else(|| changed_by_lease.first().map(|(_, _, t)| *t))
            .unwrap_or(head_tree_id);
        let tree_id = build_merged_tree(&repo, base_tree, &changes)?;

        // 4. 记录旧 ref → 原子推进集成基线(下游 acquire 从汇合结果检出)。
        let git = Git::open(&self.repo_root)?;
        let step_label = worktree_leases
            .first()
            .and_then(|l| l.metadata.get("step_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("step");
        let ref_before = git.read_ref(&refname)?;
        git.advance_integration_ref(
            &refname,
            tree_id,
            &format!(
                "mf: integrate {step_label} (+{} batch)",
                worktree_leases.len()
            ),
        )?;

        // 5. 可回滚应用:撤销日志逐项记录,任一失败 → 逆序恢复项目目录
        //    + 集成 ref 指回合并前(整体回到调用前状态)。
        if let Err(apply_err) = self.apply_changes_with_rollback(&changes) {
            if let Err(ref_err) = git.reset_integration_ref(&refname, ref_before) {
                // 双重失败是最糟路径:如实上报(目录已恢复部分 + ref 未回滚;
                // 重试合并以同一源幂等收敛)
                return Err(
                    apply_err.context(format!("合并应用失败且回滚集成 ref 失败: {ref_err:#}"))
                );
            }
            return Err(apply_err.context("合并应用失败,项目目录与集成 ref 已回滚"));
        }
        Ok(MergeOutcome::Merged)
    }

    fn release(&self, lease: &ExecutionLease) -> Result<()> {
        let Some(worktrees_root) = &self.worktrees_root else {
            return Ok(()); // 共享目录回退:无需清理
        };
        // 释放前强校验:只清理 .worktrees 下由我们命名的 worktree
        ensure_lease_under_root(worktrees_root, &lease.path)?;
        let name = lease
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .context("worktree 路径缺少目录名")?;
        anyhow::ensure!(
            name.starts_with("mf-run-"),
            "拒绝清理非 mf-run- 前缀目录: {name}"
        );
        let git = Git::open(&self.repo_root)?;
        match git.worktree_remove(name) {
            Ok(()) => Ok(()),
            Err(e) => {
                // worktree 未注册(半创建失败):只清目录
                if lease.path.exists() {
                    std::fs::remove_dir_all(&lease.path).with_context(|| {
                        format!("清理 worktree 目录失败: {}", lease.path.display())
                    })?;
                }
                log::warn!("worktree 元数据清理失败(目录已移除): {e:#}");
                Ok(())
            }
        }
    }

    fn discard_task_baselines(&self, task_id: i64) -> Result<()> {
        if self.worktrees_root.is_none() {
            return Ok(());
        }
        let git = Git::open(&self.repo_root)?;
        git.delete_integration_refs(task_id)?;
        Ok(())
    }
}

use std::path::PathBuf;

/// 租约创建时的基线树(metadata.baseline 的提交剥出树;旧租约回退主仓库 HEAD)。
/// 基线提交/树在主仓库对象库中,worktree 共享同一对象库,直接用主仓库解析。
fn lease_baseline_tree(
    repo: &git2::Repository,
    lease: &ExecutionLease,
    head_tree_id: git2::Oid,
) -> git2::Oid {
    lease
        .metadata
        .get("baseline")
        .and_then(|v| v.as_str())
        .and_then(|s| git2::Oid::from_str(s).ok())
        .and_then(|commit_oid| repo.find_commit(commit_oid).ok())
        .map(|c| c.tree_id())
        .unwrap_or(head_tree_id)
}

/// 批次对应的集成基线 ref 名:批内全部租约必须同 task+rev。
fn merge_integration_ref(leases: &[ExecutionLease]) -> Result<String> {
    let id_of = |l: &ExecutionLease| -> (i64, i64) {
        (
            l.metadata
                .get("task_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1),
            l.metadata
                .get("revision_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1),
        )
    };
    let first = id_of(&leases[0]);
    anyhow::ensure!(
        leases.iter().all(|l| id_of(l) == first),
        "汇合批次混入不同 Task/Revision 的租约"
    );
    anyhow::ensure!(
        first.0 >= 0 && first.1 >= 0,
        "租约缺少 task/revision 元数据"
    );
    Ok(Git::integration_ref(first.0, first.1))
}

/// 一次合并的原子变更单元:路径(正斜杠)、是否删除、源内容
/// (删除项为 None;内容在改动项目目录之前读出)。
struct MergeChange {
    path: String,
    deleted: bool,
    content: Option<Vec<u8>>,
}

/// 应用失败时的撤销动作(逆序执行恢复项目目录)。
enum UndoOp {
    /// 恢复被覆盖文件的旧内容。
    RestoreOverwritten { path: PathBuf, bytes: Vec<u8> },
    /// 删除本次新建的文件(新建的空目录保留,不影响内容语义)。
    RemoveCreated { path: PathBuf },
    /// 恢复被删除的文件。
    RestoreDeleted { path: PathBuf, bytes: Vec<u8> },
}

impl GitWorktreeProvider {
    /// 把变更应用到项目工作目录;每一步先记撤销项再动手,
    /// 任一失败逆序回滚已应用部分后返回错误。
    fn apply_changes_with_rollback(&self, changes: &[MergeChange]) -> Result<()> {
        let mut undo: Vec<UndoOp> = Vec::new();
        let apply = (|| -> Result<()> {
            for change in changes {
                let dst = self.repo_root.join(&change.path);
                if change.deleted {
                    if dst.is_file() {
                        let bytes = std::fs::read(&dst)
                            .with_context(|| format!("读取待删文件失败: {}", dst.display()))?;
                        std::fs::remove_file(&dst)
                            .with_context(|| format!("合并删除失败: {}", dst.display()))?;
                        undo.push(UndoOp::RestoreDeleted { path: dst, bytes });
                    }
                    continue;
                }
                if let Some(parent) = dst.parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("合并建目录失败: {}", parent.display()))?;
                    }
                }
                if dst.is_file() {
                    let bytes = std::fs::read(&dst)
                        .with_context(|| format!("读取待覆盖文件失败: {}", dst.display()))?;
                    undo.push(UndoOp::RestoreOverwritten {
                        path: dst.clone(),
                        bytes,
                    });
                } else {
                    undo.push(UndoOp::RemoveCreated { path: dst.clone() });
                }
                let content = change
                    .content
                    .as_deref()
                    .expect("非删除变更的内容已在收集阶段读出");
                std::fs::write(&dst, content)
                    .with_context(|| format!("合并写文件失败: {}", dst.display()))?;
            }
            Ok(())
        })();
        match apply {
            Ok(()) => Ok(()),
            Err(error) => {
                for op in undo.iter().rev() {
                    match op {
                        UndoOp::RestoreOverwritten { path, bytes }
                        | UndoOp::RestoreDeleted { path, bytes } => {
                            if let Some(parent) = path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = std::fs::write(path, bytes);
                        }
                        UndoOp::RemoveCreated { path } => {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }
                Err(error)
            }
        }
    }
}

/// 从基底树叠加变更,在内存 index 上构造汇合结果树:
/// `Index::add_frombuffer` 接受嵌套路径(`a/b/c.md`),
/// `write_tree_to` 递归构造子树 —— TreeBuilder::insert 只接受单段
/// 路径,不能直接用于嵌套文件。基底是「当前集成 ref 树」(已含兄弟
/// 节点已汇合的修改;预检已保证本批变更与之无重叠)。
fn build_merged_tree(
    repo: &git2::Repository,
    base_tree_id: git2::Oid,
    changes: &[MergeChange],
) -> Result<git2::Oid> {
    let base_tree = repo.find_tree(base_tree_id)?;
    // 仓库支撑的 index(add_frombuffer 要求):read_tree 重置为基底树,
    // 全程只改内存条目、绝不 write() —— 磁盘 index 不被触碰。
    let mut index = repo.index()?;
    index.read_tree(&base_tree)?;
    for change in changes {
        if change.deleted {
            // remove 对不存在的路径幂等(前序租约已删 / 批内重复删除)
            if let Err(e) = index.remove(std::path::Path::new(&change.path), 0) {
                if e.code() != git2::ErrorCode::NotFound {
                    return Err(e.into());
                }
            }
            continue;
        }
        let content = change
            .content
            .as_deref()
            .expect("非删除变更的内容已在收集阶段读出");
        let blob = repo.blob(content)?;
        // 保留基线中的可执行位(嵌套路径由 get_path 递归解析;新文件普通位)
        let mode = base_tree
            .get_path(std::path::Path::new(&change.path))
            .map(|entry| entry.filemode())
            .unwrap_or(0o100644) as u32;
        let entry = git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            file_size: content.len() as u32,
            flags: 0,
            flags_extended: 0,
            id: blob,
            path: change.path.as_bytes().to_vec(),
        };
        index.add_frombuffer(&entry, content)?;
    }
    Ok(index.write_tree_to(repo)?)
}

/// worktree 相对基线的变更:(路径, 是否删除)。
fn changed_files_with_status(
    repo: &git2::Repository,
    worktree: &std::path::Path,
    base_tree_id: git2::Oid,
) -> Result<Vec<(String, bool)>> {
    let base_tree = repo.find_tree(base_tree_id)?;
    let wt_repo = git2::Repository::open(worktree)
        .with_context(|| format!("打开 worktree 失败: {}", worktree.display()))?;
    let mut options = git2::DiffOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    let diff = wt_repo.diff_tree_to_workdir_with_index(Some(&base_tree), Some(&mut options))?;
    let mut out = Vec::new();
    for delta in diff.deltas() {
        let deleted = delta.status() == git2::Delta::Deleted;
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if !path.is_empty() {
            out.push((path, deleted));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// 两棵树之间的差异路径集(基线 → 集成 ref;兄弟节点已汇合的修改)。
fn tree_diff_paths(repo: &git2::Repository, from: git2::Oid, to: git2::Oid) -> HashSet<String> {
    let (Ok(from_tree), Ok(to_tree)) = (repo.find_tree(from), repo.find_tree(to)) else {
        return HashSet::new();
    };
    match repo.diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None) {
        Ok(diff) => diff
            .deltas()
            .filter_map(|d| {
                d.new_file()
                    .path()
                    .or_else(|| d.old_file().path())
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
            .collect(),
        Err(_) => HashSet::new(),
    }
}

/// 项目主工作目录相对基底树的未提交漂移路径集(用户本地修改):
/// 修改/删除的已跟踪文件 + 未跟踪文件(含未跟踪目录递归)。
/// 这是 merge 的应用基底(项目目录"理应"等于它 + 已应用的汇合),
/// 与批内变更路径重叠时不得覆盖 —— 由调用方判 NeedsUser。
fn project_dir_drift_paths(
    repo: &git2::Repository,
    base_tree_id: git2::Oid,
) -> Result<HashSet<String>> {
    let base_tree = repo.find_tree(base_tree_id)?;
    let mut options = git2::DiffOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    let diff = repo.diff_tree_to_workdir_with_index(Some(&base_tree), Some(&mut options))?;
    Ok(diff
        .deltas()
        .filter_map(|d| {
            d.new_file()
                .path()
                .or_else(|| d.old_file().path())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect())
}

/// worktree 相对基线的变更文件集(正斜杠规范化)。
fn changed_files(
    repo: &git2::Repository,
    worktree: &std::path::Path,
    base_tree_id: git2::Oid,
) -> Result<HashSet<String>> {
    Ok(changed_files_with_status(repo, worktree, base_tree_id)?
        .into_iter()
        .map(|(path, _)| path)
        .collect())
}

/// 确定性合并顺序:拓扑依赖序(metadata.deps 中的 step_key 先行),
/// 同层按稳定 step 键排序。依赖了未知节点视为已满足(编译器已另行校验)。
pub fn deterministic_merge_order(leases: &mut [ExecutionLease]) {
    let key_of = |lease: &ExecutionLease| -> String {
        lease
            .metadata
            .get("step_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let deps_of = |lease: &ExecutionLease| -> Vec<String> {
        lease
            .metadata
            .get("deps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| d.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let keys: HashSet<String> = leases.iter().map(|l| key_of(l)).collect();
    let mut pending: Vec<usize> = (0..leases.len()).collect();
    let mut done: HashSet<String> = HashSet::new();
    let mut order: Vec<usize> = Vec::with_capacity(leases.len());
    while !pending.is_empty() {
        // 每轮取出"依赖已满足且 step 键最小"的单个节点:
        // 拓扑序优先,同层全局按稳定键排序
        let mut candidate = None;
        for &i in &pending {
            if deps_of(&leases[i])
                .iter()
                .all(|d| done.contains(d) || !keys.contains(d))
            {
                match candidate {
                    None => candidate = Some(i),
                    Some(current) if key_of(&leases[i]) < key_of(&leases[current]) => {
                        candidate = Some(i)
                    }
                    _ => {}
                }
            }
        }
        let next = match candidate {
            Some(i) => i,
            None => {
                // 依赖环(编译器不该放行):退化为剩余键排序,保证确定性
                let mut rest = pending.clone();
                rest.sort_by_key(|i| key_of(&leases[*i]));
                order.extend(rest);
                break;
            }
        };
        done.insert(key_of(&leases[next]));
        order.push(next);
        pending.retain(|i| *i != next);
    }
    let sorted: Vec<ExecutionLease> = order.into_iter().map(|i| leases[i].clone()).collect();
    leases.clone_from_slice(&sorted);
}

/// 为合成插件清单生成 worktree 贡献条目。
pub fn contribution() -> crate::manifest::ExecutionDirectoryContribution {
    crate::manifest::ExecutionDirectoryContribution {
        id: PROVIDER_ID.into(),
        name: "Git worktree 隔离".into(),
        kind: PROVIDER_ID.into(),
        supports_parallel: true,
        isolates: true,
        description: "并行 Agent Run 各自获得独立临时 worktree,汇合时按固定顺序无冲突合并".into(),
    }
}
