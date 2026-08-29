//! Execution Directory Provider 接缝(设计 §5.6 / §6.3 / ADR 0003)。
//!
//! 内核只识别"路径租约":Task 与 Step 不感知 VCS。
//! - 默认 `ProjectDirectoryProvider` 返回项目目录(不隔离);
//! - Git worktree 等隔离实现由插件贡献(mf-plugins);
//! - `merge` 只返回合并结果,冲突 → `NeedsUser`,不是 Task 失败。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Agent Run 的执行位置租约。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionLease {
    /// 租约稳定 ID。
    pub id: String,
    /// 本次 Agent Run 的工作目录(进程 cwd)。
    pub path: PathBuf,
    /// 是否独占隔离(并行安全);false = 共享目录。
    pub isolated: bool,
    /// 提供器 ID(project-dir / worktree ...)。
    pub provider: String,
    /// 提供器自定义元数据(worktree 名、分支等)。
    pub metadata: serde_json::Value,
}

/// 汇合结果:冲突进入 needs-you,由用户处理(设计 §9.4)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged,
    NeedsUser { conflicts: Vec<String> },
    NotRequired,
}

/// acquire 上下文:内核知道的全部运行事实(无 VCS 概念)。
#[derive(Debug, Clone)]
pub struct LeaseContext {
    pub task_id: i64,
    pub step_id: i64,
    /// 活动流水线 Revision(worktree 集成基线按 task+rev 维护)。
    pub revision_id: i64,
    /// 第几次尝试(1 起;自动重试递增)。
    pub attempt: u32,
    pub project_root: PathBuf,
    pub step_key: String,
    /// 上游节点键(拓扑合并顺序;无依赖时为空)。
    pub deps: Vec<String>,
}

/// 执行目录提供器接口(设计 §6.3)。
pub trait ExecutionDirectoryProvider: Send + Sync {
    fn id(&self) -> &str;
    /// 提供器是否提供独占隔离的租约(worktree = true;项目目录 = false)。
    /// Workflow Compiler 据此判定并行安全,插件清单的隔离能力与其保持一致。
    fn isolates(&self) -> bool {
        false
    }
    /// 为一次 Agent Run 获取路径租约。
    fn acquire(&self, run: &LeaseContext) -> Result<ExecutionLease>;
    /// 汇合:按固定顺序尝试无冲突合并;冲突返回 `NeedsUser`。
    fn merge(&self, leases: &[ExecutionLease]) -> Result<MergeOutcome>;
    /// 释放租约(终态结算/取消后;未知状态保持持有)。
    fn release(&self, lease: &ExecutionLease) -> Result<()>;
    /// 任务终态(成功/取消/归档)后丢弃该任务的集成基线等持久痕迹。
    /// 默认无操作(不维护基线的提供器)。
    fn discard_task_baselines(&self, task_id: i64) -> Result<()> {
        let _ = task_id;
        Ok(())
    }
    /// 启动恢复:提供器持久化痕迹的崩溃一致性检查(如合并事务日志)。
    /// 默认无操作。失败必须如实上报 —— 不得静默吞掉或谎报已恢复。
    fn recover_interrupted(&self) -> Result<()> {
        Ok(())
    }
}

/// 默认提供器:项目目录本身,不隔离;
/// 并行节点需要用户显式开启"共享目录并行"风险开关(编译器校验)。
#[derive(Default)]
pub struct ProjectDirectoryProvider;

impl ExecutionDirectoryProvider for ProjectDirectoryProvider {
    fn id(&self) -> &str {
        "project-dir"
    }

    fn acquire(&self, run: &LeaseContext) -> Result<ExecutionLease> {
        Ok(ExecutionLease {
            id: format!("project-{}-{}", run.task_id, run.step_id),
            path: run.project_root.clone(),
            isolated: false,
            provider: "project-dir".into(),
            metadata: serde_json::json!({ "attempt": run.attempt }),
        })
    }

    fn merge(&self, _leases: &[ExecutionLease]) -> Result<MergeOutcome> {
        // 共享目录:所有运行都在项目目录里,无需合并
        Ok(MergeOutcome::NotRequired)
    }

    fn release(&self, _lease: &ExecutionLease) -> Result<()> {
        Ok(())
    }
}

/// 校验租约路径位于给定根之下(供 worktree 实现复用):
/// 拒绝词法逃逸与符号链接/接合点。
pub fn ensure_lease_under_root(root: &Path, lease_path: &Path) -> Result<()> {
    let relative = lease_path
        .strip_prefix(root)
        .map_err(|_| anyhow::anyhow!("租约路径不在根目录下: {}", lease_path.display()))?;
    if relative.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        anyhow::bail!("租约路径不允许词法逃逸: {}", lease_path.display());
    }
    let mut current = root.to_path_buf();
    let mut check = |path: &Path| -> Result<()> {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                anyhow::bail!("租约路径不得穿过符号链接/接合点: {}", path.display());
            }
        }
        Ok(())
    };
    check(&current)?;
    for component in relative.components() {
        current.push(component);
        check(&current)?;
    }
    Ok(())
}
