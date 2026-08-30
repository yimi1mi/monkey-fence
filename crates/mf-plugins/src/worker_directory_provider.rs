//! 第三方目录提供器:Plugin Host 解析结果经 worker 协议驱动(I7/F10)。
//!
//! 解析层(Plugin Host:完整贡献 ID + 版本 + 内容哈希 → 工厂,含
//! kind/能力/授权校验)与传输层(NDJSON stdio worker / 测试内存传输)
//! 分离;`ExecutionLease`/`LeaseContext`/`MergeOutcome` 与 worker 协议
//! 之间做显式映射,协议错误立即失败(不静默降级)。
//!
//! F10:生产构造**必须**携带完整插件 pin 与宿主授权的项目根
//! (`new_production`);acquire/merge/release 逐租约验证提供器身份、
//! pin、路径边界与租约 ID;worker 返回的租约 metadata 必须是 JSON
//! object(无结构 = 无法盖章归属,拒绝)。

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
    /// C7/F4 held-lease 路由依据)。
    pin: PluginSourcePin,
    /// 宿主授予的项目根(租约路径边界的权威判定;F10)。
    granted_root: std::path::PathBuf,
}

impl WorkerDirectoryProvider {
    /// **生产构造**(F10):完整插件 pin + 宿主授权根。
    /// pin 与 root 是路由/边界校验的依据,缺一不可 —— 生产路径禁止
    /// pin=None 的构造(测试用 `new`/`new_with_pin`,cfg(test))。
    pub fn new_production(
        full_contribution_id: &str,
        kind: &str,
        isolates: bool,
        transport: Box<dyn DirectoryWorkerTransport>,
        pin: PluginSourcePin,
        granted_root: std::path::PathBuf,
    ) -> Result<WorkerDirectoryProvider> {
        anyhow::ensure!(
            !pin.full_id.is_empty() && !pin.version.is_empty(),
            "生产构造必须携带完整插件 pin(贡献 ID 版本): {full_contribution_id}"
        );
        Ok(WorkerDirectoryProvider {
            full_contribution_id: full_contribution_id.to_string(),
            kind: kind.to_string(),
            isolates,
            transport,
            pin,
            granted_root,
        })
    }

    /// 测试构造(无 pin/无授权根语义;生产代码只经 `from_resolution`/
    /// `new_production`,此处 doc(hidden) 仅供测试桩使用)。
    #[doc(hidden)]
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
            pin: PluginSourcePin {
                full_id: String::new(),
                version: String::new(),
                content_hash: String::new(),
            },
            granted_root: std::path::PathBuf::new(),
        }
    }

    /// 测试构造(带 pin;无授权根语义;生产禁止)。
    #[doc(hidden)]
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
            pin,
            granted_root: std::path::PathBuf::new(),
        }
    }

    /// 本提供器的 pin(宿主路由注册用)。
    pub fn provider_pin(&self) -> &PluginSourcePin {
        &self.pin
    }

    /// I8/F10:校验 worker 返回的租约协议边界。
    /// - provider 身份必须与解析层一致(伪造拒绝);
    /// - 路径必须在**宿主授权根**内(拒绝绝对越界/盘符/前缀相似/
    ///   相对路径/.. 穿越/symlink);
    /// - isolated 必须与清单声明一致(worker 不得自封/自降能力);
    /// - 租约 ID 稳定可用(非空、无路径分隔/穿越、长度有界);
    /// - **metadata 必须是 JSON object**(F10:无结构 = 无法盖章
    ///   provider_pin 归属,拒绝);
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
        let granted_root = if self.granted_root.as_os_str().is_empty() {
            // 测试构造未携带授权根:退回上下文项目根(仅 cfg(test) 路径)
            &ctx.project_root
        } else {
            &self.granted_root
        };
        ensure_lease_under_root(granted_root, &lease.path)
            .context("worker 返回的租约路径越出宿主授权的项目根")?;
        anyhow::ensure!(
            lease.metadata.is_object(),
            "worker 返回的租约 metadata 必须是 JSON object(得到 {:?}),无法盖章归属,拒绝",
            lease.metadata
        );
        match lease.metadata.get("provider_pin") {
            None => {
                if let Some(obj) = lease.metadata.as_object_mut() {
                    obj.insert(
                        "provider_pin".into(),
                        serde_json::json!({
                            "full_id": self.pin.full_id,
                            "version": self.pin.version,
                            "content_hash": self.pin.content_hash,
                        }),
                    );
                }
            }
            Some(claimed) => {
                let claimed: PluginSourcePin = serde_json::from_value(claimed.clone())
                    .context("租约 metadata 的 provider_pin 格式非法")?;
                anyhow::ensure!(
                    !self.pin.full_id.is_empty() && claimed == self.pin,
                    "worker 返回的租约 provider_pin({claimed:?})与本提供器({:?})不一致,拒绝",
                    self.pin
                );
            }
        }
        Ok(())
    }

    /// F10:merge/release 的逐租约身份校验 —— 提供器一致、路径在授权根
    /// 内、租约 ID 合法、携带 pin 时与本提供器一致;任一不符整批拒绝,
    /// 绝不把他人提供器/越界路径的租约发给 worker。
    fn validate_existing_lease(&self, lease: &ExecutionLease) -> Result<()> {
        anyhow::ensure!(
            lease.provider == self.full_contribution_id,
            "拒绝处理他人提供器({})的租约(本提供器: {})",
            lease.provider,
            self.full_contribution_id
        );
        anyhow::ensure!(
            !lease.id.is_empty() && lease.id.len() <= 128,
            "租约 ID 非法(空或超长): {:?}",
            lease.id
        );
        anyhow::ensure!(
            !lease.id.contains('/')
                && !lease.id.contains('\\')
                && !lease.id.contains(':')
                && lease.id != "..",
            "租约 ID 含路径语义,拒绝: {:?}",
            lease.id
        );
        if !self.granted_root.as_os_str().is_empty() {
            ensure_lease_under_root(&self.granted_root, &lease.path)
                .context("租约路径越出宿主授权的项目根(拒绝发给 worker)")?;
        }
        if let Some(claimed) = lease.metadata.get("provider_pin") {
            let claimed: PluginSourcePin = serde_json::from_value(claimed.clone())
                .context("租约 metadata 的 provider_pin 格式非法")?;
            // pin 已知(生产构造)才做强等值校验;测试构造空 pin 跳过
            anyhow::ensure!(
                self.pin.full_id.is_empty() || claimed == self.pin,
                "租约 provider_pin({claimed:?})与本提供器({:?})不一致,拒绝",
                self.pin
            );
        }
        Ok(())
    }

    /// 从 Plugin Host 解析结果构造(Worker 工厂 → 启动 worker 进程)。
    /// F10:生产路径 —— 解析结果必须携带完整 pin,且宿主传入授权根。
    pub fn from_resolution(
        resolution: &crate::host::DirectoryProviderResolution,
        granted_root: std::path::PathBuf,
    ) -> Result<WorkerDirectoryProvider> {
        match &resolution.factory {
            crate::host::DirectoryProviderFactory::Worker {
                command,
                args,
                plugin_root,
            } => {
                let transport = StdioWorkerTransport::start(command, args, plugin_root)?;
                WorkerDirectoryProvider::new_production(
                    &resolution.full_contribution_id,
                    &resolution.kind,
                    resolution.isolates,
                    Box::new(transport),
                    resolution.pin.clone(),
                    granted_root,
                )
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
        // F10:逐租约校验后才发给 worker
        for lease in leases {
            self.validate_existing_lease(lease)
                .with_context(|| format!("汇合批租约 `{}` 校验失败", lease.id))?;
        }
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
        // F10:同源校验(pin/根/ID)
        self.validate_existing_lease(lease)?;
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
