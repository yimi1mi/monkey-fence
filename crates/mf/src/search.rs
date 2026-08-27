use gpui::prelude::*;
use gpui::*;
use std::path::{Path, PathBuf};

use crate::theme::Theme;

/// 项目搜索浮层(Ctrl+Shift+F,P0-E3):输入 + Aa/字/.* 开关 + 按文件分组结果,
/// 点击结果打开文件并跳行。扫描在后台线程(ignore 遍历)。
pub struct ProjectSearch {
    root: PathBuf,
    query: String,
    case_sensitive: bool,
    whole_word: bool,
    use_regex: bool,
    results: Vec<SearchGroup>,
    total_hits: usize,
    searching: bool,
    status: SharedString,
    embedded: bool,
    focus_handle: FocusHandle,
    on_open: Option<Box<dyn Fn(&Path, usize, &mut Window, &mut App)>>,
}

pub struct Dismissed;
impl EventEmitter<Dismissed> for ProjectSearch {}

impl Focusable for ProjectSearch {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Clone, Debug)]
pub struct SearchGroup {
    pub path: PathBuf,
    pub hits: Vec<Hit>,
}

#[derive(Clone, Debug)]
pub struct Hit {
    /// 1-based 行号
    pub row: usize,
    pub text: String,
    pub col: usize,
    pub len: usize,
}

const MAX_FILES: usize = 50;
const MAX_HITS_PER_FILE: usize = 12;

impl ProjectSearch {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            root,
            query: String::new(),
            case_sensitive: false,
            whole_word: false,
            use_regex: false,
            results: Vec::new(),
            total_hits: 0,
            searching: false,
            status: "输入搜索词,Enter 执行".into(),
            embedded: false,
            focus_handle: cx.focus_handle(),
            on_open: None,
        }
    }

    pub fn set_on_open(&mut self, cb: impl Fn(&Path, usize, &mut Window, &mut App) + 'static) {
        self.on_open = Some(Box::new(cb));
    }

    pub fn set_embedded(&mut self, embedded: bool) {
        self.embedded = embedded;
    }

    fn toggle(&mut self, which: u8, cx: &mut Context<Self>) {
        match which {
            b'a' => self.case_sensitive = !self.case_sensitive,
            b'w' => self.whole_word = !self.whole_word,
            b'r' => self.use_regex = !self.use_regex,
            _ => {}
        }
        cx.notify();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let q = self.query.clone();
        if q.trim().is_empty() {
            return;
        }
        self.searching = true;
        self.results.clear();
        self.total_hits = 0;
        self.status = "搜索中…".into();
        cx.notify();
        let root = self.root.clone();
        let case = self.case_sensitive;
        let word = self.whole_word;
        let regex = self.use_regex;
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move {
                scan_project(&root, &q, case, word, regex)
            });
            let (groups, total) = res.await;
            this.update(cx, |s, cx| {
                s.searching = false;
                s.results = groups;
                s.total_hits = total;
                s.status = format!("{total} 处匹配 · {} 个文件", s.results.len()).into();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

fn scan_project(root: &Path, query: &str, case: bool, word: bool, regex: bool) -> (Vec<SearchGroup>, usize) {
    let mut groups: Vec<SearchGroup> = Vec::new();
    let mut total = 0usize;
    let re = if regex {
        regex::RegexBuilder::new(query)
            .case_insensitive(!case)
            .build()
            .ok()
    } else {
        None
    };
    let needle = if case { query.to_string() } else { query.to_lowercase() };

    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(name.as_ref(), ".git" | "target" | ".mf-agent" | "node_modules" | ".worktrees")
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if groups.len() >= MAX_FILES {
            break;
        }
        let path = entry.path();
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "rs" | "toml" | "md" | "json" | "yml" | "yaml" | "txt" | "js" | "ts" | "py"
                        | "c" | "h" | "cpp" | "cs" | "go" | "java" | "lua" | "ts" | "css" | "html" | "sh" | "ps1"
                )
            });
        if !ext_ok {
            continue;
        }
        let Ok(meta) = std::fs::metadata(path) else { continue };
        if meta.len() > 2_000_000 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let mut hits = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if hits.len() >= MAX_HITS_PER_FILE {
                break;
            }
            let (col, len) = if let Some(re) = &re {
                re.find(line).map(|m| (m.start(), m.end() - m.start())).unwrap_or((0, 0))
            } else {
                let hay = if case { line.to_string() } else { line.to_lowercase() };
                hay.find(&needle).map(|c| {
                    let l = if word {
                        let is_w = |ch: char| ch.is_alphanumeric() || ch == '_';
                        let before_ok = c == 0 || !is_w(hay.as_bytes()[c - 1] as char);
                        let after = c + needle.len();
                        let after_ok = after >= hay.len() || !is_w(hay.as_bytes()[after] as char);
                        if before_ok && after_ok { needle.len() } else { 0 }
                    } else {
                        needle.len()
                    };
                    (c, l)
                }).unwrap_or((0, 0))
            };
            if len > 0 || (re.is_some() && col > 0) {
                hits.push(Hit { row: i + 1, text: line.trim().chars().take(120).collect(), col, len });
            }
        }
        if !hits.is_empty() {
            total += hits.len();
            groups.push(SearchGroup { path: path.to_path_buf(), hits });
        }
    }
    (groups, total)
}

impl Render for ProjectSearch {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.results.clone();
        let embedded = self.embedded;
        div()
            .id("project-search-root")
            .key_context("ProjectSearch")
            .size_full()
            .flex()
            .track_focus(&self.focus_handle)
            .when(!embedded, |d| {
                d.absolute()
                    .top_0()
                    .left_0()
                    .bg(gpui::black().opacity(0.4))
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
                        cx.emit(Dismissed);
                    }))
            })
            .when(embedded, |d| d.bg(rgb(Theme::bg_panel())))
            .child(
                div()
                    .id("ps-panel")
                    .flex()
                    .flex_col()
                    .bg(rgb(Theme::bg_panel()))
                    .when(!embedded, |d| {
                        d.mt(px(70.))
                            .w(px(680.))
                            .max_h(px(560.))
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(Theme::border()))
                            .shadow_lg()
                            .on_mouse_down(MouseButton::Left, |_, _, _| {})
                    })
                    .when(embedded, |d| d.size_full())
                    .child(
                        // 输入行 + 开关
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .p_2()
                            .border_b_1()
                            .border_color(rgb(Theme::border()))
                            .child(
                                div()
                                    .id("ps-input")
                                    .flex_1()
                                    .px_2()
                                    .py_1()
                                    .border_1()
                                    .border_color(rgb(Theme::accent_dim()))
                                    .rounded_sm()
                                    .text_size(px(13.))
                                    .text_color(rgb(Theme::fg()))
                                    .child(if self.query.is_empty() {
                                        "搜索…(Enter 执行)".to_string()
                                    } else {
                                        self.query.clone()
                                    })
                                    .on_key_down(cx.listener(|s, e: &KeyDownEvent, _w, cx| {
                                        if let Some(ch) = e.keystroke.key_char.clone() {
                                            s.query.push_str(&ch);
                                            cx.notify();
                                        } else if e.keystroke.key == "backspace" {
                                            s.query.pop();
                                            cx.notify();
                                        } else if e.keystroke.key == "enter" {
                                            s.run_search(cx);
                                        } else if e.keystroke.key == "escape" {
                                            cx.emit(Dismissed);
                                        }
                                    })),
                            )
                            .child(toggle_btn(b'a', "Aa", self.case_sensitive, cx))
                            .child(toggle_btn(b'w', "字", self.whole_word, cx))
                            .child(toggle_btn(b'r', ".*", self.use_regex, cx)),
                    )
                    .child(
                        div()
                            .id("ps-results")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_1()
                            .child(
                                div()
                                    .px_1()
                                    .pb_1()
                                    .text_size(px(10.5))
                                    .text_color(rgb(Theme::fg_faint()))
                                    .child(self.status.clone()),
                            )
                            .children(results.iter().map(|g| {
                                let fname = g
                                    .path
                                    .strip_prefix(&self.root)
                                    .unwrap_or(&g.path)
                                    .display()
                                    .to_string();
                                div()
                                    .id(ElementId::Name(format!("ps-file-{}", fname).into()))
                                    .mb_1()
                                    .child(
                                        div()
                                            .px_1()
                                            .py_0p5()
                                            .text_size(px(11.))
                                            .text_color(rgb(Theme::fg()))
                                            .child(format!("📄 {}  ({})", fname, g.hits.len())),
                                    )
                                    .children(g.hits.iter().map(|h| {
                                        let (pre, matched, post) = split_hit(&h.text, h.col, h.len);
                                        let path = g.path.clone();
                                        let row = h.row;
                                        div()
                                            .id(ElementId::Name(format!("ps-hit-{}-{}", fname, row).into()))
                                            .pl_3()
                                            .py_0p5()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .hover(|d| d.bg(rgb(Theme::bg_hover())))
                                            .on_click(cx.listener(move |s, _, w, cx| {
                                                if let Some(cb) = &s.on_open {
                                                    cb(&path, row, w, cx);
                                                }
                                                cx.emit(Dismissed);
                                            }))
                                            .child(
                                                div()
                                                    .flex()
                                                    .gap_2()
                                                    .text_size(px(11.))
                                                    .font_family("Consolas")
                                                    .child(div().w(px(30.)).text_color(rgb(Theme::fg_faint())).child(h.row.to_string()))
                                                    .child(
                                                        div().flex().text_color(rgb(Theme::fg_dim()))
                                                            .child(div().child(pre.clone()))
                                                            .child(div().bg(rgb(Theme::accent_dim())).text_color(rgb(Theme::fg())).child(matched.clone()))
                                                            .child(div().child(post.clone())),
                                                    ),
                                            )
                                    }))
                            })),
                    ),
            )
    }
}

/// 把命中行拆为 (前缀, 命中, 后缀)——命中列基于 trim 后文本按字符对齐
fn split_hit(text: &str, col: usize, len: usize) -> (String, String, String) {
    let chars: Vec<char> = text.chars().collect();
    let start = col.min(chars.len());
    let end = (col.saturating_add(len)).min(chars.len());
    (
        chars[..start].iter().collect(),
        chars[start..end].iter().collect(),
        chars[end..].iter().collect(),
    )
}

fn toggle_btn(which: u8, label: &str, on: bool, cx: &Context<ProjectSearch>) -> impl IntoElement {
    let label = label.to_string();
    div()
        .id(ElementId::Name(format!("ps-tg-{which}").into()))
        .w(px(26.))
        .h(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .cursor_pointer()
        .text_size(px(11.))
        .border_color(rgb(if on { Theme::accent() } else { Theme::border() }))
        .text_color(rgb(if on { Theme::accent() } else { Theme::fg_faint() }))
        .hover(|d| d.bg(rgb(Theme::bg_hover())))
        .child(label)
        .on_click(cx.listener(move |s, _, _, cx| {
            s.toggle(which, cx);
        }))
}
