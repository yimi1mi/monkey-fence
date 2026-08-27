use gpui::prelude::*;
use gpui::*;
use mf_vcs::git::Git;
use mf_vcs::p4::{Change, OpenedFile, P4};
use std::collections::HashSet;
use std::path::PathBuf;

/// 版本控制面板:SourceTree 风格(左侧变更列表 + 文件 + 提交/搁置/还原 + 历史)
pub struct VcsPanel {
    kind: VcsKind,
    root: PathBuf,
    p4_info: Option<mf_vcs::p4::P4Info>,
    opened: Vec<OpenedFile>,
    pending: Vec<Change>,
    history: Vec<Change>,
    git_status: Vec<mf_vcs::git::GitFileEntry>,
    git_log: Vec<mf_vcs::git::GitLogEntry>,
    git_branch: String,
    selected: HashSet<String>,
    submit_desc: String,
    loading: bool,
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
    git_status: Vec<mf_vcs::git::GitFileEntry>,
    git_log: Vec<mf_vcs::git::GitLogEntry>,
    git_branch: String,
}

impl VcsPanel {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut panel = Self {
            kind: VcsKind::P4,
            root,
            p4_info: None,
            opened: Vec::new(),
            pending: Vec::new(),
            history: Vec::new(),
            git_status: Vec::new(),
            git_log: Vec::new(),
            git_branch: String::new(),
            selected: HashSet::new(),
            submit_desc: String::new(),
            desc_focus: cx.focus_handle(),
            loading: false,
            error: None,
            notice: None,
            show_history: false,
            on_open_diff: None,
        };
        panel.refresh(cx);
        panel
    }

    pub fn set_on_open_diff(&mut self, cb: impl Fn(String, PathBuf, &mut Window, &mut App) + 'static) {
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
        if self.loading {
            return;
        }
        self.loading = true;
        self.error = None;
        let root = self.root.clone();
        let show_history = self.show_history;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { load_vcs(&root, show_history) })
                .await;
            this.update(cx, |p, cx| {
                p.loading = false;
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
                        p.history = data.history;
                        p.git_status = data.git_status;
                        p.git_log = data.git_log;
                        p.git_branch = data.git_branch;
                        // 清掉已不存在的选择
                        p.selected.retain(|s| {
                            p.opened.iter().any(|f| &f.depot_file == s)
                                || p.git_status.iter().any(|g| g.path.to_string_lossy() == *s)
                        });
                    }
                    Err(e) => p.error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn run_op(&mut self, label: &str, op: impl FnOnce() -> anyhow::Result<String> + Send + 'static, cx: &mut Context<Self>) {
        let label = label.to_string();
        self.notice = Some(format!("{}…", label));
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { op() }).await;
            this.update(cx, |p, cx| {
                match result {
                    Ok(out) => {
                        let brief = out.lines().take(3).collect::<Vec<_>>().join(" | ");
                        p.notice = Some(format!("{} 完成: {}", label, brief));
                    }
                    Err(e) => p.error = Some(format!("{} 失败: {}", label, e)),
                }
                p.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn p4(&self) -> P4 {
        P4::new(&self.root)
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
                .filter(|g| self.selected.contains(&g.path.to_string_lossy().into_owned()))
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
                let p4 = self.p4();
                self.run_op("提交", move || p4.submit(&desc, &files), cx);
            }
            VcsKind::Git => {
                let path = self.root.clone();
                let sel: Vec<PathBuf> = self
                    .git_status
                    .iter()
                    .filter(|g| self.selected.contains(&g.path.to_string_lossy().into_owned()))
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
                let p4 = self.p4();
                self.run_op("还原", move || p4.revert(&files), cx);
            }
            VcsKind::Git => {
                // git 还原 = checkout HEAD;MVP 用命令行 git 以简化
                let root = self.root.clone();
                let rels: Vec<String> = self
                    .git_status
                    .iter()
                    .filter(|g| self.selected.contains(&g.path.to_string_lossy().into_owned()))
                    .map(|g| g.path.to_string_lossy().into_owned())
                    .collect();
                self.run_op(
                    "还原",
                    move || {
                        for rel in &rels {
                            let st = std::process::Command::new("git")
                                .args(["checkout", "--", rel])
                                .current_dir(&root)
                                .output()
                                .map_err(anyhow::Error::from)?;
                            if !st.status.success() {
                                anyhow::bail!(
                                    "git checkout {}: {}",
                                    rel,
                                    String::from_utf8_lossy(&st.stderr)
                                );
                            }
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
        let p4 = self.p4();
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
        let p4 = self.p4();
        self.run_op("同步", move || p4.sync(None), cx);
    }

    fn act_stage(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let path = self.root.clone();
        let sel: Vec<PathBuf> = self
            .git_status
            .iter()
            .filter(|g| self.selected.contains(&g.path.to_string_lossy().into_owned()))
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
            .filter(|g| self.selected.contains(&g.path.to_string_lossy().into_owned()))
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
        self.refresh(cx);
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

fn load_vcs(root: &PathBuf, with_history: bool) -> anyhow::Result<LoadedData> {
    let mut data = LoadedData {
        p4_info: None,
        opened: Vec::new(),
        pending: Vec::new(),
        history: Vec::new(),
        git_status: Vec::new(),
        git_log: Vec::new(),
        git_branch: String::new(),
    };
    // 优先探测 P4(工作区在 client root 下且 p4 可用)
    let p4 = P4::new(root);
    if let Ok(info) = p4.info() {
        let in_client = !info.client_root.is_empty()
            && root
                .to_string_lossy()
                .to_lowercase()
                .starts_with(&info.client_root.to_lowercase());
        if in_client {
            data.p4_info = Some(info.clone());
            data.opened = p4.opened().unwrap_or_default();
            data.pending = p4.pending_changes(20).unwrap_or_default();
            if with_history {
                data.history = p4
                    .submitted_history(&info.client_stream, 30)
                    .unwrap_or_default();
            }
            return Ok(data);
        }
    }
    // 回退 Git
    if Git::is_repo(root) {
        if let Ok(git) = Git::open(root) {
            data.git_branch = git.branch().unwrap_or_default();
            data.git_status = git.status().unwrap_or_default();
            data.git_log = git.log(30).unwrap_or_default();
        }
    }
    Ok(data)
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
                .child(tool_btn("刷新", cx.listener(|p: &mut VcsPanel, _, _, cx| p.refresh(cx))))
                .child(tool_btn(
                    "同步",
                    cx.listener(Self::act_sync),
                ))
                .child(
                    div().flex_1(),
                )
                .child(tool_btn(
                    if self.show_history { "历史 ✓" } else { "历史" },
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
        if self.loading {
            col = col.child(banner(crate::theme::Theme::accent(), "加载中…".into()));
        }

        // 文件列表区
        let mut list = div().flex_1().flex().flex_col().overflow_hidden();
        match self.kind {
            VcsKind::P4 => {
                list = list.child(section_header(format!(
                    "待提交变更({})",
                    self.opened.len()
                )));
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
                    list = list.child(section_header("提交历史".into()));
                    for h in self.history.iter().take(30) {
                        list = list.child(
                            div()
                                .id(("hist", h.id as u64))
                                .pl_2()
                                .pr_2()
                                .pt_1()
                                .pb_1()
                                .flex()
                                .flex_col()
                                .border_b_1()
                                .border_color(rgb(crate::theme::Theme::border()))
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                                .child(format!("#{}", h.id)),
                                        )
                                        .child(
                                            div()
                                                .text_color(rgb(crate::theme::Theme::fg_dim()))
                                                .child(h.user.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_color(rgb(crate::theme::Theme::fg()))
                                                .overflow_hidden()
                                                .child(h.short_desc()),
                                        ),
                                ),
                        );
                    }
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
                                cx.listener(move |p: &mut VcsPanel, e: &gpui::ClickEvent, window, cx| {
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
                                })
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
                    list = list.child(section_header("提交历史".into()));
                    for (i, h) in self.git_log.iter().enumerate() {
                        list = list.child(
                            div()
                                .id(("gh", i))
                                .pl_2()
                                .pr_2()
                                .pt_1()
                                .pb_1()
                                .flex()
                                .gap_2()
                                .border_b_1()
                                .border_color(rgb(crate::theme::Theme::border()))
                                .child(
                                    div()
                                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                                        .child(h.id.clone()),
                                )
                                .child(
                                    div()
                                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                                        .child(h.author.clone()),
                                )
                                .child(
                                    div()
                                        .text_color(rgb(crate::theme::Theme::fg()))
                                        .overflow_hidden()
                                        .child(h.summary.clone()),
                                ),
                        );
                    }
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
            .child(
                div()
                    .w(px(34.))
                    .text_color(rgb(color))
                    .child(action),
            )
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
        .px_2()
        .py_1()
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
