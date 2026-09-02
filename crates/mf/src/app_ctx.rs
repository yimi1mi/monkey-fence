//! 应用级上下文:跨项目共享的 Session Registry、插件注册表、全局并发限制、
//! mfctl 管道服务与每个项目的 Orchestrator。
//!
//! 前台只显示一个项目;其他项目的任务和 Agent 在后台继续运行(见 ADR 0001)。

use crate::adapter_launch;
use crate::pipe_server::{pipe_name_for_current_process, PipeServer};
use crate::project_overview::{HubCtx, ProjectOverviewHub};
use crate::runtime_host::{KeepAwake, RuntimeHostImpl, SessionRegistry, WorkflowLauncher};
use anyhow::Result;
use mf_agent::orchestrator::{
    GlobalLimiter, Orchestrator, ProfileCatalog, WorkflowKernel, WorkflowPluginPins,
};
use mf_agent::workflow::{PluginSourcePin, WorkflowTemplateVersion};
use mf_agent::{CatalogStore, Store, TaskStatus};
use mf_kernel::handles::{ClientId, Principal, ProjectStoreHandle};
use mf_kernel::kernel::{
    CoreKernel, InProcessKernelRuntime, KernelOutcome, KernelProblem, LegacyKernelClient,
};
use mf_plugins::PluginRegistry;
use parking_lot::{Mutex, RwLock};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 生产 pin 生命周期:Plugin Host 内容寻址 pin(内置合成插件走源 pin)。
pub(crate) struct PluginHostPins {
    pub(crate) host: Arc<PluginRegistry>,
}

impl WorkflowPluginPins for PluginHostPins {
    fn pin_for_run(&self, run_key: &str, pin: &PluginSourcePin) -> Result<()> {
        self.host
            .pin_source_for_run(run_key, &pin.full_id, &pin.version, &pin.content_hash)
    }
    fn resolve_pin(&self, pin: &PluginSourcePin) -> Result<()> {
        self.host
            .resolve_source_pin(&pin.full_id, &pin.version, &pin.content_hash)
    }
    fn release_run_pins(&self, run_key: &str) -> Result<()> {
        self.host.release_run_pins(run_key)
    }
}

/// 内置 worktree 提供器的固定 pinned 身份:合成插件
/// `monkeyfence.directories` 的 `worktree` 贡献。进程内硬编码的
/// GitWorktreeProvider 只属于这个身份;第三方目录提供器必须走
/// worker/工厂解析(首版未接线),无论命名多相似都不能借名顶替。
pub const WORKTREE_PLUGIN_FULL_ID: &str = "monkeyfence.directories";
pub const WORKTREE_CONTRIBUTION_ID: &str = "worktree";

/// 在执行目录贡献里按**完整 pinned 身份**定位 worktree 提供器:
/// 来源插件必须是内置 `monkeyfence.directories`,贡献 id 与 kind 都精确
/// 是 `worktree`,且隔离/并行声明一致。返回命中的完整贡献 ID(诊断用)。
pub fn worktree_contribution_id(
    directories: &[(
        String,
        mf_plugins::contribution_registry::ContributionSource,
        mf_plugins::contribution_registry::ExecutionDirectoryContribution,
    )],
) -> Option<String> {
    let pinned_full_id = format!("{WORKTREE_PLUGIN_FULL_ID}.{WORKTREE_CONTRIBUTION_ID}");
    directories
        .iter()
        .find(|(full_id, source, contribution)| {
            source.plugin_full_id == WORKTREE_PLUGIN_FULL_ID
                && full_id == &pinned_full_id
                && contribution.id == WORKTREE_CONTRIBUTION_ID
                && contribution.kind == WORKTREE_CONTRIBUTION_ID
                && contribution.isolates
                && contribution.supports_parallel
        })
        .map(|(full_id, _, _)| full_id.clone())
}

/// 解析项目的执行目录提供器(I7:统一经 Plugin Host 的
/// 「完整贡献 ID + 版本 + 内容哈希」解析,不再各自硬编码):
/// - 内置 pinned 身份(monkeyfence.directories.worktree,空哈希)→
///   进程内 GitWorktreeProvider(Git 仓库才可创建);
/// - 第三方目录贡献(内容寻址哈希,按完整贡献 ID 稳定排序取第一个
///   可解析且授权的)→ WorkerDirectoryProvider(worker 进程驱动);
/// - 都不可用 → 内核共享项目目录(并行需显式风险开关)。
/// 返回 (提供器, 其插件包 pin);pin 随 Revision 冻结、派发强校验。
fn directory_provider_for(
    root: &Path,
    plugins: &Arc<PluginRegistry>,
) -> (
    Arc<dyn mf_agent::execution_directory::ExecutionDirectoryProvider>,
    Option<mf_agent::workflow::PluginSourcePin>,
) {
    // 1) 内置 worktree pinned 身份(解析层做精确身份/能力校验)
    let builtin_full = format!("{WORKTREE_PLUGIN_FULL_ID}.{WORKTREE_CONTRIBUTION_ID}");
    if let Ok(res) = plugins.resolve_directory_provider(
        &builtin_full,
        mf_plugins::host::BUILTIN_DIRECTORIES_VERSION,
        "",
    ) {
        if mf_vcs::git::Git::is_repo(root) {
            match mf_plugins::git_worktree_provider::GitWorktreeProvider::new(root.to_path_buf()) {
                Ok(provider) => {
                    log::info!("执行目录提供器:{builtin_full}(worktree 隔离)");
                    return (Arc::new(provider), Some(res.pin.clone()));
                }
                Err(e) => log::warn!("worktree 提供器初始化失败,回退共享目录: {e:#}"),
            }
        }
    }
    // 2) 第三方目录提供器:worker 解析(kind/能力/启用/授权在解析层校验)
    let mut candidates = plugins.contributions().execution_directories();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    for (full_contribution_id, source, contribution) in candidates {
        if source.plugin_full_id == WORKTREE_PLUGIN_FULL_ID {
            continue; // 内置身份只走上面的进程内路径
        }
        match plugins.resolve_directory_provider(
            &full_contribution_id,
            &source.plugin_version,
            &source.content_hash,
        ) {
            Ok(res) => {
                match mf_plugins::worker_directory_provider::WorkerDirectoryProvider::from_resolution(&res, root.to_path_buf()) {
                    Ok(provider) => {
                        log::info!(
                            "执行目录提供器:{full_contribution_id}(worker 驱动,kind {})",
                            contribution.kind
                        );
                        return (
                            Arc::new(provider),
                            Some(res.pin.clone()),
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "第三方目录提供器 {full_contribution_id} worker 启动失败,尝试下一候选: {e:#}"
                        );
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "第三方目录提供器 {full_contribution_id} 解析未通过,尝试下一候选: {e:#}"
                );
            }
        }
    }
    (
        Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
        None,
    )
}

/// C7:按 pinned 身份解析目录提供器(宿主实现):
/// - 内置 worktree pin → 进程内 GitWorktreeProvider(Git 根);
/// - 第三方 pin → 经 Plugin Host 的内容寻址解析 → WorkerDirectoryProvider
///   (旧版本包仍安装在位时可解析);
/// - 均失败 → None(调用方持久 NeedsYou,绝不用当前提供器顶替)。
struct PluginDirectoryResolver {
    root: PathBuf,
    plugins: Arc<PluginRegistry>,
}

impl mf_agent::execution_directory::DirectoryProviderResolver for PluginDirectoryResolver {
    fn resolve(
        &self,
        pin: &mf_agent::workflow::PluginSourcePin,
    ) -> Option<Arc<dyn mf_agent::execution_directory::ExecutionDirectoryProvider>> {
        // 内置 worktree pinned 身份(空哈希)
        if pin.full_id == WORKTREE_PLUGIN_FULL_ID
            && pin.version == mf_plugins::host::BUILTIN_DIRECTORIES_VERSION
            && pin.content_hash.is_empty()
            && pin.contribution_id
                == format!("{WORKTREE_PLUGIN_FULL_ID}.{WORKTREE_CONTRIBUTION_ID}")
        {
            if mf_vcs::git::Git::is_repo(&self.root) {
                if let Ok(provider) =
                    mf_plugins::git_worktree_provider::GitWorktreeProvider::new(self.root.clone())
                {
                    return Some(Arc::new(provider));
                }
            }
            return None;
        }
        // 第三方:内容寻址解析(版本+哈希都由 Plugin Host 校验)
        let full_contribution_id = pin.contribution_id.clone();
        if full_contribution_id.is_empty()
            || !full_contribution_id.starts_with(&format!("{}.", pin.full_id))
        {
            log::warn!("目录提供器 pin({pin:?})缺少或伪造完整贡献 ID");
            return None;
        }
        match self.plugins.resolve_directory_provider(
            &full_contribution_id,
            &pin.version,
            &pin.content_hash,
        ) {
            Ok(res) => {
                match mf_plugins::worker_directory_provider::WorkerDirectoryProvider::from_resolution(&res, self.root.clone()) {
                    Ok(provider) => Some(Arc::new(provider)),
                    Err(e) => {
                        log::warn!("目录提供器 pin({pin:?})worker 启动失败: {e:#}");
                        None
                    }
                }
            }
            Err(e) => {
                log::warn!("目录提供器 pin({pin:?})解析未通过: {e:#}");
                None
            }
        }
    }
}

#[derive(Clone)]
pub struct ProjectHandle {
    root: PathBuf,
    orchestrator: Arc<Orchestrator>,
    kernel_project: Option<ProjectStoreHandle>,
}

impl ProjectHandle {
    /// 项目根路径(只读;`ProjectHandle` 其余字段私有,orchestrator 经
    /// `AppCtx::orchestrator_of` 获取)。
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// 工作流默认 CLI 节点引用前缀:`default-cli:<完整 Agent Type 贡献 ID>`。
/// 只接受完整贡献 ID —— 短 id 无法唯一反查插件包,不得作为新引用。
pub const DEFAULT_CLI_REFERENCE_PREFIX: &str = "default-cli:";

/// 一次工作流运行的 Accepted 定位。Workflow Run 由后台
/// Operation 创建，所以接受命令时尚不存在 Task/Revision rowid；
/// UI 只保留稳定 Operation handle，随后从 Core 投影取得真实 Run。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunTarget {
    pub project_root: PathBuf,
    pub workflow_key: String,
    pub operation_handle: mf_kernel::operation::OperationHandle,
}

/// Orca 式权限物化(自由函数,AppCtx 方法与工作流 resolver 共用):
/// yolo 时返回该 Agent Type 的 yolo 参数,manual 返回空。
fn permission_argv_of(
    plugins: &Arc<PluginRegistry>,
    global_yolo: bool,
    agent_type: &str,
) -> Vec<String> {
    if !global_yolo {
        return Vec::new();
    }
    plugins
        .contributions()
        .agent_types()
        .into_iter()
        // 完整贡献 ID 优先;短 id 仅兼容显式 legacy 内置引用
        .find(|(full_id, _, a)| full_id == agent_type || a.id == agent_type)
        .and_then(|(_, _, a)| mf_plugins::builtin::yolo_args_of(&a.id))
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// 合成默认 CLI 的工作流节点快照(ADR 0004):
/// - 只用插件默认 command + 全局权限参数;不读、不复制外部配置与 Secret;
/// - `external_config = true`(沿用外部已有配置,适配器跳过隔离注入);
/// - 快照随 Revision 冻结,不写入目录库(不创建隐藏持久实例)。
fn synthesize_default_cli_snapshot(
    plugins: &Arc<PluginRegistry>,
    config: &Arc<Mutex<mf_agent::Config>>,
    full_contribution_id: &str,
) -> Result<mf_agent::AgentInstanceSnapshot> {
    let (_, source, contribution) = plugins
        .contributions()
        .agent_types()
        .into_iter()
        .find(|(full_id, _, _)| full_id == full_contribution_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "默认 CLI `{full_contribution_id}` 不存在或所属插件未启用(引用必须是完整贡献 ID)"
            )
        })?;
    let _ = source;
    let enabled = plugins
        .summaries()
        .into_iter()
        .any(|s| s.enabled && s.agents.contains(&contribution.id));
    anyhow::ensure!(enabled, "默认 CLI `{}` 所属插件未启用", contribution.name);
    anyhow::ensure!(
        mf_plugins::builtin::detect_on_path(&contribution.command).is_some(),
        "默认 CLI `{}` 的命令 `{}` 未检测到(先安装或加入 PATH)",
        contribution.name,
        contribution.command
    );
    // 工作流节点是提示驱动的单次执行:优先声明中的 oneshot,
    // 只有交互模式的类型退回 interactive。
    let declared: Vec<mf_agent::RunMode> = contribution
        .modes
        .iter()
        .filter_map(|m| mf_agent::RunMode::parse(m))
        .collect();
    let run_mode = if declared.contains(&mf_agent::RunMode::OneShot) {
        mf_agent::RunMode::OneShot
    } else {
        declared
            .first()
            .copied()
            .unwrap_or(mf_agent::RunMode::OneShot)
    };
    let global_yolo = {
        let cfg = config.lock();
        cfg.agents.permission_mode != "manual"
    };
    Ok(mf_agent::AgentInstanceSnapshot {
        id: format!("{DEFAULT_CLI_REFERENCE_PREFIX}{full_contribution_id}"),
        name: format!("{} 默认 CLI", contribution.name),
        agent_type: full_contribution_id.to_string(),
        version: 0,
        enabled: true,
        run_mode,
        executable: contribution.command.clone(),
        argv: permission_argv_of(plugins, global_yolo, full_contribution_id),
        env: vec![],
        config: serde_json::json!({}),
        // done/进程退出/终端空闲不得自动结算 —— 与离散默认 CLI 一致
        execution_contract: serde_json::json!({ "completion": "manual" }),
        sealed_secret_ids: vec![],
        external_config: true,
    })
}

/// 插件感知的工作流实例解析器(生产注入 WorkflowKernel):
/// - 普通字符串 → 目录库 Agent Instance(既有行为不变);
/// - `default-cli:<完整贡献 ID>` → 合成临时快照(不落库)。
pub(crate) struct PluginInstanceResolver {
    plugins: Arc<PluginRegistry>,
    catalog: Arc<CatalogStore>,
    config: Arc<Mutex<mf_agent::Config>>,
}

impl PluginInstanceResolver {
    pub(crate) fn new(
        plugins: Arc<PluginRegistry>,
        catalog: Arc<CatalogStore>,
        config: Arc<Mutex<mf_agent::Config>>,
    ) -> PluginInstanceResolver {
        PluginInstanceResolver {
            plugins,
            catalog,
            config,
        }
    }
}

impl mf_agent::orchestrator::WorkflowInstanceResolver for PluginInstanceResolver {
    fn resolve(&self, reference: &str) -> Result<mf_agent::AgentInstanceSnapshot> {
        if let Some(full_contribution_id) = reference.strip_prefix(DEFAULT_CLI_REFERENCE_PREFIX) {
            return synthesize_default_cli_snapshot(
                &self.plugins,
                &self.config,
                full_contribution_id,
            );
        }
        self.catalog.snapshot_agent_instance(reference, None)
    }
}

/// 单项目的会话恢复状态(新格式;全部字段带兼容默认)。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProjectSessionState {
    pub root: PathBuf,
    #[serde(default)]
    pub selected_task_id: Option<i64>,
    #[serde(default)]
    pub open_files: Vec<PathBuf>,
    #[serde(default)]
    pub active_file: Option<PathBuf>,
}

/// 上次会话的打开项目(持久化到 ~/.monkeyfence/session.json)。
/// 旧格式只有 `{projects, foreground}`;新格式增加每项目 project_states。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionState {
    pub projects: Vec<PathBuf>,
    pub foreground: Option<PathBuf>,
    #[serde(default)]
    pub project_states: Vec<ProjectSessionState>,
}

/// 纯恢复计划:过滤不存在/不属于项目的文件,校验选中 Task 仍存在。
/// `task_exists` 由调用方注入(查该项目 Orchestrator 的 Store)。
pub fn plan_restore(
    session: &SessionState,
    task_exists: impl Fn(&Path, i64) -> bool,
) -> Vec<ProjectSessionState> {
    session
        .project_states
        .iter()
        .map(|ps| {
            let (root_id, _) = crate::project_context::normalize_project_path(&ps.root);
            let root = root_id.root();
            let mut open_files: Vec<PathBuf> = ps
                .open_files
                .iter()
                .filter_map(|file| {
                    let (file_id, warning) = crate::project_context::normalize_project_path(file);
                    (warning.is_none() && file_id.as_path().starts_with(&root))
                        .then(|| file_id.root())
                })
                .collect();
            let active_file = ps.active_file.as_ref().and_then(|file| {
                let (file_id, warning) = crate::project_context::normalize_project_path(file);
                (warning.is_none() && open_files.contains(&file_id.root())).then(|| file_id.root())
            });
            if let Some(active) = &active_file {
                if let Some(index) = open_files.iter().position(|file| file == active) {
                    let active = open_files.remove(index);
                    open_files.push(active);
                }
            }
            let selected_task_id = ps.selected_task_id.filter(|id| task_exists(&root, *id));
            ProjectSessionState {
                root,
                selected_task_id,
                open_files,
                active_file,
            }
        })
        .collect()
}

/// 恢复前台项目:保存值优先,其次恢复过程中已激活的项目,最后回退到
/// 打开顺序的最后一项。输入路径均应已规范化且来自 `available`。
pub fn choose_restore_project(
    saved: Option<PathBuf>,
    current: Option<PathBuf>,
    available: &[PathBuf],
) -> Option<PathBuf> {
    saved
        .filter(|root| available.contains(root))
        .or_else(|| current.filter(|root| available.contains(root)))
        .or_else(|| available.last().cloned())
}

fn session_path() -> PathBuf {
    // GUI/E2E 可把会话状态隔离到测试目录，避免改写用户真实恢复状态。
    if let Some(path) = std::env::var_os("MONKEYFENCE_SESSION_PATH") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".monkeyfence")
        .join("session.json")
}

/// 离散 CLI 会话继承 Task 目标作为初始提示(空白目标不注入)。
fn apply_task_goal(prompt: &mut Option<String>, goal: &str) {
    if !goal.trim().is_empty() {
        *prompt = Some(goal.to_string());
    }
}

/// T2a CoreKernel tracer：opaque owner runtime + facade-bound legacy client。
/// AppCtx 看不到 service/key/具体 kernel，也不为 rename 持有写 seam。
pub(crate) struct KernelTracer {
    runtime: Arc<InProcessKernelRuntime>,
    client: LegacyKernelClient,
}

fn local_principal() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "local-user".into())
}

fn kernel_internal(error: anyhow::Error) -> KernelProblem {
    KernelProblem::Internal(format!("{error:#}"))
}

/// T3f(Issue #34):transcript durable sink——按 session→project root
/// 路由到对应 Project Store(`terminal_transcript_commit`),Store 句柄
/// LRU 缓存;失败只记日志,绝不回压 PTY reader(§8.2)。
struct StoreTranscriptSink {
    /// project root → Store(project_db_path);上限 LRU。
    stores: parking_lot::Mutex<Vec<(PathBuf, Arc<Store>)>>,
}

const SINK_STORE_CACHE: usize = 8;

impl StoreTranscriptSink {
    fn store_for(&self, root: &Path) -> Option<Arc<Store>> {
        {
            let mut cache = self.stores.lock();
            if let Some(pos) = cache.iter().position(|(path, _)| path == root) {
                let entry = cache.remove(pos);
                cache.push(entry.clone());
                return Some(entry.1);
            }
        }
        let store = Store::open(&mf_agent::project_db_path(root)).ok()?;
        let mut cache = self.stores.lock();
        if cache.len() >= SINK_STORE_CACHE {
            cache.remove(0);
        }
        cache.push((root.to_path_buf(), store.clone()));
        Some(store)
    }
}

impl crate::runtime_host::TranscriptSink for StoreTranscriptSink {
    fn commit(
        &self,
        session_handle: &str,
        project_root: Option<&Path>,
        epoch: &str,
        batch: Option<&mf_terminal::transcript::FlushBatch>,
        final_state: &str,
        durable_through_seq: u64,
        exit_code: Option<i64>,
    ) {
        let Some(root) = project_root else {
            log::warn!("transcript 会话 {session_handle} 无 project 路由,批次丢弃");
            return;
        };
        let Some(store) = self.store_for(root) else {
            log::warn!(
                "transcript 会话 {session_handle} 的 Store 打开失败({}),批次丢弃",
                root.display()
            );
            return;
        };
        let result = store.terminal_transcript_commit(
            session_handle,
            epoch,
            batch.map(|b| mf_agent::store::TranscriptSegment {
                seq_start: b.seq_start as i64,
                seq_end: b.seq_end as i64,
                bytes: &b.bytes,
            }),
            final_state,
            durable_through_seq as i64,
            exit_code,
            None,
        );
        if let Err(error) = result {
            // durable 失败只记日志:reader 不回压;exit 门闩的失败语义
            // 由调用方(生产 exit 链)另行处理。
            log::error!("transcript flush 失败({session_handle}):{error:#}");
        }
    }
}

/// mfctl pipe 的 capability-token Settlement 路由 adapter(T2d)。
/// 惰性解析 CoreKernel tracer:Core 未装配或装配失败 fail-closed,
/// 绝不回退 legacy 直写路径;Weak 引用避免 AppCtx 自持。
struct AppRunControl {
    ctx: std::sync::Weak<AppCtx>,
}

impl mf_kernel::run_control::RunControl for AppRunControl {
    fn execute_agent_run_by_token(
        &self,
        token: &str,
        command: mf_kernel::run_control::RunControlCommand,
        command_id: Option<mf_kernel::handles::CommandId>,
    ) -> Result<mf_kernel::run_control::RunControlOutcome, mf_kernel::run_control::TokenSettleProblem>
    {
        use mf_kernel::run_control::TokenSettleProblem;
        let ctx = self.ctx.upgrade().ok_or_else(|| {
            TokenSettleProblem::Kernel(KernelProblem::ServiceUnavailable(
                "Core runtime 已释放".into(),
            ))
        })?;
        let unavailable = |message: &str| {
            TokenSettleProblem::Kernel(KernelProblem::ServiceUnavailable(message.into()))
        };
        match ctx.ensure_kernel_tracer() {
            Ok(Some(tracer)) => tracer
                .client
                .execute_agent_run_by_token(token, command, command_id),
            Ok(None) => Err(unavailable("CoreKernel runtime 未装配")),
            Err(error) => Err(TokenSettleProblem::Kernel(error)),
        }
    }
}

/// Hub 后台线程的 Core Kernel 只读投影源(Issue #26):经 AppCtx facade
/// 读取权威 Snapshot。未装配/未登记时返回 None,Hub 回退旧 Store 扫描
/// (仅测试回滚模式;生产 tracer 总在且项目在 open_project 时登记)。
struct KernelProjectionAdapter {
    app: std::sync::Weak<AppCtx>,
}

impl crate::project_overview::KernelProjectionSource for KernelProjectionAdapter {
    fn workspace(
        &self,
    ) -> Option<Result<crate::project_overview::KernelWorkspaceReads, KernelProblem>> {
        let app = self.app.upgrade()?;
        let root_of_project = app.kernel_project_roots();
        if root_of_project.is_empty() {
            return None;
        }
        match app.workspace_snapshot_via_kernel() {
            Some(Ok(envelope)) => match envelope.data {
                mf_kernel::projection::SnapshotData::Workspace(data) => Some(Ok(
                    crate::project_overview::KernelWorkspaceReads::new(data, root_of_project),
                )),
                _ => Some(Err(KernelProblem::Internal(
                    "Core 返回了错误的 Snapshot 类型".into(),
                ))),
            },
            Some(Err(error)) => Some(Err(error)),
            None => None,
        }
    }

    fn workflow_run(
        &self,
        root: &Path,
        workflow_run: &mf_kernel::handles::WorkflowRunHandle,
    ) -> Option<Result<mf_kernel::projection::WorkflowRunSnapshotData, KernelProblem>> {
        let app = self.app.upgrade()?;
        match app.workflow_run_snapshot_via_kernel(root, workflow_run) {
            Some(Ok(envelope)) => match envelope.data {
                mf_kernel::projection::SnapshotData::WorkflowRun(data) => Some(Ok(data)),
                _ => Some(Err(KernelProblem::Internal(
                    "Core 返回了错误的 Snapshot 类型".into(),
                ))),
            },
            Some(Err(error)) => Some(Err(error)),
            None => None,
        }
    }
}

pub struct AppCtx {
    /// T2f(Issue #28):全部字段私有。外部读经访问器;终端写输入必须经
    /// `attach_terminal` 返回的 `TerminalChannel`,Workflow/Run/Settlement
    /// 写路径经 CoreKernel dispatch。
    registry: Arc<SessionRegistry>,
    plugins: Arc<PluginRegistry>,
    /// 用户级目录库(Agent Instance、模板、Secret、插件包;~/.monkeyfence/catalog-v1.db)。
    /// 读写 API 随 Agent Instance / Secret Store 里程碑接入。
    #[allow(dead_code)]
    catalog_store: Arc<CatalogStore>,
    limiter: Arc<GlobalLimiter>,
    keep_awake: Arc<KeepAwake>,
    catalog: Arc<RwLock<ProfileCatalog>>,
    config: Arc<Mutex<mf_agent::Config>>,
    /// 统一项目总览快照 + Orchestrator Event Hub(UI 不再直接扫描项目列表)。
    overview: Arc<ProjectOverviewHub>,
    /// 已打开项目(私有:外部走 overview snapshot / 查询方法)。
    projects: Mutex<Vec<ProjectHandle>>,
    /// Secret Store 主密钥覆盖(None = 生产 OS keyring;测试注入确定性密钥)。
    secret_master_key: Mutex<Option<[u8; 32]>>,
    #[allow(dead_code)]
    pipe: Mutex<Option<PipeServer>>,
    pipe_orchestrators: Option<Arc<Mutex<Vec<Arc<Orchestrator>>>>>,
    /// T2a CoreKernel facade tracer(惰性装配;None = 禁用或装配失败,
    /// 此时 rename 走旧入口)。AppCtx 只持有 facade,不直接写 Store。
    kernel: Mutex<Option<Arc<KernelTracer>>>,
    kernel_error: Mutex<Option<String>>,
    /// 测试专用旧 bundle 模式；生产无运行时 Store 直写 rollback。
    #[cfg(test)]
    kernel_tracer_enabled: AtomicBool,
}

impl AppCtx {
    // ---------- T2f 字段私有化访问器(只读) ----------

    /// Agent Session 注册表(读投影:tail/snapshot/alive)。终端**写输入**
    /// 必须经 `attach_terminal` 返回的 `TerminalChannel`。
    pub fn registry(&self) -> &Arc<SessionRegistry> {
        &self.registry
    }

    pub fn plugins(&self) -> &Arc<PluginRegistry> {
        &self.plugins
    }

    pub fn catalog_store(&self) -> &Arc<CatalogStore> {
        &self.catalog_store
    }

    pub fn limiter(&self) -> &Arc<GlobalLimiter> {
        &self.limiter
    }

    pub fn keep_awake(&self) -> &Arc<KeepAwake> {
        &self.keep_awake
    }

    pub fn catalog(&self) -> &Arc<RwLock<ProfileCatalog>> {
        &self.catalog
    }

    /// 引擎/Agent 配置快照(深拷贝;写经 `apply_engine_settings`)。
    pub fn config_snapshot(&self) -> mf_agent::Config {
        self.config.lock().clone()
    }

    pub fn overview(&self) -> &Arc<ProjectOverviewHub> {
        &self.overview
    }

    /// 设置保存后的引擎配置传播(并发上限、注册表配置、防休眠)。
    /// T2f:字段私有化后这是唯一的配置应用入口。
    pub fn apply_engine_settings(&self, config: mf_agent::Config) {
        self.limiter
            .set_max(config.engine.global_concurrency.max(1));
        *self.config.lock() = config.clone();
        self.registry.update_config(config.clone());
        self.keep_awake.set_enabled(config.agents.keep_awake);
    }

    /// T2f(Issue #28):终端通道唯一 UI 入口。经 CoreKernel
    /// `attach_terminal` 委托 TerminalHost shim(legacy SessionRegistry);
    /// 调用者拿到 `TerminalChannel`,不接触 raw writer/`PtyMaster`。
    pub fn attach_terminal(
        &self,
        session_handle: &str,
        after_seq: u64,
    ) -> Result<mf_terminal::TerminalChannel, KernelProblem> {
        // shim 阶段沿用 legacy 会话键空间(display handle/裸 UUIDv7);
        // 存在性由 TerminalHost 判定,格式收敛随 T3 handle 空间统一。
        let session = mf_kernel::handles::SessionHandle::from_opaque(session_handle);
        let tracer = self
            .ensure_kernel_tracer()?
            .ok_or_else(|| KernelProblem::ServiceUnavailable("CoreKernel runtime 未装配".into()))?;
        let kernel = tracer.runtime.kernel();
        let registry = self.registry.clone();
        kernel.ensure_terminal_host(move || registry as Arc<dyn mf_terminal::TerminalHost>);
        kernel.attach_terminal(session, mf_kernel::kernel::TerminalAttach { after_seq })
    }

    pub fn new() -> Arc<AppCtx> {
        let config = mf_agent::Config::load().unwrap_or_default();
        let catalog_store = CatalogStore::open_default().unwrap_or_else(|e| {
            // 目录库打不开不阻塞启动(插件页/实例页后续访问时再暴露错误),
            // 但必须留下日志,不允许静默降级到无提示状态。
            log::error!("目录库打开失败: {e:#}");
            CatalogStore::memory().expect("内存目录库初始化不可能失败")
        });
        Self::with_parts(config, catalog_store)
    }

    /// 以给定配置与目录库组装(生产 new 与测试共用同一装配链)。
    pub fn with_parts(config: mf_agent::Config, catalog_store: Arc<CatalogStore>) -> Arc<AppCtx> {
        Self::with_parts_opt(config, catalog_store, true)
    }

    ///  时不启动 mfctl 管道服务(测试并行时避免
    /// 与被测管道服务器抢注同名管道实例)。
    pub fn with_parts_opt(
        config: mf_agent::Config,
        catalog_store: Arc<CatalogStore>,
        start_pipe: bool,
    ) -> Arc<AppCtx> {
        let skills = mf_skills::load_skills(None);
        let plugins = PluginRegistry::load_with_catalog(catalog_store.clone(), &config, &skills);
        Self::with_parts_and_plugins(config, catalog_store, plugins, start_pipe)
    }

    /// 测试装配:注入自定义插件注册表(临时根;不触用户 ~/.monkeyfence)。
    /// 必须先于 open_project 调用(RuntimeHost/overview 都在打开项目时接线)。
    #[cfg(test)]
    pub fn with_parts_and_plugins_for_tests(
        config: mf_agent::Config,
        catalog_store: Arc<CatalogStore>,
        plugins: Arc<PluginRegistry>,
    ) -> Arc<AppCtx> {
        Self::with_parts_and_plugins(config, catalog_store, plugins, false)
    }

    /// 装配内核(生产与测试共用;plugins 由调用方决定来源)。
    fn with_parts_and_plugins(
        config: mf_agent::Config,
        catalog_store: Arc<CatalogStore>,
        plugins: Arc<PluginRegistry>,
        start_pipe: bool,
    ) -> Arc<AppCtx> {
        let skills = mf_skills::load_skills(None);
        let registry = SessionRegistry::new(config.clone());
        // T3f:transcript durable flush 注入(§8.2;按 project root 路由)。
        registry.set_transcript_sink(std::sync::Arc::new(StoreTranscriptSink {
            stores: parking_lot::Mutex::new(Vec::new()),
        }));
        let limiter = GlobalLimiter::new(config.engine.global_concurrency.max(1));
        let keep_awake = Arc::new(KeepAwake::new());
        keep_awake.set_enabled(config.agents.keep_awake);
        let catalog = Arc::new(RwLock::new(ProfileCatalog::default()));
        let orchs: Arc<Mutex<Vec<Arc<Orchestrator>>>> = Arc::new(Mutex::new(Vec::new()));
        let ctx = Arc::new(AppCtx {
            registry: registry.clone(),
            plugins: plugins.clone(),
            catalog_store,
            limiter: limiter.clone(),
            keep_awake: keep_awake.clone(),
            catalog: catalog.clone(),
            config: Arc::new(Mutex::new(config)),
            overview: ProjectOverviewHub::new(Arc::new(HubCtx {
                registry,
                catalog: catalog.clone(),
                plugins: plugins.clone(),
                limiter: limiter.clone(),
                keep_awake: keep_awake.clone(),
                kernel: Arc::new(parking_lot::RwLock::new(None)),
            })),
            projects: Mutex::new(Vec::new()),
            secret_master_key: Mutex::new(None),
            pipe: Mutex::new(None),
            pipe_orchestrators: Some(orchs.clone()),
            kernel: Mutex::new(None),
            kernel_error: Mutex::new(None),
            #[cfg(test)]
            kernel_tracer_enabled: AtomicBool::new(false),
        });
        ctx.refresh_catalog();
        // mfctl pipe(T2d):settlement 经 AppRunControl 惰性路由到 Core;
        // 必须在 AppCtx 建成后启动(adapter 持 Weak 引用)。
        if start_pipe {
            let run_control: Arc<dyn mf_kernel::run_control::RunControl> =
                Arc::new(AppRunControl {
                    ctx: Arc::downgrade(&ctx),
                });
            let server = PipeServer::start(Some(run_control)).ok();
            if server.is_none() {
                log::warn!("mfctl 管道服务启动失败(结算将不可用)");
            } else {
                *ctx.pipe.lock() = server;
            }
        }
        // 总览 Hub 的 Kernel 只读投影源(Issue #26):weak 引用避免循环持有
        ctx.overview
            .set_kernel_source(Arc::new(KernelProjectionAdapter {
                app: Arc::downgrade(&ctx),
            }));
        ctx
    }

    /// 测试构造:独立目录库 + 确定性 Secret 主密钥(不触 OS keyring、
    /// 不碰用户真实 ~/.monkeyfence)。
    pub fn with_catalog_for_tests(catalog: Arc<CatalogStore>) -> Arc<AppCtx> {
        let ctx = Self::with_parts_opt(mf_agent::Config::default(), catalog, false);
        ctx.set_secret_master_key_for_tests([7u8; 32]);
        ctx
    }

    /// 注入确定性 Secret 主密钥(seal/unseal/工作流派发编译共用;
    /// 生产不调用,走 OS keyring)。必须在 open_project 之前调用。
    pub fn set_secret_master_key_for_tests(&self, key: [u8; 32]) {
        *self.secret_master_key.lock() = Some(key);
    }

    // ---------- T2a CoreKernel tracer(Issue #23,workflow.rename) ----------

    /// 惰性装配并返回 tracer。只有显式 rollback 开关关闭时返回 None；
    /// owner/service/keyring 装配失败必须 fail-closed，禁止自动回退直写。
    fn ensure_kernel_tracer(&self) -> Result<Option<Arc<KernelTracer>>, KernelProblem> {
        let mut guard = self.kernel.lock();
        if let Some(tracer) = guard.as_ref() {
            return Ok(Some(tracer.clone()));
        }
        #[cfg(test)]
        if !self.kernel_tracer_enabled.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if let Some(error) = self.kernel_error.lock().clone() {
            return Err(KernelProblem::ServiceUnavailable(error));
        }
        let tracer = match Self::build_production_kernel_tracer() {
            Ok(tracer) => Arc::new(tracer),
            Err(error) => {
                let message = error.to_string();
                *self.kernel_error.lock() = Some(message.clone());
                return Err(KernelProblem::ServiceUnavailable(message));
            }
        };
        *guard = Some(tracer.clone());
        Ok(Some(tracer))
    }

    /// 生产装配由 mf-kernel opaque runtime 完成：L-OWNER → service/keyring
    /// → facade/client。AppCtx 不接触 service/key 或具体 kernel。
    fn build_production_kernel_tracer() -> Result<KernelTracer> {
        let client_id = ClientId::parse("legacy-gpui-inproc")?;
        let principal = Principal::parse(local_principal())?;
        let (runtime, client) = InProcessKernelRuntime::acquire_default(
            env!("CARGO_PKG_VERSION"),
            client_id,
            principal,
        )
        .map_err(|error| anyhow::anyhow!(error))?;
        Ok(KernelTracer { runtime, client })
    }

    /// 测试注入:绕过生产装配(service DB/keyring)安装 tracer。
    /// 必须在 open_project 之前调用。
    #[cfg(test)]
    pub(crate) fn install_kernel_tracer_for_tests(
        &self,
        runtime: Arc<InProcessKernelRuntime>,
        client: LegacyKernelClient,
    ) {
        *self.kernel.lock() = Some(Arc::new(KernelTracer { runtime, client }));
        *self.kernel_error.lock() = None;
        self.kernel_tracer_enabled.store(true, Ordering::SeqCst);
    }

    /// 测试注入:模拟回滚开关(卸载 tracer,rename 回旧入口)。
    #[cfg(test)]
    pub(crate) fn disable_kernel_tracer_for_tests(&self) {
        *self.kernel.lock() = None;
        *self.kernel_error.lock() = None;
        self.kernel_tracer_enabled.store(false, Ordering::SeqCst);
    }

    /// `workflow.rename` tracer 的 UI 入口:经 facade dispatch 写入。
    /// 生产总是返回 Some；None 仅供 cfg(test) 验证旧 bundle 数据兼容。
    /// 装配/登记/dispatch 失败均返回 Some(Err)，禁止自动直写 Store。
    pub fn rename_workflow_via_kernel(
        &self,
        root: &Path,
        workflow_key: &str,
        new_name: &str,
    ) -> Option<Result<KernelOutcome, KernelProblem>> {
        let tracer = match self.ensure_kernel_tracer() {
            Ok(Some(tracer)) => tracer,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        let project = self
            .projects
            .lock()
            .iter()
            .find(|project| project.root == root)
            .and_then(|project| project.kernel_project.clone());
        let Some(project) = project else {
            return Some(Err(KernelProblem::ServiceUnavailable(
                "Project Store 未完成 kernel 登记".into(),
            )));
        };
        Some(
            tracer
                .client
                .rename_workflow(&project, workflow_key, new_name),
        )
    }

    pub fn project_workflow_via_kernel(
        &self,
        root: &Path,
        build: impl FnOnce(ProjectStoreHandle) -> mf_kernel::kernel::ProjectWorkflowCommand,
    ) -> Option<Result<KernelOutcome, KernelProblem>> {
        let tracer = match self.ensure_kernel_tracer() {
            Ok(Some(tracer)) => tracer,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        let project = self
            .projects
            .lock()
            .iter()
            .find(|project| project.root == root)
            .and_then(|project| project.kernel_project.clone());
        let Some(project) = project else {
            return Some(Err(KernelProblem::ServiceUnavailable(
                "Project Store 未完成 kernel 登记".into(),
            )));
        };
        Some(tracer.client.dispatch_project_workflow(build(project)))
    }

    pub fn kernel_project_handle(&self, root: &Path) -> Option<ProjectStoreHandle> {
        self.projects
            .lock()
            .iter()
            .find(|project| project.root == root)
            .and_then(|project| project.kernel_project.clone())
    }

    /// Core 已登记项目的 ProjectStoreHandle → 项目根映射(总览 Hub 的
    /// Kernel Workspace 投影接线用;只读定位,不含业务事实)。
    fn kernel_project_roots(&self) -> std::collections::HashMap<String, PathBuf> {
        self.projects
            .lock()
            .iter()
            .filter_map(|project| {
                project
                    .kernel_project
                    .as_ref()
                    .map(|handle| (handle.as_str().to_owned(), project.root.clone()))
            })
            .collect()
    }

    pub fn dispatch_project_workflow_via_kernel(
        &self,
        command: mf_kernel::kernel::ProjectWorkflowCommand,
    ) -> Option<Result<KernelOutcome, KernelProblem>> {
        let tracer = match self.ensure_kernel_tracer() {
            Ok(Some(tracer)) => tracer,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        Some(tracer.client.dispatch_project_workflow(command))
    }

    pub fn workflow_snapshot_via_kernel(
        &self,
        root: &Path,
        workflow_key: &str,
    ) -> Option<Result<mf_kernel::projection::SnapshotEnvelope, KernelProblem>> {
        let tracer = match self.ensure_kernel_tracer() {
            Ok(Some(value)) => value,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        let project = self.kernel_project_handle(root)?;
        Some(tracer.client.workflow_snapshot(&project, workflow_key))
    }

    pub fn workspace_snapshot_via_kernel(
        &self,
    ) -> Option<Result<mf_kernel::projection::SnapshotEnvelope, KernelProblem>> {
        let tracer = match self.ensure_kernel_tracer() {
            Ok(Some(value)) => value,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        Some(tracer.client.workspace_snapshot())
    }

    pub fn workflow_run_snapshot_via_kernel(
        &self,
        root: &Path,
        workflow_run: &mf_kernel::handles::WorkflowRunHandle,
    ) -> Option<Result<mf_kernel::projection::SnapshotEnvelope, KernelProblem>> {
        let tracer = match self.ensure_kernel_tracer() {
            Ok(Some(value)) => value,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        let project = self.kernel_project_handle(root)?;
        Some(tracer.client.workflow_run_snapshot(&project, workflow_run))
    }

    /// Core Workflow Run 只读投影是否可用(tracer 已装配且项目已登记)。
    /// UI 读路径(`RunMonitorSnapshot::collect_via_kernel`)仅在此为 false
    /// 时回退旧 Store 投影(测试回滚模式);生产 open_project 成功即恒为
    /// true——除此之外的读取失败必须 fail-visible,不得静默回退。
    pub fn workflow_run_projection_ready(&self, root: &Path) -> bool {
        match self.ensure_kernel_tracer() {
            Ok(Some(_)) => self.kernel_project_handle(root).is_some(),
            Ok(None) | Err(_) => false,
        }
    }

    pub fn cancel_workflow_run_via_kernel(
        &self,
        root: &Path,
        task_id: i64,
    ) -> Result<KernelOutcome, KernelProblem> {
        let tracer = self.required_kernel_tracer()?;
        let project = self.required_kernel_project(root)?;
        let orchestrator = self
            .orchestrator_of(root)
            .ok_or(KernelProblem::ResourceNotFound)?;
        let task = orchestrator
            .store
            .task_view(task_id)
            .map_err(kernel_internal)?
            .ok_or(KernelProblem::ResourceNotFound)?;
        let handle = mf_kernel::handles::WorkflowRunHandle::parse(task.public_handle)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        tracer.client.cancel_workflow_run(&project, &handle)
    }

    pub fn retry_workflow_step_via_kernel(
        &self,
        root: &Path,
        step_id: i64,
        mode: mf_agent::RetryMode,
    ) -> Result<KernelOutcome, KernelProblem> {
        let tracer = self.required_kernel_tracer()?;
        let project = self.required_kernel_project(root)?;
        let orchestrator = self
            .orchestrator_of(root)
            .ok_or(KernelProblem::ResourceNotFound)?;
        let step = orchestrator
            .store
            .step_view(step_id)
            .map_err(kernel_internal)?
            .ok_or(KernelProblem::ResourceNotFound)?;
        let task = orchestrator
            .store
            .task_view(step.task_id)
            .map_err(kernel_internal)?
            .ok_or(KernelProblem::ResourceNotFound)?;
        let workflow_run = mf_kernel::handles::WorkflowRunHandle::parse(task.public_handle)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        let step = mf_kernel::handles::StepHandle::parse(step.public_handle)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        tracer
            .client
            .retry_workflow_step(&project, &workflow_run, &step, mode)
    }

    pub fn skip_workflow_step_via_kernel(
        &self,
        root: &Path,
        step_id: i64,
    ) -> Result<KernelOutcome, KernelProblem> {
        let tracer = self.required_kernel_tracer()?;
        let project = self.required_kernel_project(root)?;
        let orchestrator = self
            .orchestrator_of(root)
            .ok_or(KernelProblem::ResourceNotFound)?;
        let step = orchestrator
            .store
            .step_view(step_id)
            .map_err(kernel_internal)?
            .ok_or(KernelProblem::ResourceNotFound)?;
        let task = orchestrator
            .store
            .task_view(step.task_id)
            .map_err(kernel_internal)?
            .ok_or(KernelProblem::ResourceNotFound)?;
        let workflow_run = mf_kernel::handles::WorkflowRunHandle::parse(task.public_handle)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        let step = mf_kernel::handles::StepHandle::parse(step.public_handle)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        tracer
            .client
            .skip_workflow_step(&project, &workflow_run, &step)
    }

    pub fn settle_agent_run_via_kernel(
        &self,
        root: &Path,
        run_id: i64,
        settlement: mf_agent::Settlement,
    ) -> Result<KernelOutcome, KernelProblem> {
        let tracer = self.required_kernel_tracer()?;
        let project = self.required_kernel_project(root)?;
        let orchestrator = self
            .orchestrator_of(root)
            .ok_or(KernelProblem::ResourceNotFound)?;
        let run = orchestrator
            .store
            .run_view(run_id)
            .map_err(kernel_internal)?
            .ok_or(KernelProblem::ResourceNotFound)?;
        let task = orchestrator
            .store
            .task_view(run.task_id)
            .map_err(kernel_internal)?
            .ok_or(KernelProblem::ResourceNotFound)?;
        let workflow_run = mf_kernel::handles::WorkflowRunHandle::parse(task.public_handle)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        let agent_run = mf_kernel::handles::AgentRunHandle::parse(run.public_handle)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?;
        tracer
            .client
            .settle_agent_run(&project, &workflow_run, &agent_run, settlement)
    }

    fn required_kernel_tracer(&self) -> Result<Arc<KernelTracer>, KernelProblem> {
        self.ensure_kernel_tracer()?
            .ok_or_else(|| KernelProblem::ServiceUnavailable("CoreKernel runtime 未装配".into()))
    }

    fn required_kernel_project(&self, root: &Path) -> Result<ProjectStoreHandle, KernelProblem> {
        self.kernel_project_handle(root).ok_or_else(|| {
            KernelProblem::ServiceUnavailable("Project Store 未完成 kernel 登记".into())
        })
    }

    /// 打开的项目数。
    pub fn project_count(&self) -> usize {
        self.projects.lock().len()
    }

    /// 当前打开项目根目录快照。设置页用首个项目作为 VCS 环境测试 cwd；
    /// 不暴露内部 ProjectHandle，也不改变前后台项目状态。
    pub fn project_roots(&self) -> Vec<PathBuf> {
        self.projects
            .lock()
            .iter()
            .map(|project| project.root.clone())
            .collect()
    }

    /// 插件注册表 → Orchestrator 的 Profile 目录投影。
    pub fn refresh_catalog(&self) {
        self.plugins.refresh_detection();
        let profiles = self.plugins.agent_profiles();
        let mut catalog = self.catalog.write();
        let mut index = mf_agent::pipeline::ProfileIndex::default();
        let mut specs = std::collections::HashMap::new();
        for p in &profiles {
            // 检测到的才可用于调度;空白终端始终可用
            let detected = self
                .plugins
                .summaries()
                .iter()
                .any(|s| s.enabled && s.agents.contains(&p.id))
                && (p.runtime != mf_agent::runtime::RuntimeKind::Pty
                    || p.id == "blank-terminal"
                    || mf_plugins::builtin::detect_on_path(&p.command).is_some());
            index.entries.insert(
                p.id.clone(),
                mf_agent::pipeline::ProfileAvailability {
                    installed: true,
                    enabled: true,
                    detected,
                },
            );
            specs.insert(p.id.clone(), p.clone());
        }
        catalog.index = index;
        catalog.specs = specs;
        drop(catalog);
        self.overview.request_refresh();
    }

    /// 打开(或复用)一个项目:Store 初始化 + Orchestrator 启动。
    /// 使用全新 `workflow-v1.db` 命名空间,不读取旧 `orchestration.db`。
    pub fn open_project(&self, root: PathBuf) -> Result<Arc<Orchestrator>> {
        {
            let projects = self.projects.lock();
            if let Some(p) = projects.iter().find(|p| p.root == root) {
                return Ok(p.orchestrator.clone());
            }
        }
        // Core owner 必须先于 authoritative Project Store 打开。
        let tracer = self
            .ensure_kernel_tracer()
            .map_err(|error| anyhow::anyhow!(error))?;
        let (kernel_project, store) = if let Some(tracer) = &tracer {
            let project = tracer.runtime.open_project(&root)?;
            (Some(project.handle().clone()), project.legacy_store())
        } else {
            #[cfg(test)]
            {
                (None, Store::open(&mf_agent::project_db_path(&root))?)
            }
            #[cfg(not(test))]
            {
                return Err(anyhow::anyhow!("CoreKernel runtime 未装配"));
            }
        };
        let config = self.config.lock().clone();
        let host = RuntimeHostImpl::with_launcher(
            self.registry.clone(),
            WorkflowLauncher {
                plugins: self.plugins.clone(),
                catalog: self.catalog_store.clone(),
                secret_master_key: *self.secret_master_key.lock(),
            },
        );
        // 目录提供器:统一经 Plugin Host 解析(内置 worktree / 第三方
        // worker / 共享目录);pin 随 Revision 冻结、派发强校验。
        // C7:current_pin 与按 pin 解析历史版本的 resolver 在启动
        //(含 held-lease 恢复冲刷、任何调度线程)**之前**注入。
        let (directory, directory_pin) = directory_provider_for(&root, &self.plugins);
        let instance_resolver = Arc::new(PluginInstanceResolver::new(
            self.plugins.clone(),
            self.catalog_store.clone(),
            self.config.clone(),
        ));
        let orch = Orchestrator::start_with_routing(
            store,
            root.clone(),
            config,
            host,
            self.catalog.clone(),
            self.limiter.clone(),
            pipe_name_for_current_process(),
            directory.clone(),
            WorkflowKernel {
                catalog: self.catalog_store.clone(),
                pins: Some(Arc::new(PluginHostPins {
                    host: self.plugins.clone(),
                })),
                // 插件感知实例解析:default-cli:<完整贡献 ID> 保留引用
                // 在此合成快照;普通实例引用仍走目录库。
                instance_resolver: Some(instance_resolver.clone()),
            },
            mf_agent::orchestrator::DirectoryRouting {
                current_pin: directory_pin.clone(),
                resolver: Some(Arc::new(PluginDirectoryResolver {
                    root: root.clone(),
                    plugins: self.plugins.clone(),
                })),
            },
        );
        let orch = match orch {
            Ok(orch) => orch,
            Err(error) => {
                if let (Some(tracer), Some(project)) = (&tracer, &kernel_project) {
                    if let Err(unregister) = tracer.runtime.unregister_project_store(project) {
                        return Err(anyhow::anyhow!(
                            "Orchestrator 启动失败:{error:#}; CoreKernel 注销也失败:{unregister}"
                        ));
                    }
                }
                return Err(error);
            }
        };
        if let (Some(tracer), Some(project)) = (&tracer, &kernel_project) {
            let port = Arc::new(
                crate::run_lifecycle_port::OrchestratorRunLifecyclePort::new(orch.clone()),
            );
            if let Err(error) = tracer.runtime.register_run_lifecycle_port(project, port) {
                orch.stop();
                if let Err(unregister) = tracer.runtime.unregister_project_store(project) {
                    return Err(anyhow::anyhow!(
                        "Run lifecycle port 注册失败:{error}; CoreKernel 注销也失败:{unregister}"
                    ));
                }
                return Err(anyhow::anyhow!("Run lifecycle port 注册失败:{error}"));
            }
            let start_port = Arc::new(
                crate::workflow_start_port::OrchestratorWorkflowStartPort::new(
                    orch.clone(),
                    adapter_launch::workflow_plugin_index(&self.plugins),
                    instance_resolver,
                    &directory,
                    directory_pin.clone(),
                ),
            );
            if let Err(error) = tracer
                .runtime
                .register_workflow_start_port(project, start_port)
            {
                tracer.runtime.unregister_run_lifecycle_port(project);
                orch.stop();
                if let Err(unregister) = tracer.runtime.unregister_project_store(project) {
                    return Err(anyhow::anyhow!(
                        "Workflow Start port 注册失败:{error}; CoreKernel 注销也失败:{unregister}"
                    ));
                }
                return Err(anyhow::anyhow!("Workflow Start port 注册失败:{error}"));
            }
        }
        self.projects.lock().push(ProjectHandle {
            root: root.clone(),
            orchestrator: orch.clone(),
            kernel_project,
        });
        // Event Hub:持续消费该 Orchestrator 的 UI 事件并构建 overview
        self.overview.attach(root, orch.clone());
        self.sync_pipe_routing();
        Ok(orch)
    }

    /// 关闭项目：先 freeze Start/普通命令，再用只能取消该 Project
    /// Workflow Run 的 close token 执行 durable drain。任一 cancel 失败
    /// 保持 closing，重试继续 drain；绝不回退 Orchestrator 直写。
    pub fn try_close_project(&self, root: &PathBuf) -> Result<()> {
        let handle = {
            let projects = self.projects.lock();
            projects
                .iter()
                .find(|project| &project.root == root)
                .cloned()
        };
        if let Some(h) = handle {
            let tracer = self.kernel.lock().clone();
            let project = h.kernel_project.clone();
            let (tracer, project) = match (tracer, project) {
                (Some(tracer), Some(project)) => (tracer, project),
                #[cfg(test)]
                _ => {
                    // 旧回归 fixture 可显式不装配 Kernel；此处只停止已由
                    // 测试自行收口的 Orchestrator，不执行业务 mutation。
                    h.orchestrator.stop();
                    self.overview.detach(&h.root);
                    self.projects
                        .lock()
                        .retain(|candidate| candidate.root != h.root);
                    self.sync_pipe_routing();
                    return Ok(());
                }
                #[cfg(not(test))]
                _ => return Err(anyhow::anyhow!("CoreKernel runtime 未装配，已中止关闭")),
            };

            // L-CLOSE-FREEZE 先于任何扫描/drain：已 Accepted 但未执行
            // 的 Start Operation 在此收口，不会在重开 Project 后复活。
            let close_token = tracer
                .runtime
                .prepare_project_close(&project)
                .map_err(|error| anyhow::anyhow!("CoreKernel 准备关闭 Project 失败:{error}"))?;
            tracer.runtime.unregister_workflow_start_port(&project);

            let active_workflow_runs = h
                .orchestrator
                .tasks()?
                .into_iter()
                .filter(|task| {
                    task.active_revision.is_some()
                        && matches!(
                            task.status,
                            TaskStatus::Ready | TaskStatus::Running | TaskStatus::NeedsYou
                        )
                })
                .map(|task| {
                    mf_kernel::handles::WorkflowRunHandle::parse(task.public_handle)
                        .map_err(|error| anyhow::anyhow!("Workflow Run handle 损坏:{error}"))
                })
                .collect::<Result<Vec<_>>>()?;
            for workflow_run in &active_workflow_runs {
                let mut last_conflict = None;
                let mut cancelled = false;
                for _ in 0..3 {
                    match tracer
                        .client
                        .cancel_workflow_run_for_close(&close_token, workflow_run)
                    {
                        Ok(_) => {
                            cancelled = true;
                            break;
                        }
                        Err(KernelProblem::RevisionConflict) => {
                            last_conflict = Some(KernelProblem::RevisionConflict)
                        }
                        Err(error) => {
                            return Err(anyhow::anyhow!(
                                "CoreKernel 取消 Workflow Run {workflow_run} 失败:{error}"
                            ))
                        }
                    }
                }
                if !cancelled {
                    return Err(anyhow::anyhow!(
                        "CoreKernel 取消 Workflow Run {workflow_run} 连续冲突 3 次:{}",
                        last_conflict.expect("RevisionConflict 分支必已记录")
                    ));
                }
            }

            // 全部 durable cancel 已收口，可安全终止 legacy runtime 并关库。
            h.orchestrator.stop();
            self.overview.detach(&h.root);
            tracer.runtime.finalize_project_close(close_token);
            let mut projects = self.projects.lock();
            if let Some(index) = projects.iter().position(|project| project.root == h.root) {
                projects.remove(index);
            }
        }
        self.sync_pipe_routing();
        Ok(())
    }

    #[cfg(test)]
    pub fn close_project(&self, root: &PathBuf) {
        self.try_close_project(root).unwrap();
    }

    /// 持久化当前打开项目、前台项目与每项目恢复状态(原子写)。
    pub fn save_session(
        &self,
        foreground: Option<&PathBuf>,
        project_states: Vec<ProjectSessionState>,
    ) {
        let projects: Vec<PathBuf> = self
            .projects
            .lock()
            .iter()
            .map(|p| p.root.clone())
            .collect();
        Self::save_session_at(
            &session_path(),
            &SessionState {
                projects,
                foreground: foreground.cloned(),
                project_states,
            },
        );
    }

    /// 读取上次会话(文件缺失/损坏 → 空状态,不阻塞启动)。
    pub fn load_session() -> SessionState {
        Self::load_session_at(&session_path())
    }

    pub(crate) fn save_session_at(path: &PathBuf, state: &SessionState) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        let write = serde_json::to_string_pretty(state)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            .and_then(|text| std::fs::write(&tmp, text));
        if write.and_then(|_| std::fs::rename(&tmp, path)).is_err() {
            log::warn!("会话保存失败: {}", path.display());
        }
    }

    pub(crate) fn load_session_at(path: &PathBuf) -> SessionState {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn orchestrator_of(&self, root: &Path) -> Option<Arc<Orchestrator>> {
        self.projects
            .lock()
            .iter()
            .find(|p| &p.root == root)
            .map(|p| p.orchestrator.clone())
    }

    /// Orca 式权限物化:全局 yolo 时返回该 agent type 的 yolo 参数
    /// (manual 返回空,由用户在终端里手动批准)。
    /// 默认 CLI 的临时快照没有实例参数,权限默认在这里落地。
    pub fn permission_argv_for(&self, agent_type: &str) -> Vec<String> {
        let global_yolo = {
            let cfg = self.config.lock();
            cfg.agents.permission_mode != "manual"
        };
        permission_argv_of(&self.plugins, global_yolo, agent_type)
    }

    /// 在项目任务下创建离散 CLI 会话(设计 §4.7 / §10):
    /// 不属于 DAG、没有 Step / Agent Run,不改变 Task 状态。
    /// `external_config = true` 表示 Default CLI 只读外部配置意图
    /// (适配器跳过隔离注入,绝不写用户全局配置)。
    pub fn create_ad_hoc_session(
        &self,
        root: &Path,
        task_id: i64,
        instance_snapshot: &mf_agent::AgentInstanceSnapshot,
        launch_mode: mf_agent::RunMode,
        external_config: bool,
    ) -> Result<mf_agent::AdHocSessionView> {
        let orch = self
            .orchestrator_of(root)
            .ok_or_else(|| anyhow::anyhow!("项目未打开: {}", root.display()))?;
        if !instance_snapshot.enabled {
            anyhow::bail!("Agent Instance `{}` 已禁用", instance_snapshot.name);
        }
        let task = orch
            .store
            .task_view(task_id)?
            .ok_or_else(|| anyhow::anyhow!("任务 {task_id} 不存在"))?;
        let (resolved, _adapter) =
            adapter_launch::resolve_adapter(&self.plugins, &instance_snapshot.agent_type)?;
        if let Some((_, contribution)) = &resolved {
            if !adapter_launch::contribution_supports_mode(&contribution.modes, launch_mode) {
                anyhow::bail!(
                    "Agent Type `{}` 不支持 {} 模式",
                    instance_snapshot.agent_type,
                    launch_mode
                );
            }
        }

        let nonce = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_else(|| chrono::Utc::now().timestamp_micros());
        let run_temp = std::env::temp_dir()
            .join("monkeyfence")
            .join("ad-hoc")
            .join(format!("{}-{task_id}-{nonce}", std::process::id()));
        let run_token = format!("ad-hoc:{}:{task_id}:{nonce}", root.display());
        let mut prompt = None;
        apply_task_goal(&mut prompt, &task.goal);
        let plan = adapter_launch::compile_instance_launch(
            &self.plugins,
            &self.catalog_store,
            instance_snapshot,
            None,
            run_temp.clone(),
            root.to_path_buf(),
            prompt,
            &run_token,
            external_config,
            *self.secret_master_key.lock(),
        )?;
        orch.create_ad_hoc_session(task_id, instance_snapshot, launch_mode, run_temp, plan)
    }

    /// 分配工作流模板给任务(生产入口):插件贡献索引 + 编译 + 冻结 Revision。
    pub fn assign_workflow(
        &self,
        root: &Path,
        task_id: i64,
        version_id: i64,
        allow_unsafe_shared_directory: bool,
    ) -> Result<i64> {
        let orch = self
            .orchestrator_of(root)
            .ok_or_else(|| anyhow::anyhow!("项目未打开: {}", root.display()))?;
        let version: WorkflowTemplateVersion = self
            .catalog_store
            .template_version(version_id)?
            .ok_or_else(|| anyhow::anyhow!("模板版本 {version_id} 不存在"))?;
        let index = adapter_launch::workflow_plugin_index(&self.plugins);
        let rev = orch.assign_workflow(task_id, &version, &index, allow_unsafe_shared_directory)?;
        Ok(rev.id)
    }

    /// 任务本地工作流(项目 Store 草稿):只编译校验,不写库。
    pub fn compile_task_local_workflow(
        &self,
        root: &Path,
        task_id: i64,
    ) -> Result<mf_agent::workflow::WorkflowSnapshot> {
        let orch = self
            .orchestrator_of(root)
            .ok_or_else(|| anyhow::anyhow!("项目未打开: {}", root.display()))?;
        let index = adapter_launch::workflow_plugin_index(&self.plugins);
        orch.compile_task_local_workflow(task_id, &index)
    }

    /// 任务本地工作流(项目 Store 草稿):编译 + pin + 冻结 Revision。
    /// unsafe-parallel 开关取草稿的持久化值(非 Git 根的显式风险接受)。
    pub fn assign_task_local_workflow(&self, root: &Path, task_id: i64) -> Result<i64> {
        let orch = self
            .orchestrator_of(root)
            .ok_or_else(|| anyhow::anyhow!("项目未打开: {}", root.display()))?;
        let index = adapter_launch::workflow_plugin_index(&self.plugins);
        let rev = orch.assign_task_local_workflow(task_id, &index)?;
        Ok(rev.id)
    }

    /// 任务本地工作流「分配并确认运行」原子路径(I13):
    /// 草稿未变时不重复冻结(直接确认),草稿变化则重新编译冻结后运行。
    pub fn assign_and_confirm_task_local(&self, root: &Path, task_id: i64) -> Result<()> {
        let orch = self
            .orchestrator_of(root)
            .ok_or_else(|| anyhow::anyhow!("项目未打开: {}", root.display()))?;
        let index = adapter_launch::workflow_plugin_index(&self.plugins);
        orch.assign_and_confirm_task_local(task_id, &index)?;
        Ok(())
    }

    /// ---------- 项目工作流直接运行(ADR 0004 / Task 4) ----------

    /// 从项目工作流发起可恢复 Start Operation。本适配层只做
    /// root/key → opaque Project/Workflow 定位；Task/Revision 的创建、冻结、
    /// 调度与失败补偿全部归 Core 后台 Operation 所有。
    pub fn run_project_workflow(
        &self,
        root: &Path,
        workflow_key: &str,
        goal: &str,
    ) -> Result<WorkflowRunTarget> {
        let goal = goal.trim();
        anyhow::ensure!(!goal.is_empty(), "运行目标不能为空");
        let tracer = self
            .required_kernel_tracer()
            .map_err(|error| anyhow::anyhow!(error))?;
        let project = self
            .required_kernel_project(root)
            .map_err(|error| anyhow::anyhow!(error))?;
        let outcome = tracer
            .client
            .start_workflow_run(&project, workflow_key, goal)
            .map_err(|error| anyhow::anyhow!(error))?;
        let KernelOutcome::Accepted { operation_handle } = outcome else {
            return Err(anyhow::anyhow!(
                "CoreKernel Workflow Start 未返回 Accepted Operation"
            ));
        };
        self.overview.request_refresh();
        Ok(WorkflowRunTarget {
            project_root: root.to_path_buf(),
            workflow_key: workflow_key.to_string(),
            operation_handle,
        })
    }

    /// ---------- Secret 管理(设计 §8:明文只在 Secret Store 内) ----------

    fn secret_store(&self) -> Result<mf_plugins::builtin_secret_store::BuiltinSecretStore> {
        let _ = &self.secret_master_key; // 见下:覆盖时用确定性密钥
        if let Some(key) = *self.secret_master_key.lock() {
            mf_plugins::builtin_secret_store::BuiltinSecretStore::with_master_key(
                self.catalog_store.clone(),
                key,
            )
        } else {
            mf_plugins::builtin_secret_store::BuiltinSecretStore::open(self.catalog_store.clone())
        }
    }

    /// 加密保存 Secret,返回稳定引用 ID(实例只保存该引用)。
    pub fn seal_secret(&self, name: &str, value: &str) -> Result<String> {
        use mf_agent::secrets::SecretStore as _;
        if value.is_empty() {
            anyhow::bail!("Secret 值不能为空");
        }
        self.secret_store()?.seal(name, value.as_bytes())
    }

    /// 删除 Secret(仍被实例引用时拒绝,先解除引用)。
    pub fn delete_secret(&self, id: &str) -> Result<bool> {
        use mf_agent::secrets::SecretStore as _;
        let referenced = self
            .catalog_store
            .list_agent_instances(None)?
            .iter()
            .any(|row| {
                self.catalog_store
                    .snapshot_agent_instance(&row.id, None)
                    .map(|snap| snap.sealed_secret_ids.iter().any(|s| s == id))
                    .unwrap_or(false)
            });
        anyhow::ensure!(
            !referenced,
            "Secret `{id}` 仍被 Agent Instance 引用,先解除引用再删除"
        );
        self.secret_store()?.delete(id)
    }

    /// 脱敏描述列表(名称/长度,无明文)。
    pub fn list_secrets(&self) -> Result<Vec<mf_agent::secrets::SecretDescription>> {
        use mf_agent::secrets::SecretStore as _;
        self.secret_store()?.list()
    }

    /// 活动项目数(用于关闭确认)。
    pub fn active_runs_of(&self, root: &PathBuf) -> usize {
        self.orchestrator_of(root)
            .and_then(|o| o.store.running_runs().ok())
            .map(|r| r.len())
            .unwrap_or(0)
    }

    pub fn total_active_runs(&self) -> usize {
        self.projects
            .lock()
            .iter()
            .filter_map(|p| p.orchestrator.store.running_runs().ok())
            .map(|r| r.len())
            .sum()
    }

    fn sync_pipe_routing(&self) {
        if let Some(routing) = &self.pipe_orchestrators {
            let mut list = routing.lock();
            list.clear();
            for p in self.projects.lock().iter() {
                list.push(p.orchestrator.clone());
            }
        }
    }
}

#[cfg(test)]
mod agent_launch_selection_tests {
    use super::*;

    #[test]
    fn task_workflow_drafts_persist_per_project_in_project_store() {
        let ctx = AppCtx::with_catalog_for_tests(mf_agent::CatalogStore::memory().unwrap());
        let root = tempfile::tempdir().unwrap();
        let orch = ctx.open_project(root.path().to_path_buf()).unwrap();
        let task = orch.create_task("t", "g").unwrap();
        let draft = mf_agent::workflow::WorkflowTemplateDraft {
            key: format!("task-{}", task.id),
            name: "任务工作流".into(),
            task_local: true,
            nodes: vec![mf_agent::workflow::WorkflowNodeDraft {
                key: "a".into(),
                title: "A".into(),
                instructions: "做 A".into(),
                agent_instance_id: "inst_x".into(),
                deps: vec![],
            }],
        };
        orch.store
            .save_task_workflow(&root.path().to_string_lossy(), task.id, &draft, false)
            .unwrap();
        // 重新打开同一项目:草稿仍在(项目 Store 持久化)
        let reloaded = orch
            .store
            .load_task_workflow(&root.path().to_string_lossy(), task.id)
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.nodes[0].instructions, "做 A");
        // 另一项目同 task id 不串扰
        let other = tempfile::tempdir().unwrap();
        let orch2 = ctx.open_project(other.path().to_path_buf()).unwrap();
        assert!(orch2
            .store
            .load_task_workflow(&other.path().to_string_lossy(), task.id)
            .unwrap()
            .is_none());
        orch.stop();
        orch2.stop();
        ctx.close_project(&root.path().to_path_buf());
        ctx.close_project(&other.path().to_path_buf());
    }

    #[test]
    fn directory_provider_prefers_worktree_for_git_repos() {
        let git_root = tempfile::tempdir().unwrap();
        mf_vcs::git::Git::init(git_root.path()).unwrap();
        // load_at_with_catalog 注册内置合成贡献(含 worktree 目录提供器)
        let host = PluginRegistry::load_at_with_catalog(
            git_root.path().join("plugins"),
            mf_agent::CatalogStore::memory().unwrap(),
            &mf_agent::Config::default(),
            &[],
        );
        let (provider, pin) = directory_provider_for(git_root.path(), &host);
        assert!(provider.isolates(), "Git 仓库应使用 worktree 隔离提供器");
        assert!(
            pin.as_ref()
                .is_some_and(|p| p.full_id == WORKTREE_PLUGIN_FULL_ID),
            "Git 仓库的提供器必须携带内置 pinned 身份: {pin:?}"
        );
        let plain = tempfile::tempdir().unwrap();
        let (provider, plain_pin) = directory_provider_for(plain.path(), &host);
        assert!(!provider.isolates(), "非 Git 根应回退共享项目目录");
        assert!(plain_pin.is_none(), "共享目录回退不带插件 pin");
    }

    #[test]
    fn task_goal_becomes_launch_prompt_only_when_meaningful() {
        let mut prompt = None;
        apply_task_goal(&mut prompt, "  ");
        assert!(prompt.is_none(), "空白目标不得注入提示");
        apply_task_goal(&mut prompt, "修复登录超时");
        assert_eq!(prompt.as_deref(), Some("修复登录超时"));
    }
}

#[cfg(test)]
mod run_lifecycle_kernel_tests {
    use super::*;
    use mf_agent::{PipelineDraft, ProjectWorkflowDraft, SessionPolicy, StepDraft, StepStatus};
    use mf_kernel::command::ServiceIdempotencyKey;
    use mf_kernel::handles::{ClientId, Principal};

    #[test]
    fn open_project_registers_real_run_lifecycle_port_and_retry_writes_through_kernel() {
        let ctx = AppCtx::with_catalog_for_tests(mf_agent::CatalogStore::memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let service =
            mf_kernel::project_registry::ServiceStore::open(&tmp.path().join("service-v1.db"))
                .unwrap();
        let (runtime, client) = mf_kernel::kernel::InProcessKernelRuntime::for_test(
            service,
            ServiceIdempotencyKey::for_test(vec![0x63; 32]).unwrap(),
            ClientId::parse("gpui-run-client").unwrap(),
            Principal::parse("gpui-run-user").unwrap(),
        )
        .unwrap();
        ctx.install_kernel_tracer_for_tests(runtime, client);
        let root = tmp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let orchestrator = ctx.open_project(root.clone()).unwrap();
        let task = orchestrator.store.create_task("run", "goal").unwrap();
        orchestrator
            .store
            .create_draft_revision(
                task.id,
                &PipelineDraft {
                    steps: vec![StepDraft {
                        key: "work".into(),
                        title: "work".into(),
                        instructions: "do it".into(),
                        agent_profile: "instance".into(),
                        session_policy: SessionPolicy::Fresh,
                        deps: vec![],
                    }],
                },
            )
            .unwrap();
        orchestrator.store.activate_revision(task.id).unwrap();
        let step = orchestrator.store.task_steps(task.id).unwrap()[0].clone();
        orchestrator
            .store
            .set_step_status(step.id, StepStatus::Failed)
            .unwrap();

        assert!(matches!(
            ctx.retry_workflow_step_via_kernel(&root, step.id, mf_agent::RetryMode::FreshSession,),
            Ok(KernelOutcome::RunApplied { .. })
        ));
        assert_eq!(
            orchestrator
                .store
                .step_view(step.id)
                .unwrap()
                .unwrap()
                .status,
            StepStatus::Ready
        );
        assert_eq!(
            orchestrator.store.next_attempt_session(step.id).unwrap(),
            Some(mf_agent::NextAttemptSession::fresh())
        );
        ctx.try_close_project(&root).unwrap();
    }

    #[test]
    fn close_cancel_failure_keeps_project_open_without_direct_mutation() {
        let ctx = AppCtx::with_catalog_for_tests(mf_agent::CatalogStore::memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let service =
            mf_kernel::project_registry::ServiceStore::open(&tmp.path().join("service-close.db"))
                .unwrap();
        let (runtime, client) = mf_kernel::kernel::InProcessKernelRuntime::for_test(
            service,
            ServiceIdempotencyKey::for_test(vec![0x65; 32]).unwrap(),
            ClientId::parse("gpui-close-client").unwrap(),
            Principal::parse("gpui-close-user").unwrap(),
        )
        .unwrap();
        ctx.install_kernel_tracer_for_tests(runtime, client);
        let root = tmp.path().join("close-project");
        std::fs::create_dir_all(&root).unwrap();
        let orchestrator = ctx.open_project(root.clone()).unwrap();
        let task = orchestrator.store.create_task("run", "goal").unwrap();
        orchestrator
            .store
            .create_draft_revision(
                task.id,
                &PipelineDraft {
                    steps: vec![StepDraft {
                        key: "work".into(),
                        title: "work".into(),
                        instructions: "do it".into(),
                        agent_profile: "instance".into(),
                        session_policy: SessionPolicy::Fresh,
                        deps: vec![],
                    }],
                },
            )
            .unwrap();
        orchestrator.store.activate_revision(task.id).unwrap();
        orchestrator
            .store
            .set_task_status(task.id, TaskStatus::Running)
            .unwrap();
        let step = orchestrator.store.task_steps(task.id).unwrap()[0].clone();
        orchestrator
            .store
            .set_step_status(step.id, StepStatus::Running)
            .unwrap();
        orchestrator
            .store
            .create_run(task.id, step.id, step.revision_id, None)
            .unwrap();

        let tracer = ctx.kernel.lock().clone().unwrap();
        let project = ctx.projects.lock()[0].kernel_project.clone().unwrap();
        tracer.runtime.unregister_run_lifecycle_port(&project);
        let error = ctx.try_close_project(&root).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("run_lifecycle_port_not_registered"),
            "close 必须原样上报 Core cancel 失败:{error:#}"
        );
        assert_eq!(ctx.project_count(), 1, "cancel 失败必须保持项目打开");
        assert_eq!(
            orchestrator
                .store
                .task_view(task.id)
                .unwrap()
                .unwrap()
                .status,
            TaskStatus::Running,
            "不得回退 Orchestrator 直写取消"
        );
        let workflow_run = mf_kernel::handles::WorkflowRunHandle::parse(
            orchestrator
                .store
                .task_view(task.id)
                .unwrap()
                .unwrap()
                .public_handle,
        )
        .unwrap();
        assert!(matches!(
            tracer.client.cancel_workflow_run(&project, &workflow_run),
            Err(KernelProblem::ResourceNotFound)
        ));
        let close = tracer.runtime.prepare_project_close(&project).unwrap();
        tracer
            .runtime
            .register_run_lifecycle_port_for_close(
                &close,
                Arc::new(
                    crate::run_lifecycle_port::OrchestratorRunLifecyclePort::new(
                        orchestrator.clone(),
                    ),
                ),
            )
            .unwrap();
        ctx.try_close_project(&root)
            .expect("closing 项目应能重试 durable drain");
        assert_eq!(ctx.project_count(), 0);
    }

    #[test]
    fn open_project_registers_workflow_start_port_and_background_worker() {
        let catalog = mf_agent::CatalogStore::memory().unwrap();
        let instance = catalog
            .create_agent_instance(mf_agent::AgentInstanceDraft {
                name: "Start mock".into(),
                agent_type: "mock".into(),
                scope: mf_agent::InstanceScope::User,
                project_key: None,
                enabled: true,
                run_mode: mf_agent::RunMode::OneShot,
                executable: "mock".into(),
                argv: Vec::new(),
                env: Vec::new(),
                config: serde_json::json!({}),
                execution_contract: serde_json::json!({"completion":"manual"}),
                sealed_secret_ids: Vec::new(),
            })
            .unwrap();
        let ctx = AppCtx::with_catalog_for_tests(catalog);
        let tmp = tempfile::tempdir().unwrap();
        let service =
            mf_kernel::project_registry::ServiceStore::open(&tmp.path().join("service-start.db"))
                .unwrap();
        let (runtime, client) = mf_kernel::kernel::InProcessKernelRuntime::for_test(
            service,
            ServiceIdempotencyKey::for_test(vec![0x64; 32]).unwrap(),
            ClientId::parse("gpui-start-client").unwrap(),
            Principal::parse("gpui-start-user").unwrap(),
        )
        .unwrap();
        ctx.install_kernel_tracer_for_tests(runtime, client);
        let root = tmp.path().join("start-project");
        std::fs::create_dir_all(&root).unwrap();
        let orchestrator = ctx.open_project(root.clone()).unwrap();
        orchestrator
            .store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: "wf-start".into(),
                name: "Start".into(),
                nodes: vec![mf_agent::WorkflowNodeDraft {
                    key: "work".into(),
                    title: "work".into(),
                    instructions: "do it".into(),
                    agent_instance_id: instance.id,
                    deps: Vec::new(),
                }],
                allow_unsafe_parallel: false,
            })
            .unwrap();
        let accepted = ctx
            .run_project_workflow(&root, "wf-start", "后台启动")
            .unwrap();
        assert!(accepted.operation_handle.as_str().starts_with("op_"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while orchestrator.tasks().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let tasks = orchestrator.tasks().unwrap();
        assert_eq!(tasks.len(), 1, "后台 worker 应从 durable plan 创建唯一 Run");
        assert_eq!(tasks[0].goal, "后台启动");
        // 第二个 Accepted Start 与 close 故意紧邻：freeze 要么等它
        // 完成并随后 cancel，要么把未执行 Operation 终结为
        // project_closing；绝不得在重开后复活创建新 Run。
        ctx.run_project_workflow(&root, "wf-start", "与关闭竞态")
            .unwrap();
        ctx.try_close_project(&root).unwrap();
        let closed_count = orchestrator.tasks().unwrap().len();
        let reopened = ctx.open_project(root.clone()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let reopened_tasks = reopened.tasks().unwrap();
        assert_eq!(
            reopened_tasks.len(),
            closed_count,
            "未完成 Start Operation 不得在重开后恢复创建 Run"
        );
        assert!(reopened_tasks.iter().all(|task| !matches!(
            task.status,
            TaskStatus::Ready | TaskStatus::Running | TaskStatus::NeedsYou
        )));
        ctx.try_close_project(&root).unwrap();
    }
}

/// 测试插件:贡献命令为 `command` 的 Agent Type。`cmd` 在 Windows 总能
/// 检测到;虚构命令永远检测不到 —— 用于验证 resolver 的合成与稳定错误。
#[cfg(test)]
fn install_cli_plugin(
    host: &Arc<PluginRegistry>,
    plugin_id: &str,
    agent_id: &str,
    command: &str,
) -> String {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(
        src.path().join("monkeyfence-plugin.toml"),
        format!(
            r#"[manifest]
version = 2
publisher = "mf-test"
id = "{plugin_id}"
name = "{plugin_id} Test"
version_str = "0.1.0"
description = "resolver test plugin"

[capabilities]

[[agent_types]]
id = "{agent_id}"
name = "{agent_id} Agent"
adapter = "generic-command"
command = "{command}"
modes = ["oneshot", "interactive"]
"#
        ),
    )
    .unwrap();
    host.install_package(
        src.path(),
        mf_plugins::install::InstallSource::Local {
            path: src.path().display().to_string(),
        },
    )
    .unwrap();
    let full_id = format!("mf-test.{plugin_id}");
    host.enable(&full_id, true).unwrap();
    format!("{full_id}.{agent_id}")
}

#[cfg(test)]
fn test_resolver(host: Arc<PluginRegistry>) -> PluginInstanceResolver {
    PluginInstanceResolver::new(
        host,
        mf_agent::CatalogStore::memory().unwrap(),
        Arc::new(Mutex::new(mf_agent::Config::default())),
    )
}

#[cfg(test)]
mod default_cli_resolver_tests {
    use super::*;
    use mf_agent::orchestrator::WorkflowInstanceResolver as _;

    fn host() -> Arc<PluginRegistry> {
        PluginRegistry::load_at_with_catalog(
            tempfile::tempdir().unwrap().path().to_path_buf(),
            mf_agent::CatalogStore::memory().unwrap(),
            &mf_agent::Config::default(),
            &[],
        )
    }

    #[test]
    fn detected_cli_synthesizes_external_config_snapshot_without_persisting() {
        let host = host();
        let full_id = install_cli_plugin(&host, "cmdcli", "cmd", "cmd");
        let resolver = test_resolver(host.clone());
        let snapshot = resolver
            .resolve(&format!("{DEFAULT_CLI_REFERENCE_PREFIX}{full_id}"))
            .unwrap();
        assert!(
            snapshot.external_config,
            "default-cli 合成快照必须只读外部配置"
        );
        assert_eq!(
            snapshot.agent_type, full_id,
            "快照携带完整贡献 ID(pin 依据)"
        );
        assert_eq!(snapshot.executable, "cmd", "使用插件默认 command");
        assert_eq!(snapshot.run_mode, mf_agent::RunMode::OneShot);
        assert!(
            snapshot.sealed_secret_ids.is_empty(),
            "不从 CLI 全局配置读取 Secret"
        );
        assert_eq!(
            snapshot.execution_contract["completion"], "manual",
            "done/退出不得自动结算"
        );
        // 合成快照不落目录库(不创建隐藏持久实例)
        let rows = resolver.catalog.list_agent_instances(None).unwrap();
        assert!(rows.is_empty(), "default-cli 不得写入 CatalogStore");
    }

    #[test]
    fn unknown_short_and_disabled_references_are_stable_errors() {
        let host = host();
        let full_id = install_cli_plugin(&host, "cmdcli", "cmd", "cmd");
        let resolver = test_resolver(host.clone());

        // 未知完整贡献 ID
        let err = resolver
            .resolve("default-cli:ghost.plugin.agent")
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("不存在"));
        // 短 id 不是合法新引用(必须完整贡献 ID)
        let err = resolver.resolve("default-cli:cmd").err().unwrap();
        assert!(
            format!("{err:#}").contains("不存在"),
            "短 id 引用必须拒绝: {err:#}"
        );
        // 禁用插件
        host.disable("mf-test.cmdcli").unwrap();
        let err = resolver
            .resolve(&format!("{DEFAULT_CLI_REFERENCE_PREFIX}{full_id}"))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("未启用"));
    }

    #[test]
    fn undetected_cli_is_rejected_with_install_hint() {
        let host = host();
        let resolver = test_resolver(host.clone());
        // 虚构命令:永远不会检测到 → 稳定错误
        let full_id = install_cli_plugin(&host, "ghostcli", "ghost", "mf-ghost-cli-not-installed");
        let err = resolver
            .resolve(&format!("{DEFAULT_CLI_REFERENCE_PREFIX}{full_id}"))
            .err()
            .unwrap();
        assert!(
            format!("{err:#}").contains("未检测到"),
            "未检测 CLI 必须给出明确提示: {err:#}"
        );
    }

    #[test]
    fn plain_reference_still_resolves_catalog_instances() {
        let host = host();
        let resolver = test_resolver(host);
        // 普通引用走目录库;未知实例报目录库原错误
        let err = resolver.resolve("inst-not-exist").err().unwrap();
        assert!(format!("{err:#}").contains("inst-not-exist"));
    }
}

#[cfg(test)]
mod worktree_contribution_tests {
    use super::*;
    use mf_plugins::contribution_registry::ContributionSource;

    fn directory(
        id: &str,
        isolates: bool,
    ) -> mf_plugins::contribution_registry::ExecutionDirectoryContribution {
        mf_plugins::contribution_registry::ExecutionDirectoryContribution {
            id: id.into(),
            name: id.into(),
            kind: id.into(),
            supports_parallel: isolates,
            isolates,
            description: String::new(),
        }
    }

    fn source(full_id: &str) -> ContributionSource {
        ContributionSource {
            plugin_full_id: full_id.into(),
            plugin_version: "0.1.0".into(),
            content_hash: String::new(),
        }
    }

    #[test]
    fn worktree_provider_resolves_only_by_full_contribution_id() {
        // 伪装的"隔离但不是 worktree"贡献不得命中
        let decoy = (
            "evil.plugin.not-worktree".to_string(),
            source("evil.plugin"),
            directory("not-worktree", true),
        );
        assert_eq!(worktree_contribution_id(&[decoy]), None);

        // 正主:完整贡献 ID 以 .worktree 结尾且 id == worktree
        let real = (
            "monkeyfence.directories.worktree".to_string(),
            source("monkeyfence.directories"),
            directory("worktree", true),
        );
        assert_eq!(
            worktree_contribution_id(&[real.clone()]),
            Some("monkeyfence.directories.worktree".to_string())
        );
        // id 是 worktree 但插件声明为不隔离:不是可用 worktree 提供器
        let weak = (
            "monkeyfence.directories.worktree".to_string(),
            source("monkeyfence.directories"),
            directory("worktree", false),
        );
        assert_eq!(worktree_contribution_id(&[weak]), None);
    }

    #[test]
    fn third_party_worktree_namesake_never_resolves_to_hardcoded_provider() {
        // 伪装成 worktree 的第三方贡献(id/kind/isolates/supports_parallel
        // 全部一致)不得命中:进程内 GitWorktreeProvider 只属于内置
        // monkeyfence.directories 的 pinned 贡献;第三方目录提供器必须
        // 走 worker 解析,不能借命名顶替拿到 worktree 隔离语义
        let evil = || {
            (
                "evil.plugin.worktree".to_string(),
                source("evil.plugin"),
                directory("worktree", true),
            )
        };
        assert_eq!(
            worktree_contribution_id(&[evil()]),
            None,
            "第三方贡献不得解析为硬编码 GitWorktreeProvider"
        );
        // 正主与伪装同时在场:只命中内置贡献
        let real = (
            "monkeyfence.directories.worktree".to_string(),
            source("monkeyfence.directories"),
            directory("worktree", true),
        );
        assert_eq!(
            worktree_contribution_id(&[evil(), real.clone()]),
            Some("monkeyfence.directories.worktree".to_string())
        );
    }

    #[test]
    fn worktree_contribution_requires_declared_kind_match() {
        // 内置来源但 kind 不是 worktree(策略实现标识不符):拒绝
        let wrong_kind = (
            "monkeyfence.directories.worktree".to_string(),
            source("monkeyfence.directories"),
            mf_plugins::contribution_registry::ExecutionDirectoryContribution {
                id: "worktree".into(),
                name: "worktree".into(),
                kind: "some-other-kind".into(),
                supports_parallel: true,
                isolates: true,
                description: String::new(),
            },
        );
        assert_eq!(worktree_contribution_id(&[wrong_kind]), None);
    }
}
