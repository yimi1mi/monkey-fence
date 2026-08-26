use gpui::*;
use gpui::prelude::*;
use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::file_index::FileIndex;
use crate::file_tree::FileTree;
use crate::quick_open::{QuickItem, QuickOpen};

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
    ]
);

#[derive(Clone, Copy, PartialEq)]
pub enum LeftPanel {
    Explorer,
    Vcs,
    Agent,
}

pub struct Workspace {
    pub root: Option<PathBuf>,
    tabs: Vec<Entity<Editor>>,
    active: usize,
    file_index: Option<Entity<FileIndex>>,
    file_tree: Option<Entity<FileTree>>,
    quick_open: Option<Entity<QuickOpen>>,
    left_panel: LeftPanel,
    status_message: SharedString,
    focus_handle: FocusHandle,
    focus_editor_next: bool,
    pending_focus: Option<Entity<QuickOpen>>,
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
            left_panel: LeftPanel::Explorer,
            status_message: "就绪".into(),
            focus_handle: cx.focus_handle(),
            focus_editor_next: false,
            pending_focus: None,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    // ---------- 项目与文件 ----------

    pub fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
        self.status_message = format!("已打开 {}", path.display()).into();
        cx.notify();
    }

    pub fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(pos) = self.tabs.iter().position(|ed| {
            ed.read(cx)
                .buffer
                .read(cx)
                .path()
                .map(|p| p == path)
                .unwrap_or(false)
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
                self.tabs.push(editor);
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
        let weak2 = cx.weak_entity();
        cx.subscribe(&qo, move |_, _, _: &crate::quick_open::Dismissed, cx| {
            weak2.update(cx, |ws, cx| ws.dismiss_quick_open(cx)).ok();
        })
        .detach();
        self.quick_open = Some(qo.clone());
        self.pending_focus = Some(qo.clone());
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
            .map(|(i, ed)| {
                let b = ed.read(cx).buffer.read(cx);
                let name = b
                    .path()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "未命名".into());
                (name, i == self.active, b.is_dirty())
            })
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
            (LeftPanel::Vcs, _) => div()
                .p_3()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::Theme::FG_FAINT))
                .child("P4 / Git 面板(即将到来)"),
            (LeftPanel::Agent, _) => div()
                .p_3()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::Theme::FG_FAINT))
                .child("Agent 面板(即将到来)"),
        };
        div()
            .id("left-panel")
            .w(px(240.))
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::BG_PANEL))
            .border_r_1()
            .border_color(rgb(crate::theme::Theme::BORDER))
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
        let cursor = self.tabs.get(self.active).map(|ed| {
            let (row, col) = ed.read(cx).cursor_pos(cx);
            format!("行 {}, 列 {}", row + 1, col + 1)
        });
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
            .when(files > 0, |d| {
                d.child(div().child(format!("{} 个文件", files)))
            })
            .child(div().ml_auto().child(self.status_message.clone()))
            .when_some(cursor, |d, c| d.child(div().child(c)))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 延迟聚焦处理(需要 window)
        if let Some(qo) = self.pending_focus.take() {
            let handle = qo.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        } else if self.focus_editor_next && !self.tabs.is_empty() {
            self.focus_editor_next = false;
            if let Some(ed) = self.tabs.get(self.active) {
                let handle = ed.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        let center: AnyElement = if self.tabs.is_empty() {
            self.render_welcome().into_any_element()
        } else {
            let active_editor = self.tabs.get(self.active).cloned();
            let mut col = div()
                .id("editor-col")
                .flex()
                .flex_col()
                .flex_1()
                .child(self.render_tabs(cx));
            if let Some(ed) = active_editor {
                col = col.child(div().flex_1().child(ed));
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
            .child(
                div()
                    .id("main-row")
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_activity_bar(cx))
                    .child(self.render_left_panel(cx))
                    .child(center),
            )
            .child(self.render_status_bar(cx));

        if let Some(qo) = self.quick_open.clone() {
            root = root.child(qo);
        }
        root
    }
}
