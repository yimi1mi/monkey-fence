use gpui::prelude::*;
use gpui::*;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use std::path::PathBuf;

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
    selected: usize,
    results: Vec<QuickItem>,
    focus_handle: FocusHandle,
    file_index: Option<gpui::Entity<FileIndex>>,
    commands: Vec<QuickItem>,
    on_pick: Option<Box<dyn Fn(&QuickItem, &mut Window, &mut App)>>,
    scroll_handle: UniformListScrollHandle,
}

impl QuickOpen {
    pub fn files(file_index: gpui::Entity<FileIndex>, cx: &mut Context<Self>) -> Self {
        let mut q = Self::base(cx);
        q.mode_files = true;
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
            selected: 0,
            results: Vec::new(),
            focus_handle,
            file_index: None,
            commands: Vec::new(),
            on_pick: None,
            scroll_handle: UniformListScrollHandle::new(),
        }
    }

    pub fn register_commands(&mut self, cmds: Vec<(String, String)>, cx: &App) {
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

    fn recompute(&mut self, cx: &App) {
        if self.mode_files {
            let Some(idx) = &self.file_index else {
                self.results = Vec::new();
                return;
            };
            let paths = idx.read(cx).relative_paths();
            self.results = fuzzy_rank(&paths, &self.query)
                .into_iter()
                .map(|(_, i)| QuickItem::File(PathBuf::from(&paths[i])))
                .take(40)
                .collect();
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
        }
        self.selected = 0;
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
        if let Some(chars) = keystroke.key_char.clone() {
            let printable: String = chars.chars().filter(|c| !c.is_control()).collect();
            if !printable.is_empty() {
                self.query.push_str(&printable);
                self.recompute(cx);
                cx.notify();
            }
        } else if keystroke.key == "backspace" {
            // backspace 未被 action 捕获时兜底
            self.query.pop();
            self.recompute(cx);
            cx.notify();
        }
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

impl Focusable for QuickOpen {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QuickOpen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode_label = if self.mode_files {
            "转到文件"
        } else {
            "命令面板"
        };
        let query = self.query.clone();
        let selected = self.selected;
        let results: Vec<QuickItem> = self.results.clone();
        let caret = if std::time::SystemTime::now()
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
                            ),
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

pub struct Dismissed;

impl EventEmitter<Dismissed> for QuickOpen {}
