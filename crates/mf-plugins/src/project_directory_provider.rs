//! 插件贡献的项目目录 Execution Directory Provider(设计 §6.3)。
//!
//! 与内核默认 `ProjectDirectoryProvider` 语义一致(项目目录、不隔离),
//! 差别在于经插件贡献边界注册:清单里声明 `execution_directory_providers`
//! 的 `project-dir` 条目后由 Plugin Host 解析到本实现。
//! worktree 隔离实现见 `git_worktree_provider`。

use anyhow::Result;
use mf_agent::execution_directory::{
    ensure_lease_under_root, ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use std::path::PathBuf;

/// 提供器 ID(与清单 `execution_directory_providers[].id` 对应)。
pub const PROVIDER_ID: &str = "project-dir";

#[derive(Default)]
pub struct PluginProjectDirectoryProvider {
    /// 每次派发记录的租约(诊断/测试用)。
    history: parking_lot::Mutex<Vec<ExecutionLease>>,
}

impl PluginProjectDirectoryProvider {
    pub fn new() -> PluginProjectDirectoryProvider {
        PluginProjectDirectoryProvider::default()
    }

    /// 已发放的租约快照(诊断)。
    pub fn lease_history(&self) -> Vec<ExecutionLease> {
        self.history.lock().clone()
    }
}

impl ExecutionDirectoryProvider for PluginProjectDirectoryProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn acquire(&self, run: &LeaseContext) -> Result<ExecutionLease> {
        let lease = ExecutionLease {
            id: format!("proj-{}-{}-{}", run.task_id, run.step_key, run.attempt),
            path: run.project_root.clone(),
            isolated: false,
            provider: PROVIDER_ID.into(),
            metadata: serde_json::json!({
                "task_id": run.task_id,
                "step_key": run.step_key,
                "attempt": run.attempt,
            }),
        };
        ensure_lease_under_root(&run.project_root, &lease.path)?;
        self.history.lock().push(lease.clone());
        Ok(lease)
    }

    fn merge(&self, _leases: &[ExecutionLease]) -> Result<MergeOutcome> {
        // 共享目录:所有运行都在项目目录里,无需合并
        Ok(MergeOutcome::NotRequired)
    }

    fn release(&self, lease: &ExecutionLease) -> Result<()> {
        anyhow::ensure!(
            lease.provider == self.id() && !lease.isolated && lease.id.starts_with("proj-"),
            "拒绝释放非 project-dir 插件租约: {}",
            lease.id
        );
        Ok(())
    }
}

/// 为合成插件清单生成 project-dir 贡献条目。
pub fn contribution() -> crate::manifest::ExecutionDirectoryContribution {
    crate::manifest::ExecutionDirectoryContribution {
        id: PROVIDER_ID.into(),
        name: "项目目录".into(),
        kind: PROVIDER_ID.into(),
        supports_parallel: false,
        isolates: false,
        description: "Agent Run 直接在项目目录执行(共享目录,不隔离)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquires_project_root_without_isolation() {
        let provider = PluginProjectDirectoryProvider::new();
        let lease = provider
            .acquire(&LeaseContext {
                task_id: 7,
                step_id: 8,
                revision_id: 1,
                attempt: 1,
                project_root: PathBuf::from("."),
                step_key: "build".into(),
                deps: vec![],
            })
            .unwrap();
        assert_eq!(lease.provider, PROVIDER_ID);
        assert!(!lease.isolated);
        assert_eq!(provider.lease_history().len(), 1);
        assert!(matches!(
            provider.merge(&[lease.clone()]).unwrap(),
            MergeOutcome::NotRequired
        ));
        provider.release(&lease).unwrap();
    }
}
