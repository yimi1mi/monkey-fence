use gpui::prelude::*;
use gpui::*;
use mf_vcs::{parse_unified_diff, DiffLineKind, UnifiedDiff};

/// 只读统一 diff 视图(着色行渲染)
pub struct DiffView {
    title: SharedString,
    diff: UnifiedDiff,
    scroll_handle: UniformListScrollHandle,
}

impl DiffView {
    pub fn new(title: impl Into<SharedString>, diff_text: &str) -> Self {
        Self {
            title: title.into(),
            diff: parse_unified_diff(diff_text),
            scroll_handle: UniformListScrollHandle::new(),
        }
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = cx;
        let lines: Vec<(DiffLineKind, String, Option<usize>, Option<usize>)> = self
            .diff
            .lines
            .iter()
            .map(|l| (l.kind, l.text.clone(), l.old_no, l.new_no))
            .collect();
        let added = self.diff.added;
        let deleted = self.diff.deleted;
        div()
            .id("diff-view")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::bg()))
            .child(
                div()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .border_b_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .bg(rgb(crate::theme::Theme::bg_panel()))
                    .text_size(px(12.))
                    .child(
                        div()
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_color(rgb(crate::theme::Theme::success()))
                            .child(format!("+{}", added)),
                    )
                    .child(
                        div()
                            .text_color(rgb(crate::theme::Theme::danger()))
                            .child(format!("-{}", deleted)),
                    ),
            )
            .child(uniform_list(
                "diff-lines",
                lines.len(),
                cx.processor(move |_this, range, _window, _cx| {
                    let mut out = Vec::new();
                    for ix in range {
                        let Some((kind, text, old_no, new_no)) = lines
                            .get(ix)
                            .map(|(k, t, o, n): &(DiffLineKind, String, Option<usize>, Option<usize>)| {
                                (*k, t.clone(), *o, *n)
                            })
                        else {
                            continue;
                        };
                        let (bg, fg, prefix) = match kind {
                            DiffLineKind::Add => (
                                crate::theme::Theme::success(),
                                crate::theme::Theme::success(),
                                "+",
                            ),
                            DiffLineKind::Delete => (
                                crate::theme::Theme::danger(),
                                crate::theme::Theme::danger(),
                                "-",
                            ),
                            DiffLineKind::HunkMeta => (
                                crate::theme::Theme::bg_panel(),
                                crate::theme::Theme::accent(),
                                "@",
                            ),
                            DiffLineKind::Header => (
                                crate::theme::Theme::bg_panel(),
                                crate::theme::Theme::fg_dim(),
                                " ",
                            ),
                            DiffLineKind::Context => (
                                crate::theme::Theme::bg(),
                                crate::theme::Theme::fg_dim(),
                                " ",
                            ),
                        };
                        let gutter = format!(
                            "{:>4} {:>4} ",
                            old_no.map(|n| n.to_string()).unwrap_or_default(),
                            new_no.map(|n| n.to_string()).unwrap_or_default(),
                        );
                        // 添加/删除行整行淡色底
                        let row_bg = match kind {
                            DiffLineKind::Add => gpui::hsla(140. / 360., 0.5, 0.25, 0.18),
                            DiffLineKind::Delete => gpui::hsla(0. / 360., 0.6, 0.45, 0.15),
                            _ => gpui::hsla(0., 0., 0., 0.),
                        };
                        out.push(
                            div()
                                .id(("dl", ix))
                                .h(px(20.))
                                .flex()
                                .items_center()
                                .pl_2()
                                .bg(row_bg)
                                .font_family("Consolas")
                                .text_size(px(12.))
                                .child(
                                    div()
                                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                                        .child(gutter),
                                )
                                .child(
                                    div()
                                        .w(px(10.))
                                        .text_color(rgb(fg))
                                        .child(prefix),
                                )
                                .child(
                                    div()
                                        .text_color(rgb(fg))
                                        .overflow_hidden()
                                        .child(text),
                                ),
                        );
                        let _ = bg;
                    }
                    out
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1())
    }
}
