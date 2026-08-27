use gpui::prelude::*;
use gpui::*;
use mf_agent::{Engine, QuestionView, RunView, TaskStatus, TaskView};
use std::path::PathBuf;
use std::sync::Arc;

use crate::change_set::{ChangeEntry, ChangeSetSnapshot};
use crate::console::ConsolePane;
use crate::theme::Theme;

/// 工作项执行概览:执行步骤、关键路径与当前 Change Set。
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
    change_set: ChangeSetSnapshot,
    sig: String,
    on_open_change: Option<Box<dyn Fn(PathBuf, &mut Window, &mut App)>>,
}

impl Cockpit {
    pub fn new(engine: Arc<Engine>, root: PathBuf, shell: String, cx: &mut Context<Self>) -> Self {
        let change_set = ChangeSetSnapshot::load(&root);
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
            change_set,
            sig: String::new(),
            on_open_change: None,
        };
        this.refresh_snapshot();
        this.start_polling(cx);
        this
    }

    pub fn set_on_open_change(
        &mut self,
        callback: impl Fn(PathBuf, &mut Window, &mut App) + 'static,
    ) {
        self.on_open_change = Some(Box::new(callback));
    }

    fn spawn_pane(&mut self, cx: &mut Context<Self>) {
        if self.panes.len() >= 4 {
            return;
        }
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let shell = self.shell.clone();
        let root = self.root.clone();
        let pane = cx.new(|cx| {
            ConsolePane::new_in(id, &shell, &root, cx)
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
        let change_set = ChangeSetSnapshot::load(&self.root);
        self.run = run;
        self.tasks = tasks;
        self.questions = questions;
        self.change_set = change_set;
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
                    let change_set = ChangeSetSnapshot::load(&root);
                    (run, tasks, questions, change_set)
                });
                let (run, tasks, questions, change_set) = snap.await;
                let mut sig = format!(
                    "run:{:?};q:{};",
                    run.as_ref().map(|r| (r.id, r.status.clone())),
                    questions.len(),
                );
                for t in &tasks {
                    sig.push_str(&format!("{}:{:?},", t.id, t.status));
                }
                sig.push_str(&format!("backend:{:?};", change_set.backend));
                for entry in &change_set.entries {
                    sig.push_str(&format!("{}|{};", entry.path.display(), entry.status));
                }
                if this.update(cx, |p, cx| {
                    p.run = run;
                    p.tasks = tasks;
                    p.questions = questions;
                    p.change_set = change_set;
                    if p.sig != sig {
                        p.sig = sig;
                        cx.notify();
                    }
                })
                .is_err()
                {
                    break;
                }
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
            (if attention.is_empty() { "需要关注" } else { "需要关注 ⚠" }, Theme::danger(), attention),
            ("进行中", Theme::accent(), doing),
            ("就绪 / 排队", Theme::fg_dim(), ready),
            ("已完成", Theme::success(), done),
        ]
    }

    fn render_queue(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let run_active = self.run.as_ref().is_some_and(|r| r.status == "active");
        div()
            .id("cockpit-queue")
            .w_full()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(Theme::bg_panel()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .h(px(34.))
                    .border_b_1()
                    .border_color(rgb(Theme::border()))
                    .child(div().text_size(px(12.)).text_color(rgb(Theme::fg_dim())).child("执行概览"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("ck-start-run")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_size(px(11.))
                            .text_color(rgb(if run_active { Theme::fg_dim() } else { Theme::accent() }))
                            .child(if run_active { "● 执行中" } else { "从 Agent 创建执行" }),
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
                            .text_color(rgb(Theme::fg_faint()))
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
                                            d.bg(rgb(Theme::bg_active()))
                                                .border_l_2()
                                                .border_color(rgb(Theme::accent()))
                                        })
                                        .hover(|d| d.bg(rgb(Theme::bg_hover())))
                                        .on_click(cx.listener(move |p: &mut Self, _: &ClickEvent, _w, cx| {
                                            p.selected_task = Some(tid);
                                            cx.notify();
                                        }))
                                        .child(
                                            div().flex().items_center().gap_1()
                                                .child(div().w(px(7.)).h(px(7.)).rounded_full().bg(rgb(dot_color)))
                                                .child(div().text_size(px(12.)).text_color(rgb(Theme::fg())).child(truncate(&t.spec, 26)))
                                                .child(div().flex_1())
                                                .child(div().text_size(px(10.)).text_color(rgb(Theme::fg_faint())).child(format!("#{tid}"))),
                                        )
                                        .when(t.failure_count > 0 && t.status != TaskStatus::Completed, |d| {
                                            d.child(div().text_size(px(10.)).text_color(rgb(Theme::danger())).child(format!("失败 {} 次", t.failure_count)))
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
            Some("active") => ("● RUNNING", Theme::accent()),
            Some("done") => ("✓ 已收敛", Theme::success()),
            Some("done-with-failures") => ("✓ 收敛(有失败)", Theme::warning()),
            Some("planner-error") => ("✕ 规划失败", Theme::danger()),
            Some(_) => ("终态", Theme::fg_dim()),
            None => ("未启动", Theme::fg_faint()),
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
            .border_color(rgb(Theme::border()))
            .child(
                div()
                    .child(div().text_size(px(14.)).text_color(rgb(Theme::fg())).child(truncate(&objective, 40)))
                    .child(div().text_size(px(11.)).text_color(rgb(Theme::fg_dim())).child(format!("mf-agent 引擎 · {}", self.root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()))),
            )
            .child(div().flex_1())
            .child(metric(done, total, "TASKS", Theme::fg()))
            .child(metric(open_q, open_q, "待答问句", if open_q > 0 { Theme::warning() } else { Theme::fg_dim() }))
            .child(metric(failed.max(0) as usize, failed.max(0) as usize, "累计失败", if failed > 0 { Theme::danger() } else { Theme::fg_dim() }))
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
                    .border_color(rgb(Theme::accent()))
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
                            .border_color(rgb(Theme::border()))
                            .bg(rgb(Theme::bg_panel()))
                            .child(div().text_size(px(11.)).text_color(rgb(Theme::fg_dim())).child(format!("终端 #{}", pane.read(cx).id)))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("ck-zoom-back")
                                    .px_2()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_size(px(11.))
                                    .text_color(rgb(Theme::accent()))
                                    .hover(|d| d.bg(rgb(Theme::bg_hover())))
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
                    .border_color(rgb(Theme::border()))
                    .rounded_sm()
                    .text_size(px(12.))
                    .text_color(rgb(Theme::fg_faint()))
                    .child("⊕ 启动第一个终端(上方「⊕ 终端」)"),
            );
        }
        // 2×2 等分矩阵;不足 4 个时按行折叠
        wrap.children(matrix_rows(self.panes.len()).into_iter().map(|row| {
            div().flex_1().min_h_0().flex().gap_2().children(row.into_iter().map(|i| {
                let pane = self.panes[i].clone();
                let is_sel = self.selected == Some(i);
                let (title, dead, tail, sniff) = {
                    let p = pane.read(cx);
                    (p.title().to_string(), p.is_dead(), p.tail_lines(6), p.sniff_state())
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
                    .border_color(rgb(if is_sel { Theme::accent() } else { Theme::border() }))
                    .cursor_pointer()
                    .bg(rgb(Theme::bg()))
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
                            .border_color(rgb(Theme::border()))
                            .bg(rgb(Theme::bg_panel()))
                            .child(div().w(px(7.)).h(px(7.)).rounded_full().bg(rgb(match sniff {
                                crate::console::SniffState::Working => Theme::warning(),
                                crate::console::SniffState::Idle => Theme::success(),
                                crate::console::SniffState::Done => Theme::accent(),
                                crate::console::SniffState::Dead => Theme::danger(),
                                crate::console::SniffState::Unknown => if dead { Theme::danger() } else { Theme::fg_faint() },
                            })))
                            .child(div().text_size(px(11.)).text_ellipsis().whitespace_nowrap().overflow_hidden().flex_1().text_color(rgb(Theme::fg_dim())).child(title))
                            .child(div().text_size(px(9.)).text_color(rgb(match sniff {
                                crate::console::SniffState::Working => Theme::warning(),
                                crate::console::SniffState::Idle => Theme::success(),
                                _ => Theme::fg_faint(),
                            })).child(match sniff {
                                crate::console::SniffState::Working => "● 运行",
                                crate::console::SniffState::Idle => "○ 空闲",
                                crate::console::SniffState::Done => "✓ 完成",
                                crate::console::SniffState::Dead => "✕ 退出",
                                crate::console::SniffState::Unknown => "",
                            }))
                            .child(
                                div()
                                    .id(ElementId::Name(format!("ck-cons-close-{idx}").into()))
                                    .px_1()
                                    .text_size(px(11.))
                                    .text_color(rgb(Theme::fg_faint()))
                                    .hover(|d| d.text_color(rgb(Theme::danger())))
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
                            .text_color(rgb(Theme::fg_dim()))
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
            .border_color(rgb(Theme::border()))
            .bg(rgb(Theme::bg_panel()))
            .px_4()
            .py_1()
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(Theme::fg_faint()))
                    .child("关键路径 CRITICAL PATH"),
            )
            .child(
                div().flex().items_center().gap_1().pt_1().children(if self.tasks.is_empty() {
                    vec![div().text_size(px(11.)).text_color(rgb(Theme::fg_faint())).child("—").into_any_element()]
                } else {
                    let mut out: Vec<Div> = Vec::new();
                    for (i, n) in nodes.iter().enumerate() {
                        if i > 0 {
                            out.push(
                                div().w(px(26.)).h(px(1.)).bg(rgb(Theme::border())),
                            );
                        }
                        let node = div()
                            .w(px(168.))
                            .px_2()
                            .py_1()
                            .border_1()
                            .border_color(rgb(Theme::border()))
                            .rounded_sm()
                            .bg(rgb(Theme::bg()));
                        let node = match n {
                            Some(t) => {
                                let (c, label) = status_dot(&t.status);
                                node.child(
                                    div().text_size(px(10.)).font_family("Consolas").text_color(rgb(Theme::fg_faint())).child(format!("#{}", t.id)),
                                )
                                .child(div().text_size(px(11.)).text_color(rgb(Theme::fg())).child(truncate(&t.spec, 16)))
                                .child(div().text_size(px(10.)).font_family("Consolas").text_color(rgb(c)).child(label))
                            }
                            None => node.child(
                                div().py_1().text_size(px(11.)).text_color(rgb(Theme::fg_faint())).child("—"),
                            ),
                        };
                        out.push(node);
                    }
                    out.into_iter().map(|d| d.into_any_element()).collect::<Vec<_>>()
                }),
            )
    }

    fn render_changeset(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("ck-changeset")
            .w(px(250.))
            .min_w(px(250.))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(Theme::border()))
            .bg(rgb(Theme::bg_panel()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .h(px(30.))
                    .border_b_1()
                    .border_color(rgb(Theme::border()))
                    .text_size(px(10.))
                    .text_color(rgb(Theme::fg_faint()))
                    .child(format!("{} · {} 项", self.change_set.label(), self.change_set.entries.len())),
            )
            .child(
                div()
                    .id("ck-changeset-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(if self.change_set.entries.is_empty() {
                        vec![div().id("ck-cs-empty").p_2().text_size(px(11.)).text_color(rgb(Theme::fg_faint())).child("工作区干净").into_any_element()]
                    } else {
                        self.change_set.entries
                            .iter()
                            .take(50)
                            .map(|g| {
                                let (mark, color) = change_mark(&g);
                                let path = g.path.clone();
                                let detail = g
                                    .change
                                    .as_ref()
                                    .map(|change| format!("CL {change}"))
                                    .unwrap_or_else(|| {
                                        g.path
                                            .parent()
                                            .map(|parent| truncate(&parent.display().to_string(), 12))
                                            .unwrap_or_default()
                                    });
                                div()
                                    .id(ElementId::Name(format!("ck-cs-{}", g.path.display()).into()))
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .px_2()
                                    .py_1()
                                    .border_b_1()
                                    .border_color(rgb(Theme::bg_elevated()))
                                    .cursor_pointer()
                                    .hover(|d| d.bg(rgb(Theme::bg_hover())))
                                    .on_click(cx.listener(move |cockpit: &mut Cockpit, _, window, cx| {
                                        if let Some(callback) = &cockpit.on_open_change {
                                            callback(path.clone(), window, cx);
                                        }
                                    }))
                                    .child(div().text_size(px(10.)).font_family("Consolas").text_color(rgb(color)).child(mark))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .overflow_hidden()
                                            .text_color(rgb(Theme::fg_dim()))
                                            .child(g.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_size(px(9.))
                                            .text_color(rgb(Theme::fg_faint()))
                                            .child(detail),
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
                    .border_color(rgb(Theme::border()))
                    .text_size(px(10.))
                    .text_color(rgb(Theme::fg_faint()))
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
            .bg(rgb(Theme::bg()))
            .text_color(rgb(Theme::fg()))
            .on_key_down(cx.listener(|p: &mut Self, event: &KeyDownEvent, _w, cx| {
                if event.keystroke.key == "escape" && p.zoomed.is_some() {
                    p.zoomed = None;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .id("ck-center")
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(self.render_summary())
                    .child(self.render_queue(cx))
                    .child(self.render_dag()),
            )
            .child(self.render_changeset(cx))
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
        TaskStatus::Completed => (Theme::success(), "✓ 完成"),
        TaskStatus::Dispatched => (Theme::accent(), "● 执行中"),
        TaskStatus::Ready => (Theme::accent(), "● 就绪"),
        TaskStatus::Pending => (Theme::fg_dim(), "○ 等待"),
        TaskStatus::Failed => (Theme::danger(), "✕ 失败"),
        TaskStatus::Blocked => (Theme::danger(), "⚠ 熔断"),
    }
}

fn change_mark(entry: &ChangeEntry) -> (String, u32) {
    match entry.status.as_str() {
        "add" => ("A".into(), Theme::success()),
        "delete" => ("D".into(), Theme::danger()),
        "rename" | "branch" | "move/add" => ("R".into(), Theme::warning()),
        "staged" => ("S".into(), Theme::accent()),
        _ => ("M".into(), Theme::warning()),
    }
}

fn metric(value: usize, _alt: usize, label: &str, color: u32) -> Div {
    div()
        .pl_3()
        .border_l_1()
        .border_color(rgb(Theme::border()))
        .child(div().text_size(px(15.)).font_family("Consolas").text_color(rgb(color)).child(value.to_string()))
        .child(div().text_size(px(9.)).text_color(rgb(Theme::fg_faint())).child(label.to_string()))
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
