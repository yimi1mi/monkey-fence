use gpui::*;
use gpui::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent_panel::AgentPanel;
use crate::console::ConsoleDock;
use crate::diff_view::DiffView;
use crate::editor::Editor;
use crate::file_index::FileIndex;
use crate::file_tree::FileTree;
use crate::quick_open::{QuickItem, QuickOpen};
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
        ShowAgent,
        ToggleConsole,
        OpenSettings,
    ]
);

#[derive(Clone, Copy, PartialEq)]
pub enum LeftPanel {
    Explorer,
    Vcs,
    Agent,
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
    vcs_panel: Option<Entity<VcsPanel>>,
    agent_panel: Option<Entity<AgentPanel>>,
    console_dock: Option<Entity<ConsoleDock>>,
    settings_open: Option<Entity<SettingsView>>,
    left_panel: LeftPanel,
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
            vcs_panel: None,
            agent_panel: None,
            console_dock: None,
            settings_open: None,
            left_panel: LeftPanel::Explorer,
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

        // Agent 面板 + 编排引擎(状态存项目内,git 忽略)
        let agent = cx.new(|cx| AgentPanel::new(cx));
        let db_dir = path.join(".mf-agent");
        let _ = std::fs::create_dir_all(&db_dir);
        let skills = mf_skills::load_skills(Some(&path));
        let config = mf_agent::Config::load().unwrap_or_default();
        self.editor_font = config.editor.clone();
        if let Ok(engine) = mf_agent::Engine::start(
            db_dir.join("orchestration.db"),
            path.clone(),
            config,
            skills,
        ) {
            let engine = Arc::new(engine);
            agent.update(cx, |a, cx| a.attach_engine(engine.clone(), &path, cx));
        }
        self.agent_panel = Some(agent);

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
        let view = cx.new(|_| DiffView::new(title, &diff_text));
        self.tabs.push(Tab::Diff(view));
        self.active = self.tabs.len() - 1;
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
            ("open_folder".into(), "打开文件夹…".into()),
            ("toggle_explorer".into(), "显示资源管理器".into()),
            ("toggle_vcs".into(), "显示版本控制".into()),
            ("toggle_agent".into(), "显示 Agent 面板".into()),
            ("toggle_console".into(), "切换控制台分屏".into()),
            ("open_settings".into(), "打开设置".into()),
            ("close_tab".into(), "关闭当前标签页".into()),
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
                                "toggle_explorer" => ws.left_panel = LeftPanel::Explorer,
                                "toggle_vcs" => ws.left_panel = LeftPanel::Vcs,
                                "toggle_agent" => ws.left_panel = LeftPanel::Agent,
                                "toggle_console" => ws.toggle_console(cx),
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
        self.left_panel = match self.left_panel {
            LeftPanel::Explorer => LeftPanel::Vcs,
            LeftPanel::Vcs => LeftPanel::Agent,
            LeftPanel::Agent => LeftPanel::Explorer,
        };
        cx.notify();
    }

    fn act_show_explorer(&mut self, _: &ShowExplorer, _: &mut Window, cx: &mut Context<Self>) {
        self.left_panel = LeftPanel::Explorer;
        cx.notify();
    }

    fn act_show_vcs(&mut self, _: &ShowVcs, _: &mut Window, cx: &mut Context<Self>) {
        self.left_panel = LeftPanel::Vcs;
        cx.notify();
    }

    fn act_show_agent(&mut self, _: &ShowAgent, _: &mut Window, cx: &mut Context<Self>) {
        self.left_panel = LeftPanel::Agent;
        cx.notify();
    }

    fn act_toggle_console(&mut self, _: &ToggleConsole, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_console(cx);
    }

    fn act_open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(cx);
    }

    // ---------- 控制台分屏 ----------

    pub fn toggle_console(&mut self, cx: &mut Context<Self>) {
        if self.console_dock.take().is_none() {
            self.console_dock = Some(cx.new(|cx| ConsoleDock::new(cx)));
        }
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

    fn render_welcome(&self) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                div()
                    .text_size(px(42.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(crate::theme::Theme::FG))
                    .child("MonkeyFence"),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(rgb(crate::theme::Theme::FG_DIM))
                    .child("AI 编辑器 · 任务流转 · P4 版控"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(crate::theme::Theme::FG_FAINT))
                    .child("Ctrl+Shift+O 打开文件夹 · Ctrl+P 快速打开 · Ctrl+Shift+P 命令面板"),
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
            .border_b_1()
            .border_color(rgb(crate::theme::Theme::BORDER))
            .bg(rgb(crate::theme::Theme::BG_PANEL));
        for (name, is_active, dirty) in tabs {
            el = el.child(
                div()
                    .id(ElementId::Name(format!("tab-{}", name).into()))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .h_full()
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(is_active, |d| {
                        d.bg(rgb(crate::theme::Theme::BG))
                            .border_b_2()
                            .border_color(rgb(crate::theme::Theme::ACCENT))
                            .text_color(rgb(crate::theme::Theme::FG))
                    })
                    .when(!is_active, |d| {
                        d.text_color(rgb(crate::theme::Theme::FG_DIM))
                            .hover(|h| h.bg(rgb(crate::theme::Theme::BG_HOVER)))
                    })
                    .child(name)
                    .when(dirty, |d| {
                        d.child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(rgb(crate::theme::Theme::WARNING)),
                        )
                    }),
            );
        }
        el
    }

    fn render_activity_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let _ = cx;
        let icons: [(&'static str, &'static str, LeftPanel); 3] = [
            ("🗂", "资源管理器", LeftPanel::Explorer),
            ("⎇", "版本控制", LeftPanel::Vcs),
            ("🐒", "Agent", LeftPanel::Agent),
        ];
        let mut bar = div()
            .id("activity-bar")
            .w(px(44.))
            .flex()
            .flex_col()
            .items_center()
            .py_2()
            .gap_1()
            .bg(rgb(crate::theme::Theme::BG_PANEL))
            .border_r_1()
            .border_color(rgb(crate::theme::Theme::BORDER));
        for (icon, tip, panel) in icons {
            let is_active = self.left_panel == panel;
            bar = bar.child(
                div()
                    .id(ElementId::Name(format!("act-{}", tip).into()))
                    .size(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .text_size(px(17.))
                    .cursor_pointer()
                    .when(is_active, |d| {
                        d.bg(rgb(crate::theme::Theme::BG_ACTIVE))
                            .text_color(rgb(crate::theme::Theme::ACCENT))
                    })
                    .when(!is_active, |d| {
                        d.text_color(rgb(crate::theme::Theme::FG_DIM))
                            .hover(|h| h.bg(rgb(crate::theme::Theme::BG_HOVER)))
                    })
                    .child(icon)
                    .on_click({
                        let panel = panel;
                        cx.listener(move |this: &mut Workspace, _, _, cx| {
                            this.left_panel = panel;
                            cx.notify();
                        })
                    }),
            );
        }
        bar
    }

    fn render_left_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let title = match self.left_panel {
            LeftPanel::Explorer => "资源管理器",
            LeftPanel::Vcs => "版本控制",
            LeftPanel::Agent => "AGENT",
        };
        let body = match (&self.left_panel, &self.file_tree) {
            (LeftPanel::Explorer, Some(tree)) => div().size_full().flex().child(tree.clone()),
            (LeftPanel::Explorer, None) => div()
                .p_3()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::Theme::FG_FAINT))
                .child("尚未打开文件夹"),
            (LeftPanel::Vcs, _) => match &self.vcs_panel {
                Some(v) => div().size_full().flex().child(v.clone()),
                None => div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::Theme::FG_FAINT))
                    .child("尚未打开文件夹"),
            },
            (LeftPanel::Agent, _) => match &self.agent_panel {
                Some(a) => div().size_full().flex().child(a.clone()),
                None => div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::Theme::FG_FAINT))
                    .child("尚未打开文件夹"),
            },
        };
        let width = match self.left_panel {
            LeftPanel::Agent => px(340.),
            _ => px(250.),
        };
        div()
            .id("left-panel")
            .w(width)
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::BG_PANEL))
            .border_r_1()
            .border_color(rgb(crate::theme::Theme::BORDER))
            .overflow_hidden()
            .child(
                div()
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .px_3()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(crate::theme::Theme::FG_FAINT))
                    .child(title),
            )
            .child(body)
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let files = self
            .file_index
            .as_ref()
            .map(|f| f.read(cx).len())
            .unwrap_or(0);
        let cursor = self.tabs.get(self.active).and_then(|t| match t {
            Tab::Editor(ed) => {
                let (row, col) = ed.read(cx).cursor_pos(cx);
                Some(format!("行 {}, 列 {}", row + 1, col + 1))
            }
            Tab::Diff(_) => None,
        });
        let vcs_label = self
            .vcs_panel
            .as_ref()
            .and_then(|v| v.read(cx).client_label().or_else(|| v.read(cx).branch_label()));
        let root_name = self
            .root
            .as_ref()
            .and_then(|r| r.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未打开项目".into());
        div()
            .id("status-bar")
            .h(px(24.))
            .flex()
            .items_center()
            .px_3()
            .gap_4()
            .bg(rgb(crate::theme::Theme::BG_PANEL))
            .border_t_1()
            .border_color(rgb(crate::theme::Theme::BORDER))
            .text_size(px(11.))
            .text_color(rgb(crate::theme::Theme::FG_DIM))
            .child(div().child(root_name))
            .when_some(vcs_label, |d, v| d.child(div().text_color(rgb(crate::theme::Theme::ACCENT)).child(v)))
            .when(files > 0, |d| {
                d.child(div().child(format!("{} 个文件", files)))
            })
            .child(
                div()
                    .id("status-message")
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(self.status_message.clone()),
            )
            .when_some(cursor, |d, c| d.child(div().child(c)))
            .child(
                div()
                    .id("sb-console")
                    .h_full()
                    .flex()
                    .items_center()
                    .px_2()
                    .ml_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(rgb(if self.console_dock.is_some() {
                        crate::theme::Theme::ACCENT
                    } else {
                        crate::theme::Theme::FG_DIM
                    }))
                    .hover(|d| d.bg(rgb(crate::theme::Theme::BG_HOVER)))
                    .child("⌨ 终端")
                    .on_click(cx.listener(|ws: &mut Workspace, _: &ClickEvent, _w, cx| {
                        ws.toggle_console(cx);
                    })),
            )
            .child(
                div()
                    .id("sb-settings")
                    .h_full()
                    .flex()
                    .items_center()
                    .px_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(rgb(crate::theme::Theme::FG_DIM))
                    .hover(|d| d.bg(rgb(crate::theme::Theme::BG_HOVER)))
                    .child("⚙")
                    .on_click(cx.listener(|ws: &mut Workspace, _: &ClickEvent, _w, cx| {
                        ws.open_settings(cx);
                    })),
            )
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
            self.render_welcome().into_any_element()
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

        let mut root = div()
            .id("workspace-root")
            .key_context("Workspace")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::BG))
            .text_color(rgb(crate::theme::Theme::FG))
            .on_action(cx.listener(Self::act_open_folder))
            .on_action(cx.listener(Self::show_quick_open_files))
            .on_action(cx.listener(Self::show_command_palette))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::act_toggle_left))
            .on_action(cx.listener(Self::act_show_explorer))
            .on_action(cx.listener(Self::act_show_vcs))
            .on_action(cx.listener(Self::act_show_agent))
            .on_action(cx.listener(Self::act_toggle_console))
            .on_action(cx.listener(Self::act_open_settings))
            .child(
                div()
                    .id("main-row")
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_activity_bar(cx))
                    .child(self.render_left_panel(cx))
                    .child(center),
            );

        // 控制台 dock(最后一个窗格关闭时自动收起)
        if let Some(dock) = &self.console_dock {
            if dock.read(cx).close_pending {
                self.console_dock = None;
            }
        }
        if let Some(dock) = &self.console_dock {
            root = root.child(
                div()
                    .id("console-dock-area")
                    .h(px(240.))
                    .min_h_0()
                    .flex()
                    .child(dock.clone()),
            );
        }
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
