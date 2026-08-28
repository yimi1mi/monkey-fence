use gpui::prelude::*;
use gpui::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent_workspace::AgentWorkspace;
use crate::app_ctx::AppCtx;
use crate::console::ConsoleDock;
use crate::diff_view::DiffView;
use crate::editor::Editor;
use crate::file_index::FileIndex;
use crate::file_tree::FileTree;
use crate::navigation::{
    empty_state_for, BottomPanel, EmptyState, LeftPanel, NavAction, NavigationState, PrimarySurface,
};
use crate::quick_open::{QuickItem, QuickOpen};
use crate::search::ProjectSearch;
use crate::settings::{Dismissed, Saved, SettingsView};
use crate::task_sidebar::TaskSidebar;
use crate::vcs_panel::VcsPanel;

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

/// 标签页内容:编辑器或 diff 视图,携带所属项目标识。
#[derive(Clone)]
struct TabEntry {
    project: PathBuf,
    tab: Tab,
}

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

/// 关闭项目的确认状态(存在活动 Agent Run 时必须先确认停止)。
struct CloseConfirm {
    root: PathBuf,
    name: String,
    active_runs: usize,
}

pub struct Workspace {
    app: Arc<AppCtx>,
    /// 前台项目根(打开/切换时更新;与 projects 顺序无关)
    foreground_root: Option<PathBuf>,
    /// 项目前台上下文:文件树 / VCS 面板(其他项目的任务与 Agent 后台继续运行)
    file_tree: Option<Entity<FileTree>>,
    vcs_panel: Option<Entity<VcsPanel>>,
    tabs: Vec<TabEntry>,
    active: usize,
    quick_open: Option<Entity<QuickOpen>>,
    search_overlay: Option<Entity<ProjectSearch>>,
    console_dock: Option<Entity<ConsoleDock>>,
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
}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let app = AppCtx::new();
        let task_sidebar = cx.new(|cx| TaskSidebar::new(app.clone(), cx));
        let agent_workspace = cx.new(|cx| AgentWorkspace::new(app.clone(), cx));
        let mut ws = Self {
            app,
            foreground_root: None,
            file_tree: None,
            vcs_panel: None,
            tabs: Vec::new(),
            active: 0,
            quick_open: None,
            search_overlay: None,
            console_dock: None,
            task_sidebar: Some(task_sidebar),
            agent_workspace: Some(agent_workspace),
            close_confirm: None,
            navigation: NavigationState::default(),
            settings_open: None,
            status_message: "就绪".into(),
            focus_handle: cx.focus_handle(),
            focus_editor_next: false,
            pending_focus: None,
            editor_font: mf_agent::EditorConfig::default(),
        };
        ws.restore_session(cx);
        ws
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 恢复上次会话打开的项目(存在性过滤;按保存顺序打开,最后切到保存的前台)。
    fn restore_session(&mut self, cx: &mut Context<Self>) {
        let session = AppCtx::load_session();
        let projects: Vec<PathBuf> = session
            .projects
            .iter()
            .filter(|p| p.is_dir())
            .cloned()
            .collect();
        for root in &projects {
            self.open_folder(root.clone(), cx);
        }
        if let Some(fg) = session.foreground {
            if fg.is_dir() && projects.contains(&fg) && self.app.orchestrator_of(&fg).is_some() {
                self.set_foreground_project(&fg, cx);
                self.status_message = format!("已恢复 {} 个项目", projects.len()).into();
            }
        }
    }

    /// 项目列表/前台变化后持久化会话。
    fn persist_session(&self) {
        self.app.save_session(self.foreground_root.as_ref());
    }

    /// 前台项目根;用于终端 cwd、快速打开、搜索与标签归属。
    fn active_root(&self) -> PathBuf {
        self.foreground_root.clone().unwrap_or_default()
    }

    fn project_count(&self) -> usize {
        self.app.projects.lock().len()
    }

    // ---------- 多项目 ----------

    pub fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = std::path::absolute(&path).unwrap_or(path);
        if !path.is_dir() {
            self.status_message = format!("目录不存在: {}", path.display()).into();
            cx.notify();
            return;
        }
        // 已打开 → 切换前台项目(不终止其他项目的 Agent)
        let already = self.app.projects.lock().iter().any(|p| p.root == path);
        if already {
            self.set_foreground_project(&path, cx);
            self.status_message = format!("已切换到 {}", path.display()).into();
            cx.notify();
            return;
        }
        match self.app.open_project(path.clone()) {
            Ok(_) => {
                self.foreground_root = Some(path.clone());
                self.set_foreground_project(&path, cx);
                self.status_message =
                    format!("已打开 {}({} 个项目)", path.display(), self.project_count()).into();
                self.persist_session();
            }
            Err(e) => {
                self.status_message = format!("打开项目失败: {e:#}").into();
            }
        }
        cx.notify();
    }

    /// 前台切换:只重建文件树 / VCS 面板与搜索;调度器与会话不受影响。
    fn set_foreground_project(&mut self, root: &Path, cx: &mut Context<Self>) {
        self.foreground_root = Some(root.to_path_buf());
        self.quick_open = None;
        self.search_overlay = None;
        let _index = cx.new(|cx| FileIndex::new(root.to_path_buf(), cx));
        let tree = cx.new(|cx| FileTree::new(root.to_path_buf(), cx));
        let weak = cx.weak_entity();
        tree.update(cx, |tree, _| {
            tree.set_on_open(move |path, _window, cx| {
                weak.update(cx, |workspace, cx| workspace.open_path(path, cx))
                    .ok();
            });
        });
        self.file_tree = Some(tree);
        let vcs = cx.new(|cx| VcsPanel::new(root.to_path_buf(), cx));
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
        self.persist_session();
        // 打开项目时刷新插件目录(项目级技能等)
        self.app.refresh_catalog();
        // 关闭孤儿编辑器之外不动 tabs(标签携带项目标识,跨项目保留)
        if !self.tabs.is_empty() {
            self.activate_tab(self.active.min(self.tabs.len() - 1), cx);
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
        let was_foreground = self.active_root() == *root;
        self.app.close_project(root);
        self.tabs.retain(|t| &t.project != root);
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.close_confirm = None;
        if was_foreground {
            self.foreground_root = None;
            self.file_tree = None;
            self.vcs_panel = None;
            let next = {
                let projects = self.app.projects.lock();
                projects.first().map(|p| p.root.clone())
            };
            if let Some(next) = next {
                self.set_foreground_project(&next, cx);
            }
        }
        self.status_message = "项目已关闭".into();
        self.persist_session();
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
        let project = path
            .parent()
            .map(|p| find_owning_project(&self.app, p))
            .flatten()
            .unwrap_or_else(|| self.active_root());
        if let Some(pos) = self.tabs.iter().position(|t| match &t.tab {
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
            // 已打开:未修改则从磁盘重载(agent 可能改动了文件)
            if let Some(TabEntry {
                tab: Tab::Editor(ed),
                ..
            }) = self.tabs.get(pos)
            {
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
            cx.notify();
            return;
        }
        match mf_core::buffer::Buffer::load(path) {
            Ok(buf) => {
                let buffer = cx.new(|_| buf);
                let editor = cx.new(|cx| Editor::new(buffer, cx));
                let font = self.editor_font.clone();
                editor.update(cx, |ed, cx| ed.set_font(&font, cx));
                self.tabs.push(TabEntry {
                    project,
                    tab: Tab::Editor(editor),
                });
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
        let root = find_owning_project(&self.app, local_path.parent().unwrap_or(Path::new(".")))
            .unwrap_or_else(|| self.active_root());
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
        let rel = local_path
            .strip_prefix(&root)
            .unwrap_or(local_path)
            .to_path_buf();
        let title_owned = title.to_string();
        let weak = cx.weak_entity();
        let root_for_reject = root.clone();
        if git_review {
            view.update(cx, |dv, _| {
                dv.set_on_reject(move |patch, _window, cx| {
                    let rel = rel.clone();
                    let title = title_owned.clone();
                    let weak = weak.clone();
                    let review_root = root_for_reject.clone();
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
                            }
                            if let Some(idx) = ws.tabs.iter().position(|t| {
                                matches!(&t.tab, Tab::Diff(diff) if diff.entity_id() == view_id)
                            }) {
                                let project = ws.tabs[idx].project.clone();
                                ws.tabs.remove(idx);
                                if ws.active >= idx && ws.active > 0 {
                                    ws.active -= 1;
                                }
                                let root = project.clone();
                                let rel2 = rel.clone();
                                let _ = root;
                                let _ = rel2;
                                let _ = &title;
                            }
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                });
            });
        }
        let project = root.clone();
        self.tabs.push(TabEntry {
            project,
            tab: Tab::Diff(view.clone()),
        });
        self.active = self.tabs.len() - 1;
        if let Some(TabEntry {
            tab: Tab::Diff(dv), ..
        }) = self.tabs.get(self.active)
        {
            self.pending_focus = Some(dv.read(cx).focus_handle());
        }
        cx.notify();
    }

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
        match &self.tabs.get(index).map(|t| &t.tab) {
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
        if tab.tab.is_dirty(cx) {
            self.status_message = "文件尚未保存;保存后才能关闭标签".into();
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

    fn show_quick_open_files(
        &mut self,
        _: &QuickOpenFiles,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_open = None;
        let root = self.active_root();
        if root.as_os_str().is_empty() {
            self.status_message = "先打开一个文件夹 (Ctrl+Shift+O)".into();
            cx.notify();
            return;
        }
        let index = cx.new(|cx| FileIndex::new(root, cx));
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
        let has_folder = self.project_count() > 0;
        let mut cmds = vec![
            ("open_folder".into(), "打开文件夹…  Ctrl+Shift+O".into()),
            (
                "toggle_explorer".into(),
                "显示资源管理器  Ctrl+Shift+E".into(),
            ),
            ("toggle_vcs".into(), "显示版本控制  Ctrl+Shift+G".into()),
            ("toggle_tasks".into(), "显示任务  Ctrl+Shift+W".into()),
            (
                "show_agents".into(),
                "Agent 看板(需要你/工作中/已完成/空闲)".into(),
            ),
            ("show_pipeline".into(), "Pipeline 视图".into()),
            ("toggle_console".into(), "切换底部终端  Ctrl+`".into()),
            ("project_search".into(), "项目搜索…  Ctrl+Shift+F".into()),
            ("open_settings".into(), "打开设置  Ctrl+,".into()),
            ("close_tab".into(), "关闭当前标签页  Ctrl+W".into()),
        ];
        if has_folder {
            cmds.push(("refresh_tree".into(), "刷新文件树".into()));
        }
        qo.update(cx, |q, _| {
            q.register_commands(cmds);
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
                                let root = ws.active_root();
                                if root.as_os_str().is_empty() {
                                    p
                                } else {
                                    root.join(&p)
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
                                "toggle_explorer" => ws.navigation.apply(NavAction::ShowExplorer),
                                "toggle_vcs" => ws.navigation.apply(NavAction::ShowVcs),
                                "toggle_tasks" => ws.navigation.apply(NavAction::ShowTasks),
                                "show_agents" => ws.show_agent_workspace(AgentTab::Agents, cx),
                                "show_pipeline" => ws.show_agent_workspace(AgentTab::Pipeline, cx),
                                "toggle_console" => ws.toggle_console(cx),
                                "project_search" => ws.open_project_search(cx),
                                "open_settings" => ws.open_settings(cx),
                                "close_tab" => ws.close_tab_at(ws.active, cx),
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
        let root = self.active_root();
        if root.as_os_str().is_empty() {
            return;
        }
        let s = cx.new(|cx| ProjectSearch::new(root, cx));
        let weak = cx.weak_entity();
        s.update(cx, |sv, _| {
            sv.set_embedded(true);
            sv.set_on_open(move |path, row, _window, cx| {
                weak.update(cx, |ws, cx| {
                    ws.open_path(path, cx);
                    if let Some(TabEntry {
                        tab: Tab::Editor(ed),
                        ..
                    }) = ws.tabs.get(ws.active)
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

    fn ensure_console(&mut self, cx: &mut Context<Self>) {
        if self.console_dock.is_none() {
            let cwd = {
                let root = self.active_root();
                if root.as_os_str().is_empty() {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                } else {
                    root
                }
            };
            self.console_dock = Some(cx.new(|cx| ConsoleDock::new_in(cwd, cx)));
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
        for t in &mut self.tabs {
            if let Tab::Editor(ed) = &t.tab {
                ed.update(cx, |ed, cx| ed.set_font(cfg, cx));
            }
        }
    }
}

/// AgentWorkspace 顶层页签(命令面板用)。
#[derive(Debug, Clone, Copy)]
pub enum AgentTab {
    Agents,
    Pipeline,
}

fn find_owning_project(app: &Arc<AppCtx>, dir: &Path) -> Option<PathBuf> {
    let projects = app.projects.lock();
    projects
        .iter()
        .map(|p| p.root.clone())
        .find(|root| dir.starts_with(root))
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

    fn render_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let multi_project = self.project_count() > 1;
        let tabs: Vec<(usize, String, String, bool, bool)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let project_tag = if multi_project {
                    t.project
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                (
                    i,
                    t.tab.title(cx),
                    project_tag,
                    i == self.active,
                    t.tab.is_dirty(cx),
                )
            })
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
        for (index, name, project_tag, is_active, dirty) in tabs {
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
                    .when(!project_tag.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(9.))
                                .py_0p5()
                                .px_1()
                                .rounded_sm()
                                .bg(rgb(crate::theme::Theme::bg_active()))
                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                .child(project_tag.clone()),
                        )
                    })
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
        let icons: [(&'static str, &'static str, LeftPanel); 3] = [
            ("▱", "项目 Ctrl+Shift+E", LeftPanel::Explorer),
            ("▦", "任务 Ctrl+Shift+W", LeftPanel::Tasks),
            ("⎇", "版控 Ctrl+Shift+G", LeftPanel::Vcs),
        ];
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
            .w(px(44.))
            .flex()
            .flex_col()
            .items_center()
            .py_2()
            .gap_1()
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .border_r_1()
            .border_color(rgb(crate::theme::Theme::border()));
        for (icon, tip, panel) in icons {
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
                terminal_active,
                cx.listener(|this: &mut Workspace, _, _, cx| this.toggle_console(cx)),
            ))
            .child(activity_button(
                "✦",
                "Agent 工作区 Ctrl+Shift+/",
                agent_active,
                cx.listener(|this: &mut Workspace, _, _, cx| {
                    this.show_agent_workspace(AgentTab::Agents, cx)
                }),
            ))
            .child(activity_button(
                "⚙",
                "设置 Ctrl+,",
                self.settings_open.is_some(),
                cx.listener(|this: &mut Workspace, _, _, cx| this.open_settings(cx)),
            ))
    }

    fn render_left_panel(&self, _cx: &Context<Self>) -> impl IntoElement {
        let selected = self.navigation.left.unwrap_or(LeftPanel::Explorer);
        let title = match selected {
            LeftPanel::Explorer => "项目",
            LeftPanel::Tasks => "任务",
            LeftPanel::Vcs => "版控",
        };
        let body = match selected {
            LeftPanel::Explorer => match &self.file_tree {
                Some(tree) => div().size_full().flex().child(tree.clone()),
                None => div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("尚未打开文件夹"),
            },
            LeftPanel::Vcs => match &self.vcs_panel {
                Some(v) => div().size_full().flex().child(v.clone()),
                None => div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("尚未打开文件夹"),
            },
            LeftPanel::Tasks => match &self.task_sidebar {
                Some(t) => div().size_full().flex().child(t.clone()),
                None => div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("—"),
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

    fn render_bottom_dock(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = self.navigation.bottom.unwrap_or(BottomPanel::Terminal);
        let panel: AnyElement = match selected {
            BottomPanel::Terminal => match &self.console_dock {
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

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let cursor = self.tabs.get(self.active).and_then(|t| match &t.tab {
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
        let root_name = {
            let root = self.active_root();
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未打开项目".into())
        };
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
        .child(label.to_string())
        .on_click(move |event, window, cx| listener(event, window, cx))
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(handle) = self.pending_focus.take() {
            window.focus(&handle, cx);
        } else if self.focus_editor_next && !self.tabs.is_empty() {
            self.focus_editor_next = false;
            if let Some(TabEntry {
                tab: Tab::Editor(ed),
                ..
            }) = self.tabs.get(self.active)
            {
                let handle = ed.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        // 任务侧边栏:选择同步到 AgentWorkspace;关闭意图 → 确认
        let (intent, selected) = match &self.task_sidebar {
            Some(sidebar) => {
                let intent = sidebar.update(cx, |s, _| s.close_intent.take());
                let selected = sidebar.read(cx).selected.clone();
                (intent, selected)
            }
            None => (None, None),
        };
        if let Some(root) = intent {
            self.request_close_project(root, cx);
        }
        if let Some(aw) = &self.agent_workspace {
            aw.update(cx, |aw, cx| aw.set_selected_task(selected, cx));
        }

        let center: AnyElement =
            match empty_state_for(self.project_count() > 0, !self.tabs.is_empty()) {
                Some(EmptyState::FirstLaunch) => self.render_welcome(cx).into_any_element(),
                Some(EmptyState::ProjectReady) => self.render_project_ready(cx).into_any_element(),
                None => {
                    let active_tab = self.tabs.get(self.active).map(|t| t.tab.clone());
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

        // 终端最后一个窗格关闭时只收起底 dock,实体其余状态不受影响。
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
            center_stack = center_stack.child(self.render_bottom_dock(cx));
        }

        let mut content = div().flex_1().min_w_0().min_h_0().flex().relative();
        if self.navigation.left.is_some() {
            content = content.child(self.render_left_panel(cx));
        }
        content = content.child(center_stack);
        if self.close_confirm.is_some() {
            content = content.child(self.render_close_confirm(cx));
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
