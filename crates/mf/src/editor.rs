use gpui::*;
use std::ops::Range;
use mf_core::buffer::Buffer;
use mf_core::highlight::{self, HighlightTag};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

pub const TAB_WIDTH: usize = 4;

actions!(
    editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
        PageUp,
        PageDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        SelectHome,
        SelectEnd,
        WordLeft,
        WordRight,
        DeleteWordBackward,
        DeleteWordForward,
        Undo,
        Redo,
        Save,
        Newline,
        Tab,
        Backtab,
        DuplicateLine,
        MoveLineUp,
        MoveLineDown,
        GotoLineStart,
    ]
);

/// 每行高亮(原始字节坐标,相对行首)
struct HighlightState {
    lines: Vec<Vec<(usize, usize, HighlightTag)>>,
}

/// 一行的显示布局(prepaint 构建)
struct LineLayout {
    display: String,
    /// 原始字节偏移(相对行首)→ 显示字节偏移
    orig_to_disp: Vec<usize>,
    shaped: Option<ShapedLine>,
}

/// 最近一帧几何(paint 写回,供鼠标定位)
struct EditorGeometry {
    bounds: Bounds<Pixels>,
    gutter_width: Pixels,
    /// 绝对行号 → 布局
    lines: HashMap<usize, Arc<LineLayout>>,
    line_height: Pixels,
    scroll_top: usize,
}

pub struct Editor {
    pub buffer: Entity<Buffer>,
    focus_handle: FocusHandle,
    cursor: usize,
    anchor: usize,
    scroll_top: usize,
    desired_col: usize,
    highlight: Option<Arc<HighlightState>>,
    highlight_pending_version: u64,
    viewport_rows: usize,
    is_selecting: bool,
    geometry: Option<Arc<EditorGeometry>>,
    on_saved: Option<Box<dyn Fn(&mut Window, &mut Context<Editor>)>>,
    /// 外观(设置界面可改)
    font_family: SharedString,
    font_size: Pixels,
    line_h: Pixels,
}

impl Editor {
    pub fn new(buffer: Entity<Buffer>, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let mut ed = Self {
            buffer,
            focus_handle,
            cursor: 0,
            anchor: 0,
            scroll_top: 0,
            desired_col: 0,
            highlight: None,
            highlight_pending_version: u64::MAX,
            viewport_rows: 40,
            is_selecting: false,
            geometry: None,
            on_saved: None,
            font_family: "Consolas".into(),
            font_size: px(15.),
            line_h: px(22.),
        };
        ed.request_highlight(cx);
        ed
    }

    /// 应用设置里的编辑器字体
    pub fn set_font(&mut self, cfg: &mf_agent::EditorConfig, cx: &mut Context<Self>) {
        let size = cfg.font_size.clamp(8.0, 32.0);
        self.font_family = cfg.font_family.clone().into();
        self.font_size = px(size);
        self.line_h = px((size * 1.47).round());
        cx.notify();
    }

    pub fn cursor_pos(&self, cx: &App) -> (usize, usize) {
        self.buffer.read(cx).offset_to_pos(self.cursor)
    }

    pub fn set_on_saved(&mut self, cb: impl Fn(&mut Window, &mut Context<Editor>) + 'static) {
        self.on_saved = Some(Box::new(cb));
    }

    fn buf<R>(&self, cx: &App, f: impl FnOnce(&Buffer) -> R) -> R {
        f(self.buffer.read(cx))
    }

    pub fn selection(&self) -> std::ops::Range<usize> {
        if self.anchor <= self.cursor {
            self.anchor..self.cursor
        } else {
            self.cursor..self.anchor
        }
    }

    fn has_selection(&self) -> bool {
        self.anchor != self.cursor
    }

    // ---------- 编辑原语 ----------

    fn apply_edit(&mut self, start: usize, end: usize, text: &str, cx: &mut Context<Self>) {
        self.buffer.update(cx, |b, _| {
            let _ = b.apply(vec![mf_core::buffer::Edit {
                start,
                end,
                text: text.to_string(),
            }]);
        });
        self.cursor = start + text.len();
        self.anchor = self.cursor;
        self.request_highlight(cx);
        self.ensure_cursor_visible(cx);
        cx.notify();
    }

    fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let sel = self.selection();
        self.apply_edit(sel.start, sel.end, text, cx);
    }

    fn delete_range(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) {
        self.apply_edit(range.start, range.end, "", cx);
    }

    fn delete_char(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.has_selection() {
            let sel = self.selection();
            self.delete_range(sel, cx);
            return;
        }
        let target = if forward {
            self.next_char_boundary(self.cursor, cx)
        } else {
            self.prev_char_boundary(self.cursor, cx)
        };
        if forward {
            self.delete_range(self.cursor..target, cx);
        } else {
            self.delete_range(target..self.cursor, cx);
        }
    }

    fn delete_word(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.has_selection() {
            let sel = self.selection();
            self.delete_range(sel, cx);
            return;
        }
        let target = if forward {
            self.next_word_boundary(self.cursor, cx)
        } else {
            self.prev_word_boundary(self.cursor, cx)
        };
        if forward {
            self.delete_range(self.cursor..target, cx);
        } else {
            self.delete_range(target..self.cursor, cx);
        }
    }

    // ---------- 移动原语 ----------

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let len = self.buf(cx, |b| b.len_bytes());
        self.cursor = offset.min(len);
        self.anchor = self.cursor;
        self.update_desired_col(cx);
        self.ensure_cursor_visible(cx);
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let len = self.buf(cx, |b| b.len_bytes());
        self.cursor = offset.min(len);
        self.ensure_cursor_visible(cx);
        cx.notify();
    }

    fn update_desired_col(&mut self, cx: &App) {
        self.desired_col = self.buf(cx, |b| b.offset_to_pos(self.cursor).1);
    }

    fn ensure_cursor_visible(&mut self, cx: &App) {
        let row = self.buf(cx, |b| b.offset_to_pos(self.cursor).0);
        let first = self.scroll_top;
        let last = first + self.viewport_rows.saturating_sub(1);
        if row < first {
            self.scroll_top = row;
        } else if row > last {
            self.scroll_top = row.saturating_sub(self.viewport_rows.saturating_sub(2));
        }
    }

    fn move_vertically(&mut self, delta: i32, select: bool, cx: &mut Context<Self>) {
        let row = self.buf(cx, |b| b.offset_to_pos(self.cursor).0);
        let max_row = self.buf(cx, |b| b.len_lines() as i32 - 1);
        let target_row = (row as i32 + delta).clamp(0, max_row) as usize;
        let offset = self.buf(cx, |b| b.pos_to_offset(target_row, self.desired_col));
        if select {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn scroll_lines(&mut self, delta: i32, cx: &mut Context<Self>) {
        let max = self.buf(cx, |b| b.len_lines().saturating_sub(1));
        let new_top = (self.scroll_top as i32 + delta).clamp(0, max as i32) as usize;
        if new_top != self.scroll_top {
            self.scroll_top = new_top;
            cx.notify();
        }
    }

    // ---------- 边界计算 ----------

    fn prev_char_boundary(&self, offset: usize, cx: &App) -> usize {
        self.buf(cx, |b| {
            if offset == 0 {
                return 0;
            }
            b.char_to_byte(b.byte_to_char(offset).saturating_sub(1))
        })
    }

    fn next_char_boundary(&self, offset: usize, cx: &App) -> usize {
        self.buf(cx, |b| {
            let len = b.len_bytes();
            if offset >= len {
                return len;
            }
            b.char_to_byte((b.byte_to_char(offset) + 1).min(b.len_chars()))
        })
    }

    fn byte_at(&self, offset: usize, cx: &App) -> Option<u8> {
        self.buf(cx, |b| b.text().as_bytes().get(offset).copied())
    }

    fn line_slice(&self, offset: usize, len: usize, cx: &App) -> String {
        self.buf(cx, |b| {
            let text = b.text();
            let end = (offset + len).min(text.len());
            text[offset.min(text.len())..end].to_string()
        })
    }

    fn prev_word_boundary(&self, offset: usize, cx: &App) -> usize {
        let win = self.line_slice(offset.saturating_sub(128), 128, cx);
        let base = offset.saturating_sub(128);
        let rel = offset - base;
        let mut boundary = 0;
        let mut seen_non_ws = false;
        let mut mode_word: Option<bool> = None;
        for (bi, g) in win.grapheme_indices(true) {
            if bi >= rel {
                break;
            }
            let c = g.chars().next().unwrap_or(' ');
            if c.is_whitespace() {
                if seen_non_ws {
                    boundary = bi;
                    break;
                }
                mode_word = None;
            } else {
                let is_word = Editor::is_word_char(c);
                match mode_word {
                    None => {
                        mode_word = Some(is_word);
                        seen_non_ws = true;
                    }
                    Some(m) if m == is_word => {}
                    Some(_) => {
                        boundary = bi;
                        break;
                    }
                }
            }
        }
        base + boundary
    }

    fn next_word_boundary(&self, offset: usize, cx: &App) -> usize {
        let win = self.line_slice(offset, 128, cx);
        let mut boundary = win.len();
        let mut mode_word: Option<bool> = None;
        let mut idx = 0;
        for (_, g) in win.grapheme_indices(true) {
            let c = g.chars().next().unwrap_or(' ');
            let is_ws = c.is_whitespace();
            let is_word = Editor::is_word_char(c);
            if mode_word.is_none() {
                if !is_ws {
                    mode_word = Some(is_word);
                }
                idx = g.len();
                continue;
            }
            if is_ws {
                boundary = win.grapheme_indices(true).find(|(bi, _)| *bi >= idx).map(|(bi, _)| bi).unwrap_or(win.len());
                return offset + boundary;
            }
            if Some(is_word) != mode_word {
                boundary = win
                    .grapheme_indices(true)
                    .find(|(bi, _)| *bi >= idx)
                    .map(|(bi, _)| bi)
                    .unwrap_or(win.len());
                return offset + boundary;
            }
            idx += g.len();
        }
        offset + boundary
    }

    fn is_word_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    // ---------- 高亮 ----------

    fn request_highlight(&mut self, cx: &mut Context<Self>) {
        let version = self.buffer.read(cx).version();
        if version == self.highlight_pending_version {
            return;
        }
        self.highlight_pending_version = version;
        let path = self.buffer.read(cx).path().map(|p| p.to_path_buf());
        let text = self.buffer.read(cx).text();
        let buffer = self.buffer.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(90))
                .await;
            if buffer.update(cx, |b, _| b.version()) != version {
                return;
            }
            let state = cx
                .background_spawn(async move { build_highlight(path.as_deref(), &text) })
                .await;
            this.update(cx, move |ed, cx| {
                if ed.buffer.update(cx, |b, _| b.version()) == version {
                    ed.highlight = Some(state);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // ---------- 动作 ----------

    fn act_backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_char(false, cx);
    }

    fn act_delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_char(true, cx);
    }

    fn act_left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.prev_char_boundary(self.cursor, cx), cx);
    }

    fn act_right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_char_boundary(self.cursor, cx), cx);
    }

    fn act_select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.prev_char_boundary(self.cursor, cx), cx);
    }

    fn act_select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_char_boundary(self.cursor, cx), cx);
    }

    fn act_up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, false, cx);
    }

    fn act_down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, false, cx);
    }

    fn act_select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, true, cx);
    }

    fn act_select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, true, cx);
    }

    fn act_home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let row = self.buf(cx, |b| b.offset_to_pos(self.cursor).0);
        let off = self.buf(cx, |b| b.line_start_offset(row));
        self.move_to(off, cx);
    }

    fn act_end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let row = self.buf(cx, |b| b.offset_to_pos(self.cursor).0);
        let off = self.buf(cx, |b| b.line_byte_range(row).1);
        self.move_to(off, cx);
    }

    fn act_select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        let row = self.buf(cx, |b| b.offset_to_pos(self.cursor).0);
        let off = self.buf(cx, |b| b.line_start_offset(row));
        self.select_to(off, cx);
    }

    fn act_select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        let row = self.buf(cx, |b| b.offset_to_pos(self.cursor).0);
        let off = self.buf(cx, |b| b.line_byte_range(row).1);
        self.select_to(off, cx);
    }

    fn act_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.viewport_rows.saturating_sub(2).max(1) as i32;
        self.move_vertically(-n, false, cx);
    }

    fn act_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.viewport_rows.saturating_sub(2).max(1) as i32;
        self.move_vertically(n, false, cx);
    }

    fn act_word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.prev_word_boundary(self.cursor, cx), cx);
    }

    fn act_word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word_boundary(self.cursor, cx), cx);
    }

    fn act_delete_word_backward(
        &mut self,
        _: &DeleteWordBackward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word(false, cx);
    }

    fn act_delete_word_forward(
        &mut self,
        _: &DeleteWordForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word(true, cx);
    }

    fn act_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.anchor = 0;
        self.cursor = self.buf(cx, |b| b.len_bytes());
        cx.notify();
    }

    fn act_undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.update(cx, |b, _| {
            b.undo();
        });
        self.clamp_cursor(cx);
        self.request_highlight(cx);
        cx.notify();
    }

    fn act_redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.update(cx, |b, _| {
            b.redo();
        });
        self.clamp_cursor(cx);
        self.request_highlight(cx);
        cx.notify();
    }

    fn clamp_cursor(&mut self, cx: &App) {
        let len = self.buf(cx, |b| b.len_bytes());
        self.cursor = self.cursor.min(len);
        self.anchor = self.anchor.min(len);
        self.ensure_cursor_visible(cx);
    }

    fn act_save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.update(cx, |b, _| {
            let _ = b.save();
        });
        if let Some(cb) = &self.on_saved {
            (cb)(window, cx);
        }
        cx.notify();
    }

    fn act_newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        let row = self.buf(cx, |b| b.offset_to_pos(self.cursor).0);
        let indent = self.buf(cx, |b| {
            b.line(row)
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect::<String>()
        });
        self.insert_text(&format!("\n{}", indent), cx);
    }

    fn act_tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        let sel = self.selection();
        let (start_row, start_col) = self.buf(cx, |b| b.offset_to_pos(sel.start));
        let (end_row, end_col) = self.buf(cx, |b| b.offset_to_pos(sel.end));
        if end_row > start_row || (end_row == start_row && end_col > start_col && sel.len() > 0) {
            self.indent_selection(true, cx);
        } else {
            self.insert_text(&" ".repeat(TAB_WIDTH), cx);
        }
    }

    fn act_backtab(&mut self, _: &Backtab, _: &mut Window, cx: &mut Context<Self>) {
        self.indent_selection(false, cx);
    }

    fn indent_selection(&mut self, increase: bool, cx: &mut Context<Self>) {
        let sel = self.selection();
        let (start_row, _) = self.buf(cx, |b| b.offset_to_pos(sel.start));
        let (end_row, end_col) = self.buf(cx, |b| b.offset_to_pos(sel.end));
        let end_row = if end_col == 0 && end_row > start_row {
            end_row - 1
        } else {
            end_row
        };
        let mut edits = Vec::new();
        for row in start_row..=end_row {
            let ls = self.buf(cx, |b| b.line_start_offset(row));
            if increase {
                edits.push(mf_core::buffer::Edit {
                    start: ls,
                    end: ls,
                    text: " ".repeat(TAB_WIDTH),
                });
            } else {
                let line = self.buf(cx, |b| b.line(row));
                let remove = line
                    .chars()
                    .take_while(|c| *c == ' ')
                    .take(TAB_WIDTH)
                    .count();
                if remove > 0 {
                    edits.push(mf_core::buffer::Edit {
                        start: ls,
                        end: ls + remove,
                        text: String::new(),
                    });
                }
            }
        }
        if !edits.is_empty() {
            self.buffer.update(cx, |b, _| {
                let _ = b.apply(edits);
            });
            self.request_highlight(cx);
            cx.notify();
        }
    }

    fn act_duplicate_line(&mut self, _: &DuplicateLine, _: &mut Window, cx: &mut Context<Self>) {
        let row = self.buf(cx, |b| b.offset_to_pos(self.cursor).0);
        let text = self.buf(cx, |b| b.line(row));
        let start = self.buf(cx, |b| b.line_start_offset(row));
        self.apply_edit(start, start, &format!("{}\n", text), cx);
    }

    fn act_move_line_up(&mut self, _: &MoveLineUp, _: &mut Window, cx: &mut Context<Self>) {
        self.swap_line(-1, cx);
    }

    fn act_move_line_down(&mut self, _: &MoveLineDown, _: &mut Window, cx: &mut Context<Self>) {
        self.swap_line(1, cx);
    }

    /// 与相邻行交换:编辑坐标全部基于原文本,升序且不重叠
    fn swap_line(&mut self, dir: i32, cx: &mut Context<Self>) {
        let (row, col) = self.buf(cx, |b| b.offset_to_pos(self.cursor));
        let len_lines = self.buf(cx, |b| b.len_lines());
        let other = row as i32 - dir; // up: 与上一行交换;down: 与下一行交换
        if other < 0 || other as usize >= len_lines {
            return;
        }
        let (upper, lower) = if dir == -1 {
            (other as usize, row)
        } else {
            (row, other as usize)
        };
        let text_upper = self.buf(cx, |b| b.line(upper));
        let text_lower = self.buf(cx, |b| b.line(lower));
        let start_upper = self.buf(cx, |b| b.line_start_offset(upper));
        let start_lower = self.buf(cx, |b| b.line_start_offset(lower));
        let has_nl_lower = lower + 1 < len_lines;
        let new_row = if dir == -1 { row - 1 } else { row + 1 };
        self.buffer.update(cx, |b, _| {
            let lower_seg = format!(
                "{}{}",
                text_upper,
                if has_nl_lower { "\n" } else { "" }
            );
            let upper_seg = format!(
                "{}\n",
                text_lower
            );
            let _ = b.apply(vec![
                mf_core::buffer::Edit {
                    start: start_upper,
                    end: start_lower,
                    text: upper_seg,
                },
                mf_core::buffer::Edit {
                    start: start_lower,
                    end: start_lower + text_lower.len() + if has_nl_lower { 1 } else { 0 },
                    text: lower_seg,
                },
            ]);
        });
        let offset = self.buf(cx, |b| b.pos_to_offset(new_row, col));
        self.cursor = offset;
        self.anchor = offset;
        self.request_highlight(cx);
        self.ensure_cursor_visible(cx);
        cx.notify();
    }

    // ---------- 剪贴板 ----------

    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let sel = self.selection();
        if !sel.is_empty() {
            let text = self.buf(cx, |b| b.text()[sel.clone()].to_string());
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub fn cut_selection(&mut self, cx: &mut Context<Self>) {
        self.copy_selection(cx);
        if self.has_selection() {
            let sel = self.selection();
            self.delete_range(sel, cx);
        }
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
            self.insert_text(&text, cx);
        }
    }

    // ---------- 鼠标 ----------

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        let idx = self.index_for_mouse(event.position, cx);
        if event.modifiers.shift {
            self.select_to(idx, cx);
        } else if event.click_count == 2 {
            let start = self.prev_word_boundary(idx, cx);
            let end = self.next_word_boundary(idx, cx);
            self.anchor = start;
            self.cursor = end.max(start);
            cx.notify();
        } else if event.click_count == 3 {
            let row = self.buf(cx, |b| b.offset_to_pos(idx).0);
            let (ls, le) = self.buf(cx, |b| b.line_byte_range(row));
            self.anchor = ls;
            self.cursor = le;
            cx.notify();
        } else {
            self.move_to(idx, cx);
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let idx = self.index_for_mouse(event.position, cx);
            self.select_to(idx, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_h = self
            .geometry
            .as_ref()
            .map(|g| g.line_height.max(px(1.)).as_f32())
            .unwrap_or(22.);
        let dy = event.delta.pixel_delta(px(line_h)).y.as_f32() / line_h;
        if dy.abs() > 0.01 {
            self.scroll_lines(-(dy.round() as i32), cx);
        }
    }

    fn index_for_mouse(&self, position: Point<Pixels>, cx: &App) -> usize {
        let Some(geo) = self.geometry.as_ref() else {
            return self.cursor;
        };
        if position.y < geo.bounds.top() || position.y > geo.bounds.bottom() {
            return self.cursor;
        }
        let row_f = ((position.y - geo.bounds.top()).as_f32()
            / geo.line_height.max(px(1.)).as_f32())
        .floor();
        let row = (geo.scroll_top as f64 + row_f as f64) as usize;
        let row = row.min(self.buf(cx, |b| b.len_lines().saturating_sub(1)));
        let line_start = self.buf(cx, |b| b.line_start_offset(row));
        let x = position.x - geo.bounds.left() - geo.gutter_width;
        let Some(layout) = geo.lines.get(&row) else {
            return line_start;
        };
        if x <= px(0.) {
            return line_start;
        }
        let Some(shaped) = &layout.shaped else {
            return line_start;
        };
        let disp_idx = shaped.closest_index_for_x(x);
        let orig = disp_to_orig(&layout.orig_to_disp, disp_idx);
        (line_start + orig).min(self.buf(cx, |b| b.len_bytes()))
    }
}

fn build_highlight(path: Option<&Path>, text: &str) -> Arc<HighlightState> {
    let mut line_starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let empty: Vec<Vec<(usize, usize, HighlightTag)>> =
        vec![Vec::new(); line_starts.len()];
    let Some(cfg) = path.and_then(highlight::config_for_path) else {
        return Arc::new(HighlightState { lines: empty });
    };
    let spans = highlight::highlight(text, &cfg);
    let lines = highlight::spans_by_line(&spans, &line_starts);
    Arc::new(HighlightState { lines })
}

/// 显示字节偏移 → 原始字节偏移(orig_to_disp 单调不减)
fn disp_to_orig(map: &[usize], disp: usize) -> usize {
    match map.binary_search(&disp) {
        Ok(i) => i,
        Err(ins) => ins.saturating_sub(1),
    }
}

/// 行文本 → 显示文本(tab 展开)+ 原始→显示字节偏移映射
fn expand_line(line: &str) -> (String, Vec<usize>) {
    let mut display = String::with_capacity(line.len());
    let mut map = Vec::with_capacity(line.len() + 1);
    for (byte_off, c) in line.char_indices() {
        if c == '\t' {
            let visual_col = display.chars().count();
            let spaces = TAB_WIDTH - (visual_col % TAB_WIDTH);
            for _ in 0..spaces {
                display.push(' ');
            }
            // map 按原始字节偏移索引
            while map.len() < byte_off {
                map.push(display.len());
            }
            map.push(display.len());
        } else {
            while map.len() < byte_off {
                map.push(display.len());
            }
            map.push(display.len());
            display.push(c);
        }
    }
    map.push(display.len());
    (display, map)
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("editor-root")
            .key_context("Editor")
            .size_full()
            .relative()
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .bg(rgb(crate::theme::Theme::bg()))
            .font_family(self.font_family.clone())
            .text_size(self.font_size)
            .line_height(self.line_h)
            .text_color(rgb(crate::theme::Theme::fg()))
            .on_action(cx.listener(Self::act_backspace))
            .on_action(cx.listener(Self::act_delete))
            .on_action(cx.listener(Self::act_left))
            .on_action(cx.listener(Self::act_right))
            .on_action(cx.listener(Self::act_up))
            .on_action(cx.listener(Self::act_down))
            .on_action(cx.listener(Self::act_select_left))
            .on_action(cx.listener(Self::act_select_right))
            .on_action(cx.listener(Self::act_select_up))
            .on_action(cx.listener(Self::act_select_down))
            .on_action(cx.listener(Self::act_home))
            .on_action(cx.listener(Self::act_end))
            .on_action(cx.listener(Self::act_select_home))
            .on_action(cx.listener(Self::act_select_end))
            .on_action(cx.listener(Self::act_page_up))
            .on_action(cx.listener(Self::act_page_down))
            .on_action(cx.listener(Self::act_word_left))
            .on_action(cx.listener(Self::act_word_right))
            .on_action(cx.listener(Self::act_delete_word_backward))
            .on_action(cx.listener(Self::act_delete_word_forward))
            .on_action(cx.listener(Self::act_select_all))
            .on_action(cx.listener(Self::act_undo))
            .on_action(cx.listener(Self::act_redo))
            .on_action(cx.listener(Self::act_save))
            .on_action(cx.listener(Self::act_newline))
            .on_action(cx.listener(Self::act_tab))
            .on_action(cx.listener(Self::act_backtab))
            .on_action(cx.listener(Self::act_duplicate_line))
            .on_action(cx.listener(Self::act_move_line_up))
            .on_action(cx.listener(Self::act_move_line_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(EditorElement {
                editor: cx.entity(),
            })
    }
}

struct EditorElement {
    editor: Entity<Editor>,
}

struct PrepaintState {
    geometry: Arc<EditorGeometry>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let ed = self.editor.read(cx);
        let buffer = ed.buffer.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = style
            .line_height
            .to_pixels(AbsoluteLength::Pixels(font_size), window.rem_size());

        let total_lines = buffer.len_lines();
        let scroll_top = ed.scroll_top.min(total_lines.saturating_sub(1));
        let viewport_rows = ((bounds.size.height.as_f32() / line_height.max(px(1.)).as_f32())
            .ceil() as usize)
            .max(1)
            + 1;

        // 行号槽宽度
        let digits = total_lines.to_string().len();
        let gutter_text: SharedString = "0".repeat(digits).into();
        let gutter_run = TextRun {
            len: gutter_text.len(),
            font: style.font(),
            color: rgb(crate::theme::Theme::gutter_fg()).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let gutter_width = window
            .text_system()
            .shape_line(gutter_text, font_size, &[gutter_run], None)
            .width
            + px(24.);

        let selection = ed.selection();
        let cur_row = buffer.offset_to_pos(ed.cursor).0;

        let mut geometry_lines: HashMap<usize, Arc<LineLayout>> = HashMap::new();
        let mut selection_quads = Vec::new();
        let mut cursor_quad = None;

        for vi in 0..viewport_rows {
            let row = scroll_top + vi;
            if row >= total_lines {
                break;
            }
            let raw = buffer.line(row);
            let (display, orig_to_disp) = expand_line(&raw);
            let (line_start, _line_end) = buffer.line_byte_range(row);
            let next_line_start = if row + 1 < total_lines {
                buffer.line_start_offset(row + 1)
            } else {
                buffer.len_bytes()
            };
            let cursor_line_offset = ed.cursor.saturating_sub(line_start);

            // 高亮 spans(原始坐标)→ 显示坐标 runs:切段 → 补缝隙 → 合并同色
            let hl_spans = ed
                .highlight
                .as_ref()
                .and_then(|h| h.lines.get(row))
                .cloned()
                .unwrap_or_default();
            let mut segs: Vec<(usize, usize, Option<u32>)> = Vec::new();
            let mut prev_end = 0usize;
            for (s, e, tag) in &hl_spans {
                let ds = map_disp(&orig_to_disp, *s).min(display.len());
                let de = map_disp(&orig_to_disp, *e).min(display.len());
                if de <= ds {
                    continue;
                }
                if ds > prev_end {
                    segs.push((prev_end, ds, None));
                }
                segs.push((ds, de, Some(crate::theme::Theme::syntax(*tag))));
                prev_end = de;
            }
            if prev_end < display.len() {
                segs.push((prev_end, display.len(), None));
            }
            let default_color = style.color;
            let mut runs: Vec<TextRun> = Vec::new();
            for (s, e, c) in segs {
                if e <= s {
                    continue;
                }
                let color: Hsla = c
                    .map(|v| -> Hsla { rgb(v).into() })
                    .unwrap_or(default_color);
                match runs.last_mut() {
                    Some(last) if last.color == color => last.len += e - s,
                    _ => runs.push(TextRun {
                        len: e - s,
                        font: style.font(),
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }),
                }
            }

            let shaped = if display.is_empty() {
                None
            } else {
                Some(window.text_system().shape_line(
                    SharedString::from(display.clone()),
                    font_size,
                    &runs,
                    None,
                ))
            };

            let layout = Arc::new(LineLayout {
                display,
                orig_to_disp,
                shaped: shaped.clone(),
            });

            let y = bounds.top() + line_height * vi as f32;

            // 选区
            if !selection.is_empty()
                && selection.start < next_line_start
                && selection.end > line_start
            {
                let sel_start_col = selection.start.max(line_start) - line_start;
                let sel_end_col = selection.end.min(next_line_start) - line_start;
                let x0 = shaped
                    .as_ref()
                    .map(|s| {
                        s.x_for_index(map_disp(&layout.orig_to_disp, sel_start_col))
                    })
                    .unwrap_or(px(0.));
                let x1 = shaped
                    .as_ref()
                    .map(|s| {
                        s.x_for_index(map_disp(
                            &layout.orig_to_disp,
                            sel_end_col.min(layout.orig_to_disp.len().saturating_sub(1)),
                        ))
                    })
                    .unwrap_or(px(0.));
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(bounds.left() + gutter_width + x0, y),
                        point(bounds.left() + gutter_width + x1.max(x0), y + line_height),
                    ),
                    crate::theme::Theme::selection(),
                ));
            }

            // 光标
            if row == cur_row {
                let disp = map_disp(&layout.orig_to_disp, cursor_line_offset);
                let x = shaped
                    .as_ref()
                    .map(|s| s.x_for_index(disp))
                    .unwrap_or(px(0.));
                cursor_quad = Some(fill(
                    Bounds::new(
                        point(bounds.left() + gutter_width + x, y),
                        size(px(2.), line_height),
                    ),
                    rgb(crate::theme::Theme::cursor()),
                ));
            }

            geometry_lines.insert(row, layout);
        }

        let geometry = Arc::new(EditorGeometry {
            bounds,
            gutter_width,
            lines: geometry_lines,
            line_height,
            scroll_top,
        });

        PrepaintState {
            geometry,
            cursor: cursor_quad,
            selection: selection_quads,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _ = bounds;
        let ed = self.editor.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = style
            .line_height
            .to_pixels(AbsoluteLength::Pixels(font_size), window.rem_size());
        let total_lines = ed.buffer.read(cx).len_lines();
        let scroll_top = prepaint.geometry.scroll_top;
        let gutter_width = prepaint.geometry.gutter_width;

        // 文本区背景分色:行号区稍暗
        window.paint_quad(fill(
            Bounds::new(bounds.origin, size(gutter_width, bounds.size.height)),
            rgb(crate::theme::Theme::bg_panel()),
        ));

        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }

        for (row, layout) in &prepaint.geometry.lines {
            let vi = row.saturating_sub(scroll_top);
            let y = bounds.top() + line_height * vi as f32;

            // 行号
            let num: SharedString = format!("{}", row + 1).into();
            let num_run = TextRun {
                len: num.len(),
                font: style.font(),
                color: rgb(crate::theme::Theme::gutter_fg()).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let num_line = window
                .text_system()
                .shape_line(num, font_size, &[num_run], None);
            num_line
                .paint(
                    point(
                        bounds.left() + gutter_width - px(16.) - num_line.width,
                        y,
                    ),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .ok();

            // 代码行
            if let Some(shaped) = &layout.shaped {
                shaped
                    .paint(
                        point(bounds.left() + gutter_width, y),
                        line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
            }
        }
        let _ = total_lines;

        // 输入(IME/文本)注册
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );

        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        // 写回几何
        let geometry = prepaint.geometry.clone();
        let viewport_rows = prepaint.geometry.lines.len().max(1);
        self.editor.update(cx, |ed, _| {
            ed.geometry = Some(geometry);
            ed.viewport_rows = viewport_rows;
        });
    }
}

/// 原始字节偏移(相对行首)→ 显示字节偏移
fn map_disp(map: &[usize], orig: usize) -> usize {
    if orig >= map.len() {
        *map.last().unwrap_or(&0)
    } else {
        map[orig]
    }
}

fn byte_to_utf16(text: &str, byte_offset: usize) -> usize {
    let mut u16_count = 0;
    for (byte_off, c) in text.char_indices() {
        if byte_off >= byte_offset {
            return u16_count;
        }
        u16_count += c.len_utf16();
    }
    u16_count
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.buffer.read(cx).text();
        let s = utf16_to_byte(&text, range_utf16.start);
        let e = utf16_to_byte(&text, range_utf16.end);
        Some(text[s.min(e)..e.max(s)].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let sel = self.selection();
        let text = self.buffer.read(cx).text();
        let reversed = self.anchor > self.cursor;
        Some(UTF16Selection {
            range: byte_to_utf16(&text, sel.start)..byte_to_utf16(&text, sel.end),
            reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_text(new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(r) = range_utf16 {
            let text = self.buffer.read(cx).text();
            let s = utf16_to_byte(&text, r.start);
            let e = utf16_to_byte(&text, r.end);
            self.apply_edit(s.min(e), s.max(e), new_text, cx);
        } else {
            self.insert_text(new_text, cx);
        }
        let _ = window;
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

fn utf16_to_byte(text: &str, utf16_offset: usize) -> usize {
    let mut u16_count = 0;
    for (byte_off, c) in text.char_indices() {
        if u16_count >= utf16_offset {
            return byte_off;
        }
        u16_count += c.len_utf16();
    }
    text.len()
}
