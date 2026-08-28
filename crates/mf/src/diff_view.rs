use gpui::prelude::*;
use gpui::*;
use mf_vcs::diff::{parse_unified_diff, DiffLine, DiffLineKind, UnifiedDiff};

/// 统一 diff 视图:只读着色 + hunk 级审阅(保留/拒绝,Alt+Y/Z)。
/// 拒绝 = 从原 diff 文本截取该 hunk 构造 mini-patch,回调执行(git apply -R)。
pub struct DiffView {
    title: SharedString,
    diff: UnifiedDiff,
    /// 原始 diff 文本(mini-patch 重建用)
    diff_text: String,
    /// hunk 审阅状态:Some(true)=保留 Some(false)=拒绝 None=未决
    hunk_states: Vec<Option<bool>>,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
    on_reject: Option<Box<dyn Fn(String, &mut Window, &mut App)>>,
}

/// uniform_list 行视图:普通行 / hunk 操作条
enum Row {
    Line(usize),
    HunkBar(usize),
}

impl DiffView {
    pub fn new(title: impl Into<SharedString>, diff_text: &str, cx: &mut Context<Self>) -> Self {
        let diff = parse_unified_diff(diff_text);
        let n_hunks = diff
            .lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::HunkMeta)
            .count();
        Self {
            title: title.into(),
            diff,
            diff_text: diff_text.to_string(),
            hunk_states: vec![None; n_hunks],
            scroll_handle: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            on_reject: None,
        }
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_on_reject(&mut self, cb: impl Fn(String, &mut Window, &mut App) + 'static) {
        self.on_reject = Some(Box::new(cb));
    }

    fn has_hunks(&self) -> bool {
        !self.hunk_states.is_empty() && self.on_reject.is_some()
    }

    /// 行索引 → Row 视图(在 HunkMeta 前插操作条;已拒绝 hunk 的行过滤掉)
    fn build_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut hunk_i = 0usize;
        for (i, l) in self.diff.lines.iter().enumerate() {
            if l.kind == DiffLineKind::HunkMeta {
                if self.has_hunks() {
                    rows.push(Row::HunkBar(hunk_i));
                }
                let rejected = self.hunk_states.get(hunk_i) == Some(&Some(false));
                if !rejected {
                    rows.push(Row::Line(i));
                }
                hunk_i += 1;
            } else {
                let rejected = {
                    // 属于当前(最近一个)hunk 且该 hunk 已拒绝 → 跳过
                    let cur = hunk_i.checked_sub(1);
                    match cur {
                        Some(h) => self.hunk_states.get(h) == Some(&Some(false)),
                        None => false,
                    }
                };
                if !rejected {
                    rows.push(Row::Line(i));
                }
            }
        }
        rows
    }

    /// 从原 diff 文本重建第 hunk_i 个 hunk 的 mini-patch(含文件 header)
    fn mini_patch(&self, hunk_i: usize) -> Option<String> {
        let mut hunks = self.diff_text.split("\n@@");
        let header: Vec<&str> = hunks.next()?.lines().collect();
        if header.is_empty() {
            return None;
        }
        let target = hunks.nth(hunk_i)?;
        let mut out = header.join("\n");
        out.push_str("\n@@");
        out.push_str(target);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        Some(out)
    }

    fn verdict(&mut self, hunk_i: usize, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let patch = self.mini_patch(hunk_i);
        self.hunk_states[hunk_i] = Some(keep);
        cx.notify();
        if !keep {
            if let (Some(p), Some(cb)) = (patch, &self.on_reject) {
                cb(p, window, cx);
            }
        }
    }

    fn next_pending(&self) -> Option<usize> {
        self.hunk_states.iter().position(|s| s.is_none())
    }

    fn key_verdict(&mut self, keep: bool, all: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_hunks() {
            return;
        }
        if all {
            for i in 0..self.hunk_states.len() {
                if self.hunk_states[i] != Some(keep) {
                    self.verdict(i, keep, window, cx);
                }
            }
        } else if let Some(i) = self.next_pending() {
            self.verdict(i, keep, window, cx);
        }
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
        let rows = self.build_rows();
        let reviewing = self.has_hunks();
        let total_hunks = self.hunk_states.len();
        let pending = self.hunk_states.iter().filter(|s| s.is_none()).count();
        div()
            .id("diff-view")
            .key_context("DiffView")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::bg()))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, w, cx| {
                if e.keystroke.modifiers.alt {
                    let k = e.keystroke.key.as_str();
                    if k == "y" {
                        this.key_verdict(true, e.keystroke.modifiers.shift, w, cx);
                    } else if k == "z" {
                        this.key_verdict(false, e.keystroke.modifiers.shift, w, cx);
                    }
                }
            }))
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
                    )
                    .when(reviewing, |d| {
                        d.child(div().flex_1())
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(if pending > 0 {
                                        crate::theme::Theme::warning()
                                    } else {
                                        crate::theme::Theme::success()
                                    }))
                                    .child(format!("{} 个 hunk · {} 待审阅", total_hunks, pending)),
                            )
                            .child(
                                div()
                                    .id("dh-all-y")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::success()))
                                    .text_color(rgb(crate::theme::Theme::success()))
                                    .cursor_pointer()
                                    .text_size(px(10.5))
                                    .hover(|h| h.bg(rgb(crate::theme::Theme::bg_hover())))
                                    .child("✓ 全部保留")
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.key_verdict(true, true, w, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("dh-all-z")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::danger()))
                                    .text_color(rgb(crate::theme::Theme::danger()))
                                    .cursor_pointer()
                                    .text_size(px(10.5))
                                    .hover(|h| h.bg(rgb(crate::theme::Theme::bg_hover())))
                                    .child("✕ 全部拒绝")
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.key_verdict(false, true, w, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child("Alt+Y/Z"),
                            )
                    }),
            )
            .child(
                div()
                    .id("diff-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children({
                        let mut out: Vec<AnyElement> = Vec::new();
                        let max_rows = 3000usize;
                        for (ix, row) in rows.iter().enumerate().take(max_rows) {
                            match row {
                                Row::HunkBar(h) => {
                                    let state = self.hunk_states.get(*h).copied().flatten();
                                    let label = match state {
                                        Some(true) => "✓ 已保留",
                                        Some(false) => "✕ 已拒绝(工作区已还原该段)",
                                        None => "待审阅",
                                    };
                                    let color = match state {
                                        Some(true) => crate::theme::Theme::success(),
                                        Some(false) => crate::theme::Theme::danger(),
                                        None => crate::theme::Theme::warning(),
                                    };
                                    let hi = *h;
                                    out.push(
                                        div()
                                            .id(("dhb", ix))
                                            .h(px(24.))
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .pl_2()
                                            .pr_1()
                                            .border_b_1()
                                            .border_color(rgb(crate::theme::Theme::border()))
                                            .bg(rgb(crate::theme::Theme::bg_elevated()))
                                            .text_size(px(10.5))
                                            .child(div().text_color(rgb(color)).child(format!(
                                                "HUNK #{} · {}",
                                                hi + 1,
                                                label
                                            )))
                                            .child(div().flex_1())
                                            .when(reviewing && state.is_none(), |d| {
                                                d.child(
                                                    div()
                                                        .id(ElementId::Name(
                                                            format!("dhb-y-{}", hi).into(),
                                                        ))
                                                        .px_1p5()
                                                        .rounded_sm()
                                                        .border_1()
                                                        .border_color(rgb(
                                                            crate::theme::Theme::success(),
                                                        ))
                                                        .text_color(rgb(
                                                            crate::theme::Theme::success(),
                                                        ))
                                                        .cursor_pointer()
                                                        .child("✓ 保留")
                                                        .on_click(cx.listener(
                                                            move |this, _, w, cx| {
                                                                this.verdict(hi, true, w, cx);
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .id(ElementId::Name(
                                                            format!("dhb-z-{}", hi).into(),
                                                        ))
                                                        .px_1p5()
                                                        .rounded_sm()
                                                        .border_1()
                                                        .border_color(rgb(
                                                            crate::theme::Theme::danger(),
                                                        ))
                                                        .text_color(rgb(
                                                            crate::theme::Theme::danger(),
                                                        ))
                                                        .cursor_pointer()
                                                        .child("✕ 拒绝")
                                                        .on_click(cx.listener(
                                                            move |this, _, w, cx| {
                                                                this.verdict(hi, false, w, cx);
                                                            },
                                                        )),
                                                )
                                            })
                                            .into_any_element(),
                                    );
                                }
                                Row::Line(i) => {
                                    let Some(l) = lines.get(*i) else { continue };
                                    let (kind, text, old_no, new_no) = (l.0, l.1.clone(), l.2, l.3);
                                    let (fg, prefix) = match kind {
                                        DiffLineKind::Add => (crate::theme::Theme::success(), "+"),
                                        DiffLineKind::Delete => {
                                            (crate::theme::Theme::danger(), "-")
                                        }
                                        DiffLineKind::HunkMeta => {
                                            (crate::theme::Theme::accent(), "@")
                                        }
                                        DiffLineKind::Header => {
                                            (crate::theme::Theme::fg_dim(), " ")
                                        }
                                        DiffLineKind::Context => {
                                            (crate::theme::Theme::fg_dim(), " ")
                                        }
                                    };
                                    let gutter = format!(
                                        "{:>4} {:>4} ",
                                        old_no.map(|n| n.to_string()).unwrap_or_default(),
                                        new_no.map(|n| n.to_string()).unwrap_or_default(),
                                    );
                                    let row_bg = match kind {
                                        DiffLineKind::Add => {
                                            gpui::hsla(140. / 360., 0.5, 0.25, 0.18)
                                        }
                                        DiffLineKind::Delete => {
                                            gpui::hsla(0. / 360., 0.6, 0.45, 0.15)
                                        }
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
                                                    .text_color(
                                                        rgb(crate::theme::Theme::fg_faint()),
                                                    )
                                                    .child(gutter),
                                            )
                                            .child(
                                                div().w(px(10.)).text_color(rgb(fg)).child(prefix),
                                            )
                                            .child(
                                                div()
                                                    .text_color(rgb(fg))
                                                    .overflow_hidden()
                                                    .child(text),
                                            )
                                            .into_any_element(),
                                    );
                                }
                            }
                        }
                        if rows.len() > max_rows {
                            out.push(
                                div()
                                    .p_2()
                                    .text_size(px(11.))
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child(format!(
                                        "… 其余 {} 行未渲染(超大 diff)",
                                        rows.len() - max_rows
                                    ))
                                    .into_any_element(),
                            );
                        }
                        out
                    }),
            )
    }
}
