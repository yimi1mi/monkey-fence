use gpui::prelude::*;
use gpui::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent_workspace::AgentWorkspace;
use crate::app_ctx::{choose_restore_project, plan_restore, AppCtx, ProjectSessionState};
use crate::console::ConsoleDock;
use crate::diff_view::DiffView;
use crate::editor::Editor;
use crate::file_index::FileIndex;
use crate::file_tree::FileTree;
use crate::navigation::{
    empty_state_for, BottomPanel, EmptyState, LeftPanel, NavAction, NavigationState, PrimarySurface,
};
use crate::project_context::{
    deepest_owning_project, normalize_project_path, ActivationTarget, ProjectContextState,
    ProjectId,
};
use crate::quick_open::{QuickItem, QuickOpen};
use crate::search::ProjectSearch;
use crate::settings::{Dismissed, Saved, SettingsView};
use crate::task_sidebar::{TaskSidebar, TaskSidebarEvent};
use crate::vcs_panel::VcsPanel;
use std::collections::HashMap;

actions!(
    workspace,
    [
        OpenFolder,
        QuickOpenFiles,
        CommandPalette,
        CloseTab,
        NextTab,
        PrevTab,
        ToggleLeftPanel,
        ShowExplorer,
        ShowVcs,
        ShowBoard,
        ShowAgent,
        ShowWork,
        ToggleConsole,
        OpenProjectSearch,
        ShowTasks,
        OpenSettings,
    ]
);

/// 常驻「所有操作」菜单的数据源。快捷键只做加速，所有工作区命令都必须
/// 能从鼠标打开的菜单触达；文件相关项在没有 Project/Tab 时不展示。
pub(crate) fn workspace_command_entries(
    has_project: bool,
    has_tab: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut commands = vec![
        ("open_folder", "添加 / 打开项目…  Ctrl+Shift+O"),
        ("toggle_left", "显示 / 隐藏左侧面板  Ctrl+B"),
        ("toggle_explorer", "显示资源管理器  Ctrl+Shift+E"),
        ("toggle_tasks", "显示任务与项目管理  Ctrl+Shift+W"),
        ("toggle_vcs", "显示版本控制  Ctrl+Shift+G"),
        ("show_agents", "Agent 看板  Ctrl+Shift+/"),
        ("show_pipeline", "Pipeline 视图"),
        ("toggle_console", "显示 / 隐藏终端  Ctrl+`"),
        ("project_search", "项目搜索…  Ctrl+Shift+F"),
        ("open_settings", "打开设置  Ctrl+,"),
    ];
    if has_project {
        commands.splice(
            1..1,
            [
                ("quick_open", "快速打开文件…  Ctrl+P"),
                ("refresh_tree", "刷新当前项目文件树"),
            ],
        );
    }
    if has_tab {
        commands.extend([
            ("close_tab", "关闭当前标签页  Ctrl+W"),
            ("next_tab", "下一个标签页  Ctrl+Tab"),
            ("prev_tab", "上一个标签页  Ctrl+Shift+Tab"),
            ("save_file", "保存当前文件  Ctrl+S"),
            ("undo", "撤销  Ctrl+Z"),
            ("redo", "重做  Ctrl+Y"),
            ("select_all", "全选  Ctrl+A"),
            ("duplicate_line", "复制当前行  Ctrl+D"),
            ("move_line_up", "上移当前行  Alt+Up"),
            ("move_line_down", "下移当前行  Alt+Down"),
        ]);
    }
    commands
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectSwitcherItem {
    pub id: ProjectId,
    pub name: String,
    pub path: String,
    pub active: bool,
}

/// Explorer 的项目选择器始终投影全部已打开项目，不因数量增加而截断。
pub(crate) fn project_switcher_items(
    projects: &[ProjectId],
    active: Option<&ProjectId>,
) -> Vec<ProjectSwitcherItem> {
    projects
        .iter()
        .map(|project| ProjectSwitcherItem {
            id: project.clone(),
            name: project.display_name(),
            path: project.as_path().display().to_string(),
            active: active == Some(project),
        })
        .collect()
}

/// 标签页内容:编辑器或 diff 视图(归属由所在 ProjectSurfaceState 决定)。
#[derive(Clone)]
enum Tab {
    Editor(Entity<Editor>),
    Diff(Entity<DiffView>),
}

impl Tab {
    fn title(&self, cx: &App) -> String {
        match self {
            Tab::Editor(ed) => {
                let b = ed.read(cx).buffer.read(cx);
                b.path()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "未命名".into())
            }
            Tab::Diff(d) => format!("{} (diff)", d.read(cx).title()),
        }
    }

    fn is_dirty(&self, cx: &App) -> bool {
        match self {
            Tab::Editor(ed) => ed.read(cx).buffer.read(cx).is_dirty(),
            Tab::Diff(_) => false,
        }
    }
}

/// 每项目界面状态:标签与 ConsoleDock 按 Project 分桶;
/// 切走后保留,切回时原样恢复(A→B→A 终端内容不变)。
struct ProjectSurfaceState {
    tabs: Vec<Tab>,
    active_tab: usize,
    console_dock: Option<Entity<ConsoleDock>>,
    /// 每项目一份文件索引:重复 Ctrl+P 不再重扫,关闭项目时随 surface 释放。
    file_index: Option<Entity<FileIndex>>,
}

impl Default for ProjectSurfaceState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: 0,
            console_dock: None,
            file_index: None,
        }
    }
}

/// 关闭项目的确认状态(存在活动 Agent Run 时必须先确认停止)。
struct CloseConfirm {
    root: PathBuf,
    name: String,
    active_runs: usize,
}

/// 分隔条拖拽进行中:记录起点鼠标位置与起点尺寸。
#[derive(Clone, Copy)]
enum PanelDrag {
    Left { start_x: f32, start_w: f32 },
    Bottom { start_y: f32, start_h: f32 },
    Activity { start_x: f32, start_w: f32 },
}

const LEFT_PANEL_MIN: f32 = 180.;
const LEFT_PANEL_MAX: f32 = 640.;
const BOTTOM_DOCK_MIN: f32 = 150.;
const BOTTOM_DOCK_MAX: f32 = 720.;
const ACTIVITY_BAR_MIN: f32 = 44.;
const ACTIVITY_BAR_MAX: f32 = 220.;
/// 活动栏拉到这个宽度以上时显示「图标 + 中文名」。
const ACTIVITY_BAR_EXPANDED: f32 = 72.;

pub struct Workspace {
    app: Arc<AppCtx>,
    /// 会话恢复进行中:open_folder 只注册项目,不抢占前台。
    restoring: bool,
    /// 唯一可信的当前项目/任务来源(activation seam)。
    context: ProjectContextState,
    /// 每项目界面状态,键为规范化项目根。
    surfaces: HashMap<PathBuf, ProjectSurfaceState>,
    /// Explorer 顶部的已打开项目列表是否展开。
    project_switcher_open: bool,
    /// 项目前台上下文:文件树 / VCS 面板(其他项目的任务与 Agent 后台继续运行)
    file_tree: Option<Entity<FileTree>>,
    vcs_panel: Option<Entity<VcsPanel>>,
    quick_open: Option<Entity<QuickOpen>>,
    search_overlay: Option<Entity<ProjectSearch>>,
    task_sidebar: Option<Entity<TaskSidebar>>,
    agent_workspace: Option<Entity<AgentWorkspace>>,
    close_confirm: Option<CloseConfirm>,
    navigation: NavigationState,
    settings_open: Option<Entity<SettingsView>>,
    status_message: SharedString,
    focus_handle: FocusHandle,
    focus_editor_next: bool,
    pending_focus: Option<FocusHandle>,
    editor_font: mf_agent::EditorConfig,
    left_panel_width: Pixels,
    bottom_panel_height: Pixels,
    activity_bar_width: Pixels,
    panel_drag: Option<PanelDrag>,
}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let app = AppCtx::new();
        let task_sidebar = cx.new(|cx| TaskSidebar::new(app.clone(), cx));
        let agent_workspace = cx.new(|cx| AgentWorkspace::new(app.clone(), cx));
        let mut ws = Self {
            app,
            restoring: true,
            context: ProjectContextState::new(),
            surfaces: HashMap::new(),
            project_switcher_open: false,
            file_tree: None,
            vcs_panel: None,
            quick_open: None,
            search_overlay: None,
            task_sidebar: Some(task_sidebar.clone()),
            agent_workspace: Some(agent_workspace.clone()),
            close_confirm: None,
            navigation: NavigationState::default(),
            settings_open: None,
            status_message: "就绪".into(),
            focus_handle: cx.focus_handle(),
            focus_editor_next: false,
            pending_focus: None,
            editor_font: mf_agent::EditorConfig::default(),
            left_panel_width: px(284.),
            bottom_panel_height: px(228.),
            // 默认即展开形态:图标 + 中文名;拖窄到 72px 以下退回纯图标
            activity_bar_width: px(96.),
            panel_drag: None,
        };
        // 唯一的轻量 snapshot 监听:revision 变化时把同一份快照推给
        // TaskSidebar 与 AgentWorkspace(两者不再各自轮询数据库)。
        {
            let hub = ws.app.overview.clone();
            cx.spawn(async move |this, cx| {
                let mut last = 0u64;
                loop {
                    if let Some(snap) = hub.snapshot_if_new(last) {
                        last = snap.revision;
                        let alive = this
                            .update(cx, |ws: &mut Workspace, cx| {
                                if let Some(sb) = &ws.task_sidebar {
                                    let s = snap.clone();
                                    sb.update(cx, |sb, cx| sb.set_overview(s, cx));
                                }
                                if let Some(aw) = &ws.agent_workspace {
                                    let s = snap.clone();
                                    aw.update(cx, |aw, cx| aw.set_overview(s, cx));
                                }
                                cx.notify();
                            })
                            .is_ok();
                        if !alive {
                            return;
                        }
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                }
            })
            .detach();
        }
        cx.subscribe(
            &task_sidebar,
            |ws, _sb, ev: &TaskSidebarEvent, cx| match ev {
                TaskSidebarEvent::AddProjectRequested => ws.prompt_open_folder(cx),
                TaskSidebarEvent::Activate(target) => ws.apply_activation(target, cx),
                TaskSidebarEvent::CloseRequested(root) => {
                    let root = root.clone();
                    ws.request_close_project(root, cx);
                }
                TaskSidebarEvent::TaskArchived(root, task_id) => {
                    // 归档后从上下文移除,不得残留在 selected_task_by_project
                    let (pid, _) = normalize_project_path(root);
                    ws.context.task_gone(&pid, *task_id);
                    ws.sync_context_views(cx);
                    ws.persist_session(cx);
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe(
            &agent_workspace,
            |ws, _aw, ev: &crate::agent_workspace::AgentWorkspaceEvent, cx| match ev {
                crate::agent_workspace::AgentWorkspaceEvent::Activate(target) => {
                    ws.apply_activation(target, cx);
                }
            },
        )
        .detach();
        cx.on_app_quit(|workspace, cx| {
            // 正常退出时重新检查 dirty Buffer，避免沿用文件刚打开时的
            // “干净”会话记录。崩溃仍沿用最后一次安全快照。
            workspace.persist_session(cx);
            async {}
        })
        .detach();
        ws.restore_session(cx);
        ws.restoring = false;
        ws
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 恢复上次会话:打开项目 → 恢复 context/干净编辑器标签 → 切到保存的前台。
    /// 恢复失败局部降级,不影响启动。
    fn restore_session(&mut self, cx: &mut Context<Self>) {
        let session = AppCtx::load_session();
        let mut projects = Vec::new();
        for path in session.projects.iter().filter(|path| path.is_dir()) {
            let root = normalize_project_path(path).0.root();
            if !projects.contains(&root) {
                projects.push(root);
            }
        }
        for root in &projects {
            self.open_folder(root.clone(), cx);
        }
        // 每项目恢复:仍存在的干净文件 + 仍存在的选中 Task
        let plans = {
            let app = self.app.clone();
            plan_restore(&session, move |root, task_id| {
                app.orchestrator_of(&root)
                    .and_then(|o| o.tasks().ok())
                    .map(|ts| {
                        ts.iter()
                            .any(|t| t.id == task_id && t.status != mf_agent::TaskStatus::Archived)
                    })
                    .unwrap_or(false)
            })
        };
        for plan in &plans {
            if !projects.contains(&plan.root) {
                continue;
            }
            for file in &plan.open_files {
                self.open_path(file, cx);
            }
            if let Some(task_id) = plan.selected_task_id {
                let (pid, _) = normalize_project_path(&plan.root);
                self.apply_activation(
                    &ActivationTarget::Restore {
                        project: pid,
                        task_id: Some(task_id),
                    },
                    cx,
                );
            }
        }
        // 前台:优先保存的 foreground;否则保持最近激活的项目
        let foreground = session.foreground.and_then(|fg| {
            if !fg.is_dir() {
                return None;
            }
            let root = normalize_project_path(&fg).0.root();
            projects.contains(&root).then_some(root)
        });
        let current = self
            .context
            .snapshot()
            .project
            .map(|project| project.root());
        if let Some(fg) = choose_restore_project(foreground, current, &projects) {
            if self.app.orchestrator_of(&fg).is_some() {
                let (id, _) = normalize_project_path(&fg);
                let saved_task = plans
                    .iter()
                    .find(|p| p.root == fg)
                    .and_then(|p| p.selected_task_id);
                self.apply_activation(
                    &ActivationTarget::Restore {
                        project: id,
                        task_id: saved_task,
                    },
                    cx,
                );
            }
        }
        if !projects.is_empty() {
            self.status_message = format!("已恢复 {} 个项目", projects.len()).into();
        }
    }

    /// 项目列表/前台/每项目状态变化后持久化会话。
    /// 只记录干净编辑器文件路径(未保存 Buffer 不持久化);Diff 不持久化。
    fn persist_session(&self, cx: &App) {
        let foreground = self.context.snapshot().project.map(|p| p.root());
        let project_states: Vec<ProjectSessionState> = self
            .context
            .known_projects()
            .iter()
            .map(|pid| ProjectSessionState {
                root: pid.root(),
                selected_task_id: self.context.last_task_of(pid),
                open_files: self.surface_clean_files(pid, cx),
                active_file: self.surface_active_file(pid, cx),
            })
            .collect();
        self.app.save_session(foreground.as_ref(), project_states);
    }

    fn surface_clean_files(
        &self,
        pid: &crate::project_context::ProjectId,
        cx: &App,
    ) -> Vec<PathBuf> {
        self.surfaces
            .get(&pid.root())
            .map(|s| {
                s.tabs
                    .iter()
                    .filter_map(|t| match t {
                        Tab::Editor(ed) => {
                            let b = ed.read(cx).buffer.read(cx);
                            if b.is_dirty() {
                                None
                            } else {
                                b.path().map(|p| p.to_path_buf())
                            }
                        }
                        Tab::Diff(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn surface_active_file(
        &self,
        pid: &crate::project_context::ProjectId,
        cx: &App,
    ) -> Option<PathBuf> {
        let root = pid.root();
        let surface = self.surfaces.get(&root)?;
        match surface.tabs.get(surface.active_tab)? {
            Tab::Editor(ed) => {
                let b = ed.read(cx).buffer.read(cx);
                if b.is_dirty() {
                    None
                } else {
                    b.path().map(|p| p.to_path_buf())
                }
            }
            Tab::Diff(_) => None,
        }
    }

    /// 前台项目根(来自 ActiveProjectContext);用于终端 cwd、快速打开、搜索与标签归属。
    fn active_root(&self) -> Option<PathBuf> {
        self.context.snapshot().project.map(|p| p.root())
    }

    /// 当前项目的界面状态(无项目时 None)。
    fn current_surface(&self) -> Option<&ProjectSurfaceState> {
        self.active_root().and_then(|r| self.surfaces.get(&r))
    }

    fn current_surface_mut(&mut self) -> Option<&mut ProjectSurfaceState> {
        let root = self.active_root()?;
        self.surfaces.get_mut(&root)
    }

    fn active_tab_index(&self) -> usize {
        self.current_surface().map(|s| s.active_tab).unwrap_or(0)
    }

    /// 当前项目 ConsoleDock(每项目分桶,A→B→A 内容保留)。
    fn current_console(&self) -> Option<Entity<ConsoleDock>> {
        self.current_surface().and_then(|s| s.console_dock.clone())
    }

    /// 所有项目的编辑器(字体设置等全局操作)。
    fn all_editor_tabs(&self) -> Vec<Entity<Editor>> {
        self.surfaces
            .values()
            .flat_map(|s| s.tabs.iter())
            .filter_map(|t| match t {
                Tab::Editor(ed) => Some(ed.clone()),
                Tab::Diff(_) => None,
            })
            .collect()
    }

    fn project_count(&self) -> usize {
        self.app.project_count()
    }

    /// 解析目录所属项目(基于规范化项目身份;路径本身先规范化再比较)。
    fn find_owning_project(&self, dir: &Path) -> Option<PathBuf> {
        let (dir_id, _) = normalize_project_path(dir);
        let dir = dir_id.as_path();
        deepest_owning_project(self.context.known_projects(), dir).map(|project| project.root())
    }

    // ---------- 多项目 ----------

    pub fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !path.is_dir() {
            self.status_message = format!("目录不存在: {}", path.display()).into();
            cx.notify();
            return;
        }
        let (id, warning) = normalize_project_path(&path);
        if let Some(w) = warning {
            log::warn!("{w}");
        }
        let root = id.root();
        // 已打开 → 只激活(不终止其他项目的 Agent,也不重复建 Store/Orchestrator)
        let already = self.app.orchestrator_of(&root).is_some();
        if already {
            self.apply_activation(&ActivationTarget::Project(id), cx);
            self.status_message = format!("已切换到 {}", root.display()).into();
            return;
        }
        match self.app.open_project(root.clone()) {
            Ok(_) => {
                self.context.open_project(id.clone());
                self.surfaces.entry(root.clone()).or_default();
                if self.restoring {
                    // 恢复期只注册项目;前台由保存的 foreground 决定
                    cx.notify();
                    return;
                }
                self.apply_activation(&ActivationTarget::Project(id), cx);
                self.status_message =
                    format!("已打开 {}({} 个项目)", root.display(), self.project_count()).into();
                self.persist_session(cx);
            }
            Err(e) => {
                self.status_message = format!("打开项目失败: {e:#}").into();
            }
        }
        cx.notify();
    }

    /// 唯一 activation 入口:调用 context module 后统一联动
    /// FileTree/VCS/搜索/分桶 surface/TaskSidebar/AgentWorkspace/mark-read/持久化。
    fn apply_activation(&mut self, target: &ActivationTarget, cx: &mut Context<Self>) {
        self.project_switcher_open = false;
        let outcome = self.context.activate(target.clone());
        if outcome.project_changed || outcome.task_changed {
            log::debug!(
                "activation: {:?} → {:?} (project_changed={}, task_changed={})",
                outcome.previous,
                outcome.current,
                outcome.project_changed,
                outcome.task_changed
            );
        }
        if outcome.project_changed {
            self.rebuild_foreground(cx);
        }
        // Task / Agent 卡片是注意力入口:打开时进入 Work 表面
        if matches!(
            target,
            ActivationTarget::Task { .. } | ActivationTarget::AgentRun { .. }
        ) {
            self.navigation.apply(NavAction::ShowWork);
        }
        self.sync_context_views(cx);
        // mark-read intent(幂等)
        if let Some((pid, task_id)) = &outcome.mark_task_read {
            if let Some(orch) = self.app.orchestrator_of(&pid.root()) {
                if let Err(e) = orch.mark_task_read(*task_id) {
                    log::warn!("清除任务未读失败: {e:#}");
                }
            }
        }
        if let Some((pid, session_id)) = &outcome.mark_session_read {
            if let Some(orch) = self.app.orchestrator_of(&pid.root()) {
                if let Err(e) = orch.mark_session_read(*session_id) {
                    log::warn!("清除会话未读失败: {e:#}");
                }
            }
        }
        self.persist_session(cx);
        cx.notify();
    }

    /// 将同一份 ActiveProjectContext 投影到两个 UI module。
    /// Project 与 Task 不得由调用方分别拼接。
    fn sync_context_views(&mut self, cx: &mut Context<Self>) {
        let active = self.context.snapshot();
        let selection = match (active.project.as_ref(), active.task_id) {
            (Some(project), Some(task_id)) => Some((project.root(), task_id)),
            _ => None,
        };
        if let Some(sidebar) = &self.task_sidebar {
            sidebar.update(cx, |s, cx| {
                s.set_selection(selection.clone(), cx);
                s.set_foreground(active.project.as_ref().map(|p| p.root()), cx);
            });
        }
        if let Some(aw) = &self.agent_workspace {
            aw.update(cx, |aw, cx| aw.set_selected_task(selection, cx));
        }
    }

    /// 项目切换后重建文件树/VCS/搜索;FileTree 与 VcsPanel 可重建,不缓存。
    fn rebuild_foreground(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            self.file_tree = None;
            self.vcs_panel = None;
            self.search_overlay = None;
            return;
        };
        self.quick_open = None;
        self.search_overlay = None;
        let _index = cx.new(|cx| FileIndex::new(root.clone(), cx));
        let tree = cx.new(|cx| FileTree::new(root.clone(), cx));
        let weak = cx.weak_entity();
        tree.update(cx, |tree, _| {
            tree.set_on_open(move |path, _window, cx| {
                weak.update(cx, |workspace, cx| workspace.open_path(path, cx))
                    .ok();
            });
        });
        self.file_tree = Some(tree);
        let vcs = cx.new(|cx| VcsPanel::new(root.clone(), cx));
        let weak = cx.weak_entity();
        vcs.update(cx, |vcs, _| {
            vcs.set_on_open_diff(move |title, local_path, _window, cx| {
                weak.update(cx, |workspace, cx| {
                    workspace.open_diff(&title, &local_path, cx)
                })
                .ok();
            });
        });
        self.vcs_panel = Some(vcs);
        // 打开项目时刷新插件目录(项目级技能等)
        self.app.refresh_catalog();
        // 恢复该项目的活动标签焦点
        if let Some(surface) = self.current_surface() {
            if !surface.tabs.is_empty() {
                let idx = surface.active_tab.min(surface.tabs.len() - 1);
                match &surface.tabs[idx] {
                    Tab::Editor(_) => self.focus_active(cx),
                    Tab::Diff(dv) => self.pending_focus = Some(dv.read(cx).focus_handle()),
                }
            }
        }
    }

    /// 请求关闭项目;存在活动 Agent Run 时弹确认。
    fn request_close_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let active_runs = self.app.active_runs_of(&root);
        if active_runs > 0 {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            self.close_confirm = Some(CloseConfirm {
                root,
                name,
                active_runs,
            });
        } else {
            self.do_close_project(&root, cx);
        }
        cx.notify();
    }

    fn do_close_project(&mut self, root: &PathBuf, cx: &mut Context<Self>) {
        self.app.close_project(root);
        let (id, _) = normalize_project_path(root);
        let outcome = self.context.remove_project(&id);
        self.surfaces.remove(&id.root());
        self.close_confirm = None;
        if outcome.project_changed {
            self.rebuild_foreground(cx);
        }
        self.sync_context_views(cx);
        // 关闭后台项目也必须从 session.json 移除。
        self.persist_session(cx);
        self.status_message = "项目已关闭".into();
        cx.notify();
    }

    fn confirm_close(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        let Some(cc) = self.close_confirm.take() else {
            return;
        };
        if confirmed {
            self.do_close_project(&cc.root, cx);
        }
        cx.notify();
    }

    // ---------- 文件 ----------

    pub fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowCode);
        // 先解析文件所属 Project;跨项目文件先原子激活所属项目再打开
        let owning = path.parent().map(|p| self.find_owning_project(p)).flatten();
        let Some(project_root) = owning.or_else(|| self.active_root()) else {
            self.status_message = "先打开一个文件夹再打开文件".into();
            cx.notify();
            return;
        };
        if Some(&project_root) != self.active_root().as_ref() {
            let (id, _) = normalize_project_path(&project_root);
            self.apply_activation(&ActivationTarget::Tab { project: id }, cx);
        }
        let path = path.to_path_buf();
        let surface = self.surfaces.entry(project_root).or_default();
        if let Some(pos) = surface.tabs.iter().position(|t| match t {
            Tab::Editor(ed) => ed
                .read(cx)
                .buffer
                .read(cx)
                .path()
                .map(|p| p == &path)
                .unwrap_or(false),
            Tab::Diff(_) => false,
        }) {
            surface.active_tab = pos;
            // 已打开:未修改则从磁盘重载(agent 可能改动了文件)
            if let Some(Tab::Editor(ed)) = surface.tabs.get(pos) {
                if !ed.read(cx).buffer.read(cx).is_dirty() {
                    ed.update(cx, |editor, cx| {
                        editor.buffer.update(cx, |buffer, _| {
                            let _ = buffer.reload_from_disk();
                        });
                        cx.notify();
                    });
                }
            }
            self.focus_active(cx);
            self.persist_session(cx);
            cx.notify();
            return;
        }
        match mf_core::buffer::Buffer::load(&path) {
            Ok(buf) => {
                let buffer = cx.new(|_| buf);
                let editor = cx.new(|cx| Editor::new(buffer, cx));
                let font = self.editor_font.clone();
                let weak = cx.weak_entity();
                editor.update(cx, |ed, cx| {
                    ed.set_font(&font, cx);
                    ed.set_on_saved(move |_, cx| {
                        let weak = weak.clone();
                        // 当前 Editor 正在 update；延后到本轮结束，避免 Workspace
                        // 持久化遍历标签时重入读取同一个 Entity。
                        cx.defer(move |cx| {
                            weak.update(cx, |workspace, cx| workspace.persist_session(cx))
                                .ok();
                        });
                    });
                });
                surface.tabs.push(Tab::Editor(editor));
                surface.active_tab = surface.tabs.len() - 1;
                self.focus_active(cx);
                self.persist_session(cx);
                cx.notify();
            }
            Err(e) => {
                self.status_message = format!("打开失败: {}", e).into();
                cx.notify();
            }
        }
    }

    /// 打开文件的工作区 diff 标签页(P4 优先,回退 Git);Diff 标签归属创建它的 Project。
    pub fn open_diff(&mut self, title: &str, local_path: &Path, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowCode);
        let root = self
            .find_owning_project(local_path.parent().unwrap_or(Path::new(".")))
            .or_else(|| self.active_root());
        let Some(root) = root else {
            return;
        };
        let (diff_text, git_review) = {
            let p4 = mf_vcs::p4::P4::new(&root);
            match p4.diff_file(local_path) {
                Ok(t) if !t.trim().is_empty() => (t, false),
                Ok(_) => ("(无差异)".to_string(), false),
                Err(_) => {
                    let rel = local_path.strip_prefix(&root).unwrap_or(local_path);
                    let diff = mf_vcs::git::Git::open(&root)
                        .and_then(|g| g.diff_file(rel))
                        .unwrap_or_else(|e| format!("获取 diff 失败: {e}"));
                    (diff, true)
                }
            }
        };
        let view = cx.new(|cx| DiffView::new(title, &diff_text, cx));
        let view_id = view.entity_id();
        let weak = cx.weak_entity();
        let root_for_reject = root.clone();
        let root_for_remove = root.clone();
        if git_review {
            view.update(cx, |dv, _| {
                dv.set_on_reject(move |patch, _window, cx| {
                    let weak = weak.clone();
                    let review_root = root_for_reject.clone();
                    let remove_root = root_for_remove.clone();
                    cx.spawn(async move |cx| {
                        let command_root = review_root.clone();
                        let applied = cx.background_executor().spawn(async move {
                            use std::io::Write as _;
                            let child = std::process::Command::new("git")
                                .arg("apply")
                                .arg("-R")
                                .arg("--recount")
                                .current_dir(&command_root)
                                .stdin(std::process::Stdio::piped())
                                .stdout(std::process::Stdio::piped())
                                .stderr(std::process::Stdio::piped())
                                .spawn();
                            match child {
                                Ok(mut c) => {
                                    if let Some(mut sin) = c.stdin.take() {
                                        let _ = sin.write_all(patch.as_bytes());
                                    }
                                    let out = c.wait_with_output();
                                    out.map(|o| {
                                        if o.status.success() {
                                            Ok(())
                                        } else {
                                            Err(String::from_utf8_lossy(&o.stderr)
                                                .chars()
                                                .take(200)
                                                .collect())
                                        }
                                    })
                                    .unwrap_or_else(|e| Err(e.to_string()))
                                }
                                Err(e) => Err(e.to_string()),
                            }
                        });
                        let r = applied.await;
                        weak.update(cx, move |ws: &mut Workspace, cx| {
                            match r {
                                Ok(()) => ws.status_message = "hunk 已拒绝(git apply -R)".into(),
                                Err(e) => ws.status_message = format!("拒绝失败:{e}").into(),
                            } // 关闭对应 diff 标签(在其所属项目的分桶中)
                            if let Some(surface) = ws.surfaces.get_mut(&remove_root) {
                                if let Some(idx) = surface.tabs.iter().position(
                                    |t| matches!(t, Tab::Diff(diff) if diff.entity_id() == view_id),
                                ) {
                                    surface.tabs.remove(idx);
                                    if surface.active_tab >= idx && surface.active_tab > 0 {
                                        surface.active_tab -= 1;
                                    }
                                }
                            }
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                });
            });
        }
        let surface = self.surfaces.entry(root).or_default();
        surface.tabs.push(Tab::Diff(view.clone()));
        surface.active_tab = surface.tabs.len() - 1;
        if let Some(Tab::Diff(dv)) = surface.tabs.get(surface.active_tab) {
            self.pending_focus = Some(dv.read(cx).focus_handle());
        }
        cx.notify();
    }

    fn focus_active(&mut self, _cx: &mut Context<Self>) {
        if self
            .current_surface()
            .map(|s| !s.tabs.is_empty())
            .unwrap_or(false)
        {
            self.focus_editor_next = true;
        }
    }

    fn close_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_at(self.active_tab_index(), cx);
    }

    fn close_tab_in(surface: &mut ProjectSurfaceState, index: usize, cx: &mut Context<Self>) {
        if let Some(tab) = surface.tabs.get(index) {
            if tab.is_dirty(cx) {
                return;
            }
        } else {
            return;
        }
        surface.tabs.remove(index);
        if index < surface.active_tab {
            surface.active_tab -= 1;
        }
        surface.active_tab = surface.active_tab.min(surface.tabs.len().saturating_sub(1));
    }

    fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(surface) = self.current_surface_mut() else {
            return;
        };
        if index >= surface.tabs.len() {
            return;
        }
        surface.active_tab = index;
        match surface.tabs.get(index) {
            Some(Tab::Editor(_)) => self.focus_active(cx),
            Some(Tab::Diff(diff)) => {
                self.pending_focus = Some(diff.read(cx).focus_handle());
            }
            None => {}
        }
        self.persist_session(cx);
        cx.notify();
    }

    fn close_tab_at(&mut self, index: usize, cx: &mut Context<Self>) {
        let dirty = self
            .current_surface()
            .and_then(|s| s.tabs.get(index))
            .map(|t| t.is_dirty(cx))
            .unwrap_or(false);
        if dirty {
            self.status_message = "文件尚未保存;保存后才能关闭标签".into();
            cx.notify();
            return;
        }
        if let Some(surface) = self.current_surface_mut() {
            Workspace::close_tab_in(surface, index, cx);
        }
        self.persist_session(cx);
        cx.notify();
    }

    fn next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_next_tab(cx);
    }

    fn activate_next_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(surface) = self.current_surface_mut() {
            if !surface.tabs.is_empty() {
                let next = (surface.active_tab + 1) % surface.tabs.len();
                self.activate_tab(next, cx);
            }
        }
    }

    fn prev_tab(&mut self, _: &PrevTab, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_prev_tab(cx);
    }

    fn activate_prev_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(surface) = self.current_surface_mut() {
            if !surface.tabs.is_empty() {
                let previous = (surface.active_tab + surface.tabs.len() - 1) % surface.tabs.len();
                self.activate_tab(previous, cx);
            }
        }
    }

    // ---------- 快速打开 / 命令面板 ----------

    fn show_quick_open_files(
        &mut self,
        _: &QuickOpenFiles,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_quick_files(cx);
    }

    fn open_quick_files(&mut self, cx: &mut Context<Self>) {
        self.settings_open = None;
        let Some(root) = self.active_root() else {
            self.status_message = "请先从“所有操作”或任务侧栏添加项目".into();
            cx.notify();
            return;
        };
        let index = {
            let surface = self.surfaces.entry(root.clone()).or_default();
            match surface.file_index.clone() {
                Some(idx) => idx,
                None => {
                    let idx = cx.new(|cx| FileIndex::new(root.clone(), cx));
                    surface.file_index = Some(idx.clone());
                    idx
                }
            }
        };
        let qo = cx.new(|cx| QuickOpen::files(index, cx));
        self.wire_quick_open(qo, cx);
    }

    fn show_command_palette(
        &mut self,
        _: &CommandPalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_open = None;
        let qo = cx.new(|cx| QuickOpen::commands(cx));
        let cmds = workspace_command_entries(
            self.project_count() > 0,
            self.current_surface()
                .is_some_and(|surface| !surface.tabs.is_empty()),
        )
        .into_iter()
        .map(|(id, label)| (id.into(), label.into()))
        .collect();
        qo.update(cx, |q, cx| {
            q.register_commands(cmds, cx);
        });
        self.wire_quick_open(qo, cx);
    }

    fn wire_quick_open(&mut self, qo: Entity<QuickOpen>, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        qo.update(cx, |q, _| {
            q.set_on_pick(move |item, _window, cx| {
                weak.update(cx, |ws, cx| {
                    match item {
                        QuickItem::File(p) => {
                            let p = p.clone();
                            let path = if p.is_absolute() {
                                p
                            } else {
                                match ws.active_root() {
                                    Some(root) => root.join(&p),
                                    None => p,
                                }
                            };
                            ws.navigation.apply(NavAction::ShowCode);
                            ws.open_path(&path, cx);
                            ws.quick_open = None;
                        }
                        QuickItem::Command { id, .. } => {
                            ws.quick_open = None;
                            match id.as_ref() {
                                "open_folder" => ws.prompt_open_folder(cx),
                                "quick_open" => ws.open_quick_files(cx),
                                "toggle_left" => ws.navigation.apply(NavAction::ToggleLeft),
                                "toggle_explorer" => ws.navigation.apply(NavAction::ShowExplorer),
                                "toggle_vcs" => ws.navigation.apply(NavAction::ShowVcs),
                                "toggle_tasks" => ws.navigation.apply(NavAction::ShowTasks),
                                "show_agents" => ws.show_agent_workspace(AgentTab::Agents, cx),
                                "show_pipeline" => ws.show_agent_workspace(AgentTab::Pipeline, cx),
                                "toggle_console" => ws.toggle_console(cx),
                                "project_search" => ws.open_project_search(cx),
                                "open_settings" => ws.open_settings(cx),
                                "close_tab" => ws.close_tab_at(ws.active_tab_index(), cx),
                                "next_tab" => ws.activate_next_tab(cx),
                                "prev_tab" => ws.activate_prev_tab(cx),
                                "save_file" => {
                                    ws.focus_active(cx);
                                    cx.defer(|cx| cx.dispatch_action(&crate::editor::Save));
                                }
                                "undo" => {
                                    ws.focus_active(cx);
                                    cx.defer(|cx| cx.dispatch_action(&crate::editor::Undo));
                                }
                                "redo" => {
                                    ws.focus_active(cx);
                                    cx.defer(|cx| cx.dispatch_action(&crate::editor::Redo));
                                }
                                "select_all" => {
                                    ws.focus_active(cx);
                                    cx.defer(|cx| cx.dispatch_action(&crate::editor::SelectAll));
                                }
                                "duplicate_line" => {
                                    ws.focus_active(cx);
                                    cx.defer(|cx| {
                                        cx.dispatch_action(&crate::editor::DuplicateLine)
                                    });
                                }
                                "move_line_up" => {
                                    ws.focus_active(cx);
                                    cx.defer(|cx| cx.dispatch_action(&crate::editor::MoveLineUp));
                                }
                                "move_line_down" => {
                                    ws.focus_active(cx);
                                    cx.defer(|cx| cx.dispatch_action(&crate::editor::MoveLineDown));
                                }
                                "refresh_tree" => {
                                    if let Some(t) = &ws.file_tree {
                                        t.update(cx, |t, _| t.refresh_all());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        });
        cx.subscribe(&qo, move |ws, _, _: &crate::quick_open::Dismissed, cx| {
            ws.dismiss_quick_open(cx);
        })
        .detach();
        // QuickOpen 是独立的文本输入客户端。仅把实体挂进渲染树并不会
        // 自动转移焦点；若不显式排队，Workspace 的焦点兜底会继续接收
        // 所有按键，导致英文、中文和退格都无法到达输入框。
        self.pending_focus = Some(qo.read(cx).focus_handle(cx));
        self.quick_open = Some(qo.clone());
        cx.notify();
    }

    fn dismiss_quick_open(&mut self, cx: &mut Context<Self>) {
        if self.quick_open.take().is_some() {
            self.focus_active(cx);
            cx.notify();
        }
    }

    fn show_agent_workspace(&mut self, tab: AgentTab, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowWork);
        if let Some(aw) = &self.agent_workspace {
            aw.update(cx, |aw, cx| {
                aw.show_tab(tab, cx);
            });
        }
        cx.notify();
    }

    // ---------- 打开文件夹对话框 ----------

    fn prompt_open_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(dir) = paths.first() {
                    weak.update(cx, |ws, cx| {
                        ws.open_folder(dir.clone(), cx);
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    fn act_open_folder(&mut self, _: &OpenFolder, _: &mut Window, cx: &mut Context<Self>) {
        self.prompt_open_folder(cx);
    }

    fn act_toggle_left(&mut self, _: &ToggleLeftPanel, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ToggleLeft);
        cx.notify();
    }

    fn act_show_explorer(&mut self, _: &ShowExplorer, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowExplorer);
        cx.notify();
    }

    fn act_show_vcs(&mut self, _: &ShowVcs, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowVcs);
        cx.notify();
    }

    fn act_show_board(&mut self, _: &ShowBoard, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowTasks);
        cx.notify();
    }

    fn act_show_agent(&mut self, _: &ShowAgent, _: &mut Window, cx: &mut Context<Self>) {
        self.show_agent_workspace(AgentTab::Agents, cx);
    }

    fn act_show_work(&mut self, _: &ShowWork, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowWork);
        cx.notify();
    }

    fn act_open_project_search(
        &mut self,
        _: &OpenProjectSearch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_project_search(cx);
    }

    fn open_project_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = self.search_overlay.clone() {
            self.navigation.apply(NavAction::ShowSearch);
            self.pending_focus = Some(search.read(cx).focus_handle(cx));
            cx.notify();
            return;
        }
        let Some(root) = self.active_root() else {
            return;
        };
        let s = cx.new(|cx| ProjectSearch::new(root, cx));
        let weak = cx.weak_entity();
        s.update(cx, |sv, _| {
            sv.set_embedded(true);
            sv.set_on_open(move |path, row, _window, cx| {
                weak.update(cx, |ws, cx| {
                    ws.open_path(path, cx);
                    if let Some(Tab::Editor(ed)) =
                        ws.current_surface().and_then(|s| s.tabs.get(s.active_tab))
                    {
                        ed.update(cx, |e, cx| e.goto_line(row, cx));
                    }
                    cx.notify();
                })
                .ok();
            });
        });
        cx.subscribe(&s, move |ws, _, _: &crate::search::Dismissed, cx| {
            if ws.navigation.bottom == Some(BottomPanel::Search) {
                ws.navigation.apply(NavAction::CloseBottom);
            }
            ws.focus_active(cx);
            cx.notify();
        })
        .detach();
        self.pending_focus = Some(s.read(cx).focus_handle(cx));
        self.search_overlay = Some(s);
        self.navigation.apply(NavAction::ShowSearch);
        cx.notify();
    }

    fn act_toggle_console(&mut self, _: &ToggleConsole, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_console(cx);
    }

    fn act_show_tasks(&mut self, _: &ShowTasks, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowTasks);
        cx.notify();
    }

    fn act_open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(cx);
    }

    // ---------- 控制台分屏 ----------

    /// 每项目一份 ConsoleDock;首次创建 cwd 永远等于该项目 root。
    fn ensure_console(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let surface = self.surfaces.entry(root.clone()).or_default();
        if surface.console_dock.is_none() {
            let cwd = surface_cwd(&root);
            surface.console_dock = Some(cx.new(|cx| ConsoleDock::new_in(cwd, cx)));
        }
    }

    pub fn toggle_console(&mut self, cx: &mut Context<Self>) {
        self.ensure_console(cx);
        self.navigation.apply(NavAction::ToggleTerminal);
        cx.notify();
    }

    // ---------- 设置 ----------

    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings_open.is_some() {
            self.settings_open = None;
            cx.notify();
            return;
        }
        self.quick_open = None;
        let app = self.app.clone();
        let s = cx.new(|cx| SettingsView::new_with_app(app, cx));
        cx.subscribe(&s, move |ws, _, ev: &Saved, cx| {
            ws.apply_editor_font(&ev.0.editor, cx);
            ws.editor_font = ev.0.editor.clone();
            // 引擎并发设置实时生效
            ws.app
                .limiter
                .set_max(ev.0.engine.global_concurrency.max(1));
            *ws.app.config.lock() = ev.0.clone();
            ws.app.registry.update_config(ev.0.clone());
            ws.status_message = "设置已保存".into();
            cx.notify();
        })
        .detach();
        cx.subscribe(&s, move |ws, _, _: &Dismissed, cx| {
            ws.settings_open = None;
            ws.focus_active(cx);
            cx.notify();
        })
        .detach();
        self.pending_focus = Some(s.read(cx).focus_handle(cx));
        self.settings_open = Some(s);
        cx.notify();
    }

    fn apply_editor_font(&mut self, cfg: &mf_agent::EditorConfig, cx: &mut Context<Self>) {
        for ed in self.all_editor_tabs() {
            ed.update(cx, |ed, cx| ed.set_font(cfg, cx));
        }
    }
}

/// 终端 cwd:项目 root;无项目时不创建终端。
fn surface_cwd(root: &Path) -> PathBuf {
    root.to_path_buf()
}

/// AgentWorkspace 顶层页签(命令面板用)。
#[derive(Debug, Clone, Copy)]
pub enum AgentTab {
    Agents,
    Pipeline,
    Runs,
}

// ---------- 渲染 ----------

impl Workspace {
    fn render_welcome(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(336.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("welcome-open-folder")
                            .w_full()
                            .h(px(52.))
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_3()
                            .rounded_lg()
                            .bg(rgb(crate::theme::Theme::accent()))
                            .text_color(rgb(crate::theme::Theme::bg()))
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(0x72b3ff)))
                            .child(div().w(px(22.)).text_size(px(18.)).child("▱"))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_size(px(12.5))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("打开文件夹"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(9.5))
                                            .child("Ctrl+Shift+O(可同时打开多个项目)"),
                                    ),
                            )
                            .on_click(cx.listener(|ws: &mut Workspace, _, _, cx| {
                                ws.prompt_open_folder(cx);
                            })),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id("welcome-quick-open")
                                    .w(px(164.))
                                    .h(px(52.))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::border()))
                                    .bg(rgb(crate::theme::Theme::bg_elevated()))
                                    .cursor_pointer()
                                    .hover(|d| {
                                        d.bg(rgb(crate::theme::Theme::bg_hover()))
                                            .border_color(rgb(crate::theme::Theme::accent_dim()))
                                    })
                                    .child(
                                        div()
                                            .w(px(18.))
                                            .text_size(px(16.))
                                            .text_color(rgb(crate::theme::Theme::accent()))
                                            .child("⌕"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(
                                                div()
                                                    .text_size(px(11.5))
                                                    .text_color(rgb(crate::theme::Theme::fg()))
                                                    .child("快速打开"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(9.5))
                                                    .text_color(
                                                        rgb(crate::theme::Theme::fg_faint()),
                                                    )
                                                    .child("Ctrl+P"),
                                            ),
                                    )
                                    .on_click(cx.listener(|ws: &mut Workspace, _, window, cx| {
                                        ws.show_quick_open_files(&QuickOpenFiles, window, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("welcome-agent-workspace")
                                    .w(px(164.))
                                    .h(px(52.))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::border()))
                                    .bg(rgb(crate::theme::Theme::bg_elevated()))
                                    .cursor_pointer()
                                    .hover(|d| {
                                        d.bg(rgb(crate::theme::Theme::bg_hover()))
                                            .border_color(rgb(crate::theme::Theme::accent_dim()))
                                    })
                                    .child(
                                        div()
                                            .w(px(18.))
                                            .text_size(px(15.))
                                            .text_color(rgb(crate::theme::Theme::accent()))
                                            .child("✦"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(
                                                div()
                                                    .text_size(px(11.5))
                                                    .text_color(rgb(crate::theme::Theme::fg()))
                                                    .child("Agent 工作区"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(9.5))
                                                    .text_color(
                                                        rgb(crate::theme::Theme::fg_faint()),
                                                    )
                                                    .child("Ctrl+Shift+/"),
                                            ),
                                    )
                                    .on_click(cx.listener(|ws: &mut Workspace, _, _, cx| {
                                        ws.show_agent_workspace(AgentTab::Agents, cx);
                                    })),
                            ),
                    ),
            )
    }

    fn render_project_ready(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(336.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("project-quick-open")
                            .w_full()
                            .h(px(52.))
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_3()
                            .rounded_lg()
                            .bg(rgb(crate::theme::Theme::accent()))
                            .text_color(rgb(crate::theme::Theme::bg()))
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(0x72b3ff)))
                            .child(div().w(px(22.)).text_size(px(17.)).child("⌕"))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_size(px(12.5))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("快速打开文件"),
                                    )
                                    .child(div().text_size(px(9.5)).child("Ctrl+P")),
                            )
                            .on_click(cx.listener(|ws: &mut Workspace, _, window, cx| {
                                ws.show_quick_open_files(&QuickOpenFiles, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id("project-new-task")
                                    .w(px(164.))
                                    .h(px(52.))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::border()))
                                    .bg(rgb(crate::theme::Theme::bg_elevated()))
                                    .cursor_pointer()
                                    .hover(|d| {
                                        d.bg(rgb(crate::theme::Theme::bg_hover()))
                                            .border_color(rgb(crate::theme::Theme::accent_dim()))
                                    })
                                    .child(
                                        div()
                                            .w(px(18.))
                                            .text_size(px(15.))
                                            .text_color(rgb(crate::theme::Theme::accent()))
                                            .child("▦"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(
                                                div()
                                                    .text_size(px(11.5))
                                                    .text_color(rgb(crate::theme::Theme::fg()))
                                                    .child("新建任务"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(9.5))
                                                    .text_color(
                                                        rgb(crate::theme::Theme::fg_faint()),
                                                    )
                                                    .child("Ctrl+Shift+W"),
                                            ),
                                    )
                                    .on_click(cx.listener(|ws: &mut Workspace, _, _, cx| {
                                        ws.navigation.apply(NavAction::ShowTasks);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("project-command-palette")
                                    .w(px(164.))
                                    .h(px(52.))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::border()))
                                    .bg(rgb(crate::theme::Theme::bg_elevated()))
                                    .cursor_pointer()
                                    .hover(|d| {
                                        d.bg(rgb(crate::theme::Theme::bg_hover()))
                                            .border_color(rgb(crate::theme::Theme::accent_dim()))
                                    })
                                    .child(
                                        div()
                                            .w(px(18.))
                                            .text_size(px(15.))
                                            .text_color(rgb(crate::theme::Theme::accent()))
                                            .child("⌘"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(
                                                div()
                                                    .text_size(px(11.5))
                                                    .text_color(rgb(crate::theme::Theme::fg()))
                                                    .child("命令面板"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(9.5))
                                                    .text_color(
                                                        rgb(crate::theme::Theme::fg_faint()),
                                                    )
                                                    .child("Ctrl+Shift+P"),
                                            ),
                                    )
                                    .on_click(cx.listener(|ws: &mut Workspace, _, window, cx| {
                                        ws.show_command_palette(&CommandPalette, window, cx);
                                    })),
                            ),
                    ),
            )
    }

    /// 只渲染当前项目的标签(标签栏本身已按项目分桶)。
    fn render_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let tabs: Vec<(usize, String, bool, bool)> = self
            .current_surface()
            .map(|s| &s.tabs)
            .map(|tabs| {
                tabs.iter()
                    .enumerate()
                    .map(|(i, t)| (i, t.title(cx), i == self.active_tab_index(), t.is_dirty(cx)))
                    .collect()
            })
            .unwrap_or_default();
        let mut el = div()
            .id("tab-strip")
            .flex()
            .flex_row()
            .h(px(36.))
            .items_end()
            .gap_1()
            .px_2p5()
            .border_b_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg_panel()));
        for (index, name, is_active, dirty) in tabs {
            let label = if name.ends_with("(diff)") {
                format!("◆ {name}")
            } else {
                name.clone()
            };
            el = el.child(
                div()
                    .id(("tab", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(29.))
                    .rounded_t_lg()
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(is_active, |d| {
                        d.bg(rgb(crate::theme::Theme::bg_elevated()))
                            .border_1()
                            .border_b_0()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_color(rgb(crate::theme::Theme::fg()))
                    })
                    .when(!is_active, |d| {
                        d.text_color(rgb(crate::theme::Theme::fg_dim()))
                            .hover(|h| h.bg(rgb(crate::theme::Theme::bg_hover())))
                    })
                    .on_click(cx.listener(move |workspace: &mut Workspace, _, _, cx| {
                        workspace.navigation.apply(NavAction::ShowCode);
                        workspace.activate_tab(index, cx);
                    }))
                    .child(label)
                    .when(dirty, |d| {
                        d.child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(rgb(crate::theme::Theme::warning())),
                        )
                    })
                    .child(
                        div()
                            .id(("close-tab", index))
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_size(px(10.))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .hover(|d| {
                                d.bg(rgb(crate::theme::Theme::bg_hover()))
                                    .text_color(rgb(crate::theme::Theme::fg()))
                            })
                            .child("×")
                            .on_click(cx.listener(move |workspace: &mut Workspace, _, _, cx| {
                                cx.stop_propagation();
                                workspace.close_tab_at(index, cx);
                            })),
                    ),
            );
        }
        el
    }

    fn render_activity_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let icons: [(&'static str, &'static str, &'static str, LeftPanel); 3] = [
            ("▱", "项目 Ctrl+Shift+E", "项目", LeftPanel::Explorer),
            ("▦", "任务 Ctrl+Shift+W", "任务", LeftPanel::Tasks),
            ("⎇", "版控 Ctrl+Shift+G", "版控", LeftPanel::Vcs),
        ];
        let expanded = self.activity_bar_width.as_f32() >= ACTIVITY_BAR_EXPANDED;
        let vcs_count = self
            .vcs_panel
            .as_ref()
            .map(|v| v.read(cx).change_count())
            .unwrap_or(0);
        let unread = self
            .task_sidebar
            .as_ref()
            .map(|t| t.read(cx).unread_count())
            .unwrap_or(0);
        let mut bar = div()
            .id("activity-bar")
            .w(self.activity_bar_width)
            .flex()
            .flex_col()
            .items_center()
            .py_2()
            .gap_1()
            .when(expanded, |d| d.items_stretch().px_1())
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .border_r_1()
            .border_color(rgb(crate::theme::Theme::border()));
        bar = bar.child(activity_button(
            "⋮",
            "所有操作 Ctrl+Shift+P",
            "所有操作",
            expanded,
            self.quick_open.is_some(),
            cx.listener(|this: &mut Workspace, _, window, cx| {
                if this.quick_open.is_some() {
                    this.dismiss_quick_open(cx);
                } else {
                    this.show_command_palette(&CommandPalette, window, cx);
                }
            }),
        ));
        for (icon, tip, text, panel) in icons {
            let is_active = self.navigation.left == Some(panel);
            let badge = if panel == LeftPanel::Vcs {
                vcs_count
            } else if panel == LeftPanel::Tasks {
                unread
            } else {
                0
            };
            bar = bar.child(
                div()
                    .id(ElementId::Name(format!("act-{tip}").into()))
                    .relative()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .text_size(px(17.))
                    .cursor_pointer()
                    .when(expanded, |d| d.w_full().h(px(36.)).px_2().gap_2())
                    .when(!expanded, |d| d.size(px(36.)).justify_center())
                    .when(is_active, |d| {
                        d.bg(rgb(crate::theme::Theme::bg_active()))
                            .text_color(rgb(crate::theme::Theme::accent()))
                    })
                    .when(!is_active, |d| {
                        d.text_color(rgb(crate::theme::Theme::fg_dim()))
                            .hover(|h| h.bg(rgb(crate::theme::Theme::bg_hover())))
                    })
                    .child(icon)
                    .when(expanded, |d| {
                        d.child(div().min_w_0().flex_1().text_size(px(12.)).child(text))
                    })
                    .when(badge > 0, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(1.))
                                .right(px(1.))
                                .min_w(px(14.))
                                .h(px(14.))
                                .px_1()
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(rgb(if panel == LeftPanel::Tasks {
                                    crate::theme::Theme::warning()
                                } else {
                                    crate::theme::Theme::accent()
                                }))
                                .text_color(rgb(crate::theme::Theme::bg()))
                                .text_size(px(9.))
                                .child(badge.to_string()),
                        )
                    })
                    .on_click({
                        let panel = panel;
                        cx.listener(move |this: &mut Workspace, _, _, cx| {
                            if this.navigation.left == Some(panel) {
                                this.navigation.apply(NavAction::ToggleLeft);
                            } else {
                                this.navigation.apply(match panel {
                                    LeftPanel::Explorer => NavAction::ShowExplorer,
                                    LeftPanel::Vcs => NavAction::ShowVcs,
                                    LeftPanel::Tasks => NavAction::ShowTasks,
                                });
                            }
                            cx.notify();
                        })
                    }),
            );
        }
        let terminal_active = self.navigation.bottom == Some(BottomPanel::Terminal);
        let search_active = self.navigation.bottom == Some(BottomPanel::Search);
        let agent_active = self.navigation.surface == PrimarySurface::Work;
        bar.child(div().flex_1())
            .child(activity_button(
                "⌕",
                "搜索 Ctrl+Shift+F",
                "搜索",
                expanded,
                search_active,
                cx.listener(|this: &mut Workspace, _, _, cx| {
                    if this.navigation.bottom == Some(BottomPanel::Search) {
                        this.navigation.apply(NavAction::CloseBottom);
                        cx.notify();
                    } else {
                        this.open_project_search(cx);
                    }
                }),
            ))
            .child(activity_button(
                "⌨",
                "终端 Ctrl+`",
                "终端",
                expanded,
                terminal_active,
                cx.listener(|this: &mut Workspace, _, _, cx| this.toggle_console(cx)),
            ))
            .child(activity_button(
                "✦",
                "Agent 工作区 Ctrl+Shift+/",
                "Agent",
                expanded,
                agent_active,
                cx.listener(|this: &mut Workspace, _, _, cx| {
                    this.show_agent_workspace(AgentTab::Agents, cx)
                }),
            ))
            .child(activity_button(
                "⚙",
                "设置 Ctrl+,",
                "设置",
                expanded,
                self.settings_open.is_some(),
                cx.listener(|this: &mut Workspace, _, _, cx| this.open_settings(cx)),
            ))
    }

    fn render_project_switcher(&self, cx: &Context<Self>) -> AnyElement {
        let active = self.context.snapshot().project;
        let items = project_switcher_items(self.context.known_projects(), active.as_ref());
        let active_name = active
            .as_ref()
            .map(ProjectId::display_name)
            .unwrap_or_else(|| "尚未选择项目".into());
        let count = items.len();
        let expanded = self.project_switcher_open;
        let mut selector = div()
            .id("project-switcher")
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .child(
                div()
                    .h(px(38.))
                    .px_1p5()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .id("project-switcher-trigger")
                            .flex_1()
                            .min_w_0()
                            .h(px(28.))
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .on_click(cx.listener(|workspace: &mut Workspace, _, _, cx| {
                                workspace.project_switcher_open = !workspace.project_switcher_open;
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(crate::theme::Theme::accent()))
                                    .child("▱"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(active_name),
                            )
                            .child(
                                div()
                                    .min_w(px(18.))
                                    .h(px(18.))
                                    .px_1()
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(rgb(crate::theme::Theme::bg_active()))
                                    .text_size(px(9.))
                                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                                    .child(count.to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child(if expanded { "⌃" } else { "⌄" }),
                            ),
                    )
                    .child(
                        div()
                            .id("project-switcher-add")
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .cursor_pointer()
                            .text_size(px(15.))
                            .text_color(rgb(crate::theme::Theme::accent()))
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child("+")
                            .on_click(cx.listener(|workspace: &mut Workspace, _, _, cx| {
                                workspace.prompt_open_folder(cx);
                            })),
                    ),
            );
        if expanded {
            let mut list = div()
                .id("project-switcher-list")
                .max_h(px(240.))
                .overflow_y_scroll()
                .px_1p5()
                .pb_1()
                .flex()
                .flex_col()
                .gap_0p5();
            if items.is_empty() {
                list = list.child(
                    div()
                        .px_2()
                        .py_2()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child("还没有项目，点击右上角 + 添加"),
                );
            }
            for (index, item) in items.into_iter().enumerate() {
                let project = item.id.clone();
                list = list.child(
                    div()
                        .id(ElementId::Name(format!("project-switch-{index}").into()))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .when(item.active, |d| d.bg(rgb(crate::theme::Theme::bg_active())))
                        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                        .on_click(cx.listener(move |workspace: &mut Workspace, _, _, cx| {
                            workspace
                                .apply_activation(&ActivationTarget::Project(project.clone()), cx);
                            workspace.navigation.apply(NavAction::ShowExplorer);
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .child(div().size(px(6.)).rounded_full().bg(rgb(if item.active {
                                    crate::theme::Theme::accent()
                                } else {
                                    crate::theme::Theme::fg_faint()
                                })))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(10.5))
                                        .text_color(rgb(crate::theme::Theme::fg()))
                                        .child(item.name),
                                ),
                        )
                        .child(
                            div()
                                .ml(px(13.))
                                .truncate()
                                .text_size(px(8.5))
                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                .child(item.path),
                        ),
                );
            }
            selector = selector.child(list);
        }
        selector.into_any_element()
    }

    fn render_left_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = self.navigation.left.unwrap_or(LeftPanel::Explorer);
        let title = match selected {
            LeftPanel::Explorer => "项目",
            LeftPanel::Tasks => "任务",
            LeftPanel::Vcs => "版控",
        };
        let body: AnyElement = match selected {
            LeftPanel::Explorer => {
                let tree: AnyElement = match &self.file_tree {
                    Some(tree) => div()
                        .size_full()
                        .flex()
                        .child(tree.clone())
                        .into_any_element(),
                    None => div()
                        .p_3()
                        .text_size(px(12.))
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child("从上方选择或添加一个项目")
                        .into_any_element(),
                };
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(self.render_project_switcher(cx))
                    .child(div().flex_1().min_h_0().flex().child(tree))
                    .into_any_element()
            }
            LeftPanel::Vcs => match &self.vcs_panel {
                Some(v) => div().size_full().flex().child(v.clone()).into_any_element(),
                None => div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("尚未打开文件夹")
                    .into_any_element(),
            },
            LeftPanel::Tasks => match &self.task_sidebar {
                Some(t) => div().size_full().flex().child(t.clone()).into_any_element(),
                None => div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("—")
                    .into_any_element(),
            },
        };
        div()
            .id("left-panel")
            .w(self.left_panel_width)
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .border_r_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .overflow_hidden()
            .child(
                div()
                    .h(px(34.))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .border_b_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .child(
                        div()
                            .px_2()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child(title),
                    ),
            )
            .child(body)
    }

    fn render_bottom_dock(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = self.navigation.bottom.unwrap_or(BottomPanel::Terminal);
        let panel: AnyElement = match selected {
            BottomPanel::Terminal => match self.current_console() {
                Some(console) => div()
                    .size_full()
                    .flex()
                    .child(console.clone())
                    .into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("终端尚未启动")
                    .into_any_element(),
            },
            BottomPanel::Search => match &self.search_overlay {
                Some(search) => div()
                    .size_full()
                    .flex()
                    .child(search.clone())
                    .into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("Ctrl+Shift+F 开始项目搜索")
                    .into_any_element(),
            },
        };

        div()
            .id("bottom-dock")
            .h(self.bottom_panel_height)
            .min_h(px(150.))
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .child(
                div()
                    .id("bottom-dock-tabs")
                    .h(px(30.))
                    .flex()
                    .items_end()
                    .gap_1()
                    .px_2()
                    .border_b_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .children(
                        [
                            ("TERMINAL", BottomPanel::Terminal),
                            ("SEARCH", BottomPanel::Search),
                        ]
                        .map(|(label, target)| {
                            let active = selected == target;
                            div()
                                .id(ElementId::Name(format!("bottom-tab-{label}").into()))
                                .h(px(25.))
                                .px_3()
                                .rounded_t_md()
                                .flex()
                                .items_center()
                                .cursor_pointer()
                                .text_size(px(10.5))
                                .text_color(rgb(if active {
                                    crate::theme::Theme::fg()
                                } else {
                                    crate::theme::Theme::fg_dim()
                                }))
                                .when(active, |d| d.bg(rgb(crate::theme::Theme::bg_elevated())))
                                .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                                .child(label)
                                .on_click(cx.listener(move |ws: &mut Workspace, _, _, cx| {
                                    match target {
                                        BottomPanel::Terminal => {
                                            ws.ensure_console(cx);
                                            if ws.navigation.bottom != Some(BottomPanel::Terminal) {
                                                ws.navigation.apply(NavAction::ToggleTerminal);
                                            }
                                        }
                                        BottomPanel::Search => ws.open_project_search(cx),
                                    }
                                    cx.notify();
                                }))
                        }),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("bottom-dock-close")
                            .h(px(24.))
                            .px_2()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .hover(|d| d.text_color(rgb(crate::theme::Theme::fg())))
                            .child("⌄")
                            .on_click(cx.listener(|ws: &mut Workspace, _, _, cx| {
                                ws.navigation.apply(NavAction::CloseBottom);
                                cx.notify();
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(panel))
    }

    // ---------- 面板分隔条拖拽 ----------

    /// 左面板右缘竖向拖拽条:拖动调左栏宽。
    fn render_left_divider(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("left-panel-resize")
            .w(px(5.))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(|d| d.bg(rgb(crate::theme::Theme::accent_dim())))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, e: &MouseDownEvent, window, cx| {
                    ws.panel_drag = Some(PanelDrag::Left {
                        start_x: e.position.x.as_f32(),
                        start_w: ws.left_panel_width.as_f32(),
                    });
                    window.prevent_default();
                    cx.notify();
                }),
            )
    }

    /// 活动栏右缘竖向拖拽条:拉宽后图标显示中文名称。
    fn render_activity_divider(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("activity-bar-resize")
            .w(px(5.))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(|d| d.bg(rgb(crate::theme::Theme::accent_dim())))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, e: &MouseDownEvent, window, cx| {
                    ws.panel_drag = Some(PanelDrag::Activity {
                        start_x: e.position.x.as_f32(),
                        start_w: ws.activity_bar_width.as_f32(),
                    });
                    window.prevent_default();
                    cx.notify();
                }),
            )
    }

    /// 底部 dock 上缘横向拖拽条:拖动调高度。
    fn render_bottom_divider(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("bottom-dock-resize")
            .w_full()
            .h(px(5.))
            .flex_none()
            .cursor_row_resize()
            .hover(|d| d.bg(rgb(crate::theme::Theme::accent_dim())))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, e: &MouseDownEvent, window, cx| {
                    ws.panel_drag = Some(PanelDrag::Bottom {
                        start_y: e.position.y.as_f32(),
                        start_h: ws.bottom_panel_height.as_f32(),
                    });
                    window.prevent_default();
                    cx.notify();
                }),
            )
    }

    fn on_panel_drag_move(
        &mut self,
        e: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.panel_drag else {
            return;
        };
        // 按键已放开(可能没收到 mouse up):自愈并结束拖拽。
        if e.pressed_button != Some(MouseButton::Left) {
            self.panel_drag = None;
            cx.notify();
            return;
        }
        match drag {
            PanelDrag::Left { start_x, start_w } => {
                let w = (start_w + e.position.x.as_f32() - start_x)
                    .clamp(LEFT_PANEL_MIN, LEFT_PANEL_MAX);
                if (w - self.left_panel_width.as_f32()).abs() > 0.5 {
                    self.left_panel_width = px(w);
                    cx.notify();
                }
            }
            PanelDrag::Bottom { start_y, start_h } => {
                let h = (start_h + start_y - e.position.y.as_f32())
                    .clamp(BOTTOM_DOCK_MIN, BOTTOM_DOCK_MAX);
                if (h - self.bottom_panel_height.as_f32()).abs() > 0.5 {
                    self.bottom_panel_height = px(h);
                    cx.notify();
                }
            }
            PanelDrag::Activity { start_x, start_w } => {
                let w = (start_w + e.position.x.as_f32() - start_x)
                    .clamp(ACTIVITY_BAR_MIN, ACTIVITY_BAR_MAX);
                if (w - self.activity_bar_width.as_f32()).abs() > 0.5 {
                    self.activity_bar_width = px(w);
                    cx.notify();
                }
            }
        }
    }

    fn on_panel_drag_end(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.panel_drag.take().is_some() {
            cx.notify();
        }
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let cursor = self
            .current_surface()
            .and_then(|s| s.tabs.get(s.active_tab))
            .and_then(|t| match t {
                Tab::Editor(ed) => {
                    let (row, col) = ed.read(cx).cursor_pos(cx);
                    Some(format!("{}:{}", row + 1, col + 1))
                }
                Tab::Diff(_) => None,
            });
        let vcs_label = self.vcs_panel.as_ref().and_then(|v| {
            v.read(cx)
                .client_label()
                .or_else(|| v.read(cx).branch_label())
        });
        let vcs_count = self
            .vcs_panel
            .as_ref()
            .map(|v| v.read(cx).change_count())
            .unwrap_or(0);
        let working = self.app.limiter.active();
        let projects = self.project_count();
        let root_name = self
            .context
            .snapshot()
            .project
            .map(|p| p.display_name())
            .unwrap_or_else(|| "未打开项目".into());
        div()
            .id("status-bar")
            .h(px(26.))
            .flex()
            .items_center()
            .px_3()
            .gap_3()
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .border_t_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .text_size(px(11.))
            .text_color(rgb(crate::theme::Theme::fg_dim()))
            .child(div().child(if projects > 1 {
                format!("{root_name} · {projects} 个项目")
            } else {
                root_name.clone()
            }))
            .child(
                div()
                    .text_color(rgb(if vcs_count > 0 {
                        crate::theme::Theme::accent()
                    } else {
                        crate::theme::Theme::fg_dim()
                    }))
                    .child(if vcs_count > 0 {
                        format!(
                            "{} · {} 待提交",
                            vcs_label.unwrap_or_else(|| "VCS".into()),
                            vcs_count
                        )
                    } else {
                        vcs_label.unwrap_or_else(|| "工作区干净".into())
                    }),
            )
            .child(
                div()
                    .id("status-message")
                    .flex_1()
                    .min_w_0()
                    .text_center()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(rgb(if working > 0 {
                        crate::theme::Theme::success()
                    } else {
                        crate::theme::Theme::fg_dim()
                    }))
                    .child(if working > 0 {
                        format!("● {working} agent runs").into()
                    } else {
                        self.status_message.clone()
                    }),
            )
            .when_some(cursor, |d, c| d.child(div().child(c)))
            .child(div().child("Rust"))
            .child(div().child("UTF-8"))
    }

    /// 关闭含活动 Agent Run 的项目 → 必须先确认停止。
    fn render_close_confirm(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(cc) = &self.close_confirm else {
            return div().into_any_element();
        };
        let _ = cx;
        div()
            .id("close-confirm-overlay")
            .absolute()
            .size_full()
            .top_0()
            .left_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x00000080))
            .child(
                div()
                    .id("close-confirm-card")
                    .w(px(420.))
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::warning()))
                    .bg(rgb(crate::theme::Theme::bg_elevated()))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("关闭项目「{}」?", cc.name)),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child(format!(
                                "该项目有 {} 个活动 Agent Run。关闭将停止这些 Agent(需要你确认)。",
                                cc.active_runs
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                div()
                                    .id("cc-cancel")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::border()))
                                    .cursor_pointer()
                                    .text_size(px(11.))
                                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                                    .child("取消")
                                    .on_click(cx.listener(|ws: &mut Workspace, _, _, cx| {
                                        ws.confirm_close(false, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("cc-confirm")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(crate::theme::Theme::danger()))
                                    .text_color(rgb(crate::theme::Theme::bg()))
                                    .cursor_pointer()
                                    .text_size(px(11.))
                                    .hover(|d| d.opacity(0.85))
                                    .child("停止并关闭")
                                    .on_click(cx.listener(|ws: &mut Workspace, _, _, cx| {
                                        ws.confirm_close(true, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn activity_button(
    icon: &str,
    tip: &str,
    text: &str,
    expanded: bool,
    active: bool,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("activity-{tip}").into()))
        .relative()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .when(expanded, |d| d.w_full().h(px(36.)).px_2().gap_2())
        .when(!expanded, |d| d.size(px(36.)).justify_center())
        .text_color(rgb(if active {
            crate::theme::Theme::accent()
        } else {
            crate::theme::Theme::fg_dim()
        }))
        .when(active, |d| d.bg(rgb(crate::theme::Theme::bg_active())))
        .hover(|d| {
            d.bg(rgb(crate::theme::Theme::bg_hover()))
                .text_color(rgb(crate::theme::Theme::fg()))
        })
        .child(div().text_size(px(16.)).child(icon.to_string()))
        .when(expanded, |d| {
            d.child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(px(12.))
                    .child(text.to_string()),
            )
        })
        .on_click(move |event, window, cx| listener(event, window, cx))
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut assigned_focus_this_frame = false;
        if let Some(handle) = self.pending_focus.take() {
            window.focus(&handle, cx);
            assigned_focus_this_frame = true;
        } else if self.focus_editor_next {
            self.focus_editor_next = false;
            if let Some(Tab::Editor(ed)) = self
                .current_surface()
                .and_then(|s| s.tabs.get(s.active_tab))
                .cloned()
            {
                let handle = ed.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
                assigned_focus_this_frame = true;
            }
        }
        // 焦点兜底:窗口内没有任何焦点时(欢迎页、会话恢复无标签、关掉
        // 最后一个标签、浮层关闭后),按键会退化到 dispatch 树根,匹配不到
        // "Workspace" 上下文的绑定。把焦点收回工作区根,快捷键保持可达。
        // 新浮层在当前帧才会加入 dispatch tree，contains_focused 仍基于上一帧，
        // 此时不能立刻用工作区根覆盖刚刚应用的 pending_focus。
        let modal_owns_focus = self.quick_open.is_some() || self.settings_open.is_some();
        if !assigned_focus_this_frame
            && !modal_owns_focus
            && !self.focus_handle.contains_focused(window, cx)
        {
            window.focus(&self.focus_handle, cx);
        }
        let has_tabs = self
            .current_surface()
            .map(|s| !s.tabs.is_empty())
            .unwrap_or(false);

        let center: AnyElement = match empty_state_for(self.project_count() > 0, has_tabs) {
            Some(EmptyState::FirstLaunch) => self.render_welcome(cx).into_any_element(),
            Some(EmptyState::ProjectReady) => self.render_project_ready(cx).into_any_element(),
            None => {
                let active_tab = self
                    .current_surface()
                    .and_then(|s| s.tabs.get(s.active_tab))
                    .cloned();
                let mut col = div()
                    .id("editor-col")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .child(self.render_tabs(cx));
                match active_tab {
                    Some(Tab::Editor(ed)) => {
                        col = col.child(div().flex_1().child(ed));
                    }
                    Some(Tab::Diff(dv)) => {
                        col = col.child(div().flex_1().child(dv));
                    }
                    None => {}
                }
                col.into_any_element()
            }
        };

        // 终端最后一个窗格关闭时只收起底 dock;每项目分桶内的其余状态不受影响。
        if let Some(root) = self.active_root() {
            if let Some(dock) = self
                .surfaces
                .get(&root)
                .and_then(|s| s.console_dock.clone())
            {
                if dock.read(cx).close_pending {
                    if let Some(surface) = self.surfaces.get_mut(&root) {
                        surface.console_dock = None;
                    }
                    if self.navigation.bottom == Some(BottomPanel::Terminal) {
                        self.navigation.apply(NavAction::CloseBottom);
                    }
                }
            }
        }

        let primary: AnyElement = match self.navigation.surface {
            PrimarySurface::Code => center,
            PrimarySurface::Work => match &self.agent_workspace {
                Some(aw) => div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .child(aw.clone())
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("Agent 工作区不可用")
                    .into_any_element(),
            },
        };

        let mut center_stack = div()
            .id("center-stack")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().flex().child(primary));
        if self.navigation.bottom.is_some() {
            center_stack = center_stack
                .child(self.render_bottom_divider(cx))
                .child(self.render_bottom_dock(cx));
        }

        let mut content = div().flex_1().min_w_0().min_h_0().flex().relative();
        if self.navigation.left.is_some() {
            content = content
                .child(self.render_left_panel(cx))
                .child(self.render_left_divider(cx));
        }
        content = content.child(center_stack);
        if self.close_confirm.is_some() {
            content = content.child(self.render_close_confirm(cx));
        }

        let mut root = div()
            .id("workspace-root")
            .track_focus(&self.focus_handle)
            .key_context("Workspace")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::bg()))
            .text_color(rgb(crate::theme::Theme::fg()))
            .when(
                matches!(
                    self.panel_drag,
                    Some(PanelDrag::Left { .. }) | Some(PanelDrag::Activity { .. })
                ),
                |d| d.cursor_col_resize(),
            )
            .when(
                matches!(self.panel_drag, Some(PanelDrag::Bottom { .. })),
                |d| d.cursor_row_resize(),
            )
            .on_mouse_move(cx.listener(Self::on_panel_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_panel_drag_end))
            .on_action(cx.listener(Self::act_open_folder))
            .on_action(cx.listener(Self::show_quick_open_files))
            .on_action(cx.listener(Self::show_command_palette))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::act_toggle_left))
            .on_action(cx.listener(Self::act_show_explorer))
            .on_action(cx.listener(Self::act_show_vcs))
            .on_action(cx.listener(Self::act_show_board))
            .on_action(cx.listener(Self::act_show_agent))
            .on_action(cx.listener(Self::act_show_work))
            .on_action(cx.listener(Self::act_toggle_console))
            .on_action(cx.listener(Self::act_open_project_search))
            .on_action(cx.listener(Self::act_show_tasks))
            .on_action(cx.listener(Self::act_open_settings))
            .child(
                div()
                    .id("main-row")
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_activity_bar(cx))
                    .child(self.render_activity_divider(cx))
                    .child(content),
            );
        root = root.child(self.render_status_bar(cx));

        if let Some(qo) = self.quick_open.clone() {
            root = root.child(qo);
        }
        if let Some(s) = self.settings_open.clone() {
            root = root.child(s);
        }
        root
    }
}
