use gpui::*;
use gpui::prelude::*;
use mf_agent::TaskStatus;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent_panel::AgentPanel;
use crate::board::Board;
use crate::cockpit::Cockpit;
use crate::console::ConsoleDock;
use crate::diff_view::DiffView;
use crate::editor::Editor;
use crate::file_index::FileIndex;
use crate::file_tree::FileTree;
use crate::navigation::{
    empty_state_for, BottomPanel, EmptyState, LeftPanel, NavAction, NavigationState,
    PrimarySurface,
};
use crate::quick_open::{QuickItem, QuickOpen};
use crate::search::ProjectSearch;
use crate::settings::{Dismissed, Saved, SettingsView};
use crate::vcs_panel::VcsPanel;
use crate::work_items::{WorkItemPhase, WorkItemStore};

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
        ToggleConsole,
        OpenProjectSearch,
        ShowTasks,
        OpenSettings,
    ]
);

/// 标签页内容:编辑器或 diff 视图
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

pub struct Workspace {
    pub root: Option<PathBuf>,
    active_workspace: Option<PathBuf>,
    tabs: Vec<Tab>,
    active: usize,
    tab_sessions: HashMap<PathBuf, (Vec<Tab>, usize)>,
    file_index: Option<Entity<FileIndex>>,
    file_tree: Option<Entity<FileTree>>,
    quick_open: Option<Entity<QuickOpen>>,
    search_overlay: Option<Entity<ProjectSearch>>,
    vcs_panel: Option<Entity<VcsPanel>>,
    agent_panel: Option<Entity<AgentPanel>>,
    board: Option<Entity<Board>>,
    work_items: Option<Arc<Mutex<WorkItemStore>>>,
    console_dock: Option<Entity<ConsoleDock>>,
    cockpit: Option<Entity<Cockpit>>,
    navigation: NavigationState,
    settings_open: Option<Entity<SettingsView>>,
    status_message: SharedString,
    focus_handle: FocusHandle,
    focus_editor_next: bool,
    pending_focus: Option<FocusHandle>,
    editor_font: mf_agent::EditorConfig,
}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            root: None,
            active_workspace: None,
            tabs: Vec::new(),
            active: 0,
            tab_sessions: HashMap::new(),
            file_index: None,
            file_tree: None,
            quick_open: None,
            search_overlay: None,
            vcs_panel: None,
            agent_panel: None,
            board: None,
            work_items: None,
            console_dock: None,
            cockpit: None,
            navigation: NavigationState::default(),
            settings_open: None,
            status_message: "就绪".into(),
            focus_handle: cx.focus_handle(),
            focus_editor_next: false,
            pending_focus: None,
            editor_font: mf_agent::EditorConfig::default(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    // ---------- 项目与文件 ----------

    pub fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // 统一为绝对路径,保证 agent 沙箱与索引以同一坐标系工作
        let path = std::path::absolute(&path).unwrap_or(path);
        if self.root.as_ref() == Some(&path) {
            if self.active_workspace.as_ref() == Some(&path) {
                self.navigation.apply(NavAction::ShowCode);
                self.status_message = format!("当前工作区 {}", path.display()).into();
                cx.notify();
            } else {
                self.activate_workspace_context(path, cx);
            }
            return;
        }
        let switching_project = self.root.as_ref().is_some_and(|root| root != &path);
        let has_dirty_tabs = self.tabs.iter().any(|tab| tab.is_dirty(cx))
            || self
                .tab_sessions
                .values()
                .any(|(tabs, _)| tabs.iter().any(|tab| tab.is_dirty(cx)));
        if switching_project && has_dirty_tabs {
            self.status_message = "存在未保存文件；请先保存或关闭后再切换工作区".into();
            cx.notify();
            return;
        }
        if switching_project
            && self.work_items.as_ref().is_some_and(|work_items| {
                work_items.lock().active().is_some_and(|item| {
                    matches!(item.phase, WorkItemPhase::Running | WorkItemPhase::NeedsInput)
                })
            })
        {
            self.status_message = "当前工作项仍在执行或等待输入，暂不能切换项目".into();
            cx.notify();
            return;
        }

        if let Some(agent) = self.agent_panel.take() {
            agent.update(cx, |agent, _| agent.stop_engine());
        }
        self.quick_open = None;
        self.search_overlay = None;
        self.file_index = None;
        self.file_tree = None;
        self.vcs_panel = None;
        self.board = None;
        self.work_items = None;
        self.console_dock = None;
        self.cockpit = None;
        self.navigation = NavigationState::default();
        self.root = Some(path.clone());
        self.active_workspace = Some(path.clone());
        self.tabs.clear();
        self.tab_sessions.clear();
        self.active = 0;
        let work_items = Arc::new(Mutex::new(WorkItemStore::load(path.clone())));
        self.work_items = Some(work_items.clone());
        let board_work_items = work_items.clone();
        let board = cx.new(|cx| Board::new(path.clone(), board_work_items, cx));
        let weak_activate = cx.weak_entity();
        let weak_remove = cx.weak_entity();
        board.update(cx, |board, _| {
            board.set_on_activate(move |card, _window, cx| {
                weak_activate.update(cx, |workspace, cx| {
                    workspace.navigation.apply(NavAction::ShowWorkspaces);
                    workspace.activate_workspace_context(card.path, cx);
                })
                .ok();
            });
            board.set_can_remove(move |path, _window, cx| {
                weak_remove
                    .update(cx, |workspace, cx| {
                        let current_dirty = workspace.active_workspace.as_deref() == Some(path)
                            && workspace.tabs.iter().any(|tab| tab.is_dirty(cx));
                        let hidden_dirty = workspace
                            .tab_sessions
                            .get(path)
                            .is_some_and(|(tabs, _)| tabs.iter().any(|tab| tab.is_dirty(cx)));
                        let dirty = current_dirty || hidden_dirty;
                        if !dirty {
                            workspace.tab_sessions.remove(path);
                        }
                        !dirty
                    })
                    .unwrap_or(false)
            });
        });
        self.board = Some(board);
        let initial_workspace = work_items
            .lock()
            .active()
            .map(|item| item.workspace.clone())
            .unwrap_or_else(|| path.clone());
        self.active_workspace = None;
        self.activate_workspace_context(initial_workspace, cx);
        self.status_message = format!("已打开 {}", path.display()).into();
        cx.notify();
    }

    fn activate_workspace_context(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = std::path::absolute(&path).unwrap_or(path);
        if !path.is_dir() {
            self.status_message = format!("工作区不存在: {}", path.display()).into();
            cx.notify();
            return;
        }
        if self.active_workspace.as_ref() == Some(&path) && self.file_index.is_some() {
            self.navigation.apply(NavAction::ShowWorkspaces);
            self.status_message = format!("当前工作区 {}", path.display()).into();
            cx.notify();
            return;
        }
        if let Some(work_items) = &self.work_items {
            let store = work_items.lock();
            let switching = store
                .active()
                .is_some_and(|item| item.workspace != path);
            let busy = store.active().is_some_and(|item| {
                matches!(item.phase, WorkItemPhase::Running | WorkItemPhase::NeedsInput)
            });
            if switching && busy {
                self.status_message = "当前工作项仍在执行或等待输入，暂不能切换工作区".into();
                cx.notify();
                return;
            }
        }

        if let Some(current_workspace) = self.active_workspace.clone() {
            let tabs = std::mem::take(&mut self.tabs);
            self.tab_sessions
                .insert(current_workspace, (tabs, self.active));
        }
        let (mut tabs, active) = self
            .tab_sessions
            .remove(&path)
            .unwrap_or_else(|| (Vec::new(), 0));
        for tab in &mut tabs {
            if let Tab::Editor(editor) = tab {
                if !editor.read(cx).buffer.read(cx).is_dirty() {
                    editor.update(cx, |editor, cx| {
                        editor.buffer.update(cx, |buffer, _| {
                            let _ = buffer.reload_from_disk();
                        });
                        cx.notify();
                    });
                }
            }
        }
        self.tabs = tabs;
        self.active = active.min(self.tabs.len().saturating_sub(1));

        let reopen_console = self.navigation.bottom == Some(BottomPanel::Terminal);
        let reopen_search = self.navigation.bottom == Some(BottomPanel::Search);
        if let Some(agent) = self.agent_panel.take() {
            agent.update(cx, |agent, _| agent.stop_engine());
        }
        self.quick_open = None;
        self.search_overlay = None;
        self.file_index = None;
        self.file_tree = None;
        self.vcs_panel = None;
        self.console_dock = None;
        self.cockpit = None;
        self.active_workspace = Some(path.clone());
        if let Some(work_items) = &self.work_items {
            let mut store = work_items.lock();
            store.activate_workspace(&path);
            let _ = store.save();
        }

        let index = cx.new(|cx| FileIndex::new(path.clone(), cx));
        let tree = cx.new(|cx| FileTree::new(path.clone(), cx));
        let weak = cx.weak_entity();
        tree.update(cx, |tree, _| {
            tree.set_on_open(move |path, _window, cx| {
                weak.update(cx, |workspace, cx| workspace.open_path(path, cx))
                    .ok();
            });
        });
        self.file_index = Some(index);
        self.file_tree = Some(tree);

        let vcs = cx.new(|cx| VcsPanel::new(path.clone(), cx));
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

        let agent = cx.new(|cx| AgentPanel::new(cx));
        if let Some(work_items) = &self.work_items {
            agent.update(cx, |agent, _| agent.attach_work_items(work_items.clone()));
        }
        let db_dir = path.join(".mf-agent");
        let _ = std::fs::create_dir_all(&db_dir);
        let skills = mf_skills::load_skills(Some(&path));
        let config = mf_agent::Config::load().unwrap_or_default();
        self.editor_font = config.editor.clone();
        self.apply_editor_font(&config.editor, cx);
        let shell = config
            .terminal
            .command
            .clone()
            .filter(|command| !command.trim().is_empty())
            .unwrap_or_else(crate::console::default_shell);
        if let Ok(engine) = mf_agent::Engine::start(
            db_dir.join("orchestration.db"),
            path.clone(),
            config,
            skills,
        ) {
            let engine = Arc::new(engine);
            agent.update(cx, |agent, cx| agent.attach_engine(engine.clone(), &path, cx));
            let cockpit = cx.new(|cx| Cockpit::new(engine, path.clone(), shell, cx));
            let weak = cx.weak_entity();
            let work_root = path.clone();
            cockpit.update(cx, |cockpit, _| {
                cockpit.set_on_open_change(move |change_path, _window, cx| {
                    let path = if change_path.is_absolute() {
                        change_path
                    } else {
                        work_root.join(change_path)
                    };
                    let title = path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| "变更".into());
                    weak.update(cx, |workspace, cx| workspace.open_diff(&title, &path, cx))
                        .ok();
                });
            });
            self.cockpit = Some(cockpit);
        }
        self.agent_panel = Some(agent);

        if reopen_console {
            self.ensure_console(cx);
        }
        if reopen_search {
            self.open_project_search(cx);
        }
        self.status_message = format!("当前工作区 {}", path.display()).into();
        cx.notify();
    }

    fn active_root(&self) -> PathBuf {
        self.active_workspace
            .clone()
            .or_else(|| self.root.clone())
            .unwrap_or_default()
    }

    pub fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowCode);
        if let Some(pos) = self.tabs.iter().position(|t| match t {
            Tab::Editor(ed) => ed
                .read(cx)
                .buffer
                .read(cx)
                .path()
                .map(|p| p == path)
                .unwrap_or(false),
            Tab::Diff(_) => false,
        }) {
            self.active = pos;
            self.focus_active(cx);
            cx.notify();
            return;
        }
        match mf_core::buffer::Buffer::load(path) {
            Ok(buf) => {
                let buffer = cx.new(|_| buf);
                let editor = cx.new(|cx| Editor::new(buffer, cx));
                let font = self.editor_font.clone();
                editor.update(cx, |ed, cx| ed.set_font(&font, cx));
                self.tabs.push(Tab::Editor(editor));
                self.active = self.tabs.len() - 1;
                self.focus_active(cx);
                cx.notify();
            }
            Err(e) => {
                self.status_message = format!("打开失败: {}", e).into();
                cx.notify();
            }
        }
    }

    /// 打开文件的工作区 diff 标签页(P4 优先,回退 Git)
    pub fn open_diff(&mut self, title: &str, local_path: &Path, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowCode);
        let root = self.active_root();
        let (diff_text, git_review) = {
            let p4 = mf_vcs::p4::P4::new(&root);
            match p4.diff_file(local_path) {
                Ok(t) if !t.trim().is_empty() => (t, false),
                Ok(_) => ("(无差异)".to_string(), false),
                Err(_) => {
                    // 回退 git
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
        // hunk 审阅:拒绝 = 后台 git apply -R mini-patch,完成后重开该 diff
        let root = root.clone();
        let rel = local_path.strip_prefix(&root).unwrap_or(local_path).to_path_buf();
        let title_owned = title.to_string();
        let weak = cx.weak_entity();
        if git_review {
            view.update(cx, |dv, _| {
                dv.set_on_reject(move |patch, _window, cx| {
                let root = root.clone();
                let rel = rel.clone();
                let title = title_owned.clone();
                let weak = weak.clone();
                let review_root = root.clone();
                cx.spawn(async move |cx| {
                    let command_root = review_root.clone();
                    let applied = cx.background_executor().spawn(async move {
                        use std::io::Write as _;
                        let mut child = std::process::Command::new("git")
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
                                    if o.status.success() { Ok(()) } else {
                                        Err(String::from_utf8_lossy(&o.stderr).chars().take(200).collect())
                                    }
                                }).unwrap_or_else(|e| Err(e.to_string()))
                            }
                            Err(e) => Err(e.to_string()),
                        }
                    });
                    let r = applied.await;
                    weak.update(cx, move |ws: &mut Workspace, cx| {
                        if ws.active_root() != review_root {
                            ws.status_message = "工作区已切换；旧工作区 hunk 已处理，但未重开 Diff".into();
                            cx.notify();
                            return;
                        }
                        match r {
                            Ok(()) => ws.status_message = "hunk 已拒绝(git apply -R),diff 已刷新".into(),
                            Err(e) => ws.status_message = format!("拒绝失败:{e}").into(),
                        }
                        // 只替换触发回调的 Diff 标签，避免误删另一个已打开的 Diff。
                        if let Some(idx) = ws.tabs.iter().position(|tab| {
                            matches!(tab, Tab::Diff(diff) if diff.entity_id() == view_id)
                        }) {
                            ws.tabs.remove(idx);
                            ws.open_diff(&title, &rel, cx);
                            if let Some(refreshed) = ws.tabs.pop() {
                                let idx = idx.min(ws.tabs.len());
                                ws.tabs.insert(idx, refreshed);
                                ws.activate_tab(idx, cx);
                            }
                        } else {
                            ws.status_message = "原 Diff 标签已关闭，未自动重开".into();
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
                });
            });
        }
        self.tabs.push(Tab::Diff(view.clone()));
        self.active = self.tabs.len() - 1;
        if let Some(Tab::Diff(dv)) = self.tabs.get(self.active) {
            self.pending_focus = Some(dv.read(cx).focus_handle());
        }
        cx.notify();
    }

    /// 请求聚焦当前编辑器(render 阶段执行,因为聚焦需要 window)
    fn focus_active(&mut self, _cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.focus_editor_next = true;
        }
    }

    fn close_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_at(self.active, cx);
    }

    fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        match self.tabs.get(index) {
            Some(Tab::Editor(_)) => self.focus_active(cx),
            Some(Tab::Diff(diff)) => {
                self.pending_focus = Some(diff.read(cx).focus_handle());
            }
            None => {}
        }
        cx.notify();
    }

    fn close_tab_at(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.is_dirty(cx) {
            self.status_message = "文件尚未保存；保存后才能关闭标签".into();
            cx.notify();
            return;
        }
        self.tabs.remove(index);
        if index < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        if self.tabs.is_empty() {
            cx.notify();
        } else {
            self.activate_tab(self.active, cx);
        }
    }

    fn next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            let next = (self.active + 1) % self.tabs.len();
            self.activate_tab(next, cx);
        }
    }

    fn prev_tab(&mut self, _: &PrevTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            let previous = (self.active + self.tabs.len() - 1) % self.tabs.len();
            self.activate_tab(previous, cx);
        }
    }

    // ---------- 快速打开 / 命令面板 ----------

    fn show_quick_open_files(&mut self, _: &QuickOpenFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = None;
        let Some(index) = self.file_index.clone() else {
            self.status_message = "先打开一个文件夹 (Ctrl+Shift+O)".into();
            cx.notify();
            return;
        };
        let qo = cx.new(|cx| QuickOpen::files(index, cx));
        self.wire_quick_open(qo, window, cx);
    }

    fn show_command_palette(&mut self, _: &CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = None;
        let qo = cx.new(|cx| QuickOpen::commands(cx));
        let has_folder = self.root.is_some();
        let mut cmds = vec![
            ("open_folder".into(), "打开文件夹…  Ctrl+Shift+O".into()),
            ("toggle_explorer".into(), "显示资源管理器  Ctrl+Shift+E".into()),
            ("toggle_vcs".into(), "显示版本控制  Ctrl+Shift+G".into()),
            ("toggle_board".into(), "显示工作区  Ctrl+Shift+W".into()),
            ("toggle_agent".into(), "切换右侧 Agent 会话  Ctrl+Shift+/".into()),
            ("toggle_console".into(), "切换底部终端  Ctrl+`".into()),
            ("project_search".into(), "项目搜索…  Ctrl+Shift+F".into()),
            ("show_tasks".into(), "显示执行步骤  Ctrl+Shift+M".into()),
            ("open_settings".into(), "打开设置  Ctrl+,".into()),
            ("close_tab".into(), "关闭当前标签页  Ctrl+W".into()),
        ];
        if has_folder {
            cmds.push(("refresh_tree".into(), "刷新文件树".into()));
        }
        qo.update(cx, |q, _| {
            q.register_commands(cmds);
        });
        self.wire_quick_open(qo, window, cx);
    }

    fn wire_quick_open(&mut self, qo: Entity<QuickOpen>, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        qo.update(cx, |q, _| {
            q.set_on_pick(move |item, window, cx| {
                weak.update(cx, |ws, cx| {
                    // 命令/文件处理后关闭浮层
                    match item {
                        QuickItem::File(p) => {
                            let p = p.clone();
                            let path = if p.is_absolute() {
                                p
                            } else {
                                let root = ws.active_root();
                                if root.as_os_str().is_empty() { p } else { root.join(&p) }
                            };
                            ws.navigation.apply(NavAction::ShowCode);
                            ws.open_path(&path, cx);
                            ws.quick_open = None;
                        }
                        QuickItem::Command { id, .. } => {
                            ws.quick_open = None;
                            match id.as_ref() {
                                "open_folder" => ws.prompt_open_folder(window, cx),
                                "toggle_explorer" => ws.navigation.apply(NavAction::ShowExplorer),
                                "toggle_vcs" => ws.navigation.apply(NavAction::ShowVcs),
                                "toggle_board" => ws.navigation.apply(NavAction::ShowWorkspaces),
                                "toggle_agent" => ws.toggle_agent_dock(cx),
                                "toggle_console" => ws.toggle_console(cx),
                                "project_search" => ws.open_project_search(cx),
                                "show_tasks" => ws.show_tasks(cx),
                                "open_settings" => ws.open_settings(cx),
                                "close_tab" => ws.close_tab(&CloseTab, window, cx),
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
        // 订阅浮层的Dismissed 事件(点击遮罩关闭)
        // 注意:subscribe 回调已在 Workspace update 内,直接用回调参数,不可再 weak.update 重入
        cx.subscribe(&qo, move |ws, _, _: &crate::quick_open::Dismissed, cx| {
            ws.dismiss_quick_open(cx);
        })
        .detach();
        self.quick_open = Some(qo.clone());
        self.pending_focus = Some(qo.read(cx).focus_handle(cx));
        let _ = window;
        cx.notify();
    }

    fn dismiss_quick_open(&mut self, cx: &mut Context<Self>) {
        if self.quick_open.take().is_some() {
            self.focus_active(cx);
            cx.notify();
        }
    }

    // ---------- 打开文件夹对话框 ----------

    fn prompt_open_folder(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
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

    fn act_open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_open_folder(window, cx);
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
        self.navigation.apply(NavAction::ShowWorkspaces);
        cx.notify();
    }

    fn act_show_agent(&mut self, _: &ShowAgent, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_agent_dock(cx);
    }

    fn act_open_project_search(&mut self, _: &OpenProjectSearch, _: &mut Window, cx: &mut Context<Self>) {
        self.open_project_search(cx);
    }

    fn open_project_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = self.search_overlay.clone() {
            self.navigation.apply(NavAction::ShowSearch);
            self.pending_focus = Some(search.read(cx).focus_handle(cx));
            cx.notify();
            return;
        }
        let root = self.active_root();
        if root.as_os_str().is_empty() {
            return;
        }
        let s = cx.new(|cx| ProjectSearch::new(root, cx));
        let weak = cx.weak_entity();
        s.update(cx, |sv, _| {
            sv.set_embedded(true);
            sv.set_on_open(move |path, row, window, cx| {
                weak.update(cx, |ws, cx| {
                    ws.open_path(path, cx);
                    if let Some(Tab::Editor(ed)) = ws.tabs.get(ws.active) {
                        ed.update(cx, |e, cx| e.goto_line(row, cx));
                    }
                    cx.notify();
                })
                .ok();
                let _ = window;
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
        self.show_tasks(cx);
    }

    fn act_open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(cx);
    }

    // ---------- 控制台分屏 ----------

    fn ensure_console(&mut self, cx: &mut Context<Self>) {
        if self.console_dock.is_none() {
            let cwd = self
                .active_workspace
                .clone()
                .or_else(|| self.root.clone())
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            self.console_dock = Some(cx.new(|cx| ConsoleDock::new_in(cwd, cx)));
        }
    }

    pub fn toggle_console(&mut self, cx: &mut Context<Self>) {
        self.ensure_console(cx);
        self.navigation.apply(NavAction::ToggleTerminal);
        cx.notify();
    }

    fn toggle_agent_dock(&mut self, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ToggleAgent);
        cx.notify();
    }

    fn show_tasks(&mut self, cx: &mut Context<Self>) {
        self.navigation.apply(NavAction::ShowSteps);
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
        let s = cx.new(|cx| SettingsView::new(cx));
        cx.subscribe(&s, move |ws, _, ev: &Saved, cx| {
            ws.apply_editor_font(&ev.0.editor, cx);
            ws.editor_font = ev.0.editor.clone();
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
        for t in &mut self.tabs {
            if let Tab::Editor(ed) = t {
                ed.update(cx, |ed, cx| ed.set_font(cfg, cx));
            }
        }
    }

    // ---------- 渲染 ----------

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
                                    .child(div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).child("打开文件夹"))
                                    .child(div().text_size(px(9.5)).child("Ctrl+Shift+O")),
                            )
                            .on_click(cx.listener(|ws: &mut Workspace, _, window, cx| {
                                ws.prompt_open_folder(window, cx);
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
                                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())).border_color(rgb(crate::theme::Theme::accent_dim())))
                                    .child(div().w(px(18.)).text_size(px(16.)).text_color(rgb(crate::theme::Theme::accent())).child("⌕"))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(div().text_size(px(11.5)).text_color(rgb(crate::theme::Theme::fg())).child("快速打开"))
                                            .child(div().text_size(px(9.5)).text_color(rgb(crate::theme::Theme::fg_faint())).child("Ctrl+P")),
                                    )
                                    .on_click(cx.listener(|ws: &mut Workspace, _, window, cx| {
                                        ws.show_quick_open_files(&QuickOpenFiles, window, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("welcome-command-palette")
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
                                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())).border_color(rgb(crate::theme::Theme::accent_dim())))
                                    .child(div().w(px(18.)).text_size(px(15.)).text_color(rgb(crate::theme::Theme::accent())).child("⌘"))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(div().text_size(px(11.5)).text_color(rgb(crate::theme::Theme::fg())).child("命令面板"))
                                            .child(div().text_size(px(9.5)).text_color(rgb(crate::theme::Theme::fg_faint())).child("Ctrl+Shift+P")),
                                    )
                                    .on_click(cx.listener(|ws: &mut Workspace, _, window, cx| {
                                        ws.show_command_palette(&CommandPalette, window, cx);
                                    })),
                            )
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
                                    .child(div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).child("快速打开文件"))
                                    .child(div().text_size(px(9.5)).child("Ctrl+P")),
                            )
                            .on_click(cx.listener(|workspace: &mut Workspace, _, window, cx| {
                                workspace.show_quick_open_files(&QuickOpenFiles, window, cx);
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
                                    .id("project-work-overview")
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
                                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())).border_color(rgb(crate::theme::Theme::accent_dim())))
                                    .child(div().w(px(18.)).text_size(px(15.)).text_color(rgb(crate::theme::Theme::accent())).child("▦"))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(div().text_size(px(11.5)).text_color(rgb(crate::theme::Theme::fg())).child("工作概览"))
                                            .child(div().text_size(px(9.5)).text_color(rgb(crate::theme::Theme::fg_faint())).child("Ctrl+Shift+W")),
                                    )
                                    .on_click(cx.listener(|workspace: &mut Workspace, _, _, cx| {
                                        workspace.navigation.apply(NavAction::ShowWorkspaces);
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
                                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())).border_color(rgb(crate::theme::Theme::accent_dim())))
                                    .child(div().w(px(18.)).text_size(px(15.)).text_color(rgb(crate::theme::Theme::accent())).child("⌘"))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(div().text_size(px(11.5)).text_color(rgb(crate::theme::Theme::fg())).child("命令面板"))
                                            .child(div().text_size(px(9.5)).text_color(rgb(crate::theme::Theme::fg_faint())).child("Ctrl+Shift+P")),
                                    )
                                    .on_click(cx.listener(|workspace: &mut Workspace, _, window, cx| {
                                        workspace.show_command_palette(&CommandPalette, window, cx);
                                    })),
                            ),
                    ),
            )
    }

    fn render_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let tabs: Vec<(usize, String, bool, bool)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.title(cx), i == self.active, t.is_dirty(cx)))
            .collect();
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
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())).text_color(rgb(crate::theme::Theme::fg())))
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
        let icons: [(&'static str, &'static str, LeftPanel); 3] = [
            ("▱", "项目 Ctrl+Shift+E", LeftPanel::Explorer),
            ("▦", "工作 Ctrl+Shift+W", LeftPanel::Workspaces),
            ("⎇", "版控 Ctrl+Shift+G", LeftPanel::Vcs),
        ];
        let vcs_count = self.vcs_panel.as_ref().map(|v| v.read(cx).change_count()).unwrap_or(0);
        let unread = self.board.as_ref().map(|b| b.read(cx).unread_count()).unwrap_or(0);
        let mut bar = div()
            .id("activity-bar")
            .w(px(44.))
            .flex()
            .flex_col()
            .items_center()
            .py_2()
            .gap_1()
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .border_r_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .child(div().h(px(30.)).flex().items_center().justify_center().text_size(px(18.)).child("🐒"));
        for (icon, tip, panel) in icons {
            let is_active = self.navigation.left == Some(panel);
            let badge = if panel == LeftPanel::Vcs { vcs_count } else if panel == LeftPanel::Workspaces { unread } else { 0 };
            bar = bar.child(
                div()
                    .id(ElementId::Name(format!("act-{}", tip).into()))
                    .relative()
                    .size(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .text_size(px(17.))
                    .cursor_pointer()
                    .when(is_active, |d| {
                        d.bg(rgb(crate::theme::Theme::bg_active()))
                            .text_color(rgb(crate::theme::Theme::accent()))
                    })
                    .when(!is_active, |d| {
                        d.text_color(rgb(crate::theme::Theme::fg_dim()))
                            .hover(|h| h.bg(rgb(crate::theme::Theme::bg_hover())))
                    })
                    .child(icon)
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
                                .bg(rgb(if panel == LeftPanel::Workspaces { crate::theme::Theme::warning() } else { crate::theme::Theme::accent() }))
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
                                    LeftPanel::Workspaces => NavAction::ShowWorkspaces,
                                });
                            }
                            cx.notify();
                        })
                    }),
            );
        }
        let terminal_active = self.navigation.bottom == Some(BottomPanel::Terminal);
        let search_active = self.navigation.bottom == Some(BottomPanel::Search);
        let agent_active = self.navigation.agent_open;
        bar.child(div().flex_1())
        .child(
            activity_button(
                "⌕",
                "搜索 Ctrl+Shift+F",
                search_active,
                cx.listener(|this: &mut Workspace, _, _, cx| {
                    if this.navigation.bottom == Some(BottomPanel::Search) {
                        this.navigation.apply(NavAction::CloseBottom);
                        cx.notify();
                    } else {
                        this.open_project_search(cx);
                    }
                }),
            ),
        )
        .child(
            activity_button(
                "⌨",
                "终端 Ctrl+`",
                terminal_active,
                cx.listener(|this: &mut Workspace, _, _, cx| this.toggle_console(cx)),
            ),
        )
        .child(
            activity_button(
                "✦",
                "Agent 会话 Ctrl+Shift+/",
                agent_active,
                cx.listener(|this: &mut Workspace, _, _, cx| this.toggle_agent_dock(cx)),
            ),
        )
        .child(
            activity_button(
                "⚙",
                "设置 Ctrl+,",
                self.settings_open.is_some(),
                cx.listener(|this: &mut Workspace, _, _, cx| this.open_settings(cx)),
            ),
        )
    }

    fn render_left_panel(&self, _cx: &Context<Self>) -> impl IntoElement {
        let selected = self.navigation.left.unwrap_or(LeftPanel::Explorer);
        let title = match selected {
            LeftPanel::Explorer => "项目",
            LeftPanel::Workspaces => "工作区",
            LeftPanel::Vcs => "版控",
        };
        let body = match (selected, &self.file_tree) {
            (LeftPanel::Explorer, Some(tree)) => div().size_full().flex().child(tree.clone()),
            (LeftPanel::Explorer, None) => div()
                .p_3()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::Theme::fg_faint()))
                .child("尚未打开文件夹"),
            (LeftPanel::Vcs, _) => match &self.vcs_panel {
                Some(v) => div().size_full().flex().child(v.clone()),
                None => div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("尚未打开文件夹"),
            },
            (LeftPanel::Workspaces, _) => match &self.board {
                Some(b) => div().size_full().flex().child(b.clone()),
                None => div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("尚未打开文件夹"),
            },
        };
        div()
            .id("left-panel")
            .w(px(284.))
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

    fn render_right_dock(&self) -> impl IntoElement {
        div()
            .id("right-agent-dock")
            .w(px(324.))
            .min_w(px(280.))
            .h_full()
            .flex()
            .border_l_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .child(match &self.agent_panel {
                Some(agent) => div().size_full().flex().child(agent.clone()),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("打开项目后可用 Agent 会话"),
            })
    }

    fn render_steps_dock(&self, cx: &Context<Self>) -> AnyElement {
        let (run_status, tasks) = self
            .agent_panel
            .as_ref()
            .map(|agent| {
                let agent = agent.read(cx);
                (agent.run_status_label(), agent.tasks_snapshot())
            })
            .unwrap_or_else(|| ("未启动".into(), Vec::new()));

        div()
            .id("bottom-steps")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .child(
                div()
                    .h(px(34.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .border_b_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(11.5)).child(format!("执行步骤 · {run_status}"))),
            )
            .child(
                div()
                    .id("bottom-task-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .content_start()
                    .gap_2()
                    .when(tasks.is_empty(), |d| {
                        d.child(
                            div()
                                .p_3()
                                .text_size(px(11.))
                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                .child("暂无执行步骤；从右侧 Agent 会话创建工作项执行后，这里会显示计划。"),
                        )
                    })
                    .children(tasks.into_iter().map(|task| {
                        let color = task_status_color(task.status);
                        let deps = if task.deps.is_empty() {
                            "无依赖".to_string()
                        } else {
                            format!("依赖 {}", task.deps.iter().map(|id| format!("#{id}")).collect::<Vec<_>>().join(" · "))
                        };
                        let task_id = task.id;
                        let retryable = matches!(task.status, TaskStatus::Blocked | TaskStatus::Failed);
                        div()
                            .id(("bottom-task", task.id as u64))
                            .w(px(250.))
                            .min_h(px(88.))
                            .p_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(if retryable { crate::theme::Theme::danger() } else { crate::theme::Theme::border() }))
                            .bg(rgb(crate::theme::Theme::bg_elevated()))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().size(px(8.)).rounded_full().bg(rgb(color)))
                                    .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(11.5)).child(format!("#{}", task.id)))
                                    .child(div().ml_auto().text_size(px(10.)).text_color(rgb(color)).child(task.status.label_cn())),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .text_size(px(11.))
                                    .text_color(rgb(crate::theme::Theme::fg()))
                                    .child(task.spec.lines().next().unwrap_or("").chars().take(52).collect::<String>()),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .flex()
                                    .items_center()
                                    .text_size(px(9.5))
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child(deps)
                                    .when(retryable, |d| {
                                        d.child(
                                            div()
                                                .id(("bottom-task-reset", task_id as u64))
                                                .ml_auto()
                                                .px_1p5()
                                                .py_0p5()
                                                .rounded_sm()
                                                .border_1()
                                                .border_color(rgb(crate::theme::Theme::danger()))
                                                .text_color(rgb(crate::theme::Theme::danger()))
                                                .cursor_pointer()
                                                .child("重试")
                                                .on_click(cx.listener(move |ws: &mut Workspace, _, _, cx| {
                                                    if let Some(agent) = &ws.agent_panel {
                                                        agent.update(cx, |agent, cx| agent.reset_task(task_id, cx));
                                                    }
                                                    cx.notify();
                                                })),
                                        )
                                    }),
                            )
                    })),
            )
            .into_any_element()
    }

    fn render_bottom_dock(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = self.navigation.bottom.unwrap_or(BottomPanel::Terminal);
        let panel: AnyElement = match selected {
            BottomPanel::Terminal => match &self.console_dock {
                Some(console) => div().size_full().flex().child(console.clone()).into_any_element(),
                None => div().size_full().flex().items_center().justify_center().text_color(rgb(crate::theme::Theme::fg_faint())).child("终端尚未启动").into_any_element(),
            },
            BottomPanel::Search => match &self.search_overlay {
                Some(search) => div().size_full().flex().child(search.clone()).into_any_element(),
                None => div().size_full().flex().items_center().justify_center().text_color(rgb(crate::theme::Theme::fg_faint())).child("Ctrl+Shift+F 开始项目搜索").into_any_element(),
            },
            BottomPanel::Steps => self.render_steps_dock(cx),
        };

        div()
            .id("bottom-dock")
            .h(px(228.))
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
                    .children([
                        ("TERMINAL", BottomPanel::Terminal),
                        ("SEARCH", BottomPanel::Search),
                        ("STEPS", BottomPanel::Steps),
                    ].map(|(label, target)| {
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
                            .text_color(rgb(if active { crate::theme::Theme::fg() } else { crate::theme::Theme::fg_dim() }))
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
                                    BottomPanel::Steps => ws.show_tasks(cx),
                                }
                                cx.notify();
                            }))
                    }))
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

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let cursor = self.tabs.get(self.active).and_then(|t| match t {
            Tab::Editor(ed) => {
                let (row, col) = ed.read(cx).cursor_pos(cx);
                Some(format!("{}:{}", row + 1, col + 1))
            }
            Tab::Diff(_) => None,
        });
        let vcs_label = self
            .vcs_panel
            .as_ref()
            .and_then(|v| v.read(cx).client_label().or_else(|| v.read(cx).branch_label()));
        let vcs_count = self.vcs_panel.as_ref().map(|v| v.read(cx).change_count()).unwrap_or(0);
        let working = self.agent_panel.as_ref().map(|a| a.read(cx).working_count()).unwrap_or(0);
        let root_name = self
            .root
            .as_ref()
            .and_then(|r| r.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未打开项目".into());
        let workspace_name = self
            .active_workspace
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root_name.clone());
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
            .child(div().child(format!("{} · ⎇ {}", root_name, workspace_name)))
            .child(
                div()
                    .text_color(rgb(if vcs_count > 0 { crate::theme::Theme::accent() } else { crate::theme::Theme::fg_dim() }))
                    .child(if vcs_count > 0 {
                        format!("{} · {} 待提交", vcs_label.unwrap_or_else(|| "VCS".into()), vcs_count)
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
                    .text_color(rgb(if working > 0 { crate::theme::Theme::success() } else { crate::theme::Theme::fg_dim() }))
                    .child(if working > 0 { format!("● {working} agents working").into() } else { self.status_message.clone() }),
            )
            .when_some(cursor, |d, c| d.child(div().child(c)))
            .child(div().child("Rust"))
            .child(div().child("UTF-8"))
    }
}

fn activity_button(
    label: &str,
    tip: &str,
    active: bool,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("activity-{tip}").into()))
        .size(px(36.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(16.))
        .text_color(rgb(if active { crate::theme::Theme::accent() } else { crate::theme::Theme::fg_dim() }))
        .when(active, |d| d.bg(rgb(crate::theme::Theme::bg_active())))
        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())).text_color(rgb(crate::theme::Theme::fg())))
        .child(label.to_string())
        .on_click(move |event, window, cx| listener(event, window, cx))
}

fn task_status_color(status: TaskStatus) -> u32 {
    match status {
        TaskStatus::Pending => 0x737373,
        TaskStatus::Ready | TaskStatus::Dispatched => crate::theme::Theme::accent(),
        TaskStatus::Completed => crate::theme::Theme::success(),
        TaskStatus::Failed | TaskStatus::Blocked => crate::theme::Theme::danger(),
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 延迟聚焦处理(需要 window)
        if let Some(handle) = self.pending_focus.take() {
            window.focus(&handle, cx);
        } else if self.focus_editor_next && !self.tabs.is_empty() {
            self.focus_editor_next = false;
            if let Some(Tab::Editor(ed)) = self.tabs.get(self.active) {
                let handle = ed.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        // agent 改动文件 → 重载对应编辑器
        if let Some(agent) = &self.agent_panel {
            let touched = agent.update(cx, |a, _| a.take_touched());
            if !touched.is_empty() {
                let root = self.active_root();
                for rel in &touched {
                    let abs = root.join(rel);
                    for t in &mut self.tabs {
                        if let Tab::Editor(ed) = t {
                            let matches = ed
                                .read(cx)
                                .buffer
                                .read(cx)
                                .path()
                                .map(|p| *p == abs)
                                .unwrap_or(false);
                            if matches {
                                ed.update(cx, |ed, cx| {
                                    ed.buffer.update(cx, |b, _| {
                                        let _ = b.reload_from_disk();
                                    });
                                    cx.notify();
                                });
                            }
                        }
                    }
                }
                if let Some(tree) = &self.file_tree {
                    tree.update(cx, |t, _| t.refresh_all());
                }
            }
        }
        let center: AnyElement = match empty_state_for(self.root.is_some(), !self.tabs.is_empty()) {
            Some(EmptyState::FirstLaunch) => self.render_welcome(cx).into_any_element(),
            Some(EmptyState::ProjectReady) => self.render_project_ready(cx).into_any_element(),
            None => {
            let active_tab = self.tabs.get(self.active).cloned();
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

        // 终端最后一个窗格关闭时只收起底 dock，实体其余状态不受影响。
        if let Some(dock) = &self.console_dock {
            if dock.read(cx).close_pending {
                self.console_dock = None;
                if self.navigation.bottom == Some(BottomPanel::Terminal) {
                    self.navigation.apply(NavAction::CloseBottom);
                }
            }
        }

        let primary: AnyElement = match self.navigation.surface {
            PrimarySurface::Code => center,
            PrimarySurface::Work => match &self.cockpit {
                Some(cockpit) => div().flex_1().min_w_0().flex().child(cockpit.clone()).into_any_element(),
                None => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("执行概览不可用；请先打开项目并配置 Agent")
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
            center_stack = center_stack.child(self.render_bottom_dock(cx));
        }

        let mut content = div().flex_1().min_w_0().min_h_0().flex();
        if self.navigation.left.is_some() {
            content = content.child(self.render_left_panel(cx));
        }
        content = content.child(center_stack);
        if self.navigation.agent_open {
            content = content.child(self.render_right_dock());
        }

        let mut root = div()
            .id("workspace-root")
            .key_context("Workspace")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::bg()))
            .text_color(rgb(crate::theme::Theme::fg()))
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
