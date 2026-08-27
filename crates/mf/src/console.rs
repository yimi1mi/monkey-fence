use gpui::prelude::*;
use gpui::*;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::ops::Range;

/// 控制台分屏(参考 orca):底部 dock + 可嵌套拆分的窗格树,每格一个 ConPTY 终端
///
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

pub struct ConsolePane {
    pub id: usize,
    title: SharedString,
    lines: Vec<String>,
    focus_handle: FocusHandle,
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    dead: bool,
    scroll: UniformListScrollHandle,
    filter: TermFilter,
}

pub(crate) const MAX_LINES: usize = 5000;

impl ConsolePane {
    pub fn new(
        id: usize,
        shell: &str,
        cx: &mut Context<Self>,
    ) -> Result<Self, anyhow::Error> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 30,
            cols: 120,
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
            let mut buf = [0u8; 4096];
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

        let title: SharedString = std::path::Path::new(shell)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| shell.to_string())
            .into();

        let pane = Self {
            id,
            title,
            lines: vec![String::new()],
            focus_handle: cx.focus_handle(),
            writer: Some(writer),
            child: Some(child),
            master: Some(pair.master),
            dead: false,
            scroll: UniformListScrollHandle::new(),
            filter: TermFilter::new(),
        };
        pane.start_drain(rx, cx);
        Ok(pane)
    }

    /// PTY 启动失败时的只读兜底窗格(展示错误,不参与输入)
    fn failed(id: usize, err: anyhow::Error, cx: &mut Context<Self>) -> Self {
        Self {
            id,
            title: "终端(错误)".into(),
            lines: vec![format!("终端启动失败: {err}"), String::new()],
            focus_handle: cx.focus_handle(),
            writer: None,
            child: None,
            master: None,
            dead: true,
            scroll: UniformListScrollHandle::new(),
            filter: TermFilter::new(),
        }
    }

    fn start_drain(&self, rx: crossbeam_channel::Receiver<TermMsg>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(80))
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
                                p.filter.feed(&mut p.lines, &data);
                            }
                            if exited {
                                p.dead = true;
                                p.writer = None;
                            }
                            if p.lines.len() > 1 {
                                p.scroll
                                    .scroll_to_item(p.lines.len() - 1, ScrollStrategy::Top);
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

    fn send_bytes(&mut self, bytes: &[u8]) {
        if let Some(w) = &mut self.writer {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, _cx: &mut Context<Self>) {
        let k = &event.keystroke;
        if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
            // 常用控制字符;其余快捷键留给全局(如 Ctrl+` 切换 dock)
            if k.modifiers.control {
                match k.key.as_str() {
                    "c" => self.send_bytes(&[0x03]),
                    "d" => self.send_bytes(&[0x04]),
                    "l" => self.send_bytes(b"\x0c"),
                    _ => {}
                }
            }
            return;
        }
        let seq: Vec<u8> = if let Some(ch) = k.key_char.clone() {
            ch.into_bytes()
        } else {
            match k.key.as_str() {
                "enter" => b"\r".to_vec(),
                "backspace" => vec![0x7f],
                "tab" => b"\t".to_vec(),
                "escape" => vec![0x1b],
                "up" => b"\x1b[A".to_vec(),
                "down" => b"\x1b[B".to_vec(),
                "right" => b"\x1b[C".to_vec(),
                "left" => b"\x1b[D".to_vec(),
                "home" => b"\x1b[H".to_vec(),
                "end" => b"\x1b[F".to_vec(),
                "delete" => b"\x1b[3~".to_vec(),
                "pageup" => b"\x1b[5~".to_vec(),
                "pagedown" => b"\x1b[6~".to_vec(),
                _ => return,
            }
        };
        self.send_bytes(&seq);
    }
}

enum TermMsg {
    Data(Vec<u8>),
    Exit,
}

/// 终端字节流过滤器:剥 ANSI/OSC 序列,维护当前列实现 \r 覆盖写、\b、\t。
/// 解析状态(含未完结的转义序列)跨 feed 调用保持。
pub struct TermFilter {
    /// 0=地面 1=ESC 2=CSI 3=OSC 4=OSC_ST
    esc: u8,
    /// 当前行光标列(字符计)
    col: usize,
}

impl Default for TermFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl TermFilter {
    pub fn new() -> Self {
        Self { esc: 0, col: 0 }
    }

    pub fn feed(&mut self, lines: &mut Vec<String>, bytes: &[u8]) {
        for &b in bytes {
            match self.esc {
                0 => self.ground(b, lines),
                1 => {
                    self.esc = match b {
                        b'[' => 2,
                        b']' => 3,
                        _ => 0,
                    };
                }
                2 => {
                    // CSI 参数/中间字节,遇最终字节(0x40..=0x7e)结束
                    if (0x40..=0x7e).contains(&b) {
                        self.esc = 0;
                    }
                }
                3 => {
                    if b == 0x07 {
                        self.esc = 0;
                    } else if b == 0x1b {
                        self.esc = 4;
                    }
                }
                _ => self.esc = 0,
            }
        }
        if lines.len() > MAX_LINES {
            let drop_n = lines.len() - MAX_LINES;
            lines.drain(0..drop_n);
        }
    }

    fn ground(&mut self, b: u8, lines: &mut Vec<String>) {
        match b {
            0x1b => self.esc = 1,
            b'\n' => {
                lines.push(String::new());
                self.col = 0;
            }
            _ => {
                let line = lines.last_mut().expect("lines 非空");
                match b {
                    b'\r' => self.col = 0,
                    0x08 => self.col = self.col.saturating_sub(1),
                    b'\t' => {
                        let target = (self.col / 8 + 1) * 8;
                        while self.col < target {
                            put_char(line, &mut self.col, ' ');
                        }
                    }
                    _ if b < 0x20 => {}
                    _ => put_char(line, &mut self.col, b as char),
                }
            }
        }
    }
}

/// 按字符列写入:越界覆盖,不足补空格
fn put_char(line: &mut String, col: &mut usize, c: char) {
    let len = line.chars().count();
    if *col < len {
        let mut chars: Vec<char> = line.chars().collect();
        chars[*col] = c;
        line.clear();
        line.extend(chars);
    } else {
        for _ in len..*col {
            line.push(' ');
        }
        line.push(c);
    }
    *col += 1;
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

impl Render for ConsolePane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dead = self.dead;
        let title = self.title.clone();
        let lines: Vec<String> = self.lines.clone();
        let id = self.id;
        div()
            .id(gpui::ElementId::Name(format!("console-pane-{}", id).into()))
            .key_context("Console")
            .size_full()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .track_focus(&self.focus_handle)
            .bg(rgb(crate::theme::Theme::BG))
            .on_key_down(cx.listener(Self::on_key))
            .child(
                div()
                    .id("pane-header")
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .bg(rgb(crate::theme::Theme::BG_ELEVATED))
                    .border_b_1()
                    .border_color(rgb(crate::theme::Theme::BORDER))
                    .text_size(px(10.))
                    .child(
                        div()
                            .text_color(rgb(if is_dead {
                                crate::theme::Theme::DANGER
                            } else {
                                crate::theme::Theme::SUCCESS
                            }))
                            .child(if is_dead { "●" } else { "●" }),
                    )
                    .child(
                        div()
                            .text_color(rgb(if is_dead {
                                crate::theme::Theme::FG_FAINT
                            } else {
                                crate::theme::Theme::FG_DIM
                            }))
                            .child(if is_dead {
                                SharedString::from(format!("{}(已退出)", title))
                            } else {
                                title
                            }),
                    ),
            )
            .child(
                uniform_list(
                    "pane-lines",
                    lines.len(),
                    move |range: Range<usize>, _window: &mut Window, _cx: &mut App| {
                        let mut out = Vec::new();
                        for ix in range {
                            if ix >= lines.len() {
                                continue;
                            }
                            let text = lines[ix].clone();
                            out.push(
                                div()
                                    .id(("pl", ix))
                                    .pl_2()
                                    .pr_2()
                                    .h(px(17.))
                                    .flex()
                                    .items_center()
                                    .font_family("Consolas")
                                    .text_size(px(12.))
                                    .text_color(rgb(crate::theme::Theme::FG_DIM))
                                    .overflow_hidden()
                                    .child(text),
                            );
                        }
                        out
                    },
                )
                .track_scroll(&self.scroll)
                .flex_1(),
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
            // 激活第一个剩余窗格
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

fn default_shell() -> String {
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
        // 待聚焦窗格
        if let Some(pane) = self.focus_pending.take() {
            let h = pane.read(cx).focus_handle(cx);
            window.focus(&h, cx);
        }
        let weak = cx.weak_entity();
        let active_id = self.active_id;
        let mut el = div()
            .id("console-dock")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::BG_PANEL))
            .border_t_1()
            .border_color(rgb(crate::theme::Theme::BORDER));

        el = el.child(
            div()
                .id("console-toolbar")
                .h(px(28.))
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .border_b_1()
                .border_color(rgb(crate::theme::Theme::BORDER))
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(crate::theme::Theme::FG_FAINT))
                        .child("控制台"),
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
        .bg(rgb(crate::theme::Theme::BG_ELEVATED))
        .border_1()
        .border_color(rgb(crate::theme::Theme::BORDER))
        .text_size(px(11.))
        .text_color(rgb(crate::theme::Theme::FG_DIM))
        .cursor_pointer()
        .hover(|d| {
            d.bg(rgb(crate::theme::Theme::BG_HOVER))
                .text_color(rgb(crate::theme::Theme::FG))
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
                    d.border_t_2().border_color(rgb(crate::theme::Theme::ACCENT))
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
                        .text_color(rgb(crate::theme::Theme::FG_FAINT))
                        .hover(|d| {
                            d.bg(rgb(crate::theme::Theme::DANGER))
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
                                d.border_t_1().border_color(rgb(crate::theme::Theme::BORDER))
                            } else {
                                d.border_l_1().border_color(rgb(crate::theme::Theme::BORDER))
                            }
                        })
                        .child(child_el),
                );
            }
            el.into_any_element()
        }
    }
}

