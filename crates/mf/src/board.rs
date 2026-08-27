use gpui::prelude::*;
use gpui::*;
use mf_vcs::git::Git;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::theme::Theme;

/// 车间卡片墙(P0-E5):主仓卡 + git worktree 卡。
/// 状态流转 todo → in-progress → in-review → completed;
/// comment 为检查点;泳道视图可拖拽跨列改状态。
pub struct Board {
    root: PathBuf,
    cards: Vec<WorkspaceCard>,
    swim: bool,
    /// 展开状态菜单的卡片下标
    menu_for: Option<usize>,
    editing_comment: Option<usize>,
    comment_input: String,
    new_wt_open: bool,
    new_wt_name: String,
    status_msg: SharedString,
    focus_handle: FocusHandle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceCard {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    /// todo | in-progress | in-review | completed
    pub status: String,
    pub comment: String,
    pub unread: bool,
    pub last_activity: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct BoardFile {
    cards: Vec<WorkspaceCard>,
}

const STATUS_ORDER: [&str; 4] = ["todo", "in-progress", "in-review", "completed"];
const STATUS_CN: [(&str, &str); 4] = [
    ("todo", "待办"),
    ("in-progress", "进行中"),
    ("in-review", "审阅中"),
    ("completed", "已完成"),
];

fn status_cn(s: &str) -> &'static str {
    STATUS_CN.iter().find(|(k, _)| *k == s).map(|(_, v)| *v).unwrap_or("待办")
}
fn status_color(s: &str) -> u32 {
    match s {
        "in-progress" => Theme::accent(),
        "in-review" => Theme::warning(),
        "completed" => Theme::success(),
        _ => Theme::fg_faint(),
    }
}

/// 泳道拖拽载荷(卡片名)
pub struct CardDrag(pub String);

impl Board {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            root: root.clone(),
            cards: Vec::new(),
            swim: false,
            menu_for: None,
            editing_comment: None,
            comment_input: String::new(),
            new_wt_open: false,
            new_wt_name: String::new(),
            status_msg: "就绪".into(),
            focus_handle: cx.focus_handle(),
        };
        this.reconcile();
        this
    }

    fn store_path(&self) -> PathBuf {
        self.root.join(".mf-agent").join("workspaces.json")
    }

    fn save(&self) {
        let file = BoardFile { cards: self.cards.iter().filter(|c| !c.name.is_empty()).cloned().collect() };
        let _ = std::fs::create_dir_all(self.root.join(".mf-agent"));
        if let Ok(text) = serde_json::to_string_pretty(&file) {
            let _ = std::fs::write(self.store_path(), text);
        }
    }

    /// 主仓卡 + git worktree 对账:新 worktree 补卡,已删的移除
    pub fn reconcile(&mut self) {
        let git = match Git::open(&self.root) {
            Ok(g) => g,
            Err(_) => {
                if self.cards.is_empty() {
                    self.cards.push(main_card(&self.root));
                }
                return;
            }
        };
        let branch = git.branch().unwrap_or_default();
        let mut cards: Vec<WorkspaceCard> = vec![main_card(&self.root)];
        cards[0].branch = branch;
        let wts = git.worktree_list().unwrap_or_default();
        for (name, path) in wts {
            if name.is_empty() {
                continue;
            }
            let existing = self.cards.iter().find(|c| c.name == name).cloned();
            cards.push(existing.unwrap_or(WorkspaceCard {
                name: name.clone(),
                branch: name,
                path,
                status: "todo".into(),
                comment: String::new(),
                unread: false,
                last_activity: now_secs(),
            }));
        }
        self.cards = cards;
        self.save();
    }

    fn set_status(&mut self, idx: usize, status: &str, cx: &mut Context<Self>) {
        if let Some(c) = self.cards.get_mut(idx) {
            c.status = status.to_string();
            c.last_activity = now_secs();
        }
        self.menu_for = None;
        self.save();
        cx.notify();
    }

    fn open_worktree_dir(&self, idx: usize) {
        if let Some(c) = self.cards.get(idx) {
            let _ = std::process::Command::new("explorer")
                .arg(&c.path)
                .spawn();
        }
    }

    fn create_worktree(&mut self, cx: &mut Context<Self>) {
        let name = self.new_wt_name.trim().to_string();
        if name.is_empty() {
            self.status_msg = "名称不能为空".into();
            cx.notify();
            return;
        }
        self.status_msg = format!("创建 worktree「{name}」…").into();
        self.new_wt_open = false;
        self.new_wt_name.clear();
        cx.notify();
        let root = self.root.clone();
        let name2 = name.clone();
        cx.spawn(async move |this, cx| {
            let created = cx.background_executor().spawn(async move {
                Git::open(&root)
                    .and_then(|g| g.worktree_create(&name2))
                    .map(|p| p.to_string_lossy().into_owned())
            });
            let result = created.await;
            this.update(cx, |b: &mut Board, cx| {
                match result {
                    Ok(p) => {
                        b.status_msg = format!("已创建:{p}").into();
                        b.reconcile();
                    }
                    Err(e) => b.status_msg = format!("创建失败:{e:#}").into(),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn remove_worktree(&mut self, idx: usize, cx: &mut Context<Self>) {
        let name = match self.cards.get(idx) {
            Some(c) if !c.name.is_empty() => c.name.clone(),
            _ => return,
        };
        self.status_msg = format!("删除 worktree「{name}」…").into();
        cx.notify();
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let nm = name.clone();
            let res = cx.background_executor().spawn(async move {
                Git::open(&root).and_then(|g| g.worktree_remove(&nm))
            });
            let result = res.await;
            this.update(cx, |b: &mut Board, cx| {
                match result {
                    Ok(()) => {
                        b.status_msg = format!("已删除 {name}").into();
                        b.reconcile();
                    }
                    Err(e) => b.status_msg = format!("删除失败:{e:#}").into(),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------- 渲染 ----------

    fn card_head(&self, idx: usize, cx: &Context<Board>) -> impl IntoElement {
        let card = &self.cards[idx];
        let unread = card.unread;
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .w(px(7.))
                    .h(px(7.))
                    .rounded_full()
                    .when(unread, |d| d.bg(rgb(Theme::accent()))),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(Theme::fg()))
                    .flex_1()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(display_name(&card.name).to_string()),
            )
            .child(
                div()
                    .id(ElementId::Name(format!("bd-badge-{idx}").into()))
                    .px_1p5()
                    .rounded_full()
                    .cursor_pointer()
                    .text_size(px(9.5))
                    .text_color(rgb(Theme::bg()))
                    .bg(rgb(status_color(&card.status)))
                    .child(status_cn(&card.status))
                    .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, _w, cx| {
                        b.menu_for = if b.menu_for == Some(idx) { None } else { Some(idx) };
                        cx.notify();
                    })),
            )
    }

    fn render_card(&self, idx: usize, cx: &Context<Board>) -> impl IntoElement {
        let card = &self.cards[idx];
        let is_menu = self.menu_for == Some(idx);
        let is_edit = self.editing_comment == Some(idx);
        let mut d = div()
            .id(ElementId::Name(format!("bd-card-{idx}").into()))
            .bg(rgb(Theme::bg_elevated()))
            .border_1()
            .border_color(rgb(if self.menu_for == Some(idx) { Theme::accent() } else { Theme::border() }))
            .rounded_sm()
            .p_2()
            .mb_1p5()
            .cursor_pointer()
            .hover(|h| h.border_color(rgb(Theme::accent_dim())))
            .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, _w, cx| {
                if let Some(c) = b.cards.get_mut(idx) {
                    if c.unread {
                        c.unread = false;
                    }
                }
                b.menu_for = None;
                cx.notify();
            }))
            .child(self.card_head(idx, cx))
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgb(Theme::fg_faint()))
                    .pt_1()
                    .child(format!("⎇ {} · {}", card.branch, rel_time(card.last_activity))),
            )
            .when(!card.comment.is_empty() && !is_edit, |d| {
                d.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(rgb(Theme::fg_dim()))
                        .pt_0p5()
                        .child(format!("💬 {}", card.comment)),
                )
            })
            .when(is_edit, |d| {
                d.child(self.render_comment_input(cx))
            })
            .when(is_menu, |d| {
                // 行内展开的状态菜单(四态) + 操作行
                d.child(
                    div().pt_1p5().flex().flex_wrap().gap_1()
                        .children(STATUS_ORDER.iter().map(|s| {
                            div()
                                .id(ElementId::Name(format!("bd-st-{idx}-{s}").into()))
                                .px_1p5()
                                .py_0p5()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(Theme::border()))
                                .text_size(px(10.))
                                .cursor_pointer()
                                .hover(|h| h.border_color(rgb(Theme::accent())))
                                .text_color(rgb(status_color(s)))
                                .child(status_cn(s))
                                .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, _w, cx| {
                                    b.set_status(idx, s, cx);
                                }))
                        }))
                        .child(div().flex_1())
                        .child(
                            div()
                                .id(ElementId::Name(format!("bd-cmt-{idx}").into()))
                                .px_1p5().py_0p5().rounded_sm().border_1()
                                .border_color(rgb(Theme::border()))
                                .text_size(px(10.)).text_color(rgb(Theme::fg_dim()))
                                .cursor_pointer()
                                .child("✎ 检查点")
                                .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, _w, cx| {
                                    b.editing_comment = Some(idx);
                                    b.comment_input = b.cards.get(idx).map(|c| c.comment.clone()).unwrap_or_default();
                                    b.menu_for = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id(ElementId::Name(format!("bd-dir-{idx}").into()))
                                .px_1p5().py_0p5().rounded_sm().border_1()
                                .border_color(rgb(Theme::border()))
                                .text_size(px(10.)).text_color(rgb(Theme::fg_dim()))
                                .cursor_pointer()
                                .child("📂 目录")
                                .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, _w, cx| {
                                    b.open_worktree_dir(idx);
                                    b.menu_for = None;
                                    cx.notify();
                                })),
                        )
                        .when(!card.name.is_empty(), |d| {
                            d.child(
                                div()
                                    .id(ElementId::Name(format!("bd-rm-{idx}").into()))
                                    .px_1p5().py_0p5().rounded_sm().border_1()
                                    .border_color(rgb(Theme::border()))
                                    .text_size(px(10.)).text_color(rgb(Theme::danger()))
                                    .cursor_pointer()
                                    .child("🗑 删除")
                                    .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, _w, cx| {
                                        b.menu_for = None;
                                        b.remove_worktree(idx, cx);
                                    })),
                            )
                        }),
                )
            });
        d
    }

    fn render_comment_input(&self, cx: &Context<Board>) -> impl IntoElement {
        div()
            .id("bd-cmt-input")
            .mt_1()
            .px_1p5()
            .py_1()
            .border_1()
            .border_color(rgb(Theme::accent_dim()))
            .rounded_sm()
            .text_size(px(10.5))
            .text_color(rgb(Theme::fg()))
            .child(if self.comment_input.is_empty() {
                "输入检查点 comment,Enter 保存".to_string()
            } else {
                self.comment_input.clone()
            })
            .on_key_down(cx.listener(|b: &mut Board, e: &KeyDownEvent, _w, cx| {
                if let Some(ch) = e.keystroke.key_char.clone() {
                    b.comment_input.push_str(&ch);
                    cx.notify();
                } else if e.keystroke.key == "backspace" {
                    b.comment_input.pop();
                    cx.notify();
                } else if e.keystroke.key == "enter" {
                    if let Some(idx) = b.editing_comment.take() {
                        if let Some(c) = b.cards.get_mut(idx) {
                            c.comment = b.comment_input.clone();
                            c.last_activity = now_secs();
                        }
                        b.save();
                    }
                    b.comment_input.clear();
                    cx.notify();
                } else if e.keystroke.key == "escape" {
                    b.editing_comment = None;
                    b.comment_input.clear();
                    cx.notify();
                }
            }))
    }

    fn render_new_wt(&self, cx: &Context<Board>) -> impl IntoElement {
        div()
            .id("bd-new-wt")
            .mb_1p5()
            .p_2()
            .bg(rgb(Theme::bg_elevated()))
            .border_1()
            .border_color(rgb(Theme::accent_dim()))
            .rounded_sm()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_size(px(10.5)).text_color(rgb(Theme::fg_dim())).child("新 worktree 名称(基于当前 HEAD 建分支):"))
            .child(
                div()
                    .id("bd-new-wt-in")
                    .px_1p5()
                    .py_1()
                    .border_1()
                    .border_color(rgb(Theme::accent()))
                    .rounded_sm()
                    .text_size(px(11.5))
                    .text_color(rgb(Theme::fg()))
                    .child(if self.new_wt_name.is_empty() { "如 fix-login".to_string() } else { self.new_wt_name.clone() })
                    .on_key_down(cx.listener(|b: &mut Board, e: &KeyDownEvent, _w, cx| {
                        if let Some(ch) = e.keystroke.key_char.clone() {
                            b.new_wt_name.push_str(&ch);
                            cx.notify();
                        } else if e.keystroke.key == "backspace" {
                            b.new_wt_name.pop();
                            cx.notify();
                        } else if e.keystroke.key == "enter" {
                            b.create_worktree(cx);
                        } else if e.keystroke.key == "escape" {
                            b.new_wt_open = false;
                            b.new_wt_name.clear();
                            cx.notify();
                        }
                    })),
            )
    }

    fn render_swim_column(&self, status: &str, idx_gen: &dyn Fn() -> Vec<usize>, cx: &Context<Board>) -> impl IntoElement {
        let col_idx: Vec<usize> = idx_gen();
        let status = status.to_string();
        let status_for_drop = status.clone();
        div()
            .id(ElementId::Name(format!("bd-col-{status}").into()))
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .p_1()
            .when(status == "in-progress", |d| d.bg(rgb(Theme::bg_panel())))
            .rounded_sm()
            .on_drop::<CardDrag>(cx.listener(move |b: &mut Board, drag: &CardDrag, _w, cx| {
                if let Some(i) = b.cards.iter().position(|c| c.name == drag.0) {
                    b.set_status(i, &status_for_drop, cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_1()
                    .pb_1()
                    .text_size(px(10.))
                    .text_color(rgb(status_color(&status)))
                    .child(format!("{} {}", status_cn(&status), col_idx.len())),
            )
            .children(col_idx.iter().map(|&i| {
                let name = self.cards[i].name.clone();
                let card = &self.cards[i];
                div()
                    .id(ElementId::Name(format!("bd-swim-{i}").into()))
                    .bg(rgb(Theme::bg_elevated()))
                    .border_1()
                    .border_color(rgb(Theme::border()))
                    .rounded_sm()
                    .p_1p5()
                    .mb_1()
                    .cursor_grab()
                    .on_drag(
                        CardDrag(name.clone()),
                        move |card_drag: &CardDrag, _pt, _w, cx| {
                            cx.new(|_| DragGhost(card_drag.0.clone().into()))
                        },
                    )
                    .child(div().text_size(px(11.)).text_color(rgb(Theme::fg())).text_ellipsis().whitespace_nowrap().overflow_hidden().child(display_name(&card.name).to_string()))
                    .child(div().text_size(px(9.5)).text_color(rgb(Theme::fg_faint())).child(format!("⎇ {}", card.branch)))
            }))
    }
}

fn main_card(root: &PathBuf) -> WorkspaceCard {
    WorkspaceCard {
        name: String::new(), // 空 name = 主仓卡
        path: root.clone(),
        branch: String::new(),
        status: "in-progress".into(),
        comment: String::new(),
        unread: false,
        last_activity: now_secs(),
    }
}

fn display_name(name: &str) -> &str {
    if name.is_empty() { "(主仓库)" } else { name }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn rel_time(secs: u64) -> String {
    let now = now_secs();
    let d = now.saturating_sub(secs);
    if d < 60 {
        "刚刚".into()
    } else if d < 3600 {
        format!("{} 分钟前", d / 60)
    } else if d < 86400 {
        format!("{} 小时前", d / 3600)
    } else {
        format!("{} 天前", d / 86400)
    }
}

impl Render for Board {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_git = Git::is_repo(&self.root);
        div()
            .id("board-root")
            .key_context("Board")
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|b: &mut Board, e: &KeyDownEvent, _w, cx| {
                if e.keystroke.key == "escape" {
                    b.menu_for = None;
                    b.editing_comment = None;
                    b.new_wt_open = false;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .h(px(30.))
                    .border_b_1()
                    .border_color(rgb(Theme::border()))
                    .child(div().text_size(px(11.)).text_color(rgb(Theme::fg_dim())).child(format!("车间 · {} 张卡", self.cards.len())))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("bd-toggle-view")
                            .px_1p5().rounded_sm().cursor_pointer().text_size(px(10.5))
                            .text_color(rgb(Theme::fg_dim()))
                            .hover(|h| h.bg(rgb(Theme::bg_hover())))
                            .child(if self.swim { "☰ 列表" } else { "▤ 泳道" })
                            .on_click(cx.listener(|b: &mut Board, _: &ClickEvent, _w, cx| {
                                b.swim = !b.swim;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("bd-refresh")
                            .px_1p5().rounded_sm().cursor_pointer().text_size(px(10.5))
                            .text_color(rgb(Theme::fg_dim()))
                            .hover(|h| h.bg(rgb(Theme::bg_hover())))
                            .child("⟳")
                            .on_click(cx.listener(|b: &mut Board, _: &ClickEvent, _w, cx| {
                                b.reconcile();
                                b.status_msg = "已与 git worktree 对账".into();
                                cx.notify();
                            })),
                    )
                    .when(has_git, |d| {
                        d.child(
                            div()
                                .id("bd-new")
                                .px_1p5().rounded_sm().cursor_pointer().text_size(px(10.5))
                                .text_color(rgb(Theme::accent()))
                                .hover(|h| h.bg(rgb(Theme::bg_hover())))
                                .child("＋")
                                .on_click(cx.listener(|b: &mut Board, _: &ClickEvent, _w, cx| {
                                    b.new_wt_open = !b.new_wt_open;
                                    b.new_wt_name.clear();
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .when(self.new_wt_open, |d| {
                d.child(
                    div().p_1p5().child(self.render_new_wt(cx)),
                )
            })
            .child(if self.swim {
                // 泳道:四列
                let cols: Vec<(&str, Vec<usize>)> = STATUS_ORDER
                    .iter()
                    .map(|s| {
                        (
                            *s,
                            self.cards.iter().enumerate().filter(|(_, c)| c.status == *s).map(|(i, _)| i).collect(),
                        )
                    })
                    .collect();
                div()
                    .id("bd-swim-wrap")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap_1()
                    .p_1p5()
                    .children({
                        let mut out: Vec<AnyElement> = Vec::new();
                        for (s, idxs) in &cols {
                            let list = idxs.clone();
                            let st = s.to_string();
                            let idx_gen = move || list.clone();
                            out.push(self.render_swim_column(&st, &idx_gen, cx).into_any_element());
                        }
                        out
                    })
                    .into_any_element()
            } else {
                // 列表
                div()
                    .id("bd-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_1p5()
                    .children((0..self.cards.len()).map(|i| self.render_card(i, cx)))
                    .into_any_element()
            })
            .child(
                div()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .px_2()
                    .border_t_1()
                    .border_color(rgb(Theme::border()))
                    .text_size(px(10.))
                    .text_color(rgb(Theme::fg_faint()))
                    .child(self.status_msg.clone()),
            )
    }
}

/// 泳道拖拽幽灵视图
struct DragGhost(SharedString);
impl Render for DragGhost {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(Theme::accent_dim()))
            .text_size(px(10.5))
            .child(self.0.clone())
    }
}
