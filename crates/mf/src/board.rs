use gpui::prelude::*;
use gpui::*;
use mf_vcs::git::Git;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::theme::Theme;
use crate::work_items::{WorkItemPhase, WorkItemSeed, WorkItemStore};

/// 工作区列表:主仓工作区 + Git worktree 工作区。
/// 状态流转 todo → in-progress → in-review → completed;
/// comment 为检查点;泳道视图可拖拽跨列改状态。
pub struct Board {
    root: PathBuf,
    active_path: PathBuf,
    work_items: Arc<Mutex<WorkItemStore>>,
    cards: Vec<WorkspaceCard>,
    on_activate: Option<Box<dyn Fn(WorkspaceCard, &mut Window, &mut App)>>,
    can_remove: Option<Box<dyn Fn(&Path, &mut Window, &mut App) -> bool>>,
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

#[derive(Clone, Debug)]
pub struct WorkspaceCard {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    /// todo | in-progress | in-review | completed
    pub status: String,
    pub phase: WorkItemPhase,
    pub comment: String,
    pub unread: bool,
    pub last_activity: u64,
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

fn phase_cn(phase: WorkItemPhase) -> &'static str {
    match phase {
        WorkItemPhase::Draft => "草稿",
        WorkItemPhase::Running => "执行中",
        WorkItemPhase::NeedsInput => "需要输入",
        WorkItemPhase::Review => "待审阅",
        WorkItemPhase::ReadyToDeliver => "待交付",
        WorkItemPhase::Done => "已完成",
        WorkItemPhase::Failed => "失败",
    }
}

fn phase_color(phase: WorkItemPhase) -> u32 {
    match phase {
        WorkItemPhase::Draft => Theme::fg_faint(),
        WorkItemPhase::Running => Theme::accent(),
        WorkItemPhase::NeedsInput | WorkItemPhase::Review | WorkItemPhase::ReadyToDeliver => {
            Theme::warning()
        }
        WorkItemPhase::Done => Theme::success(),
        WorkItemPhase::Failed => Theme::danger(),
    }
}

/// 泳道拖拽载荷(卡片名)
pub struct CardDrag(pub String);

impl Board {
    pub fn new(
        root: PathBuf,
        work_items: Arc<Mutex<WorkItemStore>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            root: root.clone(),
            active_path: root.clone(),
            work_items,
            cards: Vec::new(),
            on_activate: None,
            can_remove: None,
            swim: false,
            menu_for: None,
            editing_comment: None,
            comment_input: String::new(),
            new_wt_open: false,
            new_wt_name: String::new(),
            status_msg: "就绪".into(),
            focus_handle: cx.focus_handle(),
        };
        this.sync_from_work_items();
        this.reconcile();
        this
    }

    fn sync_from_work_items(&mut self) {
        let store = self.work_items.lock();
        if let Some(active) = store.active() {
            self.active_path = active.workspace.clone();
        }
        self.cards = store
            .items()
            .iter()
            .map(|item| WorkspaceCard {
                name: if item.id == "main" { String::new() } else { item.title.clone() },
                path: item.workspace.clone(),
                branch: item.vcs_ref.clone(),
                status: item.phase.as_workspace_status().into(),
                phase: item.phase,
                comment: item.comment.clone(),
                unread: item.unread,
                last_activity: item.updated_at / 1000,
            })
            .collect();
    }

    pub fn unread_count(&self) -> usize {
        self.work_items
            .lock()
            .items()
            .iter()
            .filter(|item| item.unread)
            .count()
    }

    /// 注册工作区卡片激活回调。回调收到点击时卡片状态的稳定快照。
    pub fn set_on_activate(
        &mut self,
        cb: impl Fn(WorkspaceCard, &mut Window, &mut App) + 'static,
    ) {
        self.on_activate = Some(Box::new(cb));
    }

    pub fn set_can_remove(
        &mut self,
        callback: impl Fn(&Path, &mut Window, &mut App) -> bool + 'static,
    ) {
        self.can_remove = Some(Box::new(callback));
    }

    fn activate_card(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(card) = self.cards.get_mut(idx) else {
            return;
        };
        card.unread = false;
        let card = card.clone();
        self.menu_for = None;
        self.save_card(idx);
        if let Some(cb) = &self.on_activate {
            cb(card, window, cx);
        }
        cx.notify();
    }

    fn save_card(&self, idx: usize) {
        let Some(card) = self.cards.get(idx) else {
            return;
        };
        let mut store = self.work_items.lock();
        store.set_phase(&card.path, card.phase);
        store.update_comment(&card.path, card.comment.clone());
        store.set_unread(&card.path, card.unread);
        let _ = store.save();
    }

    /// 主仓卡 + git worktree 对账:新 worktree 补卡,已删的移除
    pub fn reconcile(&mut self) {
        let project_name = self
            .root
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "主工作区".into());
        let mut seeds = Vec::new();
        if let Ok(git) = Git::open(&self.root) {
            let branch = git.branch().unwrap_or_default();
            seeds.push(WorkItemSeed::new(project_name, &self.root, branch));
            let worktrees = match git.worktree_list() {
                Ok(worktrees) => worktrees,
                Err(error) => {
                    self.status_msg = format!("工作区对账失败: {error:#}").into();
                    return;
                }
            };
            seeds.extend(
                worktrees
                    .into_iter()
                    .filter(|(name, _)| !name.is_empty())
                    .map(|(name, path)| WorkItemSeed::new(name.clone(), path, name)),
            );
        } else {
            let p4 = mf_vcs::p4::P4::new(&self.root);
            let vcs_ref = p4
                .info()
                .map(|info| {
                    if info.client_stream.is_empty() {
                        info.client_name
                    } else {
                        info.client_stream
                    }
                })
                .unwrap_or_default();
            seeds.push(WorkItemSeed::new(project_name, &self.root, vcs_ref));
        }
        {
            let mut store = self.work_items.lock();
            store.reconcile_workspaces(seeds);
            let _ = store.save();
        }
        self.sync_from_work_items();
    }

    fn set_status(&mut self, idx: usize, status: &str, cx: &mut Context<Self>) {
        if self.cards.get(idx).is_some_and(|card| {
            matches!(card.phase, WorkItemPhase::Running | WorkItemPhase::NeedsInput)
        }) {
            self.status_msg = "执行中或等待输入的工作项不能手动改阶段".into();
            self.menu_for = None;
            cx.notify();
            return;
        }
        if let Some(c) = self.cards.get_mut(idx) {
            c.status = status.to_string();
            c.phase = WorkItemPhase::from_workspace_status(status);
            c.last_activity = now_secs();
        }
        self.menu_for = None;
        self.save_card(idx);
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

    fn remove_worktree(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (name, path) = match self.cards.get(idx) {
            Some(c) if !c.name.is_empty() => (c.name.clone(), c.path.clone()),
            _ => return,
        };
        if self
            .can_remove
            .as_ref()
            .is_some_and(|can_remove| !can_remove(&path, window, cx))
        {
            self.status_msg = "该工作区仍有未保存标签，不能删除".into();
            cx.notify();
            return;
        }
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
                    .bg(rgb(phase_color(card.phase)))
                    .child(phase_cn(card.phase))
                    .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, _w, cx| {
                        cx.stop_propagation();
                        b.menu_for = if b.menu_for == Some(idx) { None } else { Some(idx) };
                        cx.notify();
                    })),
            )
    }

    fn render_card(&self, idx: usize, cx: &Context<Board>) -> impl IntoElement {
        let card = &self.cards[idx];
        let is_menu = self.menu_for == Some(idx);
        let is_edit = self.editing_comment == Some(idx);
        let is_active = card.path == self.active_path;
        let mut d = div()
            .id(ElementId::Name(format!("bd-card-{idx}").into()))
            .bg(rgb(Theme::bg_elevated()))
            .border_1()
            .border_color(rgb(if is_menu || is_active { Theme::accent() } else { Theme::border() }))
            .rounded_sm()
            .p_2()
            .mb_1p5()
            .cursor_pointer()
            .hover(|h| h.border_color(rgb(Theme::accent_dim())))
            .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, window, cx| {
                b.activate_card(idx, window, cx);
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
                    div()
                        .id(ElementId::Name(format!("bd-menu-{idx}").into()))
                        .pt_1p5().flex().flex_wrap().gap_1()
                        .on_click(|_: &ClickEvent, _w, cx| cx.stop_propagation())
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
                                    cx.stop_propagation();
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
                                    cx.stop_propagation();
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
                                    cx.stop_propagation();
                                    b.open_worktree_dir(idx);
                                    b.menu_for = None;
                                    cx.notify();
                                })),
                        )
                        .when(!card.name.is_empty() && !is_active, |d| {
                            d.child(
                                div()
                                    .id(ElementId::Name(format!("bd-rm-{idx}").into()))
                                    .px_1p5().py_0p5().rounded_sm().border_1()
                                    .border_color(rgb(Theme::border()))
                                    .text_size(px(10.)).text_color(rgb(Theme::danger()))
                                    .cursor_pointer()
                                    .child("🗑 删除")
                                    .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, window, cx| {
                                        cx.stop_propagation();
                                        b.menu_for = None;
                                        b.remove_worktree(idx, window, cx);
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
            .on_click(|_: &ClickEvent, _w, cx| cx.stop_propagation())
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
                        b.save_card(idx);
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
                let is_active = card.path == self.active_path;
                div()
                    .id(ElementId::Name(format!("bd-swim-{i}").into()))
                    .bg(rgb(Theme::bg_elevated()))
                    .border_1()
                    .border_color(rgb(if is_active { Theme::accent() } else { Theme::border() }))
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
                    .on_click(cx.listener(move |b: &mut Board, _: &ClickEvent, window, cx| {
                        b.activate_card(i, window, cx);
                    }))
                    .child(div().text_size(px(11.)).text_color(rgb(Theme::fg())).text_ellipsis().whitespace_nowrap().overflow_hidden().child(display_name(&card.name).to_string()))
                    .child(div().text_size(px(9.5)).text_color(rgb(Theme::fg_faint())).child(format!("⎇ {}", card.branch)))
            }))
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
        self.sync_from_work_items();
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
                    .child(div().text_size(px(11.)).text_color(rgb(Theme::fg_dim())).child(format!("工作区 · {}", self.cards.len())))
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
