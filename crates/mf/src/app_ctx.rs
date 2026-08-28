//! 应用级上下文:跨项目共享的 Session Registry、插件注册表、全局并发限制、
//! mfctl 管道服务与每个项目的 Orchestrator。
//!
//! 前台只显示一个项目;其他项目的任务和 Agent 在后台继续运行(见 ADR 0001)。

use crate::pipe_server::{pipe_name_for_current_process, PipeServer};
use crate::project_overview::{HubCtx, ProjectOverviewHub};
use crate::runtime_host::{KeepAwake, RuntimeHostImpl, SessionRegistry};
use anyhow::{Context as _, Result};
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::{CatalogStore, Store, TaskStatus};
use mf_plugins::PluginRegistry;
use parking_lot::{Mutex, RwLock};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    #[allow(dead_code)]
    pipe: Mutex<Option<PipeServer>>,
    pipe_orchestrators: Option<Arc<Mutex<Vec<Arc<Orchestrator>>>>>,
}

impl AppCtx {
    pub fn new() -> Arc<AppCtx> {
        let config = mf_agent::Config::load().unwrap_or_default();
        let skills = mf_skills::load_skills(None);
        let catalog_store = CatalogStore::open_default().unwrap_or_else(|e| {
            // 目录库打不开不阻塞启动(插件页/实例页后续访问时再暴露错误),
            // 但必须留下日志,不允许静默降级到无提示状态。
            log::error!("目录库打开失败: {e:#}");
            CatalogStore::memory().expect("内存目录库初始化不可能失败")
        });
        let plugins = PluginRegistry::load_with_catalog(catalog_store.clone(), &config, &skills);
        let registry = SessionRegistry::new(config.clone());
        let limiter = GlobalLimiter::new(config.engine.global_concurrency.max(1));
        let keep_awake = Arc::new(KeepAwake::new());
        let catalog = Arc::new(RwLock::new(ProfileCatalog::default()));
        let orchs: Arc<Mutex<Vec<Arc<Orchestrator>>>> = Arc::new(Mutex::new(Vec::new()));
        let pipe_server = PipeServer::start(orchs.clone()).ok();
        if pipe_server.is_none() {
            log::warn!("mfctl 管道服务启动失败(结算将不可用)");
        }
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
            pipe: Mutex::new(pipe_server),
            pipe_orchestrators: Some(orchs),
        });
        ctx.refresh_catalog();
        ctx
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
        let host = RuntimeHostImpl::new(self.registry.clone());
        let orch = Orchestrator::start(
            store,
            root.clone(),
            config,
            host,
            self.catalog.clone(),
            self.limiter.clone(),
            pipe_name_for_current_process(),
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
