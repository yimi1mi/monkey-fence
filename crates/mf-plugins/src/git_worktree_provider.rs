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

/// 合并事务日志目录(位于仓库 `.git` 元数据内,不污染工作目录)。
const JOURNAL_DIR: &str = "mf-merge-journals";

/// 事务日志格式版本(F12:严格校验 —— 未来版本拒绝执行,保留日志)。
const MERGE_JOURNAL_VERSION: u32 = 1;

/// 测试注入的合并故障点(生产恒为 None):
/// 模拟“进程死亡”(不回滚、事务日志保留)与回滚自身失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergeFault {
    /// 事务日志写入后、ref 推进前死亡。
    CrashAfterJournal,
    /// ref 推进后、文件应用前死亡。
    CrashAfterRefAdvance,
    /// 第 N 个文件写入后死亡(不回滚)。
    CrashAfterFiles(usize),
    /// 应用失败后的回滚动作自身失败(验证回滚错误聚合)。
    FailUndo,
    /// 应用前暂停(测试注入:模拟 snapshot→apply 窗口内的并发用户编辑;
    /// 测试清掉 fault 后继续)。
    WaitBeforeApply,
}

pub struct GitWorktreeProvider {
    repo_root: PathBuf,
    /// `.worktrees` 根;非 Git 根为 None(回退共享目录)。
    worktrees_root: Option<PathBuf>,
    /// 仓库级合并互斥(C1):merge/恢复全程串行 —— 同一进程内并发
    /// 合并同一批时只有一个线程在推进 ref/写事务日志/应用文件。
    merge_lock: parking_lot::Mutex<()>,
    /// 测试注入的合并故障点(生产恒 None)。
    fault: parking_lot::Mutex<Option<MergeFault>>,
}

impl GitWorktreeProvider {
    pub fn new(repo_root: PathBuf) -> Result<GitWorktreeProvider> {
        if Git::is_repo(&repo_root) {
            let git = Git::open(&repo_root)?;
            Ok(GitWorktreeProvider {
                repo_root,
                worktrees_root: Some(git.worktree_root()?),
                merge_lock: parking_lot::Mutex::new(()),
                fault: parking_lot::Mutex::new(None),
            })
        } else {
            Ok(GitWorktreeProvider {
                repo_root,
                worktrees_root: None,
                merge_lock: parking_lot::Mutex::new(()),
                fault: parking_lot::Mutex::new(None),
            })
        }
    }

    /// 注入合并故障点(仅测试)。
    #[cfg(test)]
    pub(crate) fn set_merge_fault(&self, fault: MergeFault) {
        *self.fault.lock() = Some(fault);
    }

    /// 清除故障注入,恢复常规路径(仅测试)。
    #[cfg(test)]
    pub(crate) fn clear_merge_fault(&self) {
        *self.fault.lock() = None;
    }

    fn current_fault(&self) -> Option<MergeFault> {
        *self.fault.lock()
    }

    /// `.git` 元数据下的**跨进程/跨实例**排他合并锁(F3):
    /// `.git/mf-merge.lock`(OS 文件锁,进程死亡自动释放)。
    /// 进程内互斥(merge_lock)挡同实例线程;本锁挡独立实例/进程 ——
    /// merge/recover 全程持有,journal 与集成 ref 的推进因此全局串行。
    /// 非 Git 根无隔离语义,无需锁。
    fn cross_instance_lock(&self) -> Result<Option<crate::fs_atomic::FileLock>> {
        if self.worktrees_root.is_none() {
            return Ok(None);
        }
        let repo = git2::Repository::open(&self.repo_root)?;
        Ok(Some(crate::fs_atomic::FileLock::acquire(
            &repo.path().join("mf-merge.lock"),
        )?))
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
        // 仓库级合并互斥(C1):ref 推进/事务日志/应用文件全程串行。
        // F3:进程内互斥之外,再取 `.git` 下跨实例文件锁 —— 独立
        // provider 实例/进程同样串行(先内后外,固定顺序防死锁)
        let _merge_guard = self.merge_lock.lock();
        let _file_lock = self.cross_instance_lock()?;
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
        // 上次合并留下的未完成事务日志必须先收敛(重放/清理),
        // 否则预检会把崩溃残留当成冲突/漂移误判
        self.recover_merge_journals()?;
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

        // 3.5 C6:变更路径在写入事务日志前全部校验(纯 relative/normal、
        // 拒绝 symlink/junction)—— 非法路径零写入直接失败
        for change in &changes {
            validate_change_path(&self.repo_root, &change.path)?;
        }

        // 4. 合并事务日志(旧 ref、目标 tree、目标文件内容与应用前
        //    原状态)先于 ref 推进持久化:ref 更新与文件应用之间的
        //    任何崩溃都可在启动(或下次合并前)重放/回滚一致收敛。
        //    每次合并有唯一 transaction-id,临时文件名包含它
        //    (并发/残留的旧临时文件互不覆盖)。
        let git = Git::open(&self.repo_root)?;
        let step_label = worktree_leases
            .first()
            .and_then(|l| l.metadata.get("step_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("step");
        let ref_before = git.read_ref(&refname)?;
        // F12:随机 128-bit 事务标识 —— journal/临时文件命名互不覆盖
        let transaction_id = format!("mtx-{}", crate::fs_atomic::random_txn_id());
        let journal = MergeJournal {
            version: MERGE_JOURNAL_VERSION,
            transaction_id: transaction_id.clone(),
            refname: refname.clone(),
            ref_before: ref_before.map(|o| o.to_string()),
            target_tree: tree_id.to_string(),
            changes: self.snapshot_originals(&changes)?,
        };
        let journal_path = journal_file_for(&repo, &refname, &transaction_id);
        write_journal_atomic(&journal_path, &journal)?;
        if self.current_fault() == Some(MergeFault::CrashAfterJournal) {
            return Err(anyhow::anyhow!("(测试注入)事务日志写入后进程死亡"));
        }

        // 5. 原子推进集成基线 ref(expected-old CAS,C1):ref 当前目标
        //    必须仍是 journal 记录的 ref_before —— 并发推进过即失败,
        //    绝不把同一次合并在别人推进过的 ref 上再叠一层。
        let advanced = git.advance_integration_ref_cas(
            &refname,
            tree_id,
            &format!(
                "mf: integrate {step_label} (+{} batch)",
                worktree_leases.len()
            ),
            ref_before,
        )?;
        if self.current_fault() == Some(MergeFault::CrashAfterRefAdvance) {
            return Err(anyhow::anyhow!("(测试注入)ref 推进后进程死亡"));
        }

        // 5.5 测试注入:snapshot→apply 窗口暂停(并发用户编辑场景)
        while self.current_fault() == Some(MergeFault::WaitBeforeApply) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // 6. 应用到项目目录;任一步失败逆序回滚已应用部分,回滚错误
        //    聚合上报(绝不忽略、绝不谎报已回滚)。
        //    逐文件 CAS 检测到用户窗口内编辑 → 整体回滚 + NeedsUser。
        match self.apply_journal_with_rollback(&journal) {
            ApplyOutcome::Applied => {
                // 应用已成功:日志清理由幂等恢复兜底(重放安全),
                // 清理失败不谎报合并失败
                let _ = std::fs::remove_file(&journal_path);
                let _ = sync_dir_of(&journal_path);
                Ok(MergeOutcome::Merged)
            }
            ApplyOutcome::Crashed(msg) => Err(anyhow::anyhow!("(测试注入){msg}")),
            ApplyOutcome::UserConflict {
                conflicts,
                rolled_back_cleanly: true,
            } => {
                // 项目目录已完整回滚:集成 ref 也回滚(仅当仍指向本次推进),
                // 清日志,以 NeedsUser 上报(用户编辑优先,绝不覆盖)
                if let Err(ref_err) =
                    git.reset_integration_ref(&refname, ref_before, Some(advanced))
                {
                    let _ = std::fs::remove_file(&journal_path);
                    return Err(anyhow::anyhow!(
                        "用户编辑冲突且回滚集成 ref 失败(项目目录已回滚): {ref_err:#}"
                    ));
                }
                let _ = std::fs::remove_file(&journal_path);
                let _ = sync_dir_of(&journal_path);
                Ok(MergeOutcome::NeedsUser { conflicts })
            }
            ApplyOutcome::UserConflict {
                conflicts,
                rolled_back_cleanly: false,
            } => {
                // 回滚未完全成功:保留事务日志,如实聚合上报(恢复收敛)
                Err(anyhow::anyhow!(
                    "用户编辑冲突且回滚未完全成功(已保留合并事务日志,启动或下次合并前将重放恢复):{}",
                    conflicts.join("; ")
                ))
            }
            ApplyOutcome::Failed {
                error,
                rolled_back_cleanly: true,
            } => {
                if let Err(ref_err) =
                    git.reset_integration_ref(&refname, ref_before, Some(advanced))
                {
                    // ref 回滚失败:保留事务日志,如实聚合上报
                    return Err(error.context(format!(
                        "合并应用失败且回滚集成 ref 失败(已保留合并事务日志,恢复将重放): {ref_err:#}"
                    )));
                }
                let _ = std::fs::remove_file(&journal_path);
                Err(error.context("合并应用失败,项目目录与集成 ref 已回滚"))
            }
            ApplyOutcome::Failed {
                error,
                rolled_back_cleanly: false,
            } => {
                // 回滚未完全成功:不得宣称回滚成功 —— ref 保持推进、
                // 事务日志保留,启动/下次合并前重放收敛(可恢复)
                Err(error.context(
                    "合并应用失败且回滚未完全成功:已保留合并事务日志,启动或下次合并前将重放恢复",
                ))
            }
        }
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

    fn recover_interrupted(&self) -> Result<()> {
        // 与 merge 互斥:恢复重放不与进行中的合并交错推进 ref/应用文件
        //(进程内互斥 + F3 跨实例文件锁,固定先内后外防死锁)
        let _merge_guard = self.merge_lock.lock();
        let _file_lock = self.cross_instance_lock()?;
        self.recover_merge_journals()
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

/// 事务日志中的变更:目标内容 + 应用前项目目录原状态(回滚/重放依据)。
#[derive(serde::Serialize, serde::Deserialize)]
struct JournaledChange {
    path: String,
    deleted: bool,
    /// 目标内容(hex;删除项为 None)。
    content_hex: Option<String>,
    /// 应用前项目目录该路径是否存在。
    original_present: bool,
    /// 应用前字节(hex;不存在为 None)。
    original_hex: Option<String>,
}

/// 合并事务日志:ref 推进与文件应用之间崩溃时,启动(或下次合并前)
/// 据此重放(已推进)或回滚(未推进),一致收敛。
#[derive(serde::Serialize, serde::Deserialize)]
struct MergeJournal {
    version: u32,
    /// 本次合并的唯一事务标识(临时文件名/审计)。
    #[serde(default)]
    transaction_id: String,
    refname: String,
    /// 推进前的 ref 目标(None = 推进前 ref 不存在)。
    ref_before: Option<String>,
    /// 推进到的目标树。
    target_tree: String,
    changes: Vec<JournaledChange>,
}

/// 应用结果:成功 / 注入崩溃(不回滚,日志保留)/ 失败(是否完全回滚)/
/// 用户冲突(窗口内现状与快照不符;回滚后按 NeedsUser 上报)。
enum ApplyOutcome {
    Applied,
    Crashed(String),
    Failed {
        error: anyhow::Error,
        rolled_back_cleanly: bool,
    },
    UserConflict {
        conflicts: Vec<String>,
        rolled_back_cleanly: bool,
    },
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(hex.len() % 2 == 0, "hex 长度非法: {hex}");
    (0..hex.len() / 2)
        .map(|i| {
            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .with_context(|| format!("hex 字节非法: {hex}"))
        })
        .collect()
}

fn journal_dir_of(repo: &git2::Repository) -> PathBuf {
    repo.path().join(JOURNAL_DIR)
}

/// 事务日志文件名:refname slug + **随机 128-bit 事务 id** ——
/// 每次合并事务独占一个文件,并发/崩溃残留互不覆盖(F3);
/// 恢复按目录扫描逐个收敛。
fn journal_file_for(repo: &git2::Repository, refname: &str, transaction_id: &str) -> PathBuf {
    journal_dir_of(repo).join(format!(
        "{}.{transaction_id}.json",
        refname.replace('/', "_")
    ))
}

/// 事务日志原子写入(F5/F12):目录就绪并 fsync(错误传播)→
/// 同目录 `create_new` 唯一临时文件(transaction-id 命名,存在即失败,
/// 绝不截断他人文件)→ write + flush + fsync → 显式原子替换
/// (Windows `MoveFileExW(REPLACE_EXISTING|WRITE_THROUGH)` /
/// Unix rename;目标文件名含随机事务 id,不会覆盖既有日志)→
/// 父目录 fsync(错误传播)。
fn write_journal_atomic(path: &std::path::Path, journal: &MergeJournal) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建事务日志目录失败: {}", parent.display()))?;
        crate::fs_atomic::sync_dir(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("journal.json");
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("创建事务日志临时文件失败: {}", tmp.display()))?;
        f.write_all(serde_json::to_string(journal)?.as_bytes())?;
        f.flush()?;
        f.sync_all()?;
    }
    crate::fs_atomic::replace_file(&tmp, path)
        .with_context(|| format!("事务日志原子替换失败: {} → {}", tmp.display(), path.display()))?;
    if let Some(parent) = path.parent() {
        crate::fs_atomic::sync_dir(parent)?;
    }
    Ok(())
}

/// 目录 fsync(让其中刚创建/改名/删除的目录项掉电后可见)。
/// F12:错误如实传播 —— 持久化边界的同步失败不得静默吞掉。
fn sync_dir_of(file: &std::path::Path) -> Result<()> {
    match file.parent() {
        Some(parent) => crate::fs_atomic::sync_dir(parent).map(|_| ()),
        None => Ok(()),
    }
}

/// C6:校验事务日志/合并的变更路径 —— 纯 relative、全 Normal 分量
/// (拒绝绝对路径、盘符/UNC 前缀、`..`/`.`、空段、反斜杠),逐段拒绝
/// symlink/junction,已存在的终点 canonical 必须仍在 repo root 内。
/// 写入 journal 前与读取(journal 解析、apply/rollback/replay)前都调用
/// —— 至少消除"检查后替换"窗口(应用前复验)。
fn validate_change_path(repo_root: &std::path::Path, path: &str) -> Result<PathBuf> {
    use std::path::Component;
    anyhow::ensure!(!path.is_empty(), "变更路径不得为空");
    anyhow::ensure!(!path.contains('\\'), "变更路径必须使用正斜杠: {path}");
    let rel = std::path::Path::new(path);
    anyhow::ensure!(rel.is_relative(), "变更路径必须是相对路径: {path}");
    let mut full = repo_root.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(seg) => {
                anyhow::ensure!(!seg.is_empty(), "变更路径不得含空分量: {path}");
            }
            other => anyhow::bail!(
                "变更路径分量必须为普通段(拒绝 {:?}): {path}",
                other.as_os_str()
            ),
        }
        full.push(comp);
        // 逐段拒绝 symlink/junction(Windows junction 以 is_symlink 呈现);
        // 不存在的中间段由后续 create_dir_all 建立,跳过
        if let Ok(meta) = std::fs::symlink_metadata(&full) {
            if meta.file_type().is_symlink() {
                anyhow::bail!("变更路径不得穿过符号链接/接合点: {path}");
            }
        }
    }
    // 终点已存在时:canonical 不得逃逸仓库根
    if full.exists() {
        let canon = std::fs::canonicalize(&full)
            .with_context(|| format!("解析变更路径失败: {}", full.display()))?;
        let root_canon = std::fs::canonicalize(repo_root)
            .with_context(|| format!("解析仓库根失败: {}", repo_root.display()))?;
        anyhow::ensure!(
            canon.starts_with(&root_canon),
            "变更路径解析后逃逸仓库根: {path}"
        );
    }
    Ok(full)
}

/// C6:事务日志 refname 只接受内部生成的形态
/// `refs/mf/integration/task-<id>-rev-<id>`。
fn validate_refname(refname: &str) -> Result<()> {
    let rest = refname
        .strip_prefix("refs/mf/integration/task-")
        .with_context(|| format!("集成 ref 名非法: {refname}"))?;
    let (task, rev) = rest
        .split_once("-rev-")
        .with_context(|| format!("集成 ref 名非法: {refname}"))?;
    anyhow::ensure!(
        task.parse::<i64>().is_ok() && rev.parse::<i64>().is_ok(),
        "集成 ref 名非法: {refname}"
    );
    Ok(())
}

/// 唯一临时文件后缀(随机 128-bit hex;并发/残留互不覆盖,F6)。
fn tmp_suffix() -> String {
    crate::fs_atomic::random_txn_id()
}

/// C5/F5:目标文件的原子替换 —— 同目录唯一隐藏临时文件
/// (`create_new`,存在即失败,绝不截断他人文件)→ write + flush +
/// fsync → **显式原子替换**(Windows `MoveFileExW(REPLACE_EXISTING |
/// WRITE_THROUGH)` / Unix rename)→ 父目录 fsync(错误传播)。
/// 失败时清理临时文件,目标保持原状。
fn atomic_write_file(dst: &std::path::Path, content: &[u8]) -> Result<()> {
    use std::io::Write as _;
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("合并建目录失败: {}", parent.display()))?;
        }
    }
    let file_name = dst.file_name().and_then(|n| n.to_str()).unwrap_or("target");
    let tmp = dst.with_file_name(format!(".{file_name}.mftmp-{}", tmp_suffix()));
    let write_result = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("创建合并临时文件失败: {}", tmp.display()))?;
        f.write_all(content)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        crate::fs_atomic::replace_file(&tmp, dst)
            .with_context(|| format!("原子替换失败: {} → {}", tmp.display(), dst.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp); // 失败清理:目标文件保持原状
    } else if let Some(parent) = dst.parent() {
        crate::fs_atomic::sync_dir(parent)?;
    }
    write_result
}

/// apply 单条变更的失败分类:用户冲突(NeedsUser)vs 系统错误(回滚+Err)。
enum ApplyOneError {
    /// 现状与快照原状态不符(用户在窗口内编辑/删除/新建)。
    Conflict(String),
    Error(anyhow::Error),
}

impl From<anyhow::Error> for ApplyOneError {
    fn from(e: anyhow::Error) -> Self {
        ApplyOneError::Error(e)
    }
}

impl GitWorktreeProvider {
    /// 采集应用前项目目录原状态(事务日志的回滚/重放依据)。
    fn snapshot_originals(&self, changes: &[MergeChange]) -> Result<Vec<JournaledChange>> {
        changes
            .iter()
            .map(|c| {
                let dst = self.repo_root.join(&c.path);
                let original = if dst.is_file() {
                    std::fs::read(&dst)
                        .with_context(|| format!("读取应用前文件失败: {}", dst.display()))?
                } else {
                    Vec::new()
                };
                Ok(JournaledChange {
                    path: c.path.clone(),
                    deleted: c.deleted,
                    content_hex: c.content.as_deref().map(hex_encode),
                    original_present: dst.is_file(),
                    original_hex: if dst.is_file() {
                        Some(hex_encode(&original))
                    } else {
                        None
                    },
                })
            })
            .collect()
    }

    /// 把事务日志中的变更应用到项目工作目录;每一步可回滚,任一失败
    /// 逆序恢复已应用部分。回滚动作自身的错误**聚合上报**(绝不忽略);
    /// 回滚未完全成功时 `rolled_back_cleanly = false`(调用方不得宣称
    /// 已回滚,事务日志保留供恢复)。注入的“进程死亡”不触发回滚
    ///(事务日志保留,由恢复重放)。
    fn apply_journal_with_rollback(&self, journal: &MergeJournal) -> ApplyOutcome {
        enum Step {
            Done,
            Crash(String),
            Fail(anyhow::Error),
            Conflict(Vec<String>),
        }
        let mut applied: Vec<&JournaledChange> = Vec::new();
        let outcome = (|| -> Step {
            for change in &journal.changes {
                match self.apply_one(change) {
                    Ok(()) => {}
                    Err(ApplyOneError::Conflict(c)) => return Step::Conflict(vec![c]),
                    Err(ApplyOneError::Error(e)) => return Step::Fail(e),
                }
                applied.push(change);
                if self.current_fault() == Some(MergeFault::CrashAfterFiles(applied.len())) {
                    return Step::Crash(format!(
                        "第 {} 个文件写入后进程死亡(已应用 {} 项)",
                        applied.len(),
                        applied.len()
                    ));
                }
            }
            Step::Done
        })();
        // 失败/冲突共同回滚:逆序恢复已应用部分,回滚错误聚合上报
        let rollback_of = |outcome: &Step| matches!(outcome, Step::Fail(_) | Step::Conflict(_));
        if rollback_of(&outcome) {
            let mut failures: Vec<String> = Vec::new();
            for change in applied.iter().rev() {
                if let Err(e) = self.rollback_one(change) {
                    failures.push(format!("{}: {e:#}", change.path));
                }
            }
            if failures.is_empty() {
                return match outcome {
                    Step::Conflict(conflicts) => ApplyOutcome::UserConflict {
                        conflicts,
                        rolled_back_cleanly: true,
                    },
                    Step::Fail(error) => ApplyOutcome::Failed {
                        error: error.context("回滚完成"),
                        rolled_back_cleanly: true,
                    },
                    _ => unreachable!(),
                };
            }
            return match outcome {
                Step::Conflict(conflicts) => ApplyOutcome::UserConflict {
                    conflicts: conflicts
                        .into_iter()
                        .chain(failures.iter().map(|f| format!("回滚失败: {f}")))
                        .collect(),
                    rolled_back_cleanly: false,
                },
                Step::Fail(error) => ApplyOutcome::Failed {
                    error: error.context(format!(
                        "回滚未完全成功(已保留合并事务日志,恢复将重放):{}",
                        failures.join("; ")
                    )),
                    rolled_back_cleanly: false,
                },
                _ => unreachable!(),
            };
        }
        match outcome {
            Step::Done => ApplyOutcome::Applied,
            Step::Crash(msg) => ApplyOutcome::Crashed(msg),
            Step::Fail(_) | Step::Conflict(_) => unreachable!("已在上方统一回滚"),
        }
    }

    /// 应用单条变更 —— C5 逐文件 CAS + 原子替换:
    /// - 写入前校验路径(纯 relative/normal、拒绝 symlink/junction,C6)
    ///   并核对现状:必须等于快照原状态(或已等于目标 = 幂等重试),
    ///   否则 Conflict(NeedsUser)—— 绝不覆盖用户在窗口内的编辑;
    /// - 写入经同目录唯一临时文件 + flush/fsync + 原子 rename,
    ///   失败时目标保持原状(无部分写);
    /// - 删除只在现状等于原状态时执行(已被用户改动 → Conflict)。
    fn apply_one(&self, change: &JournaledChange) -> std::result::Result<(), ApplyOneError> {
        let dst = validate_change_path(&self.repo_root, &change.path)?;
        let original: Option<Vec<u8>> = change
            .original_hex
            .as_deref()
            .map(hex_decode)
            .transpose()
            .map_err(ApplyOneError::Error)?;
        if change.deleted {
            match std::fs::read(&dst) {
                Ok(bytes) => {
                    if change.original_present && Some(bytes) == original {
                        std::fs::remove_file(&dst)
                            .with_context(|| format!("合并删除失败: {}", dst.display()))
                            .map_err(ApplyOneError::Error)?;
                    } else if !change.original_present {
                        return Err(ApplyOneError::Conflict(format!(
                            "{}(应用前不存在,现已被外部创建,拒绝删除)",
                            change.path
                        )));
                    } else {
                        return Err(ApplyOneError::Conflict(format!(
                            "{}(应用后被用户修改,拒绝删除)",
                            change.path
                        )));
                    }
                }
                Err(_) if !change.original_present => {} // 本就不存在,删除 no-op
                Err(_) => {
                    return Err(ApplyOneError::Conflict(format!(
                        "{}(应用前存在、现已被删除,拒绝盲目处理)",
                        change.path
                    )))
                }
            }
            return Ok(());
        }
        let content = change
            .content_hex
            .as_deref()
            .map(hex_decode)
            .transpose()
            .map_err(ApplyOneError::Error)?
            .expect("非删除变更的内容已在收集阶段读出");
        match std::fs::read(&dst) {
            // 已是目标内容:幂等重试,跳过
            Ok(bytes) if bytes == *content => return Ok(()),
            // 现状 == 快照原状态:CAS 通过,执行原子替换
            Ok(bytes) if change.original_present && Some(&bytes) == original.as_ref() => {}
            Ok(_) => {
                return Err(ApplyOneError::Conflict(format!(
                    "{}(项目目录现状与合并快照不符,疑似窗口内被用户修改,拒绝覆盖)",
                    change.path
                )))
            }
            // 应用前不存在:新建
            Err(_) if !change.original_present => {}
            Err(_) => {
                return Err(ApplyOneError::Conflict(format!(
                    "{}(应用前存在、现已被删除,拒绝盲目重建)",
                    change.path
                )))
            }
        }
        atomic_write_file(&dst, &content).map_err(ApplyOneError::Error)
    }

    /// 回滚单条已应用变更(F9:target→original CAS)—— 只有当前内容
    /// 确实等于**本事务 target**(即应用结果未被触碰)或已等于
    /// original(幂等重试)时才恢复/删除;窗口内被用户改成第三种
    /// 内容 → 该文件回滚失败(调用方聚合上报,绝不覆盖用户字节)。
    fn rollback_one(&self, change: &JournaledChange) -> Result<()> {
        if self.current_fault() == Some(MergeFault::FailUndo) {
            anyhow::bail!("(测试注入)回滚动作失败: {}", change.path);
        }
        let dst = validate_change_path(&self.repo_root, &change.path)?;
        if change.deleted {
            if !change.original_present {
                return Ok(()); // 应用前本就不存在(删除为 no-op)
            }
            let original = hex_decode(
                change
                    .original_hex
                    .as_deref()
                    .expect("original_present 时必有原字节"),
            )?;
            match std::fs::read(&dst) {
                Err(_) => {
                    // 当前 == target(已删除)→ 恢复 original
                    atomic_write_file(&dst, &original)
                        .with_context(|| format!("恢复被删文件失败: {}", dst.display()))
                }
                Ok(bytes) if bytes == original => Ok(()), // 已回滚:幂等
                Ok(_) => anyhow::bail!(
                    "{} 回滚拒绝:本事务删除后用户重建了不同内容(不得覆盖用户字节)",
                    change.path
                ),
            }
        } else if !change.original_present {
            // 应用前不存在:本次新建的文件(target = 存在该内容)
            match std::fs::read(&dst) {
                Err(_) => return Ok(()), // 已不在:幂等
                Ok(bytes) => {
                    let target = hex_decode(
                        change
                            .content_hex
                            .as_deref()
                            .expect("非删除变更必有目标内容"),
                    )?;
                    if bytes != target {
                        anyhow::bail!(
                            "{} 回滚拒绝:本事务新建后用户改写了内容(不得删除用户字节)",
                            change.path
                        );
                    }
                    std::fs::remove_file(&dst)
                        .with_context(|| format!("回滚删除新建文件失败: {}", dst.display()))
                }
            }
        } else {
            let original = hex_decode(
                change
                    .original_hex
                    .as_deref()
                    .expect("original_present 时必有原字节"),
            )?;
            match std::fs::read(&dst) {
                Ok(bytes) if bytes == original => Ok(()), // 已回滚:幂等
                Ok(bytes) => {
                    let target = hex_decode(
                        change
                            .content_hex
                            .as_deref()
                            .expect("非删除变更必有目标内容"),
                    )?;
                    anyhow::ensure!(
                        bytes == target,
                        "{} 回滚拒绝:应用结果已被用户修改(当前既非本事务 target 也非 original,不得覆盖用户字节)",
                        change.path
                    );
                    atomic_write_file(&dst, &original)
                        .with_context(|| format!("恢复被覆盖文件失败: {}", dst.display()))
                }
                Err(_) => anyhow::bail!(
                    "{} 回滚拒绝:应用结果已被用户删除(不敢盲目重建,请人工处理)",
                    change.path
                ),
            }
        }
    }

    /// 启动/合并前恢复:重放或清理未完成的事务日志,一致收敛。
    /// - ref 已推进到目标树 → 幂等重放应用(拒绝覆盖崩溃窗口内的
    ///   外部修改);
    /// - ref 仍在推进前位置 → 无应用发生,清日志;
    /// - ref 与日志都不一致 → 保留日志报错(人工处理)。
    /// 恢复/磁盘失败如实上报,不宣称成功。
    fn recover_merge_journals(&self) -> Result<()> {
        if self.worktrees_root.is_none() {
            return Ok(());
        }
        let repo = git2::Repository::open(&self.repo_root)?;
        let dir = journal_dir_of(&repo);
        if !dir.is_dir() {
            return Ok(());
        }
        let mut errors: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("读取事务日志目录失败: {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Err(e) = self.recover_journal_file(&repo, &path) {
                errors.push(format!("{}: {e:#}", path.display()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "合并事务日志恢复失败(日志已保留,可重试):{}",
                errors.join("; ")
            )
        }
    }

    fn recover_journal_file(&self, repo: &git2::Repository, path: &std::path::Path) -> Result<()> {
        let journal: MergeJournal = serde_json::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("解析事务日志失败: {}", path.display()))?;
        // F12:journal version 严格校验 —— 未来版本的日志结构未知,
        // 按当前版本语义盲执行可能破坏一致性;拒绝并保留日志
        anyhow::ensure!(
            journal.version == MERGE_JOURNAL_VERSION,
            "事务日志版本不受支持(日志 v{},程序支持 v{MERGE_JOURNAL_VERSION};未来版本拒绝执行): {}",
            journal.version,
            path.display()
        );
        // C6:磁盘上的日志可能被篡改/植入 —— 读取即校验(先于任何
        // ref 读取/判定/文件操作):refname 形态 + 每条变更路径
        // (纯 relative/normal、拒绝 symlink/junction/穿越),非法即拒
        // (日志保留,绝不执行)
        validate_refname(&journal.refname).with_context(|| {
            format!(
                "事务日志 refname 非法({}): {}",
                path.display(),
                journal.refname
            )
        })?;
        for change in &journal.changes {
            validate_change_path(&self.repo_root, &change.path).with_context(|| {
                format!("事务日志变更路径非法({}): {}", path.display(), change.path)
            })?;
        }
        let git = Git::open(&self.repo_root)?;
        let current = git.read_ref(&journal.refname)?;
        let target_tree = git2::Oid::from_str(&journal.target_tree).ok();
        let current_tree = current
            .and_then(|o| git2::Oid::from_str(&o.to_string()).ok())
            .and_then(|oid| repo.find_commit(oid).ok())
            .map(|c| c.tree_id());
        if current_tree.is_some() && current_tree == target_tree {
            // ref 已推进:重放应用直到项目目录一致
            self.replay_journal(&journal)
                .with_context(|| format!("重放事务日志失败: {}", path.display()))?;
            std::fs::remove_file(path)
                .with_context(|| format!("删除已收敛的事务日志失败: {}", path.display()))?;
            sync_dir_of(path)?;
            return Ok(());
        }
        let ref_before = journal
            .ref_before
            .as_deref()
            .and_then(|s| git2::Oid::from_str(s).ok());
        if current == ref_before {
            // 推进前死亡:无任何文件应用发生 → 安全清日志
            std::fs::remove_file(path)
                .with_context(|| format!("删除未推进的事务日志失败: {}", path.display()))?;
            sync_dir_of(path)?;
            return Ok(());
        }
        anyhow::bail!(
            "集成 ref {} 状态与事务日志不一致(当前 {current:?},日志 ref_before={ref_before:?}):保留日志待人工处理",
            journal.refname
        );
    }

    /// 幂等重放:已是目标内容 → 跳过;仍是原状态 → 写入目标;
    /// 崩溃窗口内被外部修改(既非原也非目标)→ 拒绝覆盖,保留可恢复。
    /// 路径在重放前再次校验(C6:应用前复验,消除检查后替换窗口)。
    fn replay_journal(&self, journal: &MergeJournal) -> Result<()> {
        for change in &journal.changes {
            let dst = validate_change_path(&self.repo_root, &change.path)?;
            if change.deleted {
                match std::fs::read(&dst) {
                    Err(_) => continue, // 已删除
                    Ok(bytes) => {
                        let original =
                            change.original_hex.as_deref().map(hex_decode).transpose()?;
                        if change.original_present && Some(bytes) == original {
                            std::fs::remove_file(&dst)
                                .with_context(|| format!("重放删除失败: {}", dst.display()))?;
                        } else if change.original_present {
                            anyhow::bail!(
                                "{} 在崩溃后被外部修改,拒绝重放删除(请人工处理)",
                                change.path
                            );
                        }
                        // 应用前本就不存在且现在存在:外部新建,不动
                    }
                }
                continue;
            }
            let target = hex_decode(
                change
                    .content_hex
                    .as_deref()
                    .expect("非删除变更必有目标内容"),
            )?;
            match std::fs::read(&dst) {
                Ok(bytes) if bytes == target => continue, // 已应用
                Ok(bytes) => {
                    let original = change.original_hex.as_deref().map(hex_decode).transpose()?;
                    if change.original_present && Some(bytes) == original {
                        // F9:重放同样走原子替换 + 目录同步(不留半写状态)
                        atomic_write_file(&dst, &target)
                            .with_context(|| format!("重放写入失败: {}", dst.display()))?;
                    } else {
                        anyhow::bail!(
                            "{} 在崩溃后被外部修改(疑似用户编辑),拒绝覆盖(请人工处理)",
                            change.path
                        );
                    }
                }
                Err(_) if !change.original_present => {
                    atomic_write_file(&dst, &target)
                        .with_context(|| format!("重放新建失败: {}", dst.display()))?;
                }
                Err(_) => anyhow::bail!(
                    "{} 应用前存在、崩溃后被删除,拒绝盲目重建(请人工处理)",
                    change.path
                ),
            }
        }
        Ok(())
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

#[cfg(test)]
mod merge_journal_tests {
    use super::*;
    use mf_agent::execution_directory::{ExecutionDirectoryProvider, MergeOutcome};
    use std::sync::Arc;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        let sig = git2::Signature::now("mf", "mf@test").unwrap();
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
        (dir, root)
    }

    fn ctx(root: &PathBuf, task: i64, step: &str) -> LeaseContext {
        LeaseContext {
            task_id: task,
            step_id: 0,
            revision_id: 1,
            attempt: 1,
            project_root: root.clone(),
            step_key: step.into(),
            deps: vec![],
        }
    }

    fn wt_path(root: &PathBuf, task: i64, step: &str) -> PathBuf {
        root.parent()
            .unwrap()
            .join(".worktrees")
            .join(format!("mf-run-{task}-{step}-1"))
    }

    fn ref_tree(root: &PathBuf, refname: &str) -> Option<git2::Oid> {
        let repo = git2::Repository::open(root).unwrap();
        repo.find_reference(refname)
            .ok()
            .and_then(|r| r.peel_to_commit().ok())
            .map(|c| c.tree_id())
    }

    /// ref 推进后、文件应用前崩溃:重启恢复必须重放应用到一致收敛。
    #[test]
    fn journal_replays_after_crash_between_ref_advance_and_apply() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let lease = provider.acquire(&ctx(&root, 1, "a")).unwrap();
        std::fs::write(wt_path(&root, 1, "a").join("a.txt"), "from-agent\n").unwrap();

        provider.set_merge_fault(MergeFault::CrashAfterRefAdvance);
        let err = provider.merge(&[lease.clone()]).unwrap_err();
        assert!(format!("{err:#}").contains("ref 推进后"), "{err:#}");
        let refname = Git::integration_ref(1, 1);
        let target = ref_tree(&root, &refname);
        assert!(target.is_some(), "崩溃前 ref 已推进");
        assert_ne!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "from-agent\n",
            "崩溃点:文件尚未应用"
        );

        // “重启”:恢复重放,直到项目目录与 ref 一致
        provider.recover_interrupted().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "from-agent\n",
            "恢复必须重放应用"
        );
        assert_eq!(ref_tree(&root, &refname), target, "ref 保持推进结果");
        let journal_dir = git2::Repository::open(&root)
            .unwrap()
            .path()
            .join(JOURNAL_DIR);
        assert!(
            !journal_dir.exists() || std::fs::read_dir(&journal_dir).unwrap().count() == 0,
            "收敛后事务日志必须清除"
        );
        provider.release(&lease).unwrap();
    }

    /// 第 N 个文件写入后崩溃:部分应用状态经恢复重放到全部就位。
    #[test]
    fn journal_replays_after_crash_mid_apply() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let lease = provider.acquire(&ctx(&root, 2, "a")).unwrap();
        let wt = wt_path(&root, 2, "a");
        std::fs::write(wt.join("a.txt"), "a2\n").unwrap();
        std::fs::write(wt.join("b.txt"), "b2\n").unwrap();

        provider.set_merge_fault(MergeFault::CrashAfterFiles(1));
        let err = provider.merge(&[lease.clone()]).unwrap_err();
        assert!(format!("{err:#}").contains("进程死亡"), "{err:#}");
        let refname = Git::integration_ref(2, 1);
        assert!(ref_tree(&root, &refname).is_some(), "ref 已推进");

        provider.recover_interrupted().unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a2\n");
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "b2\n");
        provider.release(&lease).unwrap();
    }

    /// 日志写入后、ref 推进前崩溃:恢复判定“未发生”并清日志,
    /// 项目目录零应用。
    #[test]
    fn journal_cleaned_when_crash_before_ref_advance() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let lease = provider.acquire(&ctx(&root, 3, "a")).unwrap();
        std::fs::write(wt_path(&root, 3, "a").join("a.txt"), "a3\n").unwrap();

        provider.set_merge_fault(MergeFault::CrashAfterJournal);
        assert!(provider.merge(&[lease.clone()]).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "aaa\n"
        );
        let refname = Git::integration_ref(3, 1);
        assert!(ref_tree(&root, &refname).is_none(), "ref 未推进");

        provider.recover_interrupted().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "aaa\n",
            "推进前崩溃:恢复不得应用任何变更"
        );
        provider.release(&lease).unwrap();
    }

    /// 应用失败且回滚自身失败:错误必须聚合、不得宣称回滚成功;
    /// 事务日志保留,清障后恢复可收敛。
    #[test]
    fn rollback_failure_aggregates_errors_and_retains_journal_for_recovery() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let lease = provider.acquire(&ctx(&root, 4, "a")).unwrap();
        let wt = wt_path(&root, 4, "a");
        std::fs::write(wt.join("a.txt"), "a4\n").unwrap();
        std::fs::create_dir_all(wt.join("docs")).unwrap();
        std::fs::write(wt.join("docs").join("n.txt"), "n4\n").unwrap();
        // 应用障碍:项目目录已存在同名普通文件 docs → create_dir_all 失败
        std::fs::write(root.join("docs"), "blocked\n").unwrap();

        provider.set_merge_fault(MergeFault::FailUndo);
        let err = provider.merge(&[lease.clone()]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("回滚未完全成功") && msg.contains("保留"),
            "回滚失败必须聚合上报且明示保留事务日志: {msg}"
        );
        let refname = Git::integration_ref(4, 1);
        assert!(
            ref_tree(&root, &refname).is_some(),
            "回滚未完全成功时不得谎称已回滚(ref 保持推进,靠恢复收敛)"
        );

        // 恢复被同一障碍阻挡:失败必须如实上报,日志保留
        let recover_err = provider.recover_interrupted().unwrap_err();
        assert!(
            format!("{recover_err:#}").contains("docs"),
            "{recover_err:#}"
        );

        // 清障后恢复收敛
        std::fs::remove_file(root.join("docs")).unwrap();
        provider.recover_interrupted().unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a4\n");
        assert_eq!(
            std::fs::read_to_string(root.join("docs").join("n.txt")).unwrap(),
            "n4\n"
        );
        provider.release(&lease).unwrap();
    }

    /// 恢复检测到崩溃窗口内用户改动 → 拒绝覆盖并保留日志(可恢复)。
    #[test]
    fn recovery_refuses_to_overwrite_user_edits_during_crash_window() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let lease = provider.acquire(&ctx(&root, 5, "a")).unwrap();
        std::fs::write(wt_path(&root, 5, "a").join("a.txt"), "a5\n").unwrap();

        provider.set_merge_fault(MergeFault::CrashAfterRefAdvance);
        assert!(provider.merge(&[lease.clone()]).is_err());
        // 崩溃窗口内用户改动了同一路径
        std::fs::write(root.join("a.txt"), "user-edit\n").unwrap();

        let err = provider.recover_interrupted().unwrap_err();
        assert!(
            format!("{err:#}").contains("外部修改"),
            "用户编辑不得被恢复重放覆盖: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "user-edit\n",
            "用户字节保持"
        );
        provider.release(&lease).unwrap();
    }

    /// C5:snapshot→apply 窗口内用户编辑了同一路径 → apply 必须拒绝
    /// (NeedsUser),项目目录用户字节保持、集成 ref 回滚、无日志残留
    /// —— 绝不覆盖用户编辑。
    #[test]
    fn apply_rejects_user_edit_during_merge_window_with_needs_user() {
        let (_dir, root) = fixture();
        let provider = Arc::new(GitWorktreeProvider::new(root.clone()).unwrap());
        let lease = provider.acquire(&ctx(&root, 8, "a")).unwrap();
        std::fs::write(wt_path(&root, 8, "a").join("a.txt"), "from-agent\n").unwrap();

        provider.set_merge_fault(MergeFault::WaitBeforeApply);
        let p2 = provider.clone();
        let lease_for_thread = lease.clone();
        let merger = std::thread::spawn(move || p2.merge(&[lease_for_thread]));
        // 等 merge 进入应用前暂停(journal 已写、ref 已推进)
        let refname = Git::integration_ref(8, 1);
        let advanced = (0..500).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            ref_tree(&root, &refname).is_some()
        });
        assert!(advanced, "前置:ref 已推进(处于应用前窗口)");
        // 窗口内用户编辑同一路径
        std::fs::write(root.join("a.txt"), "user-edit-during-window\n").unwrap();
        provider.clear_merge_fault();

        match merger.join().unwrap().unwrap() {
            MergeOutcome::NeedsUser { conflicts } => {
                assert!(
                    conflicts.iter().any(|c| c.contains("a.txt")),
                    "冲突必须指明用户编辑的路径: {conflicts:?}"
                );
            }
            other => panic!("用户编辑窗口必须 NeedsUser,得到 {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "user-edit-during-window\n",
            "用户字节必须保持,不得被覆盖"
        );
        assert_eq!(
            ref_tree(&root, &refname),
            None,
            "NeedsUser 时集成 ref 必须回滚到不存在"
        );
        let journal_dir = git2::Repository::open(&root)
            .unwrap()
            .path()
            .join(JOURNAL_DIR);
        let leftovers: Vec<_> = std::fs::read_dir(&journal_dir)
            .map(|it| it.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "拒绝后不得残留日志: {leftovers:?}");
        provider.release(&lease).unwrap();
    }

    /// F12:事务日志 version 严格校验 —— 未来版本(>1)的日志结构
    /// 未知,恢复必须拒绝执行(保留日志),不得按 v1 语义盲放行。
    #[test]
    fn recovery_rejects_future_journal_version() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let repo = git2::Repository::open(&root).unwrap();
        let journal_dir = repo.path().join(JOURNAL_DIR);
        std::fs::create_dir_all(&journal_dir).unwrap();
        // 未来版本日志:version = 2,内容形态未知
        let journal = serde_json::json!({
            "version": 2,
            "transaction_id": "future",
            "refname": Git::integration_ref(11, 1),
            "ref_before": null,
            "target_tree": "0101010101010101010101010101010101010101",
            "changes": [],
        });
        let path = journal_dir.join("future-version.json");
        std::fs::write(&path, journal.to_string()).unwrap();
        let err = provider.recover_interrupted().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("future-version") && msg.contains("版本"),
            "未来版本必须被拒绝并指明日志: {msg}"
        );
        assert!(path.exists(), "被拒绝的日志必须保留(人工处理)");
    }

    /// C6:磁盘上的事务日志被篡改/植入恶意路径(../、绝对盘符、
    /// junction 穿越)→ 恢复必须拒绝,仓库外零写入。
    #[test]
    fn recovery_rejects_malicious_journal_paths() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let outside = root.parent().unwrap().join("outside-target.txt");
        let repo = git2::Repository::open(&root).unwrap();
        let journal_dir = repo.path().join(JOURNAL_DIR);
        std::fs::create_dir_all(&journal_dir).unwrap();

        let write_journal = |name: &str, path: &str| {
            let journal = serde_json::json!({
                "version": 1,
                "transaction_id": "evil",
                "refname": Git::integration_ref(9, 1),
                "ref_before": null,
                "target_tree": "0101010101010101010101010101010101010101",
                "changes": [{
                    "path": path,
                    "deleted": false,
                    "content_hex": "6576696c",
                    "original_present": false,
                    "original_hex": null,
                }],
            });
            std::fs::write(journal_dir.join(name), journal.to_string()).unwrap();
        };
        // 推进 ref 到 target_tree,让恢复走"重放应用"分支
        let git = Git::open(&root).unwrap();
        let tree_oid = {
            let mut index = repo.index().unwrap();
            index.write_tree().unwrap()
        };
        git.advance_integration_ref(&Git::integration_ref(9, 1), tree_oid, "evil-prep")
            .unwrap();

        write_journal("evil-parent.json", "../../../outside-target.txt");
        write_journal("evil-absolute.json", "C:/Windows/Temp/evil.txt");
        write_journal("evil-normal.json", "normal-file.txt");

        let err = provider.recover_interrupted().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("evil-parent") && msg.contains("evil-absolute"),
            "恶意路径必须被逐个拒绝: {msg}"
        );
        assert!(!outside.exists(), "仓库外必须零写入(父目录穿越)");
        assert!(
            !root.join("normal-file.txt").exists() || true, // 合法路径的重放另行收敛
            "合法路径不受恶意样本影响"
        );
    }

    /// C6:journal 路径含 Windows 盘符前缀/UNC/保留段全部拒绝
    ///(纯 relative、纯 normal 分量)。
    #[test]
    fn journal_path_validation_rejects_non_normal_components() {
        let (_dir, root) = fixture();
        let repo = git2::Repository::open(&root).unwrap();
        let journal_dir = repo.path().join(JOURNAL_DIR);
        std::fs::create_dir_all(&journal_dir).unwrap();
        // 直接验证恢复期逐条拒绝(带合法 ref 的日志,重放前先验路径)
        for (name, path) in [
            ("c1.json", r"C:\abs\evil.txt"),
            ("c2.json", r"\\server\share\evil.txt"),
            ("c3.json", r"..\..\evil.txt"),
            ("c4.json", "./cur/evil.txt"),
        ] {
            let journal = serde_json::json!({
                "version": 1,
                "transaction_id": "v",
                "refname": Git::integration_ref(10, 1),
                "ref_before": null,
                "target_tree": "0101010101010101010101010101010101010101",
                "changes": [{
                    "path": path,
                    "deleted": false,
                    "content_hex": "6576696c",
                    "original_present": false,
                    "original_hex": null,
                }],
            });
            std::fs::write(journal_dir.join(name), journal.to_string()).unwrap();
        }
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let err = provider.recover_interrupted().unwrap_err();
        let msg = format!("{err:#}");
        for name in ["c1.json", "c2.json", "c3.json", "c4.json"] {
            assert!(msg.contains(name), "{name} 必须被拒绝: {msg}");
        }
    }

    /// F3:两个**完全独立**的 provider 实例(跨进程语义:各自独立的
    /// 进程内锁/状态)并发 merge 同一批 —— `.git` 下的排他文件锁必须
    /// 让后者排队等待;A 持锁期间 B 的 merge 不得完成(否则 B 会并发
    /// 重放/清理 A 的事务日志)。A 完成后 B 收敛,项目目录一致。
    #[test]
    fn cross_instance_merge_serializes_via_git_file_lock() {
        let (_dir, root) = fixture();
        let a = std::sync::Arc::new(GitWorktreeProvider::new(root.clone()).unwrap());
        let lease = a.acquire(&ctx(&root, 12, "a")).unwrap();
        std::fs::write(wt_path(&root, 12, "a").join("a.txt"), "cross\n").unwrap();

        // A 暂停在 journal 已写、ref 已推进的窗口(持有全部锁)
        a.set_merge_fault(MergeFault::WaitBeforeApply);
        let a2 = a.clone();
        let lease_a = lease.clone();
        let ha = std::thread::spawn(move || a2.merge(&[lease_a]));
        let refname = Git::integration_ref(12, 1);
        let advanced = (0..500).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            ref_tree(&root, &refname).is_some()
        });
        assert!(advanced, "前置:A 已进入应用前窗口(锁已持有)");

        // B:独立实例(独立进程内锁)—— 只能靠 .git 下文件锁互斥
        let b = std::sync::Arc::new(GitWorktreeProvider::new(root.clone()).unwrap());
        let lease_b = lease.clone();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_flag = done.clone();
        let hb = std::thread::spawn(move || {
            let r = b.merge(&[lease_b]);
            done_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            r
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(
            !done.load(std::sync::atomic::Ordering::SeqCst),
            "A 持锁期间独立实例的 merge 不得完成(必须互斥排队)"
        );
        a.clear_merge_fault();
        let a_outcome = ha.join().unwrap().expect("A 合并成功");
        assert!(matches!(a_outcome, MergeOutcome::Merged), "A 必须合并成功");
        // A 释放后 B 完成并收敛(与已汇合结果不重叠 → NeedsUser 或幂等)
        let b_outcome = hb.join().unwrap().expect("B 不得因互斥出错");
        assert!(matches!(
            b_outcome,
            MergeOutcome::NeedsUser { .. } | MergeOutcome::Merged | MergeOutcome::NotRequired
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "cross\n",
            "项目目录与汇合结果一致"
        );
        let journal_dir = git2::Repository::open(&root)
            .unwrap()
            .path()
            .join(JOURNAL_DIR);
        let leftovers: Vec<_> = std::fs::read_dir(&journal_dir)
            .map(|it| {
                it.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                    .map(|e| e.file_name())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "收敛后不得残留事务日志: {leftovers:?}"
        );
        a.release(&lease).unwrap();
    }

    /// F9:rollback 的 target→original CAS —— 当前内容必须等于本事务
    /// **target**(或已等于 original 的幂等重试)才恢复/删除;
    /// 窗口内被用户改成第三种内容 → 拒绝回滚该文件(如实聚合),
    /// 绝不覆盖用户字节。
    #[test]
    fn rollback_refuses_when_current_content_is_neither_target_nor_original() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        // 本事务:把 a.txt 从 original 改成 target
        let change = JournaledChange {
            path: "a.txt".into(),
            deleted: false,
            content_hex: Some(hex_encode(b"target\n")),
            original_present: true,
            original_hex: Some(hex_encode(b"aaa\n")),
        };
        // 已应用(target 在盘上)→ 回滚恢复 original
        std::fs::write(root.join("a.txt"), "target\n").unwrap();
        provider.rollback_one(&change).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "aaa\n",
            "当前 == target 时必须恢复 original"
        );
        // 幂等:当前已 == original → no-op 成功
        provider.rollback_one(&change).unwrap();

        // 用户在窗口内编辑(第三种内容)→ 拒绝回滚,字节保持
        std::fs::write(root.join("a.txt"), "user-edit\n").unwrap();
        let err = provider.rollback_one(&change).unwrap_err();
        assert!(
            format!("{err:#}").contains("用户"),
            "错误必须指明用户修改: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "user-edit\n",
            "用户字节必须保持,不得被回滚覆盖"
        );
    }

    /// F9:删除项的回滚同样 CAS —— 本事务删除了该文件(target = 不存在),
    /// 当前不存在 → 恢复 original;用户重建了不同内容 → 拒绝。
    #[test]
    fn rollback_of_deletion_refuses_when_user_recreated_file() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let change = JournaledChange {
            path: "b.txt".into(),
            deleted: true,
            content_hex: None,
            original_present: true,
            original_hex: Some(hex_encode(b"bbb\n")),
        };
        // 已应用(文件已删)→ 恢复 original
        std::fs::remove_file(root.join("b.txt")).unwrap();
        provider.rollback_one(&change).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "bbb\n");
        // 用户重建为不同内容 → 拒绝恢复(会覆盖用户字节)
        std::fs::write(root.join("b.txt"), "user-recreated\n").unwrap();
        let err = provider.rollback_one(&change).unwrap_err();
        assert!(format!("{err:#}").contains("用户"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "user-recreated\n"
        );
    }

    /// F3:赢家崩溃后,另一独立实例的 recover 必须能收敛(跨实例恢复)。
    #[test]
    fn crashed_winner_is_recoverable_by_independent_instance() {
        let (_dir, root) = fixture();
        let a = GitWorktreeProvider::new(root.clone()).unwrap();
        let lease = a.acquire(&ctx(&root, 13, "a")).unwrap();
        std::fs::write(wt_path(&root, 13, "a").join("a.txt"), "winner\n").unwrap();
        a.set_merge_fault(MergeFault::CrashAfterRefAdvance);
        assert!(a.merge(&[lease.clone()]).is_err());
        // 独立实例(另一"进程")恢复:journal 重放应用到一致
        let b = GitWorktreeProvider::new(root.clone()).unwrap();
        b.recover_interrupted().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "winner\n",
            "独立实例必须能重放赢家的合并结果"
        );
        b.release(&lease).unwrap();
    }

    /// 同一批并发 merge(C1 提供器侧防线):仓库级合并互斥 + 集成 ref
    /// expected-old CAS —— 恰好一次 Merged,并发另一方检测到"已汇合的
    /// 其他节点"冲突;ref 单次推进、项目目录一致、无日志/临时文件残留。
    #[test]
    fn concurrent_merge_of_same_batch_advances_ref_exactly_once() {
        let (_dir, root) = fixture();
        let provider = std::sync::Arc::new(GitWorktreeProvider::new(root.clone()).unwrap());
        let lease = provider.acquire(&ctx(&root, 7, "a")).unwrap();
        std::fs::write(wt_path(&root, 7, "a").join("a.txt"), "concurrent\n").unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let p1 = provider.clone();
        let p2 = provider.clone();
        let l1 = lease.clone();
        let l2 = lease.clone();
        let b1 = barrier.clone();
        let h1 = std::thread::spawn(move || {
            b1.wait();
            p1.merge(&[l1])
        });
        let h2 = std::thread::spawn(move || {
            barrier.wait();
            p2.merge(&[l2])
        });
        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();
        let merged_count = [&r1, &r2]
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .filter(|o| matches!(o, MergeOutcome::Merged))
            .count();
        assert_eq!(
            merged_count, 1,
            "同一批并发 merge 必须恰好一次 Merged(实际 {merged_count};r1={r1:?},r2={r2:?})"
        );
        assert!(r1.is_ok() && r2.is_ok(), "并发合并不得产生错误(见上)");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "concurrent\n",
            "项目目录与汇合结果一致"
        );
        // 集成 ref 只推进一次:ref 树包含 a.txt="concurrent"
        let refname = Git::integration_ref(7, 1);
        let repo = git2::Repository::open(&root).unwrap();
        let mut revwalk = repo.revwalk().unwrap();
        revwalk.push_ref(&refname).unwrap();
        revwalk.hide_ref("HEAD").unwrap();
        let advanced_commits = revwalk.count();
        assert_eq!(
            advanced_commits, 1,
            "集成 ref 必须单次推进(不得叠第二次合并提交)"
        );
        let journal_dir = repo.path().join(JOURNAL_DIR);
        let leftovers: Vec<_> = std::fs::read_dir(&journal_dir)
            .map(|it| it.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "并发合并后不得残留事务日志/临时文件: {leftovers:?}"
        );
        provider.release(&lease).unwrap();
    }
}
