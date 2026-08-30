//! 第三方目录提供器:Plugin Host 解析结果经 worker 协议驱动(I7)。
//!
//! 解析层(Plugin Host:完整贡献 ID + 版本 + 内容哈希 → 工厂,含
//! kind/能力/授权校验)与传输层(NDJSON stdio worker / 测试内存传输)
//! 分离;`ExecutionLease`/`LeaseContext`/`MergeOutcome` 与 worker 协议
//! 之间做显式映射,协议错误立即失败(不静默降级)。

use crate::worker::WorkerClient;
use anyhow::{Context as _, Result};
use mf_agent::execution_directory::{
    ensure_lease_under_root, ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use mf_agent::workflow::PluginSourcePin;
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
    /// 提供器自身的插件包 pin(acquire 时盖进租约 metadata,
    /// C7 held-lease 路由依据;None = 未指定(测试)不盖章)。
    pin: Option<PluginSourcePin>,
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
            pin: None,
        }
    }

    /// 同 `new`,但携带插件包 pin(acquire 校验/盖章 provider_pin)。
    pub fn new_with_pin(
        full_contribution_id: &str,
        kind: &str,
        isolates: bool,
        transport: Box<dyn DirectoryWorkerTransport>,
        pin: PluginSourcePin,
    ) -> WorkerDirectoryProvider {
        WorkerDirectoryProvider {
            full_contribution_id: full_contribution_id.to_string(),
            kind: kind.to_string(),
            isolates,
            transport,
            pin: Some(pin),
        }
    }

    /// I8:校验 worker 返回的租约协议边界。
    /// - provider 身份必须与解析层一致(伪造拒绝);
    /// - 路径必须在宿主授予的项目根内(拒绝绝对越界/盘符/前缀相似/
    ///   相对路径/.. 穿越/symlink);
    /// - isolated 必须与清单声明一致(worker 不得自封/自降能力);
    /// - 租约 ID 稳定可用(非空、无路径分隔/穿越、长度有界);
    /// - metadata 里的 provider_pin:无则由宿主盖本提供器 pin(C7 路由
    ///   依据),有则必须与本提供器 pin 一致(伪造拒绝)。
    fn validate_lease(&self, lease: &mut ExecutionLease, ctx: &LeaseContext) -> Result<()> {
        anyhow::ensure!(
            lease.provider == self.full_contribution_id,
            "worker 返回的租约 provider 身份({})与解析层({})不一致,拒绝",
            lease.provider,
            self.full_contribution_id
        );
        anyhow::ensure!(
            lease.isolated == self.isolates,
            "worker 返回的租约 isolated={} 与清单声明({})不一致,拒绝",
            lease.isolated,
            self.isolates
        );
        anyhow::ensure!(
            !lease.id.is_empty() && lease.id.len() <= 128,
            "worker 返回的租约 ID 非法(空或超长): {:?}",
            lease.id
        );
        anyhow::ensure!(
            !lease.id.contains('/')
                && !lease.id.contains('\\')
                && !lease.id.contains(':')
                && lease.id != "..",
            "worker 返回的租约 ID 含路径语义,拒绝: {:?}",
            lease.id
        );
        ensure_lease_under_root(&ctx.project_root, &lease.path)
            .context("worker 返回的租约路径越出宿主授予的项目根")?;
        match (&self.pin, lease.metadata.get("provider_pin")) {
            (None, _) => {}
            (Some(pin), None) => {
                if let Some(obj) = lease.metadata.as_object_mut() {
                    obj.insert(
                        "provider_pin".into(),
                        serde_json::json!({
                            "full_id": pin.full_id,
                            "version": pin.version,
                            "content_hash": pin.content_hash,
                        }),
                    );
                }
            }
            (Some(pin), Some(claimed)) => {
                let claimed: PluginSourcePin = serde_json::from_value(claimed.clone())
                    .context("租约 metadata 的 provider_pin 格式非法")?;
                anyhow::ensure!(
                    claimed == *pin,
                    "worker 返回的租约 provider_pin({claimed:?})与本提供器({pin:?})不一致,拒绝"
                );
            }
        }
        Ok(())
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
        let mut lease: ExecutionLease =
            serde_json::from_value(result).context("worker 返回的租约格式非法")?;
        self.validate_lease(&mut lease, ctx)?;
        Ok(lease)
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
        anyhow::ensure!(
            lease.provider == self.full_contribution_id,
            "拒绝释放他人提供器({})的租约(本提供器: {})",
            lease.provider,
            self.full_contribution_id
        );
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
