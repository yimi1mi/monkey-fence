use gpui::prelude::*;
use gpui::*;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use crate::file_index::FileIndex;

actions!(
    quick_open,
    [ConfirmItem, Dismiss, SelectPrev, SelectNext, ClearQuery]
);

#[derive(Clone, Debug)]
pub enum QuickItem {
    File(PathBuf),
    Command {
        id: SharedString,
        label: SharedString,
    },
}

pub struct QuickOpen {
    mode_files: bool,
    query: String,
    /// UTF-8 byte ranges;平台 InputHandler 对外转换为 UTF-16。
    selected_range: Range<usize>,
    marked_range: Option<Range<usize>>,
    selected: usize,
    results: Vec<QuickItem>,
    focus_handle: FocusHandle,
    file_index: Option<gpui::Entity<FileIndex>>,
    commands: Vec<QuickItem>,
    on_pick: Option<Box<dyn Fn(&QuickItem, &mut Window, &mut App)>>,
    scroll_handle: UniformListScrollHandle,
    /// 文件模式过滤世代:每次输入自增,后台完成时世代不匹配则丢弃。
    filter_gen: u64,
    filter_task: Option<Task<()>>,
}

impl QuickOpen {
    #[cfg(test)]
    pub(crate) fn query_for_test(&self) -> &str {
        &self.query
    }

    #[cfg(test)]
    pub(crate) fn marked_range_for_test(&self) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    pub fn files(file_index: gpui::Entity<FileIndex>, cx: &mut Context<Self>) -> Self {
        let mut q = Self::base(cx);
        q.mode_files = true;
        // 索引流式更新时刷新结果(扫描期间列表逐步填充)
        cx.observe(&file_index, |this, _idx, cx| this.recompute(cx))
            .detach();
        q.file_index = Some(file_index);
        q.recompute(cx);
        q
    }

    pub fn commands(cx: &mut Context<Self>) -> Self {
        let mut q = Self::base(cx);
        q.mode_files = false;
        q.recompute(cx);
        q
    }

    fn base(cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        Self {
            mode_files: true,
            query: String::new(),
            selected_range: 0..0,
            marked_range: None,
            selected: 0,
            results: Vec::new(),
            focus_handle,
            file_index: None,
            commands: Vec::new(),
            on_pick: None,
            scroll_handle: UniformListScrollHandle::new(),
            filter_gen: 0,
            filter_task: None,
        }
    }

    pub fn register_commands(&mut self, cmds: Vec<(String, String)>, cx: &mut Context<Self>) {
        self.commands = cmds
            .into_iter()
            .map(|(id, label)| QuickItem::Command {
                id: id.into(),
                label: label.into(),
            })
            .collect();
        self.recompute(cx);
    }

    pub fn set_on_pick(&mut self, cb: impl Fn(&QuickItem, &mut Window, &mut App) + 'static) {
        self.on_pick = Some(Box::new(cb));
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        if self.mode_files {
            let Some(idx) = &self.file_index else {
                self.results = Vec::new();
                return;
            };
            let paths = idx.read(cx).relative_paths_arc();
            let query = self.query.clone();
            self.filter_gen += 1;
            let gen = self.filter_gen;
            // 大项目全量模糊匹配放到后台;每次输入换代,过期结果直接丢弃。
            self.filter_task = Some(cx.spawn(async move |this, cx| {
                let top = cx
                    .background_executor()
                    .spawn(async move { fuzzy_top(&paths, &query, 40) })
                    .await;
                this.update(cx, |q, cx| {
                    if q.filter_gen == gen {
                        q.results = top
                            .into_iter()
                            .map(|p| QuickItem::File(PathBuf::from(p)))
                            .collect();
                        q.selected = 0;
                        cx.notify();
                    }
                })
                .ok();
            }));
        } else {
            let labels: Vec<String> = self
                .commands
                .iter()
                .map(|c| match c {
                    QuickItem::Command { label, .. } => label.to_string(),
                    _ => String::new(),
                })
                .collect();
            self.results = fuzzy_rank(&labels, &self.query)
                .into_iter()
                .map(|(_, i)| self.commands[i].clone())
                .take(40)
                .collect();
            self.selected = 0;
        }
    }

    fn act_confirm(&mut self, _: &ConfirmItem, window: &mut Window, cx: &mut Context<Self>) {
        self.pick(self.selected, window, cx);
    }

    fn act_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(Dismissed);
    }

    fn pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.results.get(index).cloned() else {
            return;
        };
        self.selected = index;
        if let Some(cb) = &self.on_pick {
            cb(&item, window, cx);
        }
        // 关闭由外层 workspace 处理(pick 回调内清理)
    }

    fn act_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected > 0 {
            self.selected -= 1;
            cx.notify();
        }
    }

    fn act_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.results.len() {
            self.selected += 1;
            cx.notify();
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if let Some(chars) = keystroke.key_char.as_deref() {
            let printable: String = chars.chars().filter(|ch| !ch.is_control()).collect();
            if printable.is_empty() {
                cx.propagate();
                return;
            }
            let range = self.replacement_range(None);
            self.query.replace_range(range.clone(), &printable);
            let cursor = range.start + printable.len();
            self.selected_range = cursor..cursor;
            self.marked_range = None;
            self.recompute(cx);
            cx.notify();
            // 这一字符已经由 KeyDown 路径提交，不再交给平台 InputHandler
            // 重复插入；IME 组合阶段没有 key_char，仍会走 InputHandler。
            cx.stop_propagation();
        } else if keystroke.key == "backspace" {
            // backspace 未被 action 捕获时兜底
            self.query.pop();
            self.selected_range = self.query.len()..self.query.len();
            self.marked_range = None;
            self.recompute(cx);
            cx.notify();
            cx.stop_propagation();
        } else {
            cx.propagate();
        }
    }

    fn utf16_to_utf8(text: &str, offset_utf16: usize) -> usize {
        let mut utf16 = 0usize;
        for (byte, ch) in text.char_indices() {
            if utf16 >= offset_utf16 {
                return byte;
            }
            let next = utf16 + ch.len_utf16();
            if next > offset_utf16 {
                return byte;
            }
            utf16 = next;
        }
        text.len()
    }

    fn utf8_to_utf16(text: &str, offset_utf8: usize) -> usize {
        let mut utf16 = 0usize;
        for (byte, ch) in text.char_indices() {
            if byte >= offset_utf8 {
                break;
            }
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        Self::utf16_to_utf8(&self.query, range.start)..Self::utf16_to_utf8(&self.query, range.end)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        Self::utf8_to_utf16(&self.query, range.start)..Self::utf8_to_utf16(&self.query, range.end)
    }

    fn replacement_range(&self, range_utf16: Option<Range<usize>>) -> Range<usize> {
        range_utf16
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone())
    }
}

impl EntityInputHandler for QuickOpen {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(range_utf16);
        adjusted_range.replace(self.range_to_utf16(&range));
        self.query.get(range).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.replacement_range(range_utf16);
        self.query.replace_range(range.clone(), text);
        let cursor = range.start + text.len();
        self.selected_range = cursor..cursor;
        self.marked_range = None;
        self.recompute(cx);
        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.replacement_range(range_utf16);
        self.query.replace_range(range.clone(), new_text);
        let inserted = range.start..range.start + new_text.len();
        self.marked_range = (!new_text.is_empty()).then_some(inserted.clone());
        self.selected_range = new_selected_range_utf16
            .map(|selected| {
                let start = range.start + Self::utf16_to_utf8(new_text, selected.start);
                let end = range.start + Self::utf16_to_utf8(new_text, selected.end);
                start..end
            })
            .unwrap_or(inserted.end..inserted.end);
        self.recompute(cx);
        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(
            point(element_bounds.left(), element_bounds.bottom()),
            size(px(1.), element_bounds.size.height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(Self::utf8_to_utf16(&self.query, self.selected_range.end))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_range = self.range_from_utf16(range_utf16);
        cx.notify();
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.query.encode_utf16().count())
    }
}

/// nucleo 模糊评分排序,返回 (分数, 原始索引) 降序
fn fuzzy_rank(candidates: &[String], query: &str) -> Vec<(u32, usize)> {
    if query.is_empty() {
        return (0..candidates.len().min(40)).map(|i| (0, i)).collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut scored: Vec<(u32, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let haystack = Utf32String::from(c.as_str());
            pattern
                .score(haystack.slice(..), &mut matcher)
                .map(|s| (s as u32, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
}

/// 后台模糊匹配:返回前 n 个命中的原始路径字符串。
fn fuzzy_top(paths: &Arc<Vec<String>>, query: &str, n: usize) -> Vec<String> {
    fuzzy_rank(paths, query)
        .into_iter()
        .take(n)
        .filter_map(|(_, i)| paths.get(i).cloned())
        .collect()
}

impl Focusable for QuickOpen {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QuickOpen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 该实体直到本次 render 才进入 dispatch tree；在 Workspace 中提前
        // focus 可能落不到任何节点。模态输入层显示期间由自身保证焦点。
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        let input_entity = cx.entity();
        let mode_label = if self.mode_files {
            "转到文件"
        } else {
            "命令面板"
        };
        let query = self.query.clone();
        let selected = self.selected;
        let results: Vec<QuickItem> = self.results.clone();
        let caret = if self.focus_handle.is_focused(window)
            && std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| (d.as_millis() / 500) % 2 == 0)
                .unwrap_or(true)
        {
            "▏"
        } else {
            " "
        };
        div()
            .id("quick-open-root")
            .key_context("QuickOpen")
            .size_full()
            .absolute()
            .top_0()
            .left_0()
            .bg(gpui::black().opacity(0.35))
            .flex()
            .justify_center()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::act_confirm))
            .on_action(cx.listener(Self::act_dismiss))
            .on_action(cx.listener(Self::act_prev))
            .on_action(cx.listener(Self::act_next))
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.emit(Dismissed);
                }),
            )
            .child(
                div()
                    .id("quick-open-panel")
                    .mt(px(80.))
                    .w(px(560.))
                    .max_h(px(420.))
                    .flex()
                    .flex_col()
                    .rounded_md()
                    .bg(rgb(crate::theme::Theme::bg_elevated()))
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        div()
                            .id("quick-open-input")
                            .relative()
                            .px_3()
                            .py_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .border_b_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child(mode_label),
                            )
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .text_color(rgb(crate::theme::Theme::fg()))
                                    .child(format!("{}{}", query, caret)),
                            )
                            .child(div().absolute().top_0().left_0().size_full().child(
                                QuickOpenInputElement {
                                    quick_open: input_entity,
                                },
                            )),
                    )
                    .child(
                        uniform_list(
                            "quick-open-list",
                            results.len(),
                            cx.processor(move |_this, range, _window, cx| {
                                let mut out = Vec::new();
                                for ix in range {
                                    let Some(item) = results.get(ix) else {
                                        continue;
                                    };
                                    let (icon, label) = match item {
                                        QuickItem::File(p) => (
                                            "🖿",
                                            p.file_name()
                                                .map(|n| n.to_string_lossy().into_owned())
                                                .unwrap_or_default(),
                                        ),
                                        QuickItem::Command { label, .. } => {
                                            ("⌘", label.to_string())
                                        }
                                    };
                                    let dir_hint = match item {
                                        QuickItem::File(p) => p
                                            .parent()
                                            .map(|d| d.to_string_lossy().into_owned())
                                            .unwrap_or_default(),
                                        _ => String::new(),
                                    };
                                    let is_sel = ix == selected;
                                    out.push(
                                        div()
                                            .id(("qo", ix))
                                            .h(px(30.))
                                            .px_3()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .when(is_sel, |d| {
                                                d.bg(rgb(crate::theme::Theme::accent_dim()))
                                            })
                                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.pick(ix, window, cx);
                                            }))
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(
                                                        rgb(crate::theme::Theme::fg_faint()),
                                                    )
                                                    .child(icon),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(13.))
                                                    .text_color(rgb(crate::theme::Theme::fg()))
                                                    .child(label),
                                            )
                                            .when(!dir_hint.is_empty(), |d| {
                                                d.child(
                                                    div()
                                                        .ml_auto()
                                                        .text_size(px(11.))
                                                        .text_color(rgb(
                                                            crate::theme::Theme::fg_faint(),
                                                        ))
                                                        .overflow_hidden()
                                                        .child(dir_hint),
                                                )
                                            }),
                                    );
                                }
                                out
                            }),
                        )
                        .track_scroll(&self.scroll_handle)
                        .max_h(px(360.))
                        .flex_1(),
                    ),
            )
    }
}

/// 平台文本输入必须在元素 paint 阶段注册。使用独立 Element 与编辑器保持
/// 同一条已验证链路，避免通用 Canvas 的空绘制被 GPUI 跳过或复用。
struct QuickOpenInputElement {
    quick_open: Entity<QuickOpen>,
}

impl IntoElement for QuickOpenInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for QuickOpenInputElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
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
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.quick_open.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.quick_open.clone()),
            cx,
        );
    }
}

pub struct Dismissed;

impl EventEmitter<Dismissed> for QuickOpen {}
