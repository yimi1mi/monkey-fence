//! Git worktree Execution Directory Provider(设计 §6.3 / §9.4 / ADR 0003)。
//!
//! - 集成基线:每个 Task/Revision 维护 hidden ref
//!   `refs/mf/integration/task-<t>-rev-<r>`(初值 = 仓库 HEAD)。
//!   acquire 从基线检出 worktree;merge 把变更复制回项目目录后把基线
//!   推进到汇合结果 —— 串行下游从汇合结果建租约,天然看见上游修改。
//! - merge:按"拓扑依赖序 + 稳定节点键"排序;第一遍批量预检所有租约
//!   相对各自主基线的变更集,任一文件被多个租约修改 → `NeedsUser`,
//!   项目目录零部分写;第二遍才按序复制并推进基线。
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

        // 1. 第一遍:批量预检 —— 各租约相对"各自基线"的变更文件集
        //    (不持有 Diff —— git2 的 Diff 借用其所属 Repository)。
        //    任一重叠 → NeedsUser;此时项目目录零写入。
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
        if !conflicts.is_empty() {
            conflicts.sort();
            conflicts.dedup();
            return Ok(MergeOutcome::NeedsUser { conflicts });
        }

        // 2. 第二遍:无重叠时按确定顺序把变更复制回项目工作目录。
        //    不用 git apply:工作区 diff 里的 untracked 新文件没有 blob 内容,
        //    apply 无法重建;文件级复制对已跟踪修改与新建文件同样成立。
        let mut applied: Vec<(String, bool)> = Vec::new();
        for lease in &worktree_leases {
            let baseline_tree = lease_baseline_tree(&repo, lease, head_tree_id);
            for (path, deleted) in changed_files_with_status(&repo, &lease.path, baseline_tree)? {
                let src = lease.path.join(&path);
                let dst = self.repo_root.join(&path);
                if deleted {
                    if dst.is_file() {
                        std::fs::remove_file(&dst)
                            .with_context(|| format!("合并删除失败: {}", dst.display()))?;
                    }
                } else {
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("合并建目录失败: {}", parent.display()))?;
                    }
                    std::fs::copy(&src, &dst).with_context(|| {
                        format!("合并复制失败: {} -> {}", src.display(), dst.display())
                    })?;
                }
                applied.push((path, deleted));
            }
        }

        // 3. 推进 Task/Revision 集成基线:下游 acquire 从汇合结果检出。
        //    批内租约共享同一集成 ref(同 task+rev);从首个租约的基线树
        //    叠加全部已应用变更(预检保证无重叠,叠加序无关)。
        let refname = merge_integration_ref(&worktree_leases)?;
        let base_tree = changed_by_lease
            .first()
            .map(|(_, _, t)| *t)
            .unwrap_or(head_tree_id);
        let tree_id = build_merged_tree(&repo, self.repo_root.clone(), base_tree, &applied)?;
        let git = Git::open(&self.repo_root)?;
        let step_label = worktree_leases
            .first()
            .and_then(|l| l.metadata.get("step_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("step");
        git.advance_integration_ref(
            &refname,
            tree_id,
            &format!(
                "mf: integrate {step_label} (+{} batch)",
                worktree_leases.len()
            ),
        )?;
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

/// 从基线树叠加已应用变更,构造汇合结果树(冲突已在预检排除,叠加序无关)。
fn build_merged_tree(
    repo: &git2::Repository,
    project_root: PathBuf,
    base_tree_id: git2::Oid,
    applied: &[(String, bool)],
) -> Result<git2::Oid> {
    let base_tree = repo.find_tree(base_tree_id)?;
    let mut builder = repo.treebuilder(Some(&base_tree))?;
    for (path, deleted) in applied {
        if *deleted {
            // remove 对不存在的路径是 no-op(前序租约已删)
            let _ = builder.remove(path);
            continue;
        }
        let content = std::fs::read(project_root.join(path))
            .with_context(|| format!("读取已合并文件失败: {path}"))?;
        let blob = repo.blob(&content)?;
        // 保留基线中的可执行位(新文件按普通文件)
        let mode = base_tree
            .get_path(std::path::Path::new(path))
            .map(|e| e.filemode())
            .unwrap_or(0o100644);
        builder.insert(path, blob, mode)?;
    }
    Ok(builder.write()?)
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
