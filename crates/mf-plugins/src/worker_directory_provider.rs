//! 第三方目录提供器:Plugin Host 解析结果经 worker 协议驱动(I7)。
//!
//! 解析层(Plugin Host:完整贡献 ID + 版本 + 内容哈希 → 工厂,含
//! kind/能力/授权校验)与传输层(NDJSON stdio worker / 测试内存传输)
//! 分离;`ExecutionLease`/`LeaseContext`/`MergeOutcome` 与 worker 协议
//! 之间做显式映射,协议错误立即失败(不静默降级)。

use crate::worker::WorkerClient;
use anyhow::{Context as _, Result};
use mf_agent::execution_directory::{
    ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use serde_json::Value;

/// 目录提供器 worker 传输层(生产:NDJSON stdio;测试:内存桩)。
pub trait DirectoryWorkerTransport: Send + Sync {
    fn request(&self, method: &str, params: Value) -> Result<Value>;
}

/// 生产传输:NDJSON stdio worker 进程(WorkerClient)。
pub struct StdioWorkerTransport {
    client: parking_lot::Mutex<WorkerClient>,
}

impl StdioWorkerTransport {
    pub fn start(
        command: &str,
        args: &[String],
        plugin_root: &std::path::Path,
    ) -> Result<StdioWorkerTransport> {
        let exe = plugin_root.join(command);
        let client = WorkerClient::start(&exe, args, Some(plugin_root))
            .context("启动目录提供器 worker 失败")?;
        Ok(StdioWorkerTransport {
            client: parking_lot::Mutex::new(client),
        })
    }
}

impl DirectoryWorkerTransport for StdioWorkerTransport {
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.client.lock().request(method, params)
    }
}

/// 第三方目录提供器(经 worker 协议驱动)。
pub struct WorkerDirectoryProvider {
    full_contribution_id: String,
    kind: String,
    isolates: bool,
    transport: Box<dyn DirectoryWorkerTransport>,
}

impl WorkerDirectoryProvider {
    pub fn new(
        full_contribution_id: &str,
        kind: &str,
        isolates: bool,
        transport: Box<dyn DirectoryWorkerTransport>,
    ) -> WorkerDirectoryProvider {
        WorkerDirectoryProvider {
            full_contribution_id: full_contribution_id.to_string(),
            kind: kind.to_string(),
            isolates,
            transport,
        }
    }

    /// 从 Plugin Host 解析结果构造(Worker 工厂 → 启动 worker 进程)。
    pub fn from_resolution(
        resolution: &crate::host::DirectoryProviderResolution,
    ) -> Result<WorkerDirectoryProvider> {
        match &resolution.factory {
            crate::host::DirectoryProviderFactory::Worker {
                command,
                args,
                plugin_root,
            } => {
                let transport = StdioWorkerTransport::start(command, args, plugin_root)?;
                Ok(WorkerDirectoryProvider::new(
                    &resolution.full_contribution_id,
                    &resolution.kind,
                    resolution.isolates,
                    Box::new(transport),
                ))
            }
            crate::host::DirectoryProviderFactory::BuiltinWorktree => {
                anyhow::bail!("内置 worktree 身份由进程内 GitWorktreeProvider 构造,不经 worker")
            }
        }
    }
}

fn ctx_params(ctx: &LeaseContext) -> Value {
    serde_json::json!({
        "task_id": ctx.task_id,
        "step_id": ctx.step_id,
        "revision_id": ctx.revision_id,
        "attempt": ctx.attempt,
        "project_root": ctx.project_root.to_string_lossy(),
        "step_key": ctx.step_key,
        "deps": ctx.deps,
    })
}

impl ExecutionDirectoryProvider for WorkerDirectoryProvider {
    fn id(&self) -> &str {
        &self.full_contribution_id
    }

    fn isolates(&self) -> bool {
        self.isolates
    }

    fn acquire(&self, ctx: &LeaseContext) -> Result<ExecutionLease> {
        let result = self
            .transport
            .request("dir.acquire", ctx_params(ctx))
            .context("目录提供器 worker acquire 失败")?;
        serde_json::from_value(result).context("worker 返回的租约格式非法")
    }

    fn merge(&self, leases: &[ExecutionLease]) -> Result<MergeOutcome> {
        let params = serde_json::json!({ "leases": leases });
        let result = self
            .transport
            .request("dir.merge", params)
            .context("目录提供器 worker merge 失败")?;
        let kind = result
            .get("type")
            .and_then(Value::as_str)
            .context("worker 返回的汇合结果缺 type")?;
        match kind {
            "merged" => Ok(MergeOutcome::Merged),
            "not_required" => Ok(MergeOutcome::NotRequired),
            "needs_user" => {
                let conflicts: Vec<String> = result
                    .get("conflicts")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|c| c.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(MergeOutcome::NeedsUser { conflicts })
            }
            other => anyhow::bail!("worker 返回未知汇合结果 type: {other}"),
        }
    }

    fn release(&self, lease: &ExecutionLease) -> Result<()> {
        let params = serde_json::json!({ "lease": lease });
        self.transport
            .request("dir.release", params)
            .context("目录提供器 worker release 失败")?;
        Ok(())
    }

    fn discard_task_baselines(&self, task_id: i64) -> Result<()> {
        let params = serde_json::json!({ "task_id": task_id });
        self.transport
            .request("dir.discard_baselines", params)
            .context("目录提供器 worker discard_baselines 失败")?;
        Ok(())
    }
}
