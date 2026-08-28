//! 应用级上下文:跨项目共享的 Session Registry、插件注册表、全局并发限制、
//! mfctl 管道服务与每个项目的 Orchestrator。
//!
//! 前台只显示一个项目;其他项目的任务和 Agent 在后台继续运行(见 ADR 0001)。

use crate::pipe_server::{pipe_name_for_current_process, PipeServer};
use crate::runtime_host::{KeepAwake, RuntimeHostImpl, SessionRegistry};
use anyhow::{Context as _, Result};
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::Store;
use mf_agent::TaskStatus;
use mf_plugins::PluginRegistry;
use parking_lot::{Mutex, RwLock};
use std::path::PathBuf;
use std::sync::Arc;

pub struct ProjectHandle {
    pub root: PathBuf,
    pub orchestrator: Arc<Orchestrator>,
}

/// 上次会话的打开项目(持久化到 ~/.monkeyfence/session.json)。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionState {
    pub projects: Vec<PathBuf>,
    pub foreground: Option<PathBuf>,
}

fn session_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".monkeyfence")
        .join("session.json")
}

pub struct AppCtx {
    pub registry: Arc<SessionRegistry>,
    pub plugins: Arc<PluginRegistry>,
    pub limiter: Arc<GlobalLimiter>,
    pub catalog: Arc<RwLock<ProfileCatalog>>,
    pub config: Arc<Mutex<mf_agent::Config>>,
    pub projects: Mutex<Vec<ProjectHandle>>,
    pub keep_awake: KeepAwake,
    #[allow(dead_code)]
    pipe: Mutex<Option<PipeServer>>,
    pipe_orchestrators: Option<Arc<Mutex<Vec<Arc<Orchestrator>>>>>,
}

impl AppCtx {
    pub fn new() -> Arc<AppCtx> {
        let config = mf_agent::Config::load().unwrap_or_default();
        let skills = mf_skills::load_skills(None);
        let plugins = PluginRegistry::load(&config, &skills);
        let registry = SessionRegistry::new(config.clone());
        let limiter = GlobalLimiter::new(config.engine.global_concurrency.max(1));
        let catalog = Arc::new(RwLock::new(ProfileCatalog::default()));
        let orchs: Arc<Mutex<Vec<Arc<Orchestrator>>>> = Arc::new(Mutex::new(Vec::new()));
        let pipe_server = PipeServer::start(orchs.clone()).ok();
        if pipe_server.is_none() {
            log::warn!("mfctl 管道服务启动失败(结算将不可用)");
        }
        let ctx = Arc::new(AppCtx {
            registry,
            plugins,
            limiter,
            catalog,
            config: Arc::new(Mutex::new(config)),
            projects: Mutex::new(Vec::new()),
            keep_awake: KeepAwake::new(),
            pipe: Mutex::new(pipe_server),
            pipe_orchestrators: Some(orchs),
        });
        ctx.refresh_catalog();
        ctx
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
    }

    /// 打开(或复用)一个项目:Store 迁移 + Orchestrator 启动 + work-items.json 一次性导入。
    pub fn open_project(&self, root: PathBuf) -> Result<Arc<Orchestrator>> {
        {
            let projects = self.projects.lock();
            if let Some(p) = projects.iter().find(|p| p.root == root) {
                return Ok(p.orchestrator.clone());
            }
        }
        let db_dir = root.join(".mf-agent");
        std::fs::create_dir_all(&db_dir)
            .with_context(|| format!("创建 {} 失败", db_dir.display()))?;
        let store = Store::open(&db_dir.join("orchestration.db"))?;
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
        import_legacy_work_items(&orch, &root);
        self.projects.lock().push(ProjectHandle {
            root,
            orchestrator: orch.clone(),
        });
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
        }
        self.sync_pipe_routing();
    }

    /// 持久化当前打开项目与前台项目(原子写)。
    pub fn save_session(&self, foreground: Option<&PathBuf>) {
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

    pub fn orchestrator_of(&self, root: &PathBuf) -> Option<Arc<Orchestrator>> {
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

/// 旧 work-items.json 兼容导入(仅一次,忽略 vcs_ref;JSON 原文件保留不写)。
pub(crate) fn import_legacy_work_items(orch: &Arc<Orchestrator>, root: &PathBuf) {
    if orch.store.has_import("work-items").unwrap_or(false) {
        return;
    }
    let json = root.join(".mf-agent").join("work-items.json");
    if !json.is_file() {
        let _ = orch.store.mark_import("work-items");
        return;
    }
    let Ok(text) = std::fs::read_to_string(&json) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(items) = value.get("items").and_then(|i| i.as_array()) else {
        return;
    };
    for item in items {
        let title = item
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("(未命名工作项)")
            .to_string();
        let phase = item
            .get("phase")
            .and_then(|p| p.as_str())
            .unwrap_or("draft");
        let status = match phase {
            "done" | "review" | "ready-to-deliver" => TaskStatus::Succeeded,
            "failed" => TaskStatus::Failed,
            "running" | "needs-input" => TaskStatus::NeedsYou,
            _ => TaskStatus::Draft,
        };
        if let Ok(task) = orch.store.create_task(
            &title,
            &format!("(迁移自 work-items.json,忽略 vcs_ref) {title}"),
        ) {
            if status != TaskStatus::Draft {
                let _ = orch.store.set_task_status(task.id, status);
            }
            if status == TaskStatus::NeedsYou {
                let _ = orch.store.set_task_unread(task.id, true);
            }
        }
    }
    let _ = orch.store.mark_import("work-items");
}
