//! 应用级上下文:跨项目共享的 Session Registry、插件注册表、全局并发限制、
//! mfctl 管道服务与每个项目的 Orchestrator。
//!
//! 前台只显示一个项目;其他项目的任务和 Agent 在后台继续运行(见 ADR 0001)。

use crate::adapter_launch;
use crate::pipe_server::{pipe_name_for_current_process, PipeServer};
use crate::project_overview::{HubCtx, ProjectOverviewHub};
use crate::runtime_host::{KeepAwake, RuntimeHostImpl, SessionRegistry, WorkflowLauncher};
use anyhow::{Context as _, Result};
use mf_agent::orchestrator::{
    GlobalLimiter, Orchestrator, ProfileCatalog, WorkflowKernel, WorkflowPluginPins,
};
use mf_agent::workflow::{PluginSourcePin, WorkflowTemplateVersion};
use mf_agent::{CatalogStore, Store, TaskStatus};
use mf_plugins::PluginRegistry;
use parking_lot::{Mutex, RwLock};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 生产 pin 生命周期:Plugin Host 内容寻址 pin(内置合成插件走源 pin)。
struct PluginHostPins {
    host: Arc<PluginRegistry>,
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

/// 解析项目的执行目录提供器:插件贡献声明隔离能力的 worktree 优先
/// (Git 仓库才可创建 worktree),否则回退内核共享项目目录。
fn directory_provider_for(
    root: &Path,
    plugins: &Arc<PluginRegistry>,
) -> Arc<dyn mf_agent::execution_directory::ExecutionDirectoryProvider> {
    // 插件贡献的执行目录提供器:声明隔离能力的优先(Git 仓库才可用
    // worktree);未命中或创建失败回退内核共享项目目录。
    let directories = plugins.contributions().execution_directories();
    if directories
        .iter()
        .any(|(_, _, contribution)| contribution.isolates && mf_vcs::git::Git::is_repo(root))
    {
        if let Ok(provider) =
            mf_plugins::git_worktree_provider::GitWorktreeProvider::new(root.to_path_buf())
        {
            return Arc::new(provider);
        }
    }
    Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default())
}

pub struct ProjectHandle {
    pub root: PathBuf,
    pub orchestrator: Arc<Orchestrator>,
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

pub struct AppCtx {
    pub registry: Arc<SessionRegistry>,
    pub plugins: Arc<PluginRegistry>,
    /// 用户级目录库(Agent Instance、模板、Secret、插件包;~/.monkeyfence/catalog-v1.db)。
    /// 读写 API 随 Agent Instance / Secret Store 里程碑接入。
    #[allow(dead_code)]
    pub catalog_store: Arc<CatalogStore>,
    pub limiter: Arc<GlobalLimiter>,
    pub catalog: Arc<RwLock<ProfileCatalog>>,
    pub config: Arc<Mutex<mf_agent::Config>>,
    /// 统一项目总览快照 + Orchestrator Event Hub(UI 不再直接扫描项目列表)。
    pub overview: Arc<ProjectOverviewHub>,
    /// 已打开项目(私有:外部走 overview snapshot / 查询方法)。
    projects: Mutex<Vec<ProjectHandle>>,
    /// Secret Store 主密钥覆盖(None = 生产 OS keyring;测试注入确定性密钥)。
    secret_master_key: Mutex<Option<[u8; 32]>>,
    #[allow(dead_code)]
    pipe: Mutex<Option<PipeServer>>,
    pipe_orchestrators: Option<Arc<Mutex<Vec<Arc<Orchestrator>>>>>,
}

impl AppCtx {
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
        let registry = SessionRegistry::new(config.clone());
        let limiter = GlobalLimiter::new(config.engine.global_concurrency.max(1));
        let keep_awake = Arc::new(KeepAwake::new());
        let catalog = Arc::new(RwLock::new(ProfileCatalog::default()));
        let orchs: Arc<Mutex<Vec<Arc<Orchestrator>>>> = Arc::new(Mutex::new(Vec::new()));
        let pipe_server = if start_pipe {
            let server = PipeServer::start(orchs.clone()).ok();
            if server.is_none() {
                log::warn!("mfctl 管道服务启动失败(结算将不可用)");
            }
            server
        } else {
            None
        };
        let ctx = Arc::new(AppCtx {
            registry: registry.clone(),
            plugins: plugins.clone(),
            catalog_store,
            limiter: limiter.clone(),
            catalog: catalog.clone(),
            config: Arc::new(Mutex::new(config)),
            overview: ProjectOverviewHub::new(Arc::new(HubCtx {
                registry,
                catalog: catalog.clone(),
                plugins: plugins.clone(),
                limiter: limiter.clone(),
                keep_awake: keep_awake.clone(),
            })),
            projects: Mutex::new(Vec::new()),
            secret_master_key: Mutex::new(None),
            pipe: Mutex::new(pipe_server),
            pipe_orchestrators: Some(orchs),
        });
        ctx.refresh_catalog();
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

    /// 打开的项目数。
    pub fn project_count(&self) -> usize {
        self.projects.lock().len()
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
        let db_path = mf_agent::project_db_path(&root);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 {} 失败", parent.display()))?;
        }
        let store = Store::open(&db_path)?;
        let config = self.config.lock().clone();
        let host = RuntimeHostImpl::with_launcher(
            self.registry.clone(),
            WorkflowLauncher {
                plugins: self.plugins.clone(),
                catalog: self.catalog_store.clone(),
                secret_master_key: *self.secret_master_key.lock(),
            },
        );
        // 目录提供器:Git 仓库 → worktree 隔离(插件贡献解析);
        // 非 Git 根 → 共享项目目录(并行需显式风险开关,编译器默认拒绝)
        let directory: Arc<dyn mf_agent::execution_directory::ExecutionDirectoryProvider> =
            directory_provider_for(&root, &self.plugins);
        let orch = Orchestrator::start_with(
            store,
            root.clone(),
            config,
            host,
            self.catalog.clone(),
            self.limiter.clone(),
            pipe_name_for_current_process(),
            directory,
            WorkflowKernel {
                catalog: self.catalog_store.clone(),
                pins: Some(Arc::new(PluginHostPins {
                    host: self.plugins.clone(),
                })),
            },
        )?;
        self.projects.lock().push(ProjectHandle {
            root: root.clone(),
            orchestrator: orch.clone(),
        });
        // Event Hub:持续消费该 Orchestrator 的 UI 事件并构建 overview
        self.overview.attach(root, orch.clone());
        self.sync_pipe_routing();
        Ok(orch)
    }

    /// 关闭项目:停止其任务并移除;PTY 会话按 run 归属杀掉。
    pub fn close_project(&self, root: &PathBuf) {
        let handle = {
            let mut projects = self.projects.lock();
            let idx = projects.iter().position(|p| &p.root == root);
            idx.map(|i| projects.remove(i))
        };
        if let Some(h) = handle {
            let project_str = h.root.to_string_lossy().to_string();
            // 先快照活动 run(取消后 running_runs 会变空,先取后杀才有效)
            let active_runs = h.orchestrator.store.running_runs().unwrap_or_default();
            // 杀掉该项目 run 关联的会话(按项目作用域,不会误杀其他项目)
            for run in &active_runs {
                if let Some(sid) = run.session_id {
                    self.registry.kill_session(&project_str, sid);
                }
            }
            // 再取消任务(终止调度)
            if let Ok(tasks) = h.orchestrator.tasks() {
                for t in tasks {
                    if matches!(t.status, TaskStatus::Running | TaskStatus::NeedsYou) {
                        let _ = h.orchestrator.cancel_task(t.id);
                    }
                }
            }
            h.orchestrator.stop();
            // cancel_task 会 emit 状态事件；保持 drain 到所有取消操作结束，
            // 避免关闭大型项目时 bounded events_rx 反压当前线程。
            self.overview.detach(&h.root);
        }
        self.sync_pipe_routing();
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

    /// ---------- Secret 管理(设计 §8:明文只在 Secret Store 内) ----------

    fn secret_store(&self) -> Result<mf_plugins::builtin_secret_store::BuiltinSecretStore> {
        use mf_agent::secrets::SecretStore as _;
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
            .save_task_workflow(&root.path().to_string_lossy(), task.id, &draft)
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
        let provider = directory_provider_for(git_root.path(), &host);
        assert!(provider.isolates(), "Git 仓库应使用 worktree 隔离提供器");
        let plain = tempfile::tempdir().unwrap();
        let provider = directory_provider_for(plain.path(), &host);
        assert!(!provider.isolates(), "非 Git 根应回退共享项目目录");
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
