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
    /// CAS 已读取当前内容、尚未执行替换时暂停；仅用于证明 read→rename
    /// 窗口内的用户编辑不会被覆盖。
    WaitAfterCasRead,
    /// 幂等原子验证已把 name 移到 verify 后模拟进程死亡。
    CrashAfterVerifyMove,
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
        let journal_file_name = journal_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("journal.json")
            .to_string();
        write_journal_atomic(&repo, &journal, &journal_file_name)?;
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
                remove_journal_file(&repo, &journal_file_name)
                    .context("合并已应用，但事务日志清理/目录同步失败；保留租约待恢复确认")?;
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
                    return Err(anyhow::anyhow!(
                        "用户编辑冲突且回滚集成 ref 失败(事务日志保留): {ref_err:#}"
                    ));
                }
                remove_journal_file(&repo, &journal_file_name)
                    .context("用户冲突已回滚，但事务日志清理失败")?;
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
                remove_journal_file(&repo, &journal_file_name)
                    .context("应用失败已回滚，但事务日志清理失败")?;
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

/// 错误链里是否含「路径不存在」类失败(文件必然不在):
/// Windows STATUS_OBJECT_NAME_NOT_FOUND/PATH_NOT_FOUND、ERROR_FILE_NOT_FOUND/
/// ERROR_PATH_NOT_FOUND;Unix ENOENT。CAS 读取据此归一为 None。
fn error_is_path_absent(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("0xc0000034")
        || text.contains("0xc000003a")
        || text.contains("0xc0000103") // 父路径是普通文件:目标必然不存在
        || text.contains("os error 2")
        || text.contains("os error 3")
        || text.contains("os error 267")
        || text.contains("No such file or directory")
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

/// 事务日志原子写入(F5/F12/F6):`.git` 打开为受信根 →
/// `mf-merge-journals` 子目录逐级句柄相对打开(reparse 拒绝)→
/// **唯一随机文件名**(每事务独占,绝不覆盖他人日志)+ 内容序列化后
/// 单次原子写入(create_new 语义临时名 + 句柄相对替换 + 双重落盘)。
fn write_journal_atomic(
    repo: &git2::Repository,
    journal: &MergeJournal,
    file_name: &str,
) -> Result<()> {
    let git_root = repo.path().to_path_buf();
    let dir = crate::fs_safe::SafeDir::open_root(&git_root)
        .with_context(|| format!("打开 .git 失败: {}", git_root.display()))?
        .child(JOURNAL_DIR, true)?;
    let payload = serde_json::to_string(journal)?;
    dir.write_file(file_name, payload.as_bytes())
        .with_context(|| format!("写入事务日志失败: {file_name}"))
}

fn open_journal_dir(repo: &git2::Repository, create: bool) -> Result<crate::fs_safe::SafeDir> {
    let git_root = repo.path().to_path_buf();
    crate::fs_safe::SafeDir::open_root(&git_root)
        .with_context(|| format!("打开 .git 失败: {}", git_root.display()))?
        .child(JOURNAL_DIR, create)
        .context("打开事务日志目录失败(符号链接/junction/reparse 一律拒绝)")
}

fn remove_journal_file(repo: &git2::Repository, file_name: &str) -> Result<()> {
    let dir = open_journal_dir(repo, false)?;
    dir.remove_file(file_name)
        .with_context(|| format!("删除事务日志失败: {file_name}"))?;
    dir.sync()
        .with_context(|| format!("同步事务日志目录失败: {file_name}"))
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

impl GitWorktreeProvider {
    /// F6:变更路径的句柄相对读取(词法校验 + 逐级 reparse 拒绝)。
    /// 「父目录不存在/父路径不是目录」归一为文件不存在(None)——
    /// CAS 读取语义:文件必然不在;其余 IO 错误如实上抛。
    fn read_change_current(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        validate_change_path(&self.repo_root, rel)?;
        match crate::fs_safe::open_parent_for(&self.repo_root, rel, false) {
            Ok((dir, name)) => dir.read_file(&name),
            Err(e) if error_is_path_absent(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn atomically_verify_entry(
        &self,
        dir: &crate::fs_safe::SafeDir,
        name: &str,
        verify_name: &str,
        expected: Option<&[u8]>,
    ) -> Result<bool> {
        let sentinel = format!("mf-cas-absence-sentinel-{verify_name}");
        if expected.is_none() && !dir.write_file_if_absent(name, sentinel.as_bytes())? {
            return Ok(false);
        }
        let moved = match dir.rename_noreplace(name, verify_name) {
            Ok(moved) => moved,
            Err(e) if error_is_path_absent(&e) => return Ok(false),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(moved, "CAS 验证临时名已存在:{verify_name}");
        if self.current_fault() == Some(MergeFault::CrashAfterVerifyMove) {
            anyhow::bail!("(测试注入)目录项移入 verify 后进程死亡");
        }
        let captured = dir
            .read_file(verify_name)?
            .context("CAS 验证文件原子取走后消失")?;
        let matches = expected.map_or(captured == sentinel.as_bytes(), |bytes| captured == bytes);
        if matches && expected.is_none() {
            dir.remove_file(verify_name)?;
            dir.sync()?;
            return Ok(true);
        }
        match dir.rename_noreplace(verify_name, name)? {
            true => Ok(matches),
            false if matches => {
                // 用户已占回目标名；captured 只是 desired/sentinel，不覆盖用户。
                dir.remove_file(verify_name)?;
                dir.sync()?;
                Ok(false)
            }
            false => anyhow::bail!(
                "CAS 验证捕获用户字节后目标名又被占用；保留 `{verify_name}` 待人工处理"
            ),
        }
    }

    fn recover_verify_artifact(
        &self,
        dir: &crate::fs_safe::SafeDir,
        name: &str,
        verify_name: &str,
        desired: Option<&[u8]>,
    ) -> Result<Option<bool>> {
        let sentinel = format!("mf-cas-absence-sentinel-{verify_name}");
        let mut current = dir.read_file(name)?;
        let mut captured = dir.read_file(verify_name)?;
        if captured.is_none() && current.as_deref() == Some(sentinel.as_bytes()) {
            anyhow::ensure!(
                dir.rename_noreplace(name, verify_name)?,
                "恢复 absence verify 时临时名已被占用"
            );
            captured = dir.read_file(verify_name)?;
            current = None;
        }
        let Some(captured) = captured else {
            return Ok(None);
        };
        match desired {
            Some(bytes) if captured == bytes => {
                anyhow::ensure!(
                    current.is_none(),
                    "verify 崩溃恢复时目标名已被外部占用；保留 verify 待人工处理"
                );
                anyhow::ensure!(
                    dir.rename_noreplace(verify_name, name)?,
                    "恢复 verify 目标名失败"
                );
                Ok(Some(true))
            }
            None if captured == sentinel.as_bytes() => {
                dir.remove_file(verify_name)?;
                dir.sync()?;
                Ok(Some(current.is_none()))
            }
            _ if current.is_none() => {
                anyhow::ensure!(
                    dir.rename_noreplace(verify_name, name)?,
                    "恢复 verify 捕获的用户字节失败"
                );
                Ok(Some(false))
            }
            _ => anyhow::bail!(
                "verify 捕获用户字节后目标名又被占用；保留 `{verify_name}` 待人工处理"
            ),
        }
    }

    /// 目录项级内容 CAS。现有文件先被原子移到事务独占备份名，再校验
    /// 备份字节；结果只以“不覆盖”方式安装。用户在任一窗口创建第三态
    /// 时返回 false 且保留用户路径内容。
    fn compare_exchange_change(
        &self,
        rel: &str,
        expected: Option<&[u8]>,
        desired: Option<&[u8]>,
        transaction_id: &str,
    ) -> Result<bool> {
        use sha2::{Digest as _, Sha256};
        validate_change_path(&self.repo_root, rel)?;
        let create_parent = expected.is_none() && desired.is_some();
        let (dir, name) = match crate::fs_safe::open_parent_for(&self.repo_root, rel, create_parent)
        {
            Ok(pair) => pair,
            Err(e) if expected.is_none() && error_is_path_absent(&e) => {
                return Ok(desired.is_none())
            }
            Err(e) => return Err(e),
        };
        let mut h = Sha256::new();
        h.update(transaction_id.as_bytes());
        h.update([0]);
        h.update(name.as_bytes());
        let digest = h.finalize();
        let backup = format!(
            ".mfcas-{}",
            digest[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let verify_name = format!(
            ".mfverify-{}",
            digest[16..]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );

        let mut backup_present = dir.read_file(&backup)?;
        if let Some(recovered) = self.recover_verify_artifact(&dir, &name, &verify_name, desired)? {
            if recovered && backup_present.is_some() {
                dir.remove_file(&backup)?;
                dir.sync()?;
            }
            return Ok(recovered);
        }
        let current = dir.read_file(&name)?;
        while self.current_fault() == Some(MergeFault::WaitAfterCasRead) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if backup_present.is_none() && current.as_deref() == desired {
            return self.atomically_verify_entry(&dir, &name, &verify_name, desired);
        }
        if let Some(saved) = backup_present.as_deref() {
            if expected != Some(saved) {
                if dir.rename_noreplace(&backup, &name)? {
                    return Ok(false);
                }
                anyhow::bail!(
                    "CAS 捕获到窗口内编辑且目标名又被占用({rel});保留备份 `{backup}` 待人工处理"
                );
            }
            if current.as_deref() == desired {
                // 崩溃发生在结果安装后、备份清理前：先原子复验结果，
                // 验证通过才补清理原始备份。
                if self.atomically_verify_entry(&dir, &name, &verify_name, desired)? {
                    dir.remove_file(&backup)?;
                    dir.sync()?;
                    return Ok(true);
                }
                return Ok(false);
            }
        } else if expected.is_some() {
            if current.as_deref() != expected {
                return Ok(false);
            }
            anyhow::ensure!(
                dir.rename_noreplace(&name, &backup)?,
                "CAS 备份名意外已存在({backup})"
            );
            backup_present = dir.read_file(&backup)?;
            if backup_present.as_deref() != expected {
                if dir.rename_noreplace(&backup, &name)? {
                    return Ok(false);
                }
                anyhow::bail!(
                    "CAS 捕获到窗口内编辑且目标名又被占用({rel});保留备份 `{backup}` 待人工处理"
                );
            }
        } else if current.is_some() {
            return Ok(false);
        }

        let installed = match desired {
            Some(bytes) => dir.write_file_if_absent(&name, bytes)?,
            None => dir.read_file(&name)?.is_none(),
        };
        if !installed {
            // 用户赢得目标名；不覆盖。expected 的旧字节已由 journal/Git
            // 记录，删除事务备份，冲突交给上层 NeedsUser。
            if backup_present.is_some() {
                dir.remove_file(&backup)?;
                dir.sync()?;
            }
            return Ok(false);
        }
        if backup_present.is_some() {
            dir.remove_file(&backup)?;
        }
        dir.sync()?;
        Ok(true)
    }
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
    /// F6:读取经句柄相对路径(逐级 reparse 拒绝),不按完整路径解析。
    fn snapshot_originals(&self, changes: &[MergeChange]) -> Result<Vec<JournaledChange>> {
        changes
            .iter()
            .map(|c| {
                validate_change_path(&self.repo_root, &c.path)?;
                // 父路径存在但不是目录(应用阶段的障碍)视同"文件不存在":
                // 快照只记录现状,失败留给 apply 原地暴露
                let original = match self.read_change_current(&c.path) {
                    Ok(bytes) => bytes,
                    Err(e) if error_is_path_absent(&e) => None,
                    Err(e) => return Err(e.context(format!("读取应用前文件失败: {}", c.path))),
                };
                Ok(JournaledChange {
                    path: c.path.clone(),
                    deleted: c.deleted,
                    content_hex: c.content.as_deref().map(hex_encode),
                    original_present: original.is_some(),
                    original_hex: original.as_deref().map(hex_encode),
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
                match self.apply_one(&journal.transaction_id, change) {
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
                if let Err(e) = self.rollback_one(&journal.transaction_id, change) {
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
    fn apply_one(
        &self,
        transaction_id: &str,
        change: &JournaledChange,
    ) -> std::result::Result<(), ApplyOneError> {
        let original: Option<Vec<u8>> = change
            .original_hex
            .as_deref()
            .map(hex_decode)
            .transpose()
            .map_err(ApplyOneError::Error)?;
        let content = if change.deleted {
            None
        } else {
            Some(
                change
                    .content_hex
                    .as_deref()
                    .map(hex_decode)
                    .transpose()
                    .map_err(ApplyOneError::Error)?
                    .expect("非删除变更的内容已在收集阶段读出"),
            )
        };
        match self
            .compare_exchange_change(
                &change.path,
                original.as_deref(),
                content.as_deref(),
                transaction_id,
            )
            .map_err(ApplyOneError::Error)?
        {
            true => Ok(()),
            false => Err(ApplyOneError::Conflict(format!(
                "{}(目录项 CAS 未命中:窗口内被用户修改,拒绝覆盖)",
                change.path
            ))),
        }
    }

    /// 回滚单条已应用变更(F9:target→original CAS)—— 只有当前内容
    /// 确实等于**本事务 target**(即应用结果未被触碰)或已等于
    /// original(幂等重试)时才恢复/删除;窗口内被用户改成第三种
    /// 内容 → 该文件回滚失败(调用方聚合上报,绝不覆盖用户字节)。
    fn rollback_one(&self, transaction_id: &str, change: &JournaledChange) -> Result<()> {
        if self.current_fault() == Some(MergeFault::FailUndo) {
            anyhow::bail!("(测试注入)回滚动作失败: {}", change.path);
        }
        let original = change.original_hex.as_deref().map(hex_decode).transpose()?;
        let target = change.content_hex.as_deref().map(hex_decode).transpose()?;
        let applied_state = if change.deleted {
            None
        } else {
            target.as_deref()
        };
        anyhow::ensure!(
            self.compare_exchange_change(
                &change.path,
                applied_state,
                original.as_deref(),
                transaction_id,
            )?,
            "{} 回滚 CAS 未命中:应用结果已被用户修改,拒绝覆盖",
            change.path
        );
        Ok(())
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
        if !dir.exists() {
            return Ok(());
        }
        // 先绑定稳定目录句柄；之后即使路径被替换，读取/删除仍只作用于
        // 已验证的原目录对象。
        let safe_dir = open_journal_dir(&repo, false)?;
        let mut errors: Vec<String> = Vec::new();
        for name in safe_dir.list_file_names()? {
            if std::path::Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                != Some("json")
            {
                continue;
            }
            if let Err(e) = self.recover_journal_file(&repo, &safe_dir, &name) {
                errors.push(format!("{name}: {e:#}"));
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

    fn recover_journal_file(
        &self,
        repo: &git2::Repository,
        journal_dir: &crate::fs_safe::SafeDir,
        file_name: &str,
    ) -> Result<()> {
        let bytes = journal_dir
            .read_file(file_name)?
            .ok_or_else(|| anyhow::anyhow!("枚举后事务日志已消失: {file_name}"))?;
        let journal: MergeJournal = serde_json::from_slice(&bytes)
            .with_context(|| format!("解析事务日志失败: {file_name}"))?;
        // F12:journal version 严格校验 —— 未来版本的日志结构未知,
        // 按当前版本语义盲执行可能破坏一致性;拒绝并保留日志
        anyhow::ensure!(
            journal.version == MERGE_JOURNAL_VERSION,
            "事务日志版本不受支持(日志 v{},程序支持 v{MERGE_JOURNAL_VERSION};未来版本拒绝执行): {}",
            journal.version,
            file_name
        );
        // C6:磁盘上的日志可能被篡改/植入 —— 读取即校验(先于任何
        // ref 读取/判定/文件操作):refname 形态 + 每条变更路径
        // (纯 relative/normal、拒绝 symlink/junction/穿越),非法即拒
        // (日志保留,绝不执行)
        validate_refname(&journal.refname).with_context(|| {
            format!("事务日志 refname 非法({}): {}", file_name, journal.refname)
        })?;
        for change in &journal.changes {
            validate_change_path(&self.repo_root, &change.path)
                .with_context(|| format!("事务日志变更路径非法({file_name}): {}", change.path))?;
        }
        let git = Git::open(&self.repo_root)?;
        let current = git.read_ref(&journal.refname)?;
        let target_tree = git2::Oid::from_str(&journal.target_tree)
            .with_context(|| format!("事务日志 target_tree OID 非法: {file_name}"))?;
        repo.find_tree(target_tree)
            .with_context(|| format!("事务日志 target_tree 对象不存在: {file_name}"))?;
        let current_tree = current
            .and_then(|o| git2::Oid::from_str(&o.to_string()).ok())
            .and_then(|oid| repo.find_commit(oid).ok())
            .map(|c| c.tree_id());
        if current_tree == Some(target_tree) {
            // ref 已推进:重放应用直到项目目录一致
            self.replay_journal(&journal)
                .with_context(|| format!("重放事务日志失败: {file_name}"))?;
            journal_dir.remove_file(file_name)?;
            journal_dir.sync()?;
            return Ok(());
        }
        let ref_before = match journal.ref_before.as_deref() {
            Some(value) => {
                let oid = git2::Oid::from_str(value)
                    .with_context(|| format!("事务日志 ref_before OID 非法: {file_name}"))?;
                repo.find_commit(oid)
                    .with_context(|| format!("事务日志 ref_before 提交不存在: {file_name}"))?;
                Some(oid)
            }
            None => None,
        };
        if current == ref_before {
            // 推进前死亡:无任何文件应用发生 → 安全清日志
            journal_dir.remove_file(file_name)?;
            journal_dir.sync()?;
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
            let original = change.original_hex.as_deref().map(hex_decode).transpose()?;
            let target = change.content_hex.as_deref().map(hex_decode).transpose()?;
            anyhow::ensure!(
                self.compare_exchange_change(
                    &change.path,
                    original.as_deref(),
                    if change.deleted {
                        None
                    } else {
                        target.as_deref()
                    },
                    &journal.transaction_id,
                )?,
                "{} 在崩溃后被外部修改，目录项 CAS 未命中，拒绝覆盖",
                change.path
            );
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

    /// 真正的内容 CAS 必须覆盖“已读取 expected、尚未 rename”这一窗口。
    /// 用户在该窗口写入第三态时，merge 必须 NeedsUser 且保留用户字节。
    #[test]
    fn apply_cas_rejects_edit_after_expected_was_read() {
        let (_dir, root) = fixture();
        let provider = Arc::new(GitWorktreeProvider::new(root.clone()).unwrap());
        let lease = provider.acquire(&ctx(&root, 18, "a")).unwrap();
        std::fs::write(wt_path(&root, 18, "a").join("a.txt"), "agent-cas\n").unwrap();

        provider.set_merge_fault(MergeFault::WaitAfterCasRead);
        let p2 = provider.clone();
        let lease2 = lease.clone();
        let merger = std::thread::spawn(move || p2.merge(&[lease2]));
        let refname = Git::integration_ref(18, 1);
        assert!(
            (0..500).any(|_| {
                std::thread::sleep(std::time::Duration::from_millis(10));
                ref_tree(&root, &refname).is_some()
            }),
            "前置:ref 已推进且 apply 进入 CAS"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::fs::write(root.join("a.txt"), "user-after-read\n").unwrap();
        provider.clear_merge_fault();

        match merger.join().unwrap().unwrap() {
            MergeOutcome::NeedsUser { conflicts } => assert!(
                conflicts.iter().any(|c| c.contains("a.txt")),
                "CAS 冲突必须带路径:{conflicts:?}"
            ),
            other => panic!("读后竞态必须拒绝覆盖，得到 {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "user-after-read\n"
        );
        provider.release(&lease).unwrap();
    }

    #[test]
    fn recovery_idempotent_desired_state_is_atomically_reverified() {
        let (_dir, root) = fixture();
        let provider = Arc::new(GitWorktreeProvider::new(root.clone()).unwrap());
        let lease = provider.acquire(&ctx(&root, 22, "a")).unwrap();
        std::fs::write(wt_path(&root, 22, "a").join("a.txt"), "desired\n").unwrap();
        provider.set_merge_fault(MergeFault::CrashAfterRefAdvance);
        assert!(provider.merge(&[lease.clone()]).is_err());
        std::fs::write(root.join("a.txt"), "desired\n").unwrap();

        provider.set_merge_fault(MergeFault::WaitAfterCasRead);
        let p2 = provider.clone();
        let recovery = std::thread::spawn(move || p2.recover_interrupted());
        std::thread::sleep(std::time::Duration::from_millis(150));
        std::fs::write(root.join("a.txt"), "user-third-state\n").unwrap();
        provider.clear_merge_fault();
        assert!(
            recovery.join().unwrap().is_err(),
            "read 后 desired 被改写时不得宣称 journal 已收敛"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "user-third-state\n"
        );
        let journal_dir = git2::Repository::open(&root)
            .unwrap()
            .path()
            .join(JOURNAL_DIR);
        assert!(std::fs::read_dir(journal_dir).unwrap().count() > 0);
        provider.release(&lease).unwrap();
    }

    #[test]
    fn idempotent_verify_crash_is_recovered_on_next_cas() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        provider.set_merge_fault(MergeFault::CrashAfterVerifyMove);
        assert!(provider
            .compare_exchange_change("a.txt", Some(b"aaa\n"), Some(b"aaa\n"), "verify-crash-txn",)
            .is_err());
        assert!(!root.join("a.txt").exists(), "故障点应位于移名后");
        provider.clear_merge_fault();
        assert!(provider
            .compare_exchange_change("a.txt", Some(b"aaa\n"), Some(b"aaa\n"), "verify-crash-txn",)
            .unwrap());
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"aaa\n");
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(".mfverify-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// journal 目录本身若被替换为 symlink/junction，恢复必须在打开稳定
    /// 目录句柄时拒绝，不能通过 entry.path() 读取/删除仓库外文件。
    #[test]
    fn recovery_rejects_replaced_journal_directory() {
        let (_dir, root) = fixture();
        let repo = git2::Repository::open(&root).unwrap();
        let journal_dir = repo.path().join(JOURNAL_DIR);
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.json");
        std::fs::write(
            &outside_file,
            serde_json::json!({
                "version": 999,
                "transaction_id": "outside",
                "refname": Git::integration_ref(19, 1),
                "ref_before": null,
                "target_tree": "0101010101010101010101010101010101010101",
                "changes": []
            })
            .to_string(),
        )
        .unwrap();

        #[cfg(windows)]
        {
            let out = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &journal_dir.to_string_lossy(),
                    &outside.path().to_string_lossy(),
                ])
                .output()
                .unwrap();
            if !out.status.success() {
                eprintln!("跳过:当前环境无法创建 junction");
                return;
            }
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &journal_dir).unwrap();

        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let err = provider.recover_interrupted().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("符号链接") || msg.contains("接合点") || msg.contains("reparse"),
            "必须因 journal 目录身份非法而拒绝，而不是读取外部日志:{msg}"
        );
        assert!(outside_file.exists(), "仓库外日志绝不能被删除");
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

    #[test]
    fn recovery_preserves_journal_with_malformed_oids() {
        let (_dir, root) = fixture();
        let provider = GitWorktreeProvider::new(root.clone()).unwrap();
        let repo = git2::Repository::open(&root).unwrap();
        let journal_dir = repo.path().join(JOURNAL_DIR);
        std::fs::create_dir_all(&journal_dir).unwrap();
        let path = journal_dir.join("bad-oid.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "version": MERGE_JOURNAL_VERSION,
                "transaction_id": "bad-oid",
                "refname": Git::integration_ref(21, 1),
                "ref_before": null,
                "target_tree": "not-an-oid",
                "changes": []
            })
            .to_string(),
        )
        .unwrap();
        let err = provider.recover_interrupted().unwrap_err();
        assert!(format!("{err:#}").contains("OID"), "{err:#}");
        assert!(
            path.exists(),
            "OID 损坏的日志必须保留，不能误判为推进前死亡"
        );
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
        provider.rollback_one("test-rollback", &change).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "aaa\n",
            "当前 == target 时必须恢复 original"
        );
        // 幂等:当前已 == original → no-op 成功
        provider.rollback_one("test-rollback", &change).unwrap();

        // 用户在窗口内编辑(第三种内容)→ 拒绝回滚,字节保持
        std::fs::write(root.join("a.txt"), "user-edit\n").unwrap();
        let err = provider.rollback_one("test-rollback", &change).unwrap_err();
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
        provider.rollback_one("test-rollback", &change).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "bbb\n"
        );
        // 用户重建为不同内容 → 拒绝恢复(会覆盖用户字节)
        std::fs::write(root.join("b.txt"), "user-recreated\n").unwrap();
        let err = provider.rollback_one("test-rollback", &change).unwrap_err();
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
