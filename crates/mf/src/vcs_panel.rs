use gpui::prelude::*;
use gpui::*;
use mf_plugins::vcs_provider::VcsEnvironment;
use mf_vcs::git::Git;
use mf_vcs::p4::{Change, OpenedFile, P4};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const HISTORY_PAGE_SIZE: usize = 20;
const HISTORY_FILES_PAGE_SIZE: usize = 100;

#[derive(Clone, Copy, Debug)]
enum HistoryLoad {
    Hidden,
    First,
    Next { p4_skip: usize, git_skip: usize },
}

fn take_history_page<T>(mut rows: Vec<T>) -> (Vec<T>, bool) {
    let has_more = rows.len() > HISTORY_PAGE_SIZE;
    rows.truncate(HISTORY_PAGE_SIZE);
    (rows, has_more)
}

fn history_page_bounds(total: usize, page: usize) -> std::ops::Range<usize> {
    let start = page.saturating_mul(HISTORY_PAGE_SIZE).min(total);
    let end = (start + HISTORY_PAGE_SIZE).min(total);
    start..end
}

fn history_file_page_bounds(total: usize, page: usize) -> std::ops::Range<usize> {
    let start = page.saturating_mul(HISTORY_FILES_PAGE_SIZE).min(total);
    let end = (start + HISTORY_FILES_PAGE_SIZE).min(total);
    start..end
}

fn project_is_in_p4_client(root: &std::path::Path, client_root: &str) -> bool {
    fn components(path: &str) -> Vec<String> {
        path.replace('/', "\\")
            .split('\\')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_lowercase)
            .collect()
    }

    let root = components(&root.to_string_lossy());
    let client = components(client_root);
    !client.is_empty() && root.starts_with(&client)
}

/// 异步刷新闸门。刷新进行中再次请求时，必须在当前请求结束后重放一次。
#[derive(Debug, Default)]
struct RefreshGate {
    loading: bool,
    pending: bool,
}

#[derive(Clone, Debug)]
struct HistoryFileView {
    action: String,
    path: String,
    old_path: Option<String>,
}

#[derive(Clone, Debug)]
enum HistoryDetailsState {
    Loading,
    Loaded(Vec<HistoryFileView>),
    Error(String),
}

#[derive(Clone, Debug)]
enum HistoryCommitTarget {
    P4(i64),
    Git(String),
}

impl HistoryCommitTarget {
    fn key(&self) -> String {
        match self {
            Self::P4(change) => format!("p4:{change}"),
            Self::Git(oid) => format!("git:{oid}"),
        }
    }
}

impl RefreshGate {
    fn request(&mut self) -> bool {
        if self.loading {
            self.pending = true;
            return false;
        }
        self.loading = true;
        true
    }

    fn finish(&mut self) -> bool {
        self.loading = false;
        std::mem::take(&mut self.pending)
    }

    fn loading(&self) -> bool {
        self.loading
    }
}

/// 版本控制面板:SourceTree 风格(左侧变更列表 + 文件 + 提交/搁置/还原 + 历史)
pub struct VcsPanel {
    kind: VcsKind,
    root: PathBuf,
    environment: VcsEnvironment,
    p4_info: Option<mf_vcs::p4::P4Info>,
    opened: Vec<OpenedFile>,
    pending: Vec<Change>,
    history: Vec<Change>,
    history_has_more: bool,
    history_page: usize,
    expanded_history: Option<String>,
    history_details: HashMap<String, HistoryDetailsState>,
    history_file_pages: HashMap<String, usize>,
    git_status: Vec<mf_vcs::git::GitFileEntry>,
    git_log: Vec<mf_vcs::git::GitLogEntry>,
    git_branch: String,
    selected: HashSet<String>,
    submit_desc: String,
    refresh_gate: RefreshGate,
    error: Option<String>,
    notice: Option<String>,
    show_history: bool,
    on_open_diff: Option<Box<dyn Fn(String, PathBuf, &mut Window, &mut App)>>,
    desc_focus: FocusHandle,
}

#[derive(Clone, Copy, PartialEq)]
pub enum VcsKind {
    P4,
    Git,
}

#[derive(Clone)]
struct LoadedData {
    p4_info: Option<mf_vcs::p4::P4Info>,
    opened: Vec<OpenedFile>,
    pending: Vec<Change>,
    history: Vec<Change>,
    history_has_more: bool,
    git_status: Vec<mf_vcs::git::GitFileEntry>,
    git_log: Vec<mf_vcs::git::GitLogEntry>,
    git_branch: String,
}

impl VcsPanel {
    pub fn new(root: PathBuf, environment: VcsEnvironment, cx: &mut Context<Self>) -> Self {
        let mut panel = Self {
            kind: VcsKind::P4,
            root,
            environment,
            p4_info: None,
            opened: Vec::new(),
            pending: Vec::new(),
            history: Vec::new(),
            history_has_more: false,
            history_page: 0,
            expanded_history: None,
            history_details: HashMap::new(),
            history_file_pages: HashMap::new(),
            git_status: Vec::new(),
            git_log: Vec::new(),
            git_branch: String::new(),
            selected: HashSet::new(),
            submit_desc: String::new(),
            desc_focus: cx.focus_handle(),
            refresh_gate: RefreshGate::default(),
            error: None,
            notice: None,
            show_history: false,
            on_open_diff: None,
        };
        panel.refresh(cx);
        panel
    }

    pub fn set_environment(&mut self, environment: VcsEnvironment, cx: &mut Context<Self>) {
        self.environment = environment;
        self.expanded_history = None;
        self.history_details.clear();
        self.history_file_pages.clear();
        self.refresh(cx);
    }

    pub fn set_on_open_diff(
        &mut self,
        cb: impl Fn(String, PathBuf, &mut Window, &mut App) + 'static,
    ) {
        self.on_open_diff = Some(Box::new(cb));
    }

    pub fn client_label(&self) -> Option<String> {
        self.p4_info
            .as_ref()
            .map(|i| format!("{} @ {}", i.client_name, i.server_name))
    }

    pub fn branch_label(&self) -> Option<String> {
        if self.kind == VcsKind::Git && !self.git_branch.is_empty() {
            Some(format!(" {}", self.git_branch))
        } else {
            None
        }
    }

    pub fn change_count(&self) -> usize {
        match self.kind {
            VcsKind::P4 => self.opened.len(),
            VcsKind::Git => self.git_status.len(),
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let history = if self.show_history {
            HistoryLoad::First
        } else {
            HistoryLoad::Hidden
        };
        self.start_refresh(history, false, cx);
    }

    fn load_more_history(&mut self, cx: &mut Context<Self>) {
        if self.refresh_gate.loading() || !self.history_has_more {
            return;
        }
        self.start_refresh(
            HistoryLoad::Next {
                p4_skip: self.history.len(),
                git_skip: self.git_log.len(),
            },
            true,
            cx,
        );
    }

    fn start_refresh(
        &mut self,
        history_load: HistoryLoad,
        append_history: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.refresh_gate.request() {
            return;
        }
        self.error = None;
        let root = self.root.clone();
        let environment = self.environment.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { load_vcs(&root, &environment, history_load) })
                .await;
            this.update(cx, |p, cx| {
                let refresh_again = p.refresh_gate.finish();
                match result {
                    Ok(data) => {
                        p.kind = if data.p4_info.is_some() {
                            VcsKind::P4
                        } else if !data.git_status.is_empty() || !data.git_branch.is_empty() {
                            VcsKind::Git
                        } else {
                            p.kind
                        };
                        p.p4_info = data.p4_info;
                        p.opened = data.opened;
                        p.pending = data.pending;
                        if append_history {
                            p.history.extend(data.history);
                            p.git_log.extend(data.git_log);
                            let count = match p.kind {
                                VcsKind::P4 => p.history.len(),
                                VcsKind::Git => p.git_log.len(),
                            };
                            p.history_page = count.saturating_sub(1) / HISTORY_PAGE_SIZE;
                        } else {
                            p.history = data.history;
                            p.git_log = data.git_log;
                            p.history_page = 0;
                        }
                        p.expanded_history = None;
                        p.history_has_more = data.history_has_more;
                        p.git_status = data.git_status;
                        p.git_branch = data.git_branch;
                        // 清掉已不存在的选择
                        p.selected.retain(|s| {
                            p.opened.iter().any(|f| &f.depot_file == s)
                                || p.git_status.iter().any(|g| g.path.to_string_lossy() == *s)
                        });
                    }
                    Err(e) => p.error = Some(e.to_string()),
                }
                if refresh_again {
                    p.refresh(cx);
                } else {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn run_op(
        &mut self,
        label: &str,
        op: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let label = label.to_string();
        self.notice = Some(format!("{}…", label));
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { op() }).await;
            this.update(cx, |p, cx| {
                match result {
                    Ok(out) => {
                        p.notice = Some(format!(
                            "{}完成 · {}",
                            label,
                            summarize_operation_output(&out)
                        ));
                    }
                    Err(e) => p.error = Some(format!("{} 失败: {}", label, e)),
                }
                p.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn p4(&self) -> Option<P4> {
        self.environment.p4(&self.root)
    }

    fn selected_local_paths(&self) -> Vec<PathBuf> {
        match self.kind {
            VcsKind::P4 => self
                .opened
                .iter()
                .filter(|f| self.selected.contains(&f.depot_file))
                .map(|f| f.local_path())
                .collect(),
            VcsKind::Git => self
                .git_status
                .iter()
                .filter(|g| {
                    self.selected
                        .contains(&g.path.to_string_lossy().into_owned())
                })
                .map(|g| self.root.join(&g.path))
                .collect(),
        }
    }

    fn act_submit(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.submit_desc.trim().is_empty() {
            self.error = Some("请填写提交描述".into());
            cx.notify();
            return;
        }
        let files = self.selected_local_paths();
        let desc = self.submit_desc.clone();
        self.submit_desc.clear();
        match self.kind {
            VcsKind::P4 => {
                let Some(p4) = self.p4() else {
                    self.error = Some("Perforce 插件实例已关闭".into());
                    cx.notify();
                    return;
                };
                self.run_op("提交", move || p4.submit(&desc, &files), cx);
            }
            VcsKind::Git => {
                let path = self.root.clone();
                let sel: Vec<PathBuf> = self
                    .git_status
                    .iter()
                    .filter(|g| {
                        self.selected
                            .contains(&g.path.to_string_lossy().into_owned())
                    })
                    .map(|g| g.path.clone())
                    .collect();
                let desc2 = desc.clone();
                self.run_op(
                    "提交",
                    move || {
                        let git = Git::open(&path)?;
                        // 未暂存文件先全部暂存(选中集)
                        for rel in &sel {
                            git.stage(std::slice::from_ref(rel))?;
                        }
                        git.commit(&desc2)
                    },
                    cx,
                );
            }
        }
    }

    fn act_revert(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let files = self.selected_local_paths();
        if files.is_empty() {
            self.error = Some("先勾选要还原的文件".into());
            cx.notify();
            return;
        }
        match self.kind {
            VcsKind::P4 => {
                let Some(p4) = self.p4() else {
                    self.error = Some("Perforce 插件实例已关闭".into());
                    cx.notify();
                    return;
                };
                self.run_op("还原", move || p4.revert(&files), cx);
            }
            VcsKind::Git => {
                // git 还原 = checkout HEAD;MVP 用命令行 git 以简化
                let root = self.root.clone();
                let git_cli = self.environment.git_cli(&root);
                let rels: Vec<String> = self
                    .git_status
                    .iter()
                    .filter(|g| {
                        self.selected
                            .contains(&g.path.to_string_lossy().into_owned())
                    })
                    .map(|g| g.path.to_string_lossy().into_owned())
                    .collect();
                self.run_op(
                    "还原",
                    move || {
                        let git_cli =
                            git_cli.ok_or_else(|| anyhow::anyhow!("Git 插件实例已关闭"))?;
                        for rel in &rels {
                            git_cli.checkout(rel)?;
                        }
                        Ok("已还原".into())
                    },
                    cx,
                );
            }
        }
    }

    fn act_shelve(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        // 搁置需要编号变更列表:先创建再 reopen 再 shelve
        let files = self.selected_local_paths();
        let desc = if self.submit_desc.trim().is_empty() {
            "MonkeyFence 搁置".to_string()
        } else {
            self.submit_desc.clone()
        };
        let Some(p4) = self.p4() else {
            self.error = Some("Perforce 插件实例已关闭".into());
            cx.notify();
            return;
        };
        self.run_op(
            "搁置",
            move || {
                let cl = p4.new_changelist(&desc)?;
                if !files.is_empty() {
                    p4.reopen(&cl.to_string(), &files)?;
                }
                p4.shelve(cl)?;
                Ok(format!("已搁置到 CL {}", cl))
            },
            cx,
        );
    }

    fn act_sync(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(p4) = self.p4() else {
            self.error = Some("Perforce 插件实例已关闭".into());
            cx.notify();
            return;
        };
        self.run_op("同步", move || p4.sync(None), cx);
    }

    fn act_stage(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let path = self.root.clone();
        let sel: Vec<PathBuf> = self
            .git_status
            .iter()
            .filter(|g| {
                self.selected
                    .contains(&g.path.to_string_lossy().into_owned())
            })
            .map(|g| g.path.clone())
            .collect();
        if sel.is_empty() {
            self.error = Some("先勾选文件".into());
            cx.notify();
            return;
        }
        self.run_op(
            "暂存",
            move || {
                let git = Git::open(&path)?;
                git.stage(&sel)?;
                Ok(format!("已暂存 {} 个文件", sel.len()))
            },
            cx,
        );
    }

    fn act_unstage(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let path = self.root.clone();
        let sel: Vec<PathBuf> = self
            .git_status
            .iter()
            .filter(|g| {
                self.selected
                    .contains(&g.path.to_string_lossy().into_owned())
            })
            .map(|g| g.path.clone())
            .collect();
        self.run_op(
            "取消暂存",
            move || {
                let git = Git::open(&path)?;
                git.unstage(&sel)?;
                Ok(format!("已取消暂存 {} 个文件", sel.len()))
            },
            cx,
        );
    }

    fn open_file_diff(&mut self, idx: usize, window: &mut Window, cx: &mut App) {
        let Some(cb) = &self.on_open_diff else { return };
        match self.kind {
            VcsKind::P4 => {
                if let Some(f) = self.opened.get(idx).cloned() {
                    let title = f.file_name();
                    let local = f.local_path();
                    (cb)(title, local, window, cx);
                }
            }
            VcsKind::Git => {
                if let Some(g) = self.git_status.get(idx).cloned() {
                    let title = g
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let local = self.root.join(&g.path);
                    (cb)(title, local, window, cx);
                }
            }
        }
    }

    fn toggle_history(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.show_history = !self.show_history;
        if self.show_history {
            self.refresh(cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_history_commit(&mut self, target: HistoryCommitTarget, cx: &mut Context<Self>) {
        let key = target.key();
        if self.expanded_history.as_deref() == Some(&key) {
            self.expanded_history = None;
            cx.notify();
            return;
        }
        self.expanded_history = Some(key.clone());
        self.history_file_pages.entry(key.clone()).or_insert(0);
        if matches!(
            self.history_details.get(&key),
            Some(HistoryDetailsState::Loading | HistoryDetailsState::Loaded(_))
        ) {
            cx.notify();
            return;
        }
        self.history_details
            .insert(key.clone(), HistoryDetailsState::Loading);
        let root = self.root.clone();
        let environment = self.environment.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_history_details(&root, &environment, target) })
                .await;
            this.update(cx, |panel, cx| {
                panel.history_details.insert(
                    key,
                    match result {
                        Ok(files) => HistoryDetailsState::Loaded(files),
                        Err(error) => HistoryDetailsState::Error(format!("{error:#}")),
                    },
                );
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_history_commit(
        &self,
        target: HistoryCommitTarget,
        revision: String,
        author: String,
        summary: String,
        cx: &Context<Self>,
    ) -> AnyElement {
        let key = target.key();
        let expanded = self.expanded_history.as_deref() == Some(&key);
        let details_label = match self.history_details.get(&key) {
            Some(HistoryDetailsState::Loading) => "读取文件…".to_string(),
            Some(HistoryDetailsState::Loaded(files)) => format!("{} 个文件", files.len()),
            Some(HistoryDetailsState::Error(_)) => "读取失败".to_string(),
            None => "查看文件".to_string(),
        };
        let details = expanded.then(|| self.render_history_details(&key, cx));
        let mut commit = div()
            .id(ElementId::Name(format!("history-commit-{key}").into()))
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .child(
                div()
                    .id(ElementId::Name(format!("history-toggle-{key}").into()))
                    .min_h(px(30.))
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|row| row.bg(rgb(crate::theme::Theme::bg_hover())))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.toggle_history_commit(target.clone(), cx)
                    }))
                    .child(
                        div()
                            .w(px(12.))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(if expanded { "▾" } else { "▸" }),
                    )
                    .child(
                        div()
                            .w(px(74.))
                            .text_size(px(10.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(crate::theme::Theme::accent()))
                            .child(revision),
                    )
                    .child(
                        div()
                            .w(px(86.))
                            .text_size(px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .overflow_hidden()
                            .child(author),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(10.))
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .overflow_hidden()
                            .child(summary),
                    )
                    .child(
                        div()
                            .text_size(px(8.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(details_label),
                    ),
            );
        if let Some(details) = details {
            commit = commit.child(details);
        }
        commit.into_any_element()
    }

    fn previous_history_file_page(&mut self, key: &str, cx: &mut Context<Self>) {
        let page = self.history_file_pages.entry(key.to_string()).or_insert(0);
        *page = page.saturating_sub(1);
        cx.notify();
    }

    fn next_history_file_page(&mut self, key: &str, cx: &mut Context<Self>) {
        let total = match self.history_details.get(key) {
            Some(HistoryDetailsState::Loaded(files)) => files.len(),
            _ => 0,
        };
        let pages = (total + HISTORY_FILES_PAGE_SIZE - 1) / HISTORY_FILES_PAGE_SIZE;
        let page = self.history_file_pages.entry(key.to_string()).or_insert(0);
        if *page + 1 < pages {
            *page += 1;
        }
        cx.notify();
    }

    fn render_history_details(&self, key: &str, cx: &Context<Self>) -> AnyElement {
        let mut body = div()
            .id(ElementId::Name(format!("history-details-{key}").into()))
            .ml_4()
            .border_l_2()
            .border_color(rgb(crate::theme::Theme::accent_dim()))
            .bg(rgb(crate::theme::Theme::bg_elevated()))
            .flex()
            .flex_col();
        match self.history_details.get(key) {
            Some(HistoryDetailsState::Loading) => {
                body = body.child(history_detail_message("正在读取提交文件列表…", false));
            }
            Some(HistoryDetailsState::Error(error)) => {
                body = body.child(history_detail_message(
                    &format!("读取失败：{error}（收起后重新展开可重试）"),
                    true,
                ));
            }
            Some(HistoryDetailsState::Loaded(files)) if files.is_empty() => {
                body = body.child(history_detail_message("该提交没有文件变化", false));
            }
            Some(HistoryDetailsState::Loaded(files)) => {
                let page = self.history_file_pages.get(key).copied().unwrap_or(0);
                let bounds = history_file_page_bounds(files.len(), page);
                body = body.child(
                    div()
                        .min_h(px(24.))
                        .px_2()
                        .flex()
                        .items_center()
                        .text_size(px(9.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child(format!(
                            "文件变更 {}–{} / {}",
                            bounds.start + 1,
                            bounds.end,
                            files.len()
                        )),
                );
                for file in &files[bounds.clone()] {
                    let path = match &file.old_path {
                        Some(old) => format!("{old}  →  {}", file.path),
                        None => file.path.clone(),
                    };
                    body = body.child(
                        div()
                            .min_h(px(24.))
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .border_b_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .child(
                                div()
                                    .w(px(20.))
                                    .text_size(px(9.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(history_action_color(&file.action)))
                                    .child(file.action.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(9.5))
                                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                                    .overflow_hidden()
                                    .child(path),
                            ),
                    );
                }
                if files.len() > HISTORY_FILES_PAGE_SIZE {
                    let pages =
                        (files.len() + HISTORY_FILES_PAGE_SIZE - 1) / HISTORY_FILES_PAGE_SIZE;
                    let previous_key = key.to_string();
                    let next_key = key.to_string();
                    let mut footer = div()
                        .min_h(px(30.))
                        .px_2()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .text_size(px(9.))
                        .text_color(rgb(crate::theme::Theme::fg_faint()));
                    footer = footer
                        .child(if page > 0 {
                            tool_btn(
                                "上一页",
                                cx.listener(move |panel, _, _, cx| {
                                    panel.previous_history_file_page(&previous_key, cx)
                                }),
                            )
                            .into_any_element()
                        } else {
                            tool_btn_disabled("上一页").into_any_element()
                        })
                        .child(format!("文件第 {} / {} 页", page + 1, pages))
                        .child(if page + 1 < pages {
                            tool_btn(
                                "下一页",
                                cx.listener(move |panel, _, _, cx| {
                                    panel.next_history_file_page(&next_key, cx)
                                }),
                            )
                            .into_any_element()
                        } else {
                            tool_btn_disabled("下一页").into_any_element()
                        });
                    body = body.child(footer);
                }
            }
            None => {}
        }
        body.into_any_element()
    }

    fn previous_history_page(&mut self, cx: &mut Context<Self>) {
        self.history_page = self.history_page.saturating_sub(1);
        self.expanded_history = None;
        cx.notify();
    }

    fn next_history_page(&mut self, cx: &mut Context<Self>) {
        let count = match self.kind {
            VcsKind::P4 => self.history.len(),
            VcsKind::Git => self.git_log.len(),
        };
        let loaded_pages = (count + HISTORY_PAGE_SIZE - 1) / HISTORY_PAGE_SIZE;
        if self.history_page + 1 < loaded_pages {
            self.history_page += 1;
            self.expanded_history = None;
            cx.notify();
        } else if self.history_has_more {
            self.load_more_history(cx);
        }
    }

    fn render_history_footer(&self, count: usize, cx: &Context<Self>) -> AnyElement {
        let loading = self.refresh_gate.loading();
        let mut row = div()
            .min_h(px(30.))
            .px_2()
            .py_1()
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .text_size(px(10.))
            .text_color(rgb(crate::theme::Theme::fg_faint()));
        if loading {
            row = row.child("正在获取历史…");
        } else if count == 0 {
            row = row.child("没有可见的提交记录");
        } else {
            let loaded_pages = (count + HISTORY_PAGE_SIZE - 1) / HISTORY_PAGE_SIZE;
            let has_next = self.history_page + 1 < loaded_pages || self.history_has_more;
            row = row
                .child(if self.history_page > 0 {
                    tool_btn(
                        "上一页",
                        cx.listener(|panel: &mut VcsPanel, _, _, cx| {
                            panel.previous_history_page(cx)
                        }),
                    )
                    .into_any_element()
                } else {
                    tool_btn_disabled("上一页").into_any_element()
                })
                .child(format!(
                    "第 {} 页 · 每页 {} 条",
                    self.history_page + 1,
                    HISTORY_PAGE_SIZE
                ))
                .child(if has_next {
                    tool_btn(
                        "下一页",
                        cx.listener(|panel: &mut VcsPanel, _, _, cx| panel.next_history_page(cx)),
                    )
                    .into_any_element()
                } else {
                    tool_btn_disabled("下一页").into_any_element()
                });
        }
        row.into_any_element()
    }

    fn on_desc_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(chars) = event.keystroke.key_char.clone() {
            let printable: String = chars.chars().filter(|c| !c.is_control()).collect();
            if !printable.is_empty() {
                self.submit_desc.push_str(&printable);
                cx.notify();
            }
        } else if event.keystroke.key == "backspace" {
            self.submit_desc.pop();
            cx.notify();
        }
    }
}

fn load_vcs(
    root: &PathBuf,
    environment: &VcsEnvironment,
    history_load: HistoryLoad,
) -> anyhow::Result<LoadedData> {
    let mut data = LoadedData {
        p4_info: None,
        opened: Vec::new(),
        pending: Vec::new(),
        history: Vec::new(),
        history_has_more: false,
        git_status: Vec::new(),
        git_log: Vec::new(),
        git_branch: String::new(),
    };
    // 优先探测 P4(工作区在 client root 下且 p4 可用)
    let mut p4_connection_error = None;
    if let Some(p4) = environment.p4(root) {
        match p4.info() {
            Ok(info) => {
                let in_client = project_is_in_p4_client(root, &info.client_root);
                if in_client {
                    data.p4_info = Some(info.clone());
                    data.opened = p4.opened().unwrap_or_default();
                    data.pending = p4.pending_changes(20).unwrap_or_default();
                    if !matches!(history_load, HistoryLoad::Hidden) {
                        let skip = match history_load {
                            HistoryLoad::Next { p4_skip, .. } => p4_skip,
                            _ => 0,
                        };
                        let page = p4.submitted_history_page(
                            &info.client_stream,
                            skip,
                            (HISTORY_PAGE_SIZE + 1) as u32,
                        )?;
                        let (page, has_more) = take_history_page(page);
                        data.history_has_more = has_more;
                        data.history = page;
                    }
                    return Ok(data);
                }
            }
            Err(error) => p4_connection_error = Some(error),
        }
    }
    // 回退 Git
    if environment.git.is_some() && Git::is_repo(root) {
        if let Ok(git) = Git::open(root) {
            data.git_branch = git.branch().unwrap_or_default();
            data.git_status = git.status().unwrap_or_default();
            if !matches!(history_load, HistoryLoad::Hidden) {
                let skip = match history_load {
                    HistoryLoad::Next { git_skip, .. } => git_skip,
                    _ => 0,
                };
                let (page, has_more) =
                    take_history_page(git.log_page(skip, HISTORY_PAGE_SIZE + 1)?);
                data.history_has_more = has_more;
                data.git_log = page;
            }
            return Ok(data);
        }
    }
    if let Some(error) = p4_connection_error {
        anyhow::bail!("Perforce 连接失败: {error:#}");
    }
    Ok(data)
}

fn load_history_details(
    root: &PathBuf,
    environment: &VcsEnvironment,
    target: HistoryCommitTarget,
) -> anyhow::Result<Vec<HistoryFileView>> {
    match target {
        HistoryCommitTarget::P4(change) => {
            let p4 = environment
                .p4(root)
                .ok_or_else(|| anyhow::anyhow!("Perforce 插件实例已关闭"))?;
            let mut files: Vec<HistoryFileView> = p4
                .describe(change)?
                .into_iter()
                .map(|file| HistoryFileView {
                    action: p4_history_action(&file.action).into(),
                    path: file.depot_file,
                    old_path: None,
                })
                .collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(files)
        }
        HistoryCommitTarget::Git(oid) => {
            let git = Git::open(root)?;
            Ok(git
                .commit_files(&oid)?
                .into_iter()
                .map(|file| HistoryFileView {
                    action: file.action.code().into(),
                    path: file.path.to_string_lossy().into_owned(),
                    old_path: file
                        .old_path
                        .map(|path| path.to_string_lossy().into_owned()),
                })
                .collect())
        }
    }
}

fn p4_history_action(action: &str) -> &'static str {
    match action {
        "add" | "branch" => "A",
        "delete" => "D",
        "move/add" | "move/delete" => "R",
        "edit" | "integrate" => "M",
        _ => "?",
    }
}

fn history_action_color(action: &str) -> u32 {
    match action {
        "A" | "C" => crate::theme::Theme::success(),
        "D" => crate::theme::Theme::danger(),
        "R" => crate::theme::Theme::accent(),
        _ => crate::theme::Theme::warning(),
    }
}

fn history_detail_message(text: &str, error: bool) -> Div {
    div()
        .px_2()
        .py_2()
        .text_size(px(9.5))
        .text_color(rgb(if error {
            crate::theme::Theme::danger()
        } else {
            crate::theme::Theme::fg_faint()
        }))
        .child(text.to_string())
}

fn summarize_operation_output(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return "无额外输出".into();
    }
    let first: String = lines[0].chars().take(96).collect();
    let ellipsis = if lines[0].chars().count() > 96 {
        "…"
    } else {
        ""
    };
    if lines.len() == 1 {
        format!("{first}{ellipsis}")
    } else {
        format!("{} 条结果 · {first}{ellipsis}", lines.len())
    }
}

fn action_color(action: &str) -> u32 {
    match action {
        "add" | "branch" => crate::theme::Theme::success(),
        "delete" | "move/delete" => crate::theme::Theme::danger(),
        _ => crate::theme::Theme::warning(),
    }
}

impl Focusable for VcsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.desc_focus.clone()
    }
}

impl Render for VcsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut col = div()
            .id("vcs-panel")
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(12.));

        // 工具栏
        col = col.child(
            div()
                .h(px(32.))
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .border_b_1()
                .border_color(rgb(crate::theme::Theme::border()))
                .child(tool_btn(
                    "刷新",
                    cx.listener(|p: &mut VcsPanel, _, _, cx| p.refresh(cx)),
                ))
                .child(tool_btn("同步", cx.listener(Self::act_sync)))
                .child(div().flex_1())
                .child(tool_btn(
                    if self.show_history {
                        "历史 ✓"
                    } else {
                        "历史"
                    },
                    cx.listener(Self::toggle_history),
                )),
        );

        // 状态行
        if let Some(err) = &self.error {
            col = col.child(banner(crate::theme::Theme::danger(), format!("⚠ {}", err)));
        }
        if let Some(n) = &self.notice {
            col = col.child(banner(crate::theme::Theme::fg_dim(), n.clone()));
        }
        if self.refresh_gate.loading() {
            col = col.child(banner(crate::theme::Theme::accent(), "加载中…".into()));
        }

        // 文件列表区
        let mut list = div()
            .id("vcs-file-list")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_y_scroll();
        match self.kind {
            VcsKind::P4 => {
                list = list.child(section_header(format!("待提交变更({})", self.opened.len())));
                let default_files: Vec<(usize, OpenedFile)> = self
                    .opened
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.change == "default")
                    .map(|(i, f)| (i, f.clone()))
                    .collect();
                for (idx, f) in default_files {
                    list = list.child(self.render_p4_row(idx, &f, cx));
                }
                for ch in self.pending.iter().take(10) {
                    list = list.child(
                        div()
                            .id(("cl", ch.id as u64))
                            .pl_2()
                            .pr_2()
                            .pt_1()
                            .pb_1()
                            .border_b_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_color(rgb(crate::theme::Theme::accent()))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(format!("CL {}", ch.id)),
                                    )
                                    .child(
                                        div()
                                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                                            .overflow_hidden()
                                            .child(ch.short_desc()),
                                    )
                                    .when(ch.shelved, |d| {
                                        d.child(
                                            div()
                                                .text_color(rgb(crate::theme::Theme::warning()))
                                                .child("[搁置]"),
                                        )
                                    }),
                            ),
                    );
                }
                if self.show_history {
                    list = list.child(section_header(format!(
                        "提交历史 · 第 {} 页（已加载 {}）",
                        self.history_page + 1,
                        self.history.len(),
                    )));
                    let bounds = history_page_bounds(self.history.len(), self.history_page);
                    for h in &self.history[bounds] {
                        list = list.child(self.render_history_commit(
                            HistoryCommitTarget::P4(h.id),
                            format!("#{}", h.id),
                            h.user.clone(),
                            h.short_desc(),
                            cx,
                        ));
                    }
                    list = list.child(self.render_history_footer(self.history.len(), cx));
                }
            }
            VcsKind::Git => {
                list = list.child(section_header(format!("变更({})", self.git_status.len())));
                let entries: Vec<(usize, mf_vcs::git::GitFileEntry)> = self
                    .git_status
                    .iter()
                    .enumerate()
                    .map(|(i, g)| (i, g.clone()))
                    .collect();
                for (idx, g) in entries {
                    let path_str = g.path.to_string_lossy().into_owned();
                    let checked = self.selected.contains(&path_str);
                    let name = g
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let dir = g
                        .path
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let staged = g.status.is_staged();
                    let label = g.status.label();
                    let color = if staged {
                        crate::theme::Theme::success()
                    } else {
                        match g.status {
                            mf_vcs::git::GitStatus::New => crate::theme::Theme::success(),
                            mf_vcs::git::GitStatus::Deleted => crate::theme::Theme::danger(),
                            _ => crate::theme::Theme::warning(),
                        }
                    };
                    list = list.child(
                        div()
                            .id(("gf", idx))
                            .h(px(26.))
                            .pl_2()
                            .pr_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .cursor_pointer()
                            .on_click({
                                let path_str = path_str.clone();
                                cx.listener(
                                    move |p: &mut VcsPanel, e: &gpui::ClickEvent, window, cx| {
                                        if e.click_count() == 2 {
                                            p.open_file_diff(idx, window, cx);
                                            return;
                                        }
                                        if p.selected.contains(&path_str) {
                                            p.selected.remove(&path_str);
                                        } else {
                                            p.selected.insert(path_str.clone());
                                        }
                                        cx.notify();
                                    },
                                )
                            })
                            .child(div().w(px(12.)).child(if checked { "☑" } else { "☐" }))
                            .child(div().w(px(28.)).text_color(rgb(color)).child(label))
                            .child(div().text_color(rgb(crate::theme::Theme::fg())).child(name))
                            .child(
                                div()
                                    .ml_auto()
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .overflow_hidden()
                                    .child(dir),
                            ),
                    );
                }
                if self.show_history {
                    list = list.child(section_header(format!(
                        "提交历史 · 第 {} 页（已加载 {}）",
                        self.history_page + 1,
                        self.git_log.len(),
                    )));
                    let bounds = history_page_bounds(self.git_log.len(), self.history_page);
                    for h in &self.git_log[bounds] {
                        list = list.child(self.render_history_commit(
                            HistoryCommitTarget::Git(h.full_id.clone()),
                            h.id.clone(),
                            h.author.clone(),
                            h.summary.clone(),
                            cx,
                        ));
                    }
                    list = list.child(self.render_history_footer(self.git_log.len(), cx));
                }
            }
        }
        col = col.child(list);

        // 提交区(SourceTree 式:描述 + 动作)
        col.child(
            div()
                .border_t_1()
                .border_color(rgb(crate::theme::Theme::border()))
                .p_2()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .id("submit-desc")
                        .h(px(52.))
                        .p_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(crate::theme::Theme::border()))
                        .bg(rgb(crate::theme::Theme::bg()))
                        .track_focus(&self.desc_focus)
                        .when(self.desc_focus.is_focused(window), |d| {
                            d.border_color(rgb(crate::theme::Theme::accent()))
                        })
                        .on_key_down(cx.listener(Self::on_desc_key))
                        .text_size(px(12.))
                        .text_color(rgb(crate::theme::Theme::fg()))
                        .child(if self.submit_desc.is_empty() {
                            SharedString::from("提交描述(点击后输入)")
                        } else {
                            SharedString::from(self.submit_desc.clone())
                        }),
                )
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(tool_btn("提交", cx.listener(Self::act_submit)))
                        .child(tool_btn("还原", cx.listener(Self::act_revert)))
                        .when(self.kind == VcsKind::P4, |d| {
                            d.child(tool_btn("搁置", cx.listener(Self::act_shelve)))
                        })
                        .when(self.kind == VcsKind::Git, |d| {
                            d.child(tool_btn("暂存", cx.listener(Self::act_stage)))
                                .child(tool_btn("取消暂存", cx.listener(Self::act_unstage)))
                        }),
                ),
        )
    }
}

impl VcsPanel {
    fn render_p4_row(&self, idx: usize, f: &OpenedFile, cx: &Context<Self>) -> impl IntoElement {
        let checked = self.selected.contains(&f.depot_file);
        let name = f.file_name();
        let dir = f
            .client_file
            .rsplit('/')
            .nth(1)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let color = action_color(&f.action);
        let action = f.action.clone();
        let depot = f.depot_file.clone();
        div()
            .id(("pf", idx))
            .h(px(26.))
            .pl_2()
            .pr_2()
            .flex()
            .items_center()
            .gap_2()
            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
            .cursor_pointer()
            .on_click({
                let depot = depot.clone();
                cx.listener(move |p: &mut VcsPanel, _, _, cx| {
                    if p.selected.contains(&depot) {
                        p.selected.remove(&depot);
                    } else {
                        p.selected.insert(depot.clone());
                    }
                    cx.notify();
                })
            })
            .on_click({
                // 双击打开 diff
                cx.listener(move |p: &mut VcsPanel, e: &gpui::ClickEvent, window, cx| {
                    if e.click_count() == 2 {
                        p.open_file_diff(idx, window, cx);
                    }
                })
            })
            .child(div().w(px(12.)).child(if checked { "☑" } else { "☐" }))
            .child(div().w(px(34.)).text_color(rgb(color)).child(action))
            .child(div().text_color(rgb(crate::theme::Theme::fg())).child(name))
            .child(
                div()
                    .ml_auto()
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .overflow_hidden()
                    .child(dir),
            )
    }
}

fn section_header(text: String) -> impl IntoElement {
    div()
        .h(px(24.))
        .flex()
        .items_center()
        .px_2()
        .bg(rgb(crate::theme::Theme::bg_elevated()))
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(crate::theme::Theme::fg_dim()))
        .child(text)
}

fn banner(color: u32, text: String) -> impl IntoElement {
    div()
        .max_h(px(34.))
        .px_2()
        .py_1()
        .overflow_hidden()
        .text_size(px(11.))
        .text_color(rgb(color))
        .bg(rgb(crate::theme::Theme::bg_elevated()))
        .child(text)
}

fn tool_btn(
    label: &str,
    listener: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("tool-btn-{}", label).into()))
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(rgb(crate::theme::Theme::bg_elevated()))
        .border_1()
        .border_color(rgb(crate::theme::Theme::border()))
        .text_size(px(11.))
        .text_color(rgb(crate::theme::Theme::fg_dim()))
        .hover(|d| {
            d.bg(rgb(crate::theme::Theme::bg_hover()))
                .text_color(rgb(crate::theme::Theme::fg()))
        })
        .cursor_pointer()
        .child(label.to_string())
        .on_click(move |e, window, cx| (listener)(e, window, cx))
}

fn tool_btn_disabled(label: &str) -> impl IntoElement {
    div()
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(crate::theme::Theme::border()))
        .text_size(px(11.))
        .text_color(rgb(crate::theme::Theme::fg_faint()))
        .child(label.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        history_file_page_bounds, history_page_bounds, p4_history_action, project_is_in_p4_client,
        summarize_operation_output, take_history_page, RefreshGate, HISTORY_FILES_PAGE_SIZE,
        HISTORY_PAGE_SIZE,
    };

    #[test]
    fn refresh_requested_while_loading_is_replayed() {
        let mut gate = RefreshGate::default();
        assert!(gate.request(), "第一次请求应立即开始");
        assert!(!gate.request(), "加载中不得并发启动第二次请求");
        assert!(
            gate.finish(),
            "加载中发生的第二次请求必须在当前请求结束后重放"
        );
    }

    #[test]
    fn history_pages_expose_twenty_rows_and_more_flag() {
        let (first, has_more) = take_history_page((0..HISTORY_PAGE_SIZE + 1).collect::<Vec<_>>());
        assert_eq!(first.len(), 20);
        assert!(has_more);

        let (last, has_more) = take_history_page((0..7).collect::<Vec<_>>());
        assert_eq!(last.len(), 7);
        assert!(!has_more);
        assert_eq!(history_page_bounds(45, 0), 0..20);
        assert_eq!(history_page_bounds(45, 1), 20..40);
        assert_eq!(history_page_bounds(45, 2), 40..45);
        assert_eq!(history_file_page_bounds(20_433, 0), 0..100);
        assert_eq!(history_file_page_bounds(20_433, 204), 20_400..20_433);
        assert_eq!(HISTORY_FILES_PAGE_SIZE, 100);
        assert_eq!(p4_history_action("add"), "A");
        assert_eq!(p4_history_action("edit"), "M");
        assert_eq!(p4_history_action("delete"), "D");
        assert_eq!(p4_history_action("move/add"), "R");
    }

    #[test]
    fn p4_client_root_accepts_mixed_windows_separators() {
        let project =
            std::path::Path::new(r"E:\Beyond_v2d0\hongjinmin_DM42.Beyond_Beyond_v2d0_project");
        assert!(project_is_in_p4_client(
            project,
            r"E:/Beyond_v2d0\hongjinmin_DM42.Beyond_Beyond_v2d0_project"
        ));
        assert!(!project_is_in_p4_client(
            std::path::Path::new(r"E:\Beyond_v2d0\project-other"),
            r"E:\Beyond_v2d0\project"
        ));
    }

    #[test]
    fn operation_notice_stays_single_line_and_bounded() {
        let long_path = format!("//Depot/{}#2 - deleted", "nested/".repeat(40));
        let summary =
            summarize_operation_output(&format!("{long_path}\n{long_path}\n{long_path}\n"));
        assert!(summary.starts_with("3 条结果 · "));
        assert!(!summary.contains('\n'));
        assert!(summary.chars().count() < 120);
    }
}
