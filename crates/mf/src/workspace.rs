use gpui::*;
use gpui::prelude::*;
use mf_agent::TaskStatus;
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
use crate::quick_open::{QuickItem, QuickOpen};
use crate::search::ProjectSearch;
use crate::settings::{Dismissed, Saved, SettingsView};
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
        ToggleConsole,
        OpenProjectSearch,
        ShowTasks,
        OpenSettings,
        SetModeZed,
        SetModeOrca,
        SetModeDual,
    ]
);

#[derive(Clone, Copy, PartialEq)]
pub enum LeftPanel {
    Explorer,
    Vcs,
    BoardPanel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BottomPanel {
    Terminal,
    Search,
    Tasks,
}

/// 工作方式(三模式):Zed=专注手写 / Orca=AI 驾驶舱 / Dual=双轨协同
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LayoutMode {
    Zed,
    Orca,
    Dual,
}

impl LayoutMode {
    pub fn label(&self) -> &'static str {
        match self {
            LayoutMode::Zed => "🧑‍💻 Zed",
            LayoutMode::Orca => "🤖 Orca",
            LayoutMode::Dual => "⚡ 双轨",
        }
    }
}

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
    tabs: Vec<Tab>,
    active: usize,
    file_index: Option<Entity<FileIndex>>,
    file_tree: Option<Entity<FileTree>>,
    quick_open: Option<Entity<QuickOpen>>,
    search_overlay: Option<Entity<ProjectSearch>>,
    vcs_panel: Option<Entity<VcsPanel>>,
    agent_panel: Option<Entity<AgentPanel>>,
    board: Option<Entity<Board>>,
    console_dock: Option<Entity<ConsoleDock>>,
    cockpit: Option<Entity<Cockpit>>,
    layout_mode: LayoutMode,
    settings_open: Option<Entity<SettingsView>>,
    left_panel: LeftPanel,
    left_dock_open: bool,
    right_dock_open: bool,
    bottom_dock_open: bool,
    bottom_panel: BottomPanel,
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
            tabs: Vec::new(),
            active: 0,
            file_index: None,
            file_tree: None,
            quick_open: None,
            search_overlay: None,
            vcs_panel: None,
            agent_panel: None,
            board: None,
            console_dock: None,
            cockpit: None,
            layout_mode: LayoutMode::Dual,
            settings_open: None,
            left_panel: LeftPanel::BoardPanel,
            left_dock_open: true,
            right_dock_open: true,
            bottom_dock_open: true,
            bottom_panel: BottomPanel::Terminal,
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
        self.root = Some(path.clone());
        self.tabs.clear();
        self.active = 0;
        let index = cx.new(|cx| FileIndex::new(path.clone(), cx));
        let tree = cx.new(|cx| FileTree::new(path.clone(), cx));
        let weak = cx.weak_entity();
        tree.update(cx, |t, _| {
            t.set_on_open(move |path, _window, cx| {
                weak.update(cx, |ws, cx| ws.open_path(path, cx)).ok();
            });
        });
        self.file_index = Some(index);
        self.file_tree = Some(tree);

        // 版控面板(P4/Git 自动检测)
        let vcs = cx.new(|cx| VcsPanel::new(path.clone(), cx));
        let weak = cx.weak_entity();
        vcs.update(cx, |v, _| {
            v.set_on_open_diff(move |title, local_path, window, cx| {
                weak.update(cx, |ws, cx| ws.open_diff(&title, &local_path, cx))
                    .ok();
                let _ = window;
            });
        });
        self.vcs_panel = Some(vcs);
        self.board = Some(cx.new(|cx| Board::new(path.clone(), cx)));

        // Agent 面板 + 编排引擎(状态存项目内,git 忽略)
        let agent = cx.new(|cx| AgentPanel::new(cx));
        let db_dir = path.join(".mf-agent");
        let _ = std::fs::create_dir_all(&db_dir);
        let skills = mf_skills::load_skills(Some(&path));
        let config = mf_agent::Config::load().unwrap_or_default();
        self.editor_font = config.editor.clone();
        let term_cmd = config
            .terminal
            .command
            .clone()
            .filter(|s| !s.trim().is_empty());
        if let Ok(engine) = mf_agent::Engine::start(
            db_dir.join("orchestration.db"),
            path.clone(),
            config,
            skills,
        ) {
                let engine = Arc::new(engine);
            agent.update(cx, |a, cx| a.attach_engine(engine.clone(), &path, cx));
            let shell = term_cmd.unwrap_or_else(crate::console::default_shell);
            self.cockpit = Some(cx.new(|cx| Cockpit::new(engine, path.clone(), shell, cx)));
            }
        self.agent_panel = Some(agent);
        if self.console_dock.is_none() {
            self.console_dock = Some(cx.new(|cx| ConsoleDock::new(cx)));
        }

        self.status_message = format!("已打开 {}", path.display()).into();
        cx.notify();
    }

    pub fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
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
        let root = self.root.clone().unwrap_or_default();
        let diff_text = {
            let p4 = mf_vcs::p4::P4::new(&root);
            match p4.diff_file(local_path) {
                Ok(t) if !t.trim().is_empty() => t,
                Ok(_) => "(无差异)".to_string(),
                Err(_) => {
                    // 回退 git
                    let rel = local_path.strip_prefix(&root).unwrap_or(local_path);
                    mf_vcs::git::Git::open(&root)
                        .and_then(|g| g.diff_file(rel))
                        .unwrap_or_else(|e| format!("获取 diff 失败: {e}"))
                }
            }
        };
        let view = cx.new(|cx| DiffView::new(title, &diff_text, cx));
        // hunk 审阅:拒绝 = 后台 git apply -R mini-patch,完成后重开该 diff
        let root = root.clone();
        let rel = local_path.strip_prefix(&root).unwrap_or(local_path).to_path_buf();
        let title_owned = title.to_string();
        let weak = cx.weak_entity();
        view.update(cx, |dv, _| {
            dv.set_on_reject(move |patch, _window, cx| {
                let root = root.clone();
                let rel = rel.clone();
                let title = title_owned.clone();
                let weak = weak.clone();
                cx.spawn(async move |cx| {
                    let applied = cx.background_executor().spawn(async move {
                        use std::io::Write as _;
                        let mut child = std::process::Command::new("git")
                            .arg("apply")
                            .arg("-R")
                            .arg("--recount")
                            .current_dir(&root)
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
                        match r {
                            Ok(()) => ws.status_message = "hunk 已拒绝(git apply -R),diff 已刷新".into(),
                            Err(e) => ws.status_message = format!("拒绝失败:{e}").into(),
                        }
                        // 重开同文件 diff 刷新视图(复用当前 Diff tab 位置)
                        if let Some(idx) = ws.tabs.iter().rposition(|t| matches!(t, Tab::Diff(_))) {
                            ws.tabs.remove(idx);
                        }
                        ws.open_diff(&title, &rel, cx);
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            });
        });
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
        if self.tabs.is_empty() {
            return;
        }
        self.tabs.remove(self.active);
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.focus_active(cx);
        cx.notify();
    }

    fn next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
            self.focus_active(cx);
            cx.notify();
        }
    }

    fn prev_tab(&mut self, _: &PrevTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
            self.focus_active(cx);
            cx.notify();
        }
    }

    // ---------- 快速打开 / 命令面板 ----------

    fn show_quick_open_files(&mut self, _: &QuickOpenFiles, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.file_index.clone() else {
            self.status_message = "先打开一个文件夹 (Ctrl+Shift+O)".into();
            cx.notify();
            return;
        };
        let qo = cx.new(|cx| QuickOpen::files(index, cx));
        self.wire_quick_open(qo, window, cx);
    }

    fn show_command_palette(&mut self, _: &CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        let qo = cx.new(|cx| QuickOpen::commands(cx));
        let has_folder = self.root.is_some();
        let mut cmds = vec![
            ("open_folder".into(), "打开文件夹…  Ctrl+Shift+O".into()),
            ("toggle_explorer".into(), "显示资源管理器  Ctrl+Shift+E".into()),
            ("toggle_vcs".into(), "显示版本控制  Ctrl+Shift+G".into()),
            ("toggle_board".into(), "显示车间卡片墙  Ctrl+Shift+W".into()),
            ("toggle_agent".into(), "切换右侧 Agent 会话  Ctrl+Shift+/".into()),
            ("toggle_console".into(), "切换底部终端  Ctrl+`".into()),
            ("project_search".into(), "项目搜索…  Ctrl+Shift+F".into()),
            ("show_tasks".into(), "显示任务 DAG  Ctrl+Shift+M".into()),
            ("open_settings".into(), "打开设置  Ctrl+,".into()),
            ("close_tab".into(), "关闭当前标签页  Ctrl+W".into()),
            ("mode_zed".into(), "模式: Zed · 我写代码(编辑器优先) [Alt+1]".into()),
            ("mode_orca".into(), "模式: Orca · AI 驱动(驾驶舱) [Alt+2]".into()),
            ("mode_dual".into(), "模式: 双轨 · 人机协同 [Alt+3]".into()),
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
                            ws.open_path(&p, cx);
                            ws.quick_open = None;
                        }
                        QuickItem::Command { id, .. } => {
                            ws.quick_open = None;
                            match id.as_ref() {
                                "open_folder" => ws.prompt_open_folder(window, cx),
                                "toggle_explorer" => {
                                    ws.left_panel = LeftPanel::Explorer;
                                    ws.left_dock_open = true;
                                }
                                "toggle_vcs" => {
                                    ws.left_panel = LeftPanel::Vcs;
                                    ws.left_dock_open = true;
                                }
                                "toggle_board" => {
                                    ws.left_panel = LeftPanel::BoardPanel;
                                    ws.left_dock_open = true;
                                }
                                "toggle_agent" => ws.toggle_agent_dock(cx),
                                "toggle_console" => ws.toggle_console(cx),
                                "project_search" => ws.open_project_search(cx),
                                "show_tasks" => ws.show_tasks(cx),
                                "open_settings" => ws.open_settings(cx),
                                "mode_zed" => ws.set_layout_mode(LayoutMode::Zed, cx),
                                "mode_orca" => ws.set_layout_mode(LayoutMode::Orca, cx),
                                "mode_dual" => ws.set_layout_mode(LayoutMode::Dual, cx),
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
                    ws.focus_active(cx);
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
        self.left_dock_open = !self.left_dock_open;
        cx.notify();
    }

    fn act_show_explorer(&mut self, _: &ShowExplorer, _: &mut Window, cx: &mut Context<Self>) {
        self.left_panel = LeftPanel::Explorer;
        self.left_dock_open = true;
        cx.notify();
    }

    fn act_show_vcs(&mut self, _: &ShowVcs, _: &mut Window, cx: &mut Context<Self>) {
        self.left_panel = LeftPanel::Vcs;
        self.left_dock_open = true;
        cx.notify();
    }

    fn act_show_board(&mut self, _: &ShowBoard, _: &mut Window, cx: &mut Context<Self>) {
        self.left_panel = LeftPanel::BoardPanel;
        self.left_dock_open = true;
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
            if self.layout_mode == LayoutMode::Orca {
                self.layout_mode = LayoutMode::Dual;
            }
            self.bottom_panel = BottomPanel::Search;
            self.bottom_dock_open = true;
            self.pending_focus = Some(search.read(cx).focus_handle(cx));
            cx.notify();
            return;
        }
        let Some(root) = self.root.clone() else { return };
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
            ws.bottom_dock_open = false;
            ws.focus_active(cx);
            cx.notify();
        })
        .detach();
        self.pending_focus = Some(s.read(cx).focus_handle(cx));
        self.search_overlay = Some(s);
        if self.layout_mode == LayoutMode::Orca {
            self.layout_mode = LayoutMode::Dual;
        }
        self.bottom_panel = BottomPanel::Search;
        self.bottom_dock_open = true;
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

    // ---------- 三模式(工作方式) ----------

    fn act_set_mode_zed(&mut self, _: &SetModeZed, _: &mut Window, cx: &mut Context<Self>) {
        self.set_layout_mode(LayoutMode::Zed, cx);
    }

    fn act_set_mode_orca(&mut self, _: &SetModeOrca, _: &mut Window, cx: &mut Context<Self>) {
        self.set_layout_mode(LayoutMode::Orca, cx);
    }

    fn act_set_mode_dual(&mut self, _: &SetModeDual, _: &mut Window, cx: &mut Context<Self>) {
        self.set_layout_mode(LayoutMode::Dual, cx);
    }

    fn set_layout_mode(&mut self, mode: LayoutMode, cx: &mut Context<Self>) {
        self.layout_mode = mode;
        match mode {
            LayoutMode::Zed => {
                self.left_panel = LeftPanel::Explorer;
                self.left_dock_open = true;
                self.right_dock_open = false;
                self.bottom_dock_open = false;
            }
            LayoutMode::Orca => {
                self.right_dock_open = false;
                self.bottom_dock_open = false;
            }
            LayoutMode::Dual => {
                self.left_panel = LeftPanel::BoardPanel;
                self.left_dock_open = true;
                self.right_dock_open = true;
                self.bottom_dock_open = true;
            }
        }
        self.focus_active(cx);
        cx.notify();
    }

    // ---------- 控制台分屏 ----------

    pub fn toggle_console(&mut self, cx: &mut Context<Self>) {
        if self.console_dock.is_none() {
            self.console_dock = Some(cx.new(|cx| ConsoleDock::new(cx)));
        }
        if self.bottom_dock_open && self.bottom_panel == BottomPanel::Terminal {
            self.bottom_dock_open = false;
        } else {
            if self.layout_mode == LayoutMode::Orca {
                self.layout_mode = LayoutMode::Dual;
            }
            self.bottom_panel = BottomPanel::Terminal;
            self.bottom_dock_open = true;
        }
        cx.notify();
    }

    fn toggle_agent_dock(&mut self, cx: &mut Context<Self>) {
        if self.layout_mode == LayoutMode::Orca {
            self.layout_mode = LayoutMode::Dual;
            self.right_dock_open = true;
        } else {
            self.right_dock_open = !self.right_dock_open;
        }
        cx.notify();
    }

    fn show_tasks(&mut self, cx: &mut Context<Self>) {
        if self.layout_mode == LayoutMode::Orca {
            self.layout_mode = LayoutMode::Dual;
        }
        self.bottom_panel = BottomPanel::Tasks;
        self.bottom_dock_open = true;
        cx.notify();
    }

    // ---------- 设置 ----------

    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings_open.is_some() {
            self.settings_open = None;
            cx.notify();
            return;
        }
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

    fn render_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let tabs: Vec<(String, bool, bool)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (t.title(cx), i == self.active, t.is_dirty(cx)))
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
        for (name, is_active, dirty) in tabs {
            el = el.child(
                div()
                    .id(ElementId::Name(format!("tab-{}", name).into()))
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
                    .child(if name.ends_with("(diff)") {
                        format!("◆ {name}")
                    } else {
                        name
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
                            .text_size(px(10.))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("×"),
                    ),
            );
        }
        el
    }

    fn render_activity_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let icons: [(&'static str, &'static str, LeftPanel); 3] = [
            ("▱", "项目 Ctrl+Shift+E", LeftPanel::Explorer),
            ("⎇", "版控 Ctrl+Shift+G", LeftPanel::Vcs),
            ("▦", "车间 Ctrl+Shift+W", LeftPanel::BoardPanel),
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
            let is_active = self.left_dock_open && self.left_panel == panel;
            let badge = if panel == LeftPanel::Vcs { vcs_count } else if panel == LeftPanel::BoardPanel { unread } else { 0 };
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
                                .bg(rgb(if panel == LeftPanel::BoardPanel { crate::theme::Theme::warning() } else { crate::theme::Theme::accent() }))
                                .text_color(rgb(crate::theme::Theme::bg()))
                                .text_size(px(9.))
                                .child(badge.to_string()),
                        )
                    })
                    .on_click({
                        let panel = panel;
                        cx.listener(move |this: &mut Workspace, _, _, cx| {
                            this.left_panel = panel;
                            this.left_dock_open = true;
                            cx.notify();
                        })
                    }),
            );
        }
        let search_active = self.bottom_dock_open && self.bottom_panel == BottomPanel::Search;
        let agent_active = self.right_dock_open && self.layout_mode != LayoutMode::Orca;
        bar.child(
            activity_button(
                "⌕",
                "搜索 Ctrl+Shift+F",
                search_active,
                cx.listener(|this: &mut Workspace, _, _, cx| this.open_project_search(cx)),
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
        .child(div().flex_1())
        .child(
            activity_button(
                "⚙",
                "设置 Ctrl+,",
                self.settings_open.is_some(),
                cx.listener(|this: &mut Workspace, _, _, cx| this.open_settings(cx)),
            ),
        )
    }

    fn render_left_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let body = match (&self.left_panel, &self.file_tree) {
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
            (LeftPanel::BoardPanel, _) => match &self.board {
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
                    .children([
                        ("项目", LeftPanel::Explorer),
                        ("版控", LeftPanel::Vcs),
                        ("车间", LeftPanel::BoardPanel),
                    ].map(|(label, panel)| {
                        let active = self.left_panel == panel;
                        div()
                            .id(ElementId::Name(format!("dock-tab-{label}").into()))
                            .h(px(26.))
                            .px_2p5()
                            .flex()
                            .items_center()
                            .rounded_t_md()
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .text_color(rgb(if active { crate::theme::Theme::fg() } else { crate::theme::Theme::fg_dim() }))
                            .when(active, |d| d.bg(rgb(crate::theme::Theme::bg_elevated())))
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child(label)
                            .on_click(cx.listener(move |ws: &mut Workspace, _, _, cx| {
                                ws.left_panel = panel;
                                ws.left_dock_open = true;
                                cx.notify();
                            }))
                    })),
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

    fn render_tasks_dock(&self, cx: &Context<Self>) -> AnyElement {
        let (run_status, tasks) = self
            .agent_panel
            .as_ref()
            .map(|agent| {
                let agent = agent.read(cx);
                (agent.run_status_label(), agent.tasks_snapshot())
            })
            .unwrap_or_else(|| ("未启动".into(), Vec::new()));

        div()
            .id("bottom-tasks")
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
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(11.5)).child(format!("RUN · {run_status}")))
                    .child(div().flex_1())
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(px(10.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child("＋ 新建任务"),
                    ),
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
                                .child("暂无任务；从右侧 Agent 会话输入目标后，DAG 会实时显示在这里。"),
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
        let panel: AnyElement = match self.bottom_panel {
            BottomPanel::Terminal => match &self.console_dock {
                Some(console) => div().size_full().flex().child(console.clone()).into_any_element(),
                None => div().size_full().flex().items_center().justify_center().text_color(rgb(crate::theme::Theme::fg_faint())).child("终端尚未启动").into_any_element(),
            },
            BottomPanel::Search => match &self.search_overlay {
                Some(search) => div().size_full().flex().child(search.clone()).into_any_element(),
                None => div().size_full().flex().items_center().justify_center().text_color(rgb(crate::theme::Theme::fg_faint())).child("Ctrl+Shift+F 开始项目搜索").into_any_element(),
            },
            BottomPanel::Tasks => self.render_tasks_dock(cx),
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
                        ("TASKS", BottomPanel::Tasks),
                    ].map(|(label, target)| {
                        let active = self.bottom_panel == target;
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
                                        if ws.console_dock.is_none() {
                                            ws.console_dock = Some(cx.new(|cx| ConsoleDock::new(cx)));
                                        }
                                        ws.bottom_panel = BottomPanel::Terminal;
                                        ws.bottom_dock_open = true;
                                    }
                                    BottomPanel::Search => ws.open_project_search(cx),
                                    BottomPanel::Tasks => ws.show_tasks(cx),
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
                                ws.bottom_dock_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(panel))
    }

    /// 状态栏最左:工作方式三段开关(Zed 手写 / Orca AI 驾驶舱 / 双轨)
    fn render_mode_switch(&self, cx: &Context<Self>) -> impl IntoElement {
        let cur = self.layout_mode;
        div()
            .id("mode-switch")
            .flex()
            .items_center()
            .h(px(18.))
            .rounded_sm()
            .overflow_hidden()
            .border_1()
            .border_color(rgb(crate::theme::Theme::accent_dim()))
            .children([LayoutMode::Zed, LayoutMode::Orca, LayoutMode::Dual].map(|m| {
                let active = cur == m;
                div()
                    .id(ElementId::Name(format!("mode-{}", m.label()).into()))
                    .flex()
                    .items_center()
                    .px_2()
                    .h_full()
                    .cursor_pointer()
                    .text_size(px(11.))
                    .when(active, |d| {
                        d.bg(rgb(crate::theme::Theme::accent()))
                            .text_color(rgb(crate::theme::Theme::bg()))
                    })
                    .when(!active, |d| {
                        d.text_color(rgb(crate::theme::Theme::fg_dim()))
                            .hover(|h| h.bg(rgb(crate::theme::Theme::bg_hover())))
                    })
                    .child(m.label())
                    .on_click(cx.listener(move |ws: &mut Workspace, _: &ClickEvent, _w, cx| {
                        ws.set_layout_mode(m, cx);
                    }))
            }))
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
            .child(self.render_mode_switch(cx))
            .child(div().child(format!("⎇ {root_name}")))
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
                let root = self.root.clone().unwrap_or_default();
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
        let center: AnyElement = if self.tabs.is_empty() {
            self.render_welcome(cx).into_any_element()
        } else {
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
        };

        // 终端最后一个窗格关闭时只收起底 dock，实体其余状态不受影响。
        if let Some(dock) = &self.console_dock {
            if dock.read(cx).close_pending {
                self.console_dock = None;
                if self.bottom_panel == BottomPanel::Terminal {
                    self.bottom_dock_open = false;
                }
            }
        }

        let content: AnyElement = if self.layout_mode == LayoutMode::Orca {
            match &self.cockpit {
                Some(cockpit) => div().flex_1().min_w_0().flex().child(cockpit.clone()).into_any_element(),
                None => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("驾驶舱不可用(未打开项目或引擎启动失败)")
                    .into_any_element(),
            }
        } else {
            let mut center_stack = div()
                .id("center-stack")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().flex().child(center));
            if self.bottom_dock_open {
                center_stack = center_stack.child(self.render_bottom_dock(cx));
            }

            let mut normal = div().flex_1().min_w_0().min_h_0().flex();
            if self.left_dock_open {
                normal = normal.child(self.render_left_panel(cx));
            }
            normal = normal.child(center_stack);
            if self.right_dock_open {
                normal = normal.child(self.render_right_dock());
            }
            normal.into_any_element()
        };

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
            .on_action(cx.listener(Self::act_set_mode_zed))
            .on_action(cx.listener(Self::act_set_mode_orca))
            .on_action(cx.listener(Self::act_set_mode_dual))
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
