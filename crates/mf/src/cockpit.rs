use gpui::prelude::*;
use gpui::*;
use mf_agent::{Engine, QuestionView, RunView, TaskStatus, TaskView};
use mf_vcs::git::{Git, GitFileEntry};
use std::path::PathBuf;
use std::sync::Arc;

use crate::console::ConsolePane;
use crate::theme::Theme;

/// Orca 模式驾驶舱(AI 驱动):左侧任务 Run 队列 + 中央 agent 终端矩阵 + DAG 关键路径 + 右侧 Change set。
/// 数据来自 mf-agent 引擎(DB 轮询快照)与 git status;终端为真实 ConPTY。
///
/// 交互:矩阵格 单击=选中 / 双击=放大为完整终端;放大态可输入,⬜ 还原矩阵。
pub struct Cockpit {
    engine: Arc<Engine>,
    root: PathBuf,
    shell: String,
    panes: Vec<Entity<ConsolePane>>,
    zoomed: Option<usize>,
    selected: Option<usize>,
    next_pane_id: usize,
    selected_task: Option<i64>,
    // 数据快照(轮询刷新,签名变化才重绘)
    run: Option<RunView>,
    tasks: Vec<TaskView>,
    questions: Vec<QuestionView>,
    git_status: Vec<GitFileEntry>,
    sig: String,
}

impl Cockpit {
    pub fn new(engine: Arc<Engine>, root: PathBuf, shell: String, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            engine,
            root,
            shell,
            panes: Vec::new(),
            zoomed: None,
            selected: None,
            next_pane_id: 0,
            selected_task: None,
            run: None,
            tasks: Vec::new(),
            questions: Vec::new(),
            git_status: Vec::new(),
            sig: String::new(),
        };
        this.refresh_snapshot();
        this.spawn_pane(cx);
        this.spawn_pane(cx);
        this.start_polling(cx);
        this
    }

    fn spawn_pane(&mut self, cx: &mut Context<Self>) {
        if self.panes.len() >= 4 {
            return;
        }
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let shell = self.shell.clone();
        let pane = cx.new(|cx| {
            ConsolePane::new(id, &shell, cx)
                .unwrap_or_else(|e| ConsolePane::failed(id, e, cx))
        });
        self.panes.push(pane);
        cx.notify();
    }

    fn close_pane(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.panes.len() {
            return;
        }
        self.panes.remove(idx);
        if let Some(z) = self.zoomed {
            if z == idx {
                self.zoomed = None;
            } else if z > idx {
                self.zoomed = Some(z - 1);
            }
        }
        if let Some(s) = self.selected {
            if s == idx {
                self.selected = None;
            } else if s > idx {
                self.selected = Some(s - 1);
            }
        }
        cx.notify();
    }

    /// 启动一个新的 Run(无 API key 时 mock provider 即刻演示完整流转)
    fn start_run(&mut self, cx: &mut Context<Self>) {
        if self.run.as_ref().is_some_and(|r| r.status == "active") {
            return;
        }
        let _ = self.engine.start_run("cockpit: 驾驶舱演示 Run(规划→派发→收敛)");
        self.refresh_snapshot();
        cx.notify();
    }

    // ---------- 数据轮询 ----------

    fn refresh_snapshot(&mut self) {
        let run = self.engine.latest_active_run().ok().flatten();
        // latest_run 按 id DESC 取,收敛后仍返回终态 run,展示得以保留
        let (tasks, questions) = match &run {
            Some(r) => (
                self.engine.tasks_of_run(r.id).unwrap_or_default(),
                self.engine.open_questions(r.id).unwrap_or_default(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let git_status = Git::open(&self.root)
            .and_then(|g| g.status())
            .unwrap_or_default();
        self.run = run;
        self.tasks = tasks;
        self.questions = questions;
        self.git_status = git_status;
    }

    fn start_polling(&self, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(700))
                    .await;
                // 快照计算放后台线程(git status 可能略慢)
                let engine = engine.clone();
                let root = root.clone();
                let snap = cx.background_executor().spawn(async move {
                    let run = engine.latest_active_run().ok().flatten();
                    let (tasks, questions) = match &run {
                        Some(r) => (
                            engine.tasks_of_run(r.id).unwrap_or_default(),
                            engine.open_questions(r.id).unwrap_or_default(),
                        ),
                        None => (Vec::new(), Vec::new()),
                    };
                    let git_status = Git::open(&root)
                        .and_then(|g| g.status())
                        .unwrap_or_default();
                    (run, tasks, questions, git_status)
                });
                let (run, tasks, questions, git_status) = snap.await;
                let mut sig = format!(
                    "run:{:?};q:{};",
                    run.as_ref().map(|r| (r.id, r.status.clone())),
                    questions.len(),
                );
                for t in &tasks {
                    sig.push_str(&format!("{}:{:?},", t.id, t.status));
                }
                for g in &git_status {
                    sig.push_str(&format!("{}|{:?};", g.path.display(), g.status));
                }
                this.update(cx, |p, cx| {
                    p.run = run;
                    p.tasks = tasks;
                    p.questions = questions;
                    p.git_status = git_status;
                    if p.sig != sig {
                        p.sig = sig;
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    // ---------- 渲染 ----------

    fn queue_groups(&self) -> Vec<(&'static str, u32, Vec<&TaskView>)> {
        let qs: Vec<i64> = self.questions.iter().filter(|q| q.answer.is_none()).map(|q| q.task_id.unwrap_or(0)).collect();
        let mut attention = Vec::new();
        let mut doing = Vec::new();
        let mut ready = Vec::new();
        let mut done = Vec::new();
        for t in &self.tasks {
            let has_q = qs.contains(&t.id);
            match t.status {
                TaskStatus::Blocked => attention.push(t),
                TaskStatus::Dispatched if has_q => attention.push(t),
                TaskStatus::Dispatched => doing.push(t),
                TaskStatus::Ready | TaskStatus::Pending => ready.push(t),
                TaskStatus::Completed | TaskStatus::Failed => done.push(t),
            }
        }
        vec![
            (if attention.is_empty() { "需要关注" } else { "需要关注 ⚠" }, Theme::DANGER, attention),
            ("进行中", Theme::ACCENT, doing),
            ("就绪 / 排队", Theme::FG_DIM, ready),
            ("已完成", Theme::SUCCESS, done),
        ]
    }

    fn render_queue(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let run_active = self.run.as_ref().is_some_and(|r| r.status == "active");
        div()
            .id("cockpit-queue")
            .w(px(330.))
            .min_w(px(330.))
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(Theme::BORDER))
            .bg(rgb(Theme::BG_PANEL))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .h(px(34.))
                    .border_b_1()
                    .border_color(rgb(Theme::BORDER))
                    .child(div().text_size(px(12.)).text_color(rgb(Theme::FG_DIM)).child("RUN 队列"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("ck-start-run")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(if run_active { Theme::FG_DIM } else { Theme::ACCENT }))
                            .hover(|d| d.bg(rgb(Theme::BG_HOVER)))
                            .child(if run_active { "● 运行中" } else { "▶ 启动 Run" })
                            .on_click(cx.listener(|p: &mut Self, _: &ClickEvent, _w, cx| {
                                p.start_run(cx);
                            })),
                    )
                    .child(
                        div()
                            .id("ck-new-pane")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(Theme::FG_DIM))
                            .hover(|d| d.bg(rgb(Theme::BG_HOVER)))
                            .child("⊕ 终端")
                            .on_click(cx.listener(|p: &mut Self, _: &ClickEvent, _w, cx| {
                                p.spawn_pane(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("ck-queue-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_1()
                    .children(if self.tasks.is_empty() {
                        vec![div()
                            .p_2()
                            .text_size(px(11.5))
                            .text_color(rgb(Theme::FG_FAINT))
                            .child("暂无任务。点「▶ 启动 Run」让引擎规划并派发(mock provider 可无 key 演示)。")
                            .into_any_element()]
                    } else {
                        self.queue_groups()
                            .into_iter()
                            .filter(|(_, _, v)| !v.is_empty())
                            .flat_map(|(name, color, v)| {
                                let header = div()
                                    .px_1()
                                    .pt_2()
                                    .pb_1()
                                    .text_size(px(10.))
                                    .text_color(rgb(color))
                                    .child(format!("{} {}", name, v.len()));
                                let rows = v.iter().map(|t| {
                                    let tid = t.id;
                                    let sel = self.selected_task == Some(tid);
                                    let (dot_color, _) = status_dot(&t.status);
                                    div()
                                        .id(ElementId::Name(format!("ck-task-{tid}").into()))
                                        .px_2()
                                        .py_1()
                                        .mb_1()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .when(sel, |d| {
                                            d.bg(rgb(Theme::BG_ACTIVE))
                                                .border_l_2()
                                                .border_color(rgb(Theme::ACCENT))
                                        })
                                        .hover(|d| d.bg(rgb(Theme::BG_HOVER)))
                                        .on_click(cx.listener(move |p: &mut Self, _: &ClickEvent, _w, cx| {
                                            p.selected_task = Some(tid);
                                            cx.notify();
                                        }))
                                        .child(
                                            div().flex().items_center().gap_1()
                                                .child(div().w(px(7.)).h(px(7.)).rounded_full().bg(rgb(dot_color)))
                                                .child(div().text_size(px(12.)).text_color(rgb(Theme::FG)).child(truncate(&t.spec, 26)))
                                                .child(div().flex_1())
                                                .child(div().text_size(px(10.)).text_color(rgb(Theme::FG_FAINT)).child(format!("#{tid}"))),
                                        )
                                        .when(t.failure_count > 0 && t.status != TaskStatus::Completed, |d| {
                                            d.child(div().text_size(px(10.)).text_color(rgb(Theme::DANGER)).child(format!("失败 {} 次", t.failure_count)))
                                        })
                                        .into_any_element()
                                });
                                vec![header.into_any_element()].into_iter().chain(rows).collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>()
                    }),
            )
    }

    fn render_summary(&self) -> impl IntoElement {
        let total = self.tasks.len();
        let done = self.tasks.iter().filter(|t| t.status == TaskStatus::Completed).count();
        let failed: i32 = self.tasks.iter().map(|t| t.failure_count).sum();
        let open_q = self.questions.iter().filter(|q| q.answer.is_none()).count();
        let (status_label, status_color) = match self.run.as_ref().map(|r| r.status.as_str()) {
            Some("active") => ("● RUNNING", Theme::ACCENT),
            Some("done") => ("✓ 已收敛", Theme::SUCCESS),
            Some("done-with-failures") => ("✓ 收敛(有失败)", Theme::WARNING),
            Some("planner-error") => ("✕ 规划失败", Theme::DANGER),
            Some(_) => ("终态", Theme::FG_DIM),
            None => ("未启动", Theme::FG_FAINT),
        };
        let objective = self
            .run
            .as_ref()
            .map(|r| r.objective.clone())
            .unwrap_or_else(|| "—".into());
        div()
            .id("ck-summary")
            .h(px(62.))
            .flex()
            .items_center()
            .gap_4()
            .px_4()
            .border_b_1()
            .border_color(rgb(Theme::BORDER))
            .child(
                div()
                    .child(div().text_size(px(14.)).text_color(rgb(Theme::FG)).child(truncate(&objective, 40)))
                    .child(div().text_size(px(11.)).text_color(rgb(Theme::FG_DIM)).child(format!("mf-agent 引擎 · {}", self.root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()))),
            )
            .child(div().flex_1())
            .child(metric(done, total, "TASKS", Theme::FG))
            .child(metric(open_q, open_q, "待答问句", if open_q > 0 { Theme::WARNING } else { Theme::FG_DIM }))
            .child(metric(failed.max(0) as usize, failed.max(0) as usize, "累计失败", if failed > 0 { Theme::DANGER } else { Theme::FG_DIM }))
            .child(
                div()
                    .px_3()
                    .text_size(px(11.))
                    .font_family("Consolas")
                    .text_color(rgb(status_color))
                    .child(status_label),
            )
    }

    fn render_matrix(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let wrap = div().id("ck-matrix").flex_1().min_h_0().flex().flex_col().p_2().gap_2();
        if let Some(z) = self.zoomed {
            let pane = self.panes[z].clone();
            return wrap.child(
                div()
                    .id("ck-zoom")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(rgb(Theme::ACCENT))
                    .rounded_sm()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("ck-zoom-head")
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .h(px(26.))
                            .border_b_1()
                            .border_color(rgb(Theme::BORDER))
                            .bg(rgb(Theme::BG_PANEL))
                            .child(div().text_size(px(11.)).text_color(rgb(Theme::FG_DIM)).child(format!("终端 #{}", pane.read(cx).id)))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("ck-zoom-back")
                                    .px_2()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_size(px(11.))
                                    .text_color(rgb(Theme::ACCENT))
                                    .hover(|d| d.bg(rgb(Theme::BG_HOVER)))
                                    .child("⬜ 还原矩阵")
                                    .on_click(cx.listener(|p: &mut Self, _: &ClickEvent, _w, cx| {
                                        p.zoomed = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().flex_1().min_h_0().child(pane)),
            );
        }
        if self.panes.is_empty() {
            return wrap.child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .border_dashed()
                    .border_color(rgb(Theme::BORDER))
                    .rounded_sm()
                    .text_size(px(12.))
                    .text_color(rgb(Theme::FG_FAINT))
                    .child("⊕ 启动第一个终端(上方「⊕ 终端」)"),
            );
        }
        // 2×2 等分矩阵;不足 4 个时按行折叠
        wrap.children(matrix_rows(self.panes.len()).into_iter().map(|row| {
            div().flex_1().min_h_0().flex().gap_2().children(row.into_iter().map(|i| {
                let pane = self.panes[i].clone();
                let is_sel = self.selected == Some(i);
                let (title, dead, tail) = {
                    let p = pane.read(cx);
                    (p.title().to_string(), p.is_dead(), p.tail_lines(6))
                };
                let idx = i;
                div()
                    .id(ElementId::Name(format!("ck-cons-{idx}").into()))
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_1()
                    .rounded_sm()
                    .overflow_hidden()
                    .border_color(rgb(if is_sel { Theme::ACCENT } else { Theme::BORDER }))
                    .cursor_pointer()
                    .bg(rgb(Theme::BG))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |p: &mut Self, event: &MouseDownEvent, _w, cx| {
                        if event.click_count == 2 {
                            p.zoomed = Some(idx);
                            p.selected = Some(idx);
                        } else {
                            p.selected = Some(idx);
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .h(px(24.))
                            .border_b_1()
                            .border_color(rgb(Theme::BORDER))
                            .bg(rgb(Theme::BG_PANEL))
                            .child(div().w(px(7.)).h(px(7.)).rounded_full().bg(rgb(if dead { Theme::DANGER } else { Theme::SUCCESS })))
                            .child(div().text_size(px(11.)).text_ellipsis().whitespace_nowrap().overflow_hidden().flex_1().text_color(rgb(Theme::FG_DIM)).child(title))
                            .child(
                                div()
                                    .id(ElementId::Name(format!("ck-cons-close-{idx}").into()))
                                    .px_1()
                                    .text_size(px(11.))
                                    .text_color(rgb(Theme::FG_FAINT))
                                    .hover(|d| d.text_color(rgb(Theme::DANGER)))
                                    .child("✕")
                                    .on_click(cx.listener(move |p: &mut Self, _: &ClickEvent, _w, cx| {
                                        p.close_pane(idx, cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .p_1()
                            .font_family("Consolas")
                            .text_size(px(10.5))
                            .text_color(rgb(Theme::FG_DIM))
                            .child(if tail.is_empty() {
                                "…".to_string()
                            } else {
                                tail.join("\n")
                            }),
                    )
            }))
        }))
    }

    fn render_dag(&self) -> impl IntoElement {
        // 关键路径:最后一个完成 → 第一个未完成 → 一个等待
        let done_last = self.tasks.iter().filter(|t| t.status == TaskStatus::Completed).last();
        let doing = self.tasks.iter().find(|t| matches!(t.status, TaskStatus::Dispatched | TaskStatus::Ready));
        let waiting = self.tasks.iter().find(|t| t.status == TaskStatus::Pending);
        let nodes: Vec<Option<&TaskView>> = vec![done_last, doing, waiting];
        div()
            .id("ck-dag")
            .h(px(96.))
            .border_t_1()
            .border_color(rgb(Theme::BORDER))
            .bg(rgb(Theme::BG_PANEL))
            .px_4()
            .py_1()
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(Theme::FG_FAINT))
                    .child("关键路径 CRITICAL PATH"),
            )
            .child(
                div().flex().items_center().gap_1().pt_1().children(if self.tasks.is_empty() {
                    vec![div().text_size(px(11.)).text_color(rgb(Theme::FG_FAINT)).child("—").into_any_element()]
                } else {
                    let mut out: Vec<Div> = Vec::new();
                    for (i, n) in nodes.iter().enumerate() {
                        if i > 0 {
                            out.push(
                                div().w(px(26.)).h(px(1.)).bg(rgb(Theme::BORDER)),
                            );
                        }
                        let node = div()
                            .w(px(168.))
                            .px_2()
                            .py_1()
                            .border_1()
                            .border_color(rgb(Theme::BORDER))
                            .rounded_sm()
                            .bg(rgb(Theme::BG));
                        let node = match n {
                            Some(t) => {
                                let (c, label) = status_dot(&t.status);
                                node.child(
                                    div().text_size(px(10.)).font_family("Consolas").text_color(rgb(Theme::FG_FAINT)).child(format!("#{}", t.id)),
                                )
                                .child(div().text_size(px(11.)).text_color(rgb(Theme::FG)).child(truncate(&t.spec, 16)))
                                .child(div().text_size(px(10.)).font_family("Consolas").text_color(rgb(c)).child(label))
                            }
                            None => node.child(
                                div().py_1().text_size(px(11.)).text_color(rgb(Theme::FG_FAINT)).child("—"),
                            ),
                        };
                        out.push(node);
                    }
                    out.into_iter().map(|d| d.into_any_element()).collect::<Vec<_>>()
                }),
            )
    }

    fn render_changeset(&self) -> impl IntoElement {
        div()
            .id("ck-changeset")
            .w(px(250.))
            .min_w(px(250.))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(Theme::BORDER))
            .bg(rgb(Theme::BG_PANEL))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .h(px(30.))
                    .border_b_1()
                    .border_color(rgb(Theme::BORDER))
                    .text_size(px(10.))
                    .text_color(rgb(Theme::FG_FAINT))
                    .child(format!("CHANGE SET · {} 项", self.git_status.len())),
            )
            .child(
                div()
                    .id("ck-changeset-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(if self.git_status.is_empty() {
                        vec![div().id("ck-cs-empty").p_2().text_size(px(11.)).text_color(rgb(Theme::FG_FAINT)).child("工作区干净").into_any_element()]
                    } else {
                        self.git_status
                            .iter()
                            .take(50)
                            .map(|g| {
                                let (mark, color) = git_mark(&g);
                                div()
                                    .id(ElementId::Name(format!("ck-cs-{}", g.path.display()).into()))
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .px_2()
                                    .py_1()
                                    .border_b_1()
                                    .border_color(rgb(Theme::BG_ELEVATED))
                                    .child(div().text_size(px(10.)).font_family("Consolas").text_color(rgb(color)).child(mark))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .overflow_hidden()
                                            .text_color(rgb(Theme::FG_DIM))
                                            .child(g.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_size(px(9.))
                                            .text_color(rgb(Theme::FG_FAINT))
                                            .child(g.path.parent().map(|p| truncate(&p.display().to_string(), 12)).unwrap_or_default()),
                                    )
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(Theme::BORDER))
                    .text_size(px(10.))
                    .text_color(rgb(Theme::FG_FAINT))
                    .child("执行上下文:主仓库 · 终端矩阵 ConPTY"),
            )
    }
}

impl Render for Cockpit {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("cockpit-root")
            .key_context("Cockpit")
            .size_full()
            .flex()
            .min_h_0()
            .bg(rgb(Theme::BG))
            .text_color(rgb(Theme::FG))
            .on_key_down(cx.listener(|p: &mut Self, event: &KeyDownEvent, _w, cx| {
                if event.keystroke.key == "escape" && p.zoomed.is_some() {
                    p.zoomed = None;
                    cx.notify();
                }
            }))
            .child(self.render_queue(cx))
            .child(
                div()
                    .id("ck-center")
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(self.render_summary())
                    .child(self.render_matrix(cx))
                    .child(self.render_dag()),
            )
            .child(self.render_changeset())
    }
}

// ---------- 帮助 ----------

fn matrix_rows(n: usize) -> Vec<Vec<usize>> {
    let n = n.min(4);
    let mut rows = Vec::new();
    match n {
        0 => {}
        1 => rows.push(vec![0]),
        2 | 3 => {
            rows.push(vec![0]);
            if n > 1 {
                rows.push(vec![1]);
            }
            if n > 2 {
                rows.push(vec![2]);
            }
        }
        _ => {
            rows.push(vec![0, 1]);
            rows.push(vec![2, 3]);
        }
    }
    rows
}

fn status_dot(s: &TaskStatus) -> (u32, &'static str) {
    match s {
        TaskStatus::Completed => (Theme::SUCCESS, "✓ 完成"),
        TaskStatus::Dispatched => (Theme::ACCENT, "● 执行中"),
        TaskStatus::Ready => (Theme::ACCENT, "● 就绪"),
        TaskStatus::Pending => (Theme::FG_DIM, "○ 等待"),
        TaskStatus::Failed => (Theme::DANGER, "✕ 失败"),
        TaskStatus::Blocked => (Theme::DANGER, "⚠ 熔断"),
    }
}

fn git_mark(g: &GitFileEntry) -> (String, u32) {
    use mf_vcs::git::GitStatus;
    match g.status {
        GitStatus::New => ("A".into(), Theme::SUCCESS),
        GitStatus::Modified => ("M".into(), Theme::WARNING),
        GitStatus::Deleted => ("D".into(), Theme::DANGER),
        GitStatus::Renamed => ("R".into(), Theme::WARNING),
        GitStatus::Staged { .. } => ("S".into(), Theme::ACCENT),
    }
}

fn metric(value: usize, _alt: usize, label: &str, color: u32) -> Div {
    div()
        .pl_3()
        .border_l_1()
        .border_color(rgb(Theme::BORDER))
        .child(div().text_size(px(15.)).font_family("Consolas").text_color(rgb(color)).child(value.to_string()))
        .child(div().text_size(px(9.)).text_color(rgb(Theme::FG_FAINT)).child(label.to_string()))
}

fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for ch in s.chars() {
        if count >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
        count += 1;
    }
    out
}
