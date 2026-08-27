use gpui::prelude::*;
use gpui::*;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};

use crate::term::Screen;

/// VT 嗅探状态(agent CLI 通用横幅识别)
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SniffState {
    Idle,
    Working,
    Done,
    Dead,
    Unknown,
}

/// 控制台分屏(参考 orca):底部 dock + 可嵌套拆分的窗格树,每格一个 ConPTY 终端。
/// 渲染走完整 VT 网格(颜色/光标/alt-screen),可运行 TUI 应用。

/// 窗格树节点;叶子带业务 id(与 ConsolePane.id 一致),纯函数树操作不依赖 cx
#[derive(Clone)]
pub struct LeafPane {
    pub id: usize,
    pub pane: Entity<ConsolePane>,
}

#[derive(Clone)]
pub enum SplitNode {
    Leaf(LeafPane),
    Split {
        /// true = 上下堆叠,false = 左右并排
        vertical: bool,
        children: Vec<SplitNode>,
    },
}

const TERM_ROWS: usize = 26;
const TERM_COLS: usize = 120;
const LINE_H: f32 = 16.0;
const FONT: &str = "Consolas";

pub struct ConsolePane {
    pub id: usize,
    fallback_title: SharedString,
    screen: Screen,
    focus_handle: FocusHandle,
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    dead: bool,
}

impl ConsolePane {
    pub fn new(
        id: usize,
        shell: &str,
        cx: &mut Context<Self>,
    ) -> Result<Self, anyhow::Error> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: TERM_ROWS as u16,
            cols: TERM_COLS as u16,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(std::env::current_dir().unwrap_or_else(|_| ".".into()));
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx) = crossbeam_channel::bounded::<TermMsg>(512);
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(TermMsg::Exit);
                            break;
                        }
                        Ok(n) => {
                            if tx.send(TermMsg::Data(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                    }
                }
            })?;

        let fallback_title: SharedString = std::path::Path::new(shell)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| shell.to_string())
            .into();

        let pane = Self {
            id,
            fallback_title,
            screen: Screen::new(TERM_ROWS, TERM_COLS),
            focus_handle: cx.focus_handle(),
            writer: Some(writer),
            child: Some(child),
            master: Some(pair.master),
            dead: false,
        };
        pane.start_drain(rx, cx);
        Ok(pane)
    }

    /// PTY 启动失败时的只读兜底窗格(展示错误,不参与输入)
    pub(crate) fn failed(id: usize, err: anyhow::Error, cx: &mut Context<Self>) -> Self {
        let mut screen = Screen::new(TERM_ROWS, TERM_COLS);
        screen.feed(
            format!(
                "\x1b[31m终端启动失败:\x1b[0m {}\r\n",
                err.to_string().replace('\n', " ")
            )
            .as_bytes(),
        );
        Self {
            id,
            fallback_title: "错误".into(),
            screen,
            focus_handle: cx.focus_handle(),
            writer: None,
            child: None,
            master: None,
            dead: true,
        }
    }

    fn start_drain(&self, rx: crossbeam_channel::Receiver<TermMsg>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(40))
                    .await;
                let mut data: Vec<u8> = Vec::new();
                let mut exited = false;
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        TermMsg::Data(d) => data.extend_from_slice(&d),
                        TermMsg::Exit => exited = true,
                    }
                }
                if !data.is_empty() || exited {
                    let alive = this
                        .update(cx, |p, cx| {
                            if !data.is_empty() {
                                p.screen.feed(&data);
                            }
                            if exited {
                                p.dead = true;
                                p.writer = None;
                            }
                            cx.notify();
                        })
                        .is_ok();
                    if !alive || exited {
                        break;
                    }
                }
            }
        })
        .detach();
    }

    pub fn title(&self) -> SharedString {
        if !self.screen.title.is_empty() {
            self.screen.title.clone().into()
        } else {
            self.fallback_title.clone()
        }
    }

    /// shell 是否已退出(矩阵缩略格状态点用)
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// 屏幕尾部 n 个非空行(矩阵缩略/状态嗅探)
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        self.screen.tail_lines(n)
    }

    /// VT 嗅探:从屏幕尾部推断 CLI agent 状态(P0-E6,识别任意 TUI agent 的通用横幅)
    pub fn sniff_state(&self) -> SniffState {
        if self.dead {
            return SniffState::Dead;
        }
        for line in self.screen.tail_lines(4).iter().rev() {
            if line.contains("anything") || line.contains("›") {
                return SniffState::Idle; // "Ask ... to do anything" 等待输入
            }
            if line.contains("Worked for") || line.contains("tokens used") {
                return SniffState::Done;
            }
            if line.contains("Working") || line.contains("esc to interrupt") {
                return SniffState::Working;
            }
        }
        SniffState::Unknown
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        if let Some(w) = &mut self.writer {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, _cx: &mut Context<Self>) {
        let k = &event.keystroke;
        let seq: Vec<u8> = if let Some(ch) = k.key_char.clone() {
            ch.into_bytes()
        } else {
            let mods = &k.modifiers;
            // 修饰前缀:xterm 惯例 1=C 2=S 3=A 4=C+S 5=C+A 6=S+A 7=C+S+A
            let mod_code = if mods.control && mods.shift {
                "4"
            } else if mods.control && mods.alt {
                "5"
            } else if mods.control {
                "1"
            } else if mods.shift {
                "2"
            } else if mods.alt {
                "3"
            } else {
                "1"
            };
            let key = k.key.as_str();
            let csi_mod = |letter: char| -> Vec<u8> {
                format!("\x1b[1;{}{}", mod_code, letter).into_bytes()
            };
            match key {
                "enter" => b"\r".to_vec(),
                "backspace" => vec![0x7f],
                "tab" => b"\t".to_vec(),
                "escape" => vec![0x1b],
                "up" => csi_mod('A'),
                "down" => csi_mod('B'),
                "right" => csi_mod('C'),
                "left" => csi_mod('D'),
                "home" => csi_mod('H'),
                "end" => csi_mod('F'),
                "delete" => b"\x1b[3~".to_vec(),
                "insert" => b"\x1b[2~".to_vec(),
                "pageup" => b"\x1b[5~".to_vec(),
                "pagedown" => b"\x1b[6~".to_vec(),
                "f1" => b"\x1bOP".to_vec(),
                "f2" => b"\x1bOQ".to_vec(),
                "f3" => b"\x1bOR".to_vec(),
                "f4" => b"\x1bOS".to_vec(),
                "f5" => b"\x1b[15~".to_vec(),
                "f6" => b"\x1b[17~".to_vec(),
                "f7" => b"\x1b[18~".to_vec(),
                "f8" => b"\x1b[19~".to_vec(),
                "f9" => b"\x1b[20~".to_vec(),
                "f10" => b"\x1b[21~".to_vec(),
                "f11" => b"\x1b[23~".to_vec(),
                "f12" => b"\x1b[24~".to_vec(),
                _ => {
                    // Ctrl+字母等控制字符
                    if mods.control {
                        match key {
                            "c" => vec![0x03],
                            "d" => vec![0x04],
                            "l" => b"\x0c".to_vec(),
                            "z" => vec![0x1a],
                            "a" => vec![0x01],
                            "e" => vec![0x05],
                            "k" => vec![0x0b],
                            "u" => vec![0x15],
                            "w" => vec![0x17],
                            _ => return,
                        }
                    } else {
                        return;
                    }
                }
            }
        };
        self.send_bytes(&seq);
    }
}

enum TermMsg {
    Data(Vec<u8>),
    Exit,
}

impl Drop for ConsolePane {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
        drop(self.master.take());
    }
}

impl Focusable for ConsolePane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

const DEF_FG: [u8; 3] = [0xd6, 0xd9, 0xe3];
const DEF_BG: [u8; 3] = [0x16, 0x16, 0x1e];

fn rgba(c: [u8; 3]) -> gpui::Rgba {
    gpui::Rgba {
        r: c[0] as f32 / 255.,
        g: c[1] as f32 / 255.,
        b: c[2] as f32 / 255.,
        a: 1.0,
    }
}

impl Render for ConsolePane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dead = self.dead;
        let title = self.title();
        let id = self.id;
        // 快照:每行着色分段 + 光标反色
        let rows = self.screen.rows;
        let cols = self.screen.cols;
        let (cur_r, cur_c) = self.screen.cursor();
        let cursor_visible = self.screen.cursor_visible && !is_dead;
        let mut row_elements: Vec<AnyElement> = Vec::with_capacity(rows);
        for r in 0..rows {
            // 收集该行 run:(fg, bg, bold, underline, text)
            let mut runs: Vec<([u8; 3], [u8; 3], bool, bool, String)> = Vec::new();
            let mut c = 0usize;
            while c < cols {
                let cell = self.screen.cell(r, c);
                let mut fg = if cell.fg.default { DEF_FG } else { cell.fg.rgb };
                let mut bg = if cell.bg.default { DEF_BG } else { cell.bg.rgb };
                if cursor_visible && r == cur_r && c == cur_c {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let bold = cell.bold;
                let ul = cell.underline;
                let mut text = String::new();
                while c < cols {
                    let nc = self.screen.cell(r, c);
                    let mut nfg = if nc.fg.default { DEF_FG } else { nc.fg.rgb };
                    let mut nbg = if nc.bg.default { DEF_BG } else { nc.bg.rgb };
                    if cursor_visible && r == cur_r && c == cur_c {
                        std::mem::swap(&mut nfg, &mut nbg);
                    }
                    if nfg == fg && nbg == bg && nc.bold == bold && nc.underline == ul {
                        text.push(nc.ch);
                        c += 1;
                    } else {
                        break;
                    }
                }
                runs.push((fg, bg, bold, ul, text));
            }
            let mut line = div()
                .id(("tl", r))
                .flex()
                .flex_row()
                .h(px(LINE_H))
                .min_w_full()
                .items_center()
                .font_family(FONT)
                .text_size(px(12.5));
            if runs.is_empty() {
                line = line.child(div().w(px(1.)));
            }
            for (i, (fg, bg, bold, ul, text)) in runs.into_iter().enumerate() {
                let has_bg = bg != DEF_BG;
                let mut run = div()
                    .id(("tr", r * 1000 + i))
                    .text_color(rgba(fg))
                    .when(bold, |d| d.font_weight(FontWeight::BOLD))
                    .when(ul, |d| d.underline())
                    .child(text);
                if has_bg {
                    run = run.bg(rgba(bg)).min_h(px(LINE_H));
                }
                line = line.child(run);
            }
            row_elements.push(line.into_any_element());
        }

        div()
            .id(gpui::ElementId::Name(format!("console-pane-{}", id).into()))
            .key_context("Console")
            .size_full()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .track_focus(&self.focus_handle)
            .bg(rgb(crate::theme::Theme::bg()))
            .on_key_down(cx.listener(Self::on_key))
            // orca 式窗格 tab 头:状态点 + 标题(OSC 0/2)+ 网格尺寸
            .child(
                div()
                    .id("pane-header")
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .bg(rgb(crate::theme::Theme::bg_elevated()))
                    .border_b_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .text_size(px(11.))
                    .child(
                        div()
                            .text_color(rgb(if is_dead {
                                crate::theme::Theme::danger()
                            } else {
                                crate::theme::Theme::success()
                            }))
                            .child("●"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(rgb(if is_dead {
                                crate::theme::Theme::fg_faint()
                            } else {
                                crate::theme::Theme::fg_dim()
                            }))
                            .child(if is_dead {
                                SharedString::from(format!("{} (已退出)", title))
                            } else {
                                title
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(format!("{}×{}", cols, rows)),
                    ),
            )
            .child(
                div()
                    .id("pane-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .p_1()
                    .flex()
                    .flex_col()
                    .children(row_elements),
            )
    }
}

// ---------- 分屏 dock ----------

pub struct ConsoleDock {
    tree: SplitNode,
    next_id: usize,
    active_id: usize,
    /// render 时待聚焦的窗格
    focus_pending: Option<Entity<ConsolePane>>,
    /// 最后一个窗格被关闭 → 外层应在 render 时关闭整个 dock
    pub close_pending: bool,
    shell: String,
}

impl ConsoleDock {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut dock = Self {
            tree: empty_tree(),
            next_id: 1,
            active_id: 0,
            focus_pending: None,
            close_pending: false,
            shell: default_shell(),
        };
        let leaf = dock.new_leaf(cx);
        dock.tree = SplitNode::Split {
            vertical: false,
            children: vec![SplitNode::Leaf(leaf.clone())],
        };
        dock.active_id = leaf.id;
        dock.focus_pending = Some(leaf.pane.clone());
        dock
    }

    pub fn pane_count(&self) -> usize {
        count_leaves(&self.tree)
    }

    /// 新建窗格并插入到当前激活窗格旁(vertical=true 上下,false 左右)
    pub fn split_active(&mut self, vertical: bool, cx: &mut Context<Self>) {
        let leaf = self.new_leaf(cx);
        insert_split_at(&mut self.tree, self.active_id, vertical, leaf.clone());
        self.active_id = leaf.id;
        self.focus_pending = Some(leaf.pane.clone());
        cx.notify();
    }

    /// 新建窗格追加到末尾(不拆分)
    pub fn append_pane(&mut self, cx: &mut Context<Self>) {
        let leaf = self.new_leaf(cx);
        if let SplitNode::Split { children, .. } = &mut self.tree {
            children.push(SplitNode::Leaf(leaf.clone()));
        }
        self.active_id = leaf.id;
        self.focus_pending = Some(leaf.pane.clone());
        cx.notify();
    }

    fn new_leaf(&mut self, cx: &mut Context<Self>) -> LeafPane {
        let id = self.next_id;
        self.next_id += 1;
        let shell = self.shell.clone();
        let pane = cx.new(|cx| {
            ConsolePane::new(id, &shell, cx).unwrap_or_else(|e| ConsolePane::failed(id, e, cx))
        });
        LeafPane { id, pane }
    }

    pub fn close_pane(&mut self, id: usize, cx: &mut Context<Self>) {
        remove_leaf(&mut self.tree, id);
        collapse_splits(&mut self.tree);
        if count_leaves(&self.tree) == 0 {
            self.tree = empty_tree();
            self.close_pending = true;
        } else {
            let mut first: Option<LeafPane> = None;
            find_first_leaf(&self.tree, &mut |l| {
                first = Some(l.clone());
            });
            if let Some(l) = first {
                self.active_id = l.id;
                self.focus_pending = Some(l.pane.clone());
            }
        }
        cx.notify();
    }

    pub fn close_active(&mut self, cx: &mut Context<Self>) {
        self.close_pane(self.active_id, cx);
    }
}

pub(crate) fn default_shell() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
}

fn empty_tree() -> SplitNode {
    SplitNode::Split {
        vertical: false,
        children: Vec::new(),
    }
}

fn count_leaves(node: &SplitNode) -> usize {
    match node {
        SplitNode::Leaf(_) => 1,
        SplitNode::Split { children, .. } => children.iter().map(count_leaves).sum(),
    }
}

fn find_first_leaf(node: &SplitNode, f: &mut impl FnMut(&LeafPane)) {
    match node {
        SplitNode::Leaf(l) => f(l),
        SplitNode::Split { children, .. } => {
            for c in children {
                find_first_leaf(c, f);
            }
        }
    }
}

/// 在 id 叶子处包一层分叉:[old, new]
fn insert_split_at(node: &mut SplitNode, id: usize, vertical: bool, new_leaf: LeafPane) -> bool {
    match node {
        SplitNode::Leaf(l) if l.id == id => {
            let old = l.clone();
            *node = SplitNode::Split {
                vertical,
                children: vec![SplitNode::Leaf(old), SplitNode::Leaf(new_leaf)],
            };
            true
        }
        SplitNode::Leaf(_) => false,
        SplitNode::Split { children, .. } => {
            for c in children.iter_mut() {
                if insert_split_at(c, id, vertical, new_leaf.clone()) {
                    return true;
                }
            }
            false
        }
    }
}

/// 精确移除业务 id 对应的叶子
fn remove_leaf(node: &mut SplitNode, id: usize) -> bool {
    match node {
        SplitNode::Leaf(_) => false,
        SplitNode::Split { children, .. } => {
            let before = children.len();
            children.retain(|c| !matches!(c, SplitNode::Leaf(l) if l.id == id));
            if children.len() != before {
                return true;
            }
            for c in children.iter_mut() {
                if remove_leaf(c, id) {
                    return true;
                }
            }
            false
        }
    }
}

/// 单子分叉折叠
fn collapse_splits(node: &mut SplitNode) {
    if let SplitNode::Split { children, .. } = node {
        for c in children.iter_mut() {
            collapse_splits(c);
        }
        if children.len() == 1 {
            let only = children.pop().unwrap();
            *node = only;
        }
    }
}

impl Render for ConsoleDock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(pane) = self.focus_pending.take() {
            let h = pane.read(cx).focus_handle(cx);
            window.focus(&h, cx);
        }
        let weak = cx.weak_entity();
        let active_id = self.active_id;
        let pane_count = self.pane_count();
        let mut el = div()
            .id("console-dock")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .border_t_1()
            .border_color(rgb(crate::theme::Theme::border()));

        el = el.child(
            div()
                .id("console-toolbar")
                .h(px(28.))
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .border_b_1()
                .border_color(rgb(crate::theme::Theme::border()))
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child("终端"),
                )
                .child(dock_btn("＋ 新窗格", cx.listener(|d: &mut ConsoleDock, _, _, cx| {
                    d.append_pane(cx);
                })))
                .child(dock_btn("⬒ 右分屏", cx.listener(|d: &mut ConsoleDock, _, _, cx| {
                    d.split_active(false, cx);
                })))
                .child(dock_btn("⬓ 下分屏", cx.listener(|d: &mut ConsoleDock, _, _, cx| {
                    d.split_active(true, cx);
                })))
                .child(dock_btn("✕ 关闭窗格", cx.listener(|d: &mut ConsoleDock, _, _, cx| {
                    d.close_active(cx);
                }))),
        );

        let tree = self.tree.clone();
        let content = render_node(&tree, active_id, &weak);
        el = el.child(
            div()
                .id("console-body")
                .flex_1()
                .min_h_0()
                .flex()
                .child(content),
        );

        // orca 式底部状态条:窗格数 + 快捷键提示
        el = el.child(
            div()
                .id("console-statusbar")
                .h(px(22.))
                .flex()
                .items_center()
                .gap_4()
                .px_3()
                .border_t_1()
                .border_color(rgb(crate::theme::Theme::border()))
                .bg(rgb(crate::theme::Theme::bg_elevated()))
                .text_size(px(10.))
                .text_color(rgb(crate::theme::Theme::fg_faint()))
                .child(div().child(format!("{} panes", pane_count)))
                .child(
                    div()
                        .ml_auto()
                        .flex()
                        .gap_3()
                        .child(div().child("ctrl+` 切换终端"))
                        .child(div().child("ctrl+shift+p 命令面板")),
                ),
        );
        el
    }
}

fn dock_btn(
    label: &str,
    listener: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("dock-btn-{}", label).into()))
        .px_2()
        .py(px(2.))
        .rounded_sm()
        .bg(rgb(crate::theme::Theme::bg_elevated()))
        .border_1()
        .border_color(rgb(crate::theme::Theme::border()))
        .text_size(px(11.))
        .text_color(rgb(crate::theme::Theme::fg_dim()))
        .cursor_pointer()
        .hover(|d| {
            d.bg(rgb(crate::theme::Theme::bg_hover()))
                .text_color(rgb(crate::theme::Theme::fg()))
        })
        .child(label.to_string())
        .on_click(move |e, window, cx| (listener)(e, window, cx))
}

fn render_node(node: &SplitNode, active_id: usize, weak: &gpui::WeakEntity<ConsoleDock>) -> AnyElement {
    match node {
        SplitNode::Leaf(l) => {
            let leaf = l.clone();
            let leaf_click = leaf.clone();
            let weak = weak.clone();
            let weak_close = weak.clone();
            let is_active = leaf.id == active_id;
            let close_leaf = leaf.clone();
            div()
                .id(ElementId::Name(format!("leaf-{}", leaf.id).into()))
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .relative()
                // 点击窗格即激活(orca 交互)
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    weak.update(cx, |dock, cx| {
                        if dock.active_id != leaf_click.id {
                            dock.active_id = leaf_click.id;
                            dock.focus_pending = Some(leaf_click.pane.clone());
                            cx.notify();
                        }
                    })
                    .ok();
                })
                // orca 风格:激活窗格顶部亮边
                .when(is_active, |d| {
                    d.border_t_2().border_color(rgb(crate::theme::Theme::accent()))
                })
                .child(div().flex_1().min_h_0().child(leaf.pane.clone()))
                // 悬浮关闭按钮(每格独立关闭)
                .child(
                    div()
                        .id(ElementId::Name(format!("leaf-close-{}", close_leaf.id).into()))
                        .absolute()
                        .top_1()
                        .right_1()
                        .w(px(16.))
                        .h(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .hover(|d| {
                            d.bg(rgb(crate::theme::Theme::danger()))
                                .text_color(rgb(0xffffff))
                        })
                        .child("✕")
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            weak_close.update(cx, |dock, cx| {
                                dock.close_pane(close_leaf.id, cx);
                            })
                            .ok();
                        }),
                )
                .into_any_element()
        }
        SplitNode::Split { vertical, children } => {
            let mut el = div()
                .id("console-split")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex();
            el = if *vertical { el.flex_col() } else { el.flex_row() };
            for (i, c) in children.iter().enumerate() {
                let child_el = render_node(c, active_id, weak);
                el = el.child(
                    div()
                        .id(("cs", i))
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .when(i > 0, |d| {
                            if *vertical {
                                d.border_t_1().border_color(rgb(crate::theme::Theme::border()))
                            } else {
                                d.border_l_1().border_color(rgb(crate::theme::Theme::border()))
                            }
                        })
                        .child(child_el),
                );
            }
            el.into_any_element()
        }
    }
}
