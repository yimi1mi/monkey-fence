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
}

pub struct GitWorktreeProvider {
    repo_root: PathBuf,
    /// `.worktrees` 根;非 Git 根为 None(回退共享目录)。
    worktrees_root: Option<PathBuf>,
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
                fault: parking_lot::Mutex::new(None),
            })
        } else {
            Ok(GitWorktreeProvider {
                repo_root,
                worktrees_root: None,
                fault: parking_lot::Mutex::new(None),
            })
        }
    }

    /// 注入合并故障点(仅测试)。
    #[cfg(test)]
    pub(crate) fn set_merge_fault(&self, fault: MergeFault) {
        *self.fault.lock() = Some(fault);
    }

    fn current_fault(&self) -> Option<MergeFault> {
        *self.fault.lock()
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

        // 4. 合并事务日志(旧 ref、目标 tree、目标文件内容与应用前
        //    原状态)先于 ref 推进持久化:ref 更新与文件应用之间的
        //    任何崩溃都可在启动(或下次合并前)重放/回滚一致收敛。
        let git = Git::open(&self.repo_root)?;
        let step_label = worktree_leases
            .first()
            .and_then(|l| l.metadata.get("step_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("step");
        let ref_before = git.read_ref(&refname)?;
        let journal = MergeJournal {
            version: 1,
            refname: refname.clone(),
            ref_before: ref_before.map(|o| o.to_string()),
            target_tree: tree_id.to_string(),
            changes: self.snapshot_originals(&changes)?,
        };
        let journal_path = journal_file_for(&repo, &refname);
        write_journal_atomic(&journal_path, &journal)?;
        if self.current_fault() == Some(MergeFault::CrashAfterJournal) {
            return Err(anyhow::anyhow!("(测试注入)事务日志写入后进程死亡"));
        }

        // 5. 原子推进集成基线 ref(下游 acquire 从汇合结果检出)。
        git.advance_integration_ref(
            &refname,
            tree_id,
            &format!(
                "mf: integrate {step_label} (+{} batch)",
                worktree_leases.len()
            ),
        )?;
        if self.current_fault() == Some(MergeFault::CrashAfterRefAdvance) {
            return Err(anyhow::anyhow!("(测试注入)ref 推进后进程死亡"));
        }

        // 6. 应用到项目目录;任一步失败逆序回滚已应用部分,回滚错误
        //    聚合上报(绝不忽略、绝不谎报已回滚)。
        match self.apply_journal_with_rollback(&journal) {
            ApplyOutcome::Applied => {
                let _ = std::fs::remove_file(&journal_path);
                Ok(MergeOutcome::Merged)
            }
            ApplyOutcome::Crashed(msg) => Err(anyhow::anyhow!("(测试注入){msg}")),
            ApplyOutcome::Failed {
                error,
                rolled_back_cleanly: true,
            } => {
                if let Err(ref_err) = git.reset_integration_ref(&refname, ref_before) {
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
    refname: String,
    /// 推进前的 ref 目标(None = 推进前 ref 不存在)。
    ref_before: Option<String>,
    /// 推进到的目标树。
    target_tree: String,
    changes: Vec<JournaledChange>,
}

/// 应用结果:成功 / 注入崩溃(不回滚,日志保留)/ 失败(是否完全回滚)。
enum ApplyOutcome {
    Applied,
    Crashed(String),
    Failed {
        error: anyhow::Error,
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

fn journal_file_for(repo: &git2::Repository, refname: &str) -> PathBuf {
    journal_dir_of(repo).join(format!("{}.json", refname.replace('/', "_")))
}

/// 事务日志原子写入:同目录临时文件 → 落盘 → 改名。
fn write_journal_atomic(path: &std::path::Path, journal: &MergeJournal) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string(journal)?.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
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
        }
        let mut applied: Vec<&JournaledChange> = Vec::new();
        let outcome = (|| -> Step {
            for change in &journal.changes {
                if let Err(e) = self.apply_one(change) {
                    return Step::Fail(e);
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
        match outcome {
            Step::Done => ApplyOutcome::Applied,
            Step::Crash(msg) => ApplyOutcome::Crashed(msg),
            Step::Fail(error) => {
                let mut failures: Vec<String> = Vec::new();
                for change in applied.iter().rev() {
                    if let Err(e) = self.rollback_one(change) {
                        failures.push(format!("{}: {e:#}", change.path));
                    }
                }
                if failures.is_empty() {
                    ApplyOutcome::Failed {
                        error,
                        rolled_back_cleanly: true,
                    }
                } else {
                    ApplyOutcome::Failed {
                        error: error.context(format!(
                            "回滚未完全成功(已保留合并事务日志,恢复将重放):{}",
                            failures.join("; ")
                        )),
                        rolled_back_cleanly: false,
                    }
                }
            }
        }
    }

    /// 应用单条变更(删除 → 移除存在者;写入 → 建目录 + 覆盖/新建)。
    fn apply_one(&self, change: &JournaledChange) -> Result<()> {
        let dst = self.repo_root.join(&change.path);
        if change.deleted {
            if dst.is_file() {
                std::fs::remove_file(&dst)
                    .with_context(|| format!("合并删除失败: {}", dst.display()))?;
            }
            return Ok(());
        }
        let content = change
            .content_hex
            .as_deref()
            .map(hex_decode)
            .transpose()?
            .expect("非删除变更的内容已在收集阶段读出");
        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("合并建目录失败: {}", parent.display()))?;
            }
        }
        std::fs::write(&dst, content).with_context(|| format!("合并写文件失败: {}", dst.display()))
    }

    /// 回滚单条已应用变更(按应用前原状态恢复)。
    fn rollback_one(&self, change: &JournaledChange) -> Result<()> {
        if self.current_fault() == Some(MergeFault::FailUndo) {
            anyhow::bail!("(测试注入)回滚动作失败: {}", change.path);
        }
        let dst = self.repo_root.join(&change.path);
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
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            return std::fs::write(&dst, original)
                .with_context(|| format!("恢复被删文件失败: {}", dst.display()));
        }
        if !change.original_present {
            // 应用前不存在:本次新建的文件 → 删除(新建的空目录保留)
            if dst.is_file() {
                return std::fs::remove_file(&dst)
                    .with_context(|| format!("回滚删除新建文件失败: {}", dst.display()));
            }
            return Ok(());
        }
        let original = hex_decode(
            change
                .original_hex
                .as_deref()
                .expect("original_present 时必有原字节"),
        )?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dst, original)
            .with_context(|| format!("恢复被覆盖文件失败: {}", dst.display()))
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
            return Ok(());
        }
        anyhow::bail!(
            "集成 ref {} 状态与事务日志不一致(当前 {current:?},日志 ref_before={ref_before:?}):保留日志待人工处理",
            journal.refname
        );
    }

    /// 幂等重放:已是目标内容 → 跳过;仍是原状态 → 写入目标;
    /// 崩溃窗口内被外部修改(既非原也非目标)→ 拒绝覆盖,保留可恢复。
    fn replay_journal(&self, journal: &MergeJournal) -> Result<()> {
        for change in &journal.changes {
            let dst = self.repo_root.join(&change.path);
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
                        std::fs::write(&dst, &target)
                            .with_context(|| format!("重放写入失败: {}", dst.display()))?;
                    } else {
                        anyhow::bail!(
                            "{} 在崩溃后被外部修改(疑似用户编辑),拒绝覆盖(请人工处理)",
                            change.path
                        );
                    }
                }
                Err(_) if !change.original_present => {
                    if let Some(parent) = dst.parent() {
                        if !parent.as_os_str().is_empty() {
                            std::fs::create_dir_all(parent)
                                .with_context(|| format!("重放建目录失败: {}", parent.display()))?;
                        }
                    }
                    std::fs::write(&dst, &target)
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
    use mf_agent::execution_directory::ExecutionDirectoryProvider;

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
}
