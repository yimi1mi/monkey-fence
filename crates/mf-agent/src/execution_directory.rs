//! Execution Directory Provider 接缝(设计 §5.6 / §6.3 / ADR 0003)。
//!
//! 内核只识别"路径租约":Task 与 Step 不感知 VCS。
//! - 默认 `ProjectDirectoryProvider` 返回项目目录(不隔离);
//! - Git worktree 等隔离实现由插件贡献(mf-plugins);
//! - `merge` 只返回合并结果,冲突 → `NeedsUser`,不是 Task 失败。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Kernel durable outbox action 的稳定交付身份。
///
/// 该值来自持久化的 `(outbox_id, action_index)`，因此 Core 在外部动作
/// 成功、ack 前崩溃后会以完全相同的 key 重放。`scope` 只用于同一 action
/// 内派生 provider 子操作（例如逐 lease release），不得放 capability token、
/// API key 或回答明文。
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunActionDeliveryKey {
    outbox_id: i64,
    action_index: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    scope: String,
}

impl RunActionDeliveryKey {
    pub fn new(outbox_id: i64, action_index: u32) -> Self {
        Self {
            outbox_id,
            action_index,
            scope: String::new(),
        }
    }

    /// 派生确定性的子操作 key。调用方只传静态阶段名或领域 ID。
    pub fn scoped(&self, scope: impl Into<String>) -> Self {
        Self {
            outbox_id: self.outbox_id,
            action_index: self.action_index,
            scope: scope.into(),
        }
    }

    pub fn outbox_id(&self) -> i64 {
        self.outbox_id
    }

    pub fn action_index(&self) -> u32 {
        self.action_index
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }
}

impl std::fmt::Debug for RunActionDeliveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunActionDeliveryKey")
            .field("outbox_id", &self.outbox_id)
            .field("action_index", &self.action_index)
            .field("scope_present", &(!self.scope.is_empty()))
            .finish()
    }
}

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
    /// durable RunAction 路径的 merge。生产 worker 必须把 `delivery`
    /// 原样传到插件；内置 provider 若已由 lease/batch identity + 持久
    /// merge CAS 严格幂等，可使用默认实现。
    fn merge_for_delivery(
        &self,
        delivery: &RunActionDeliveryKey,
        leases: &[ExecutionLease],
    ) -> Result<MergeOutcome> {
        let _ = delivery;
        self.merge(leases)
    }
    /// 释放租约(终态结算/取消后;未知状态保持持有)。
    ///
    /// # 幂等契约
    ///
    /// `lease.id` 是该外部操作的稳定身份。同一租约可能在
    /// provider 已释放、但 Core 尚未把 `execution_leases`
    /// 标记为 released 时因崩溃而重放。实现必须按完整
    /// lease identity 幂等：重复释放同一租约必须成功，同时
    /// 仍必须拒绝 provider/pin/path 等静态身份不匹配的伪造租约。
    fn release(&self, lease: &ExecutionLease) -> Result<()>;
    /// durable RunAction 路径的 release。`delivery` 是补充交付身份；
    /// `lease.id` 仍是释放对象的权威领域身份，不允许仅凭 key 删除路径。
    fn release_for_delivery(
        &self,
        delivery: &RunActionDeliveryKey,
        lease: &ExecutionLease,
    ) -> Result<()> {
        let _ = delivery;
        self.release(lease)
    }
    /// 在每个真实使用点（启动/merge/release）复验租约身份。需要绑定
    /// 目录对象身份的第三方 provider 应覆盖；默认内置实现无需额外状态。
    fn validate_for_use(&self, _lease: &ExecutionLease) -> Result<()> {
        Ok(())
    }
    /// 任务终态(成功/取消/归档)后丢弃该任务的集成基线等持久痕迹。
    /// `task_id` 是该外部操作的稳定身份；同一 task 的重复调用
    /// 必须成功。默认无操作(不维护基线的提供器)。
    fn discard_task_baselines(&self, task_id: i64) -> Result<()> {
        let _ = task_id;
        Ok(())
    }
    /// durable task cleanup 路径。不同 task 必须派生不同 scope；worker
    /// 应以 delivery + task identity 做跨进程幂等收口。
    fn discard_task_baselines_for_delivery(
        &self,
        delivery: &RunActionDeliveryKey,
        task_id: i64,
    ) -> Result<()> {
        let _ = delivery;
        self.discard_task_baselines(task_id)
    }
    /// 启动恢复:提供器持久化痕迹的崩溃一致性检查(如合并事务日志)。
    /// 默认无操作。失败必须如实上报 —— 不得静默吞掉或谎报已恢复。
    fn recover_interrupted(&self) -> Result<()> {
        Ok(())
    }
}

/// 按 pinned 身份解析目录提供器(C7):held lease 的 merge/release
/// 与重启恢复必须路由到租约(或 Revision)冻结的完整
/// `full_id + version + content_hash` 对应的提供器实现 —— 插件升级/
/// 更换后绝不能用当前进程内提供器顶替旧租约的操作。
/// 解析失败(旧版本已卸载)由调用方转入持久 NeedsYou。
pub trait DirectoryProviderResolver: Send + Sync {
    fn resolve(
        &self,
        pin: &crate::workflow::PluginSourcePin,
    ) -> Option<std::sync::Arc<dyn ExecutionDirectoryProvider>>;
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

    fn release(&self, lease: &ExecutionLease) -> Result<()> {
        anyhow::ensure!(
            lease.provider == self.id() && !lease.isolated && lease.id.starts_with("project-"),
            "拒绝释放非 project-dir 租约: {}",
            lease.id
        );
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

#[cfg(test)]
mod delivery_key_tests {
    use super::*;

    #[test]
    fn debug_never_prints_scope_even_if_a_caller_passes_a_secret() {
        let sentinel = "mft-never-print-delivery-scope";
        let key = RunActionDeliveryKey::new(1, 2).scoped(sentinel);
        let debug = format!("{key:?}");
        assert!(!debug.contains(sentinel));
        assert!(debug.contains("scope_present"));
    }
}
