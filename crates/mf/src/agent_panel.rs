use gpui::prelude::*;
use gpui::*;
use mf_agent::{Engine, EngineEvent, TaskStatus, TaskView};
use parking_lot::Mutex;
use std::ops::Range;
use std::sync::Arc;

/// Agent 面板:目标输入 → 任务看板 → 运行日志 → 人机问答
pub struct AgentPanel {
    engine: Arc<Mutex<Option<Arc<Engine>>>>,
    root_label: String,
    objective: String,
    active_run: Option<i64>,
    run_status: String,
    tasks: Vec<TaskView>,
    logs: Vec<LogLine>,
    open_question: Option<mf_agent::QuestionView>,
    answer: String,
    /// agent 改过的文件(相对路径),由 workspace 在 render 时取走并重载对应编辑器
    pending_touched: Vec<String>,
    on_files_touched:
        Option<Box<dyn Fn(&[String], &mut Window, &mut App)>>,
    input_focus: FocusHandle,
}

#[derive(Clone)]
struct LogLine {
    worker: String,
    text: String,
}

impl AgentPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let panel = Self {
            engine: Arc::new(Mutex::new(None)),
            root_label: String::new(),
            objective: String::new(),
            active_run: None,
            run_status: "空闲".into(),
            tasks: Vec::new(),
            logs: Vec::new(),
            open_question: None,
            answer: String::new(),
            pending_touched: Vec::new(),
            on_files_touched: None,
            input_focus: cx.focus_handle(),
        };
        panel.start_event_pump(cx);
        panel
    }

    /// 打开项目时注入引擎
    pub fn attach_engine(&mut self, engine: Arc<Engine>, root: &std::path::Path, cx: &mut Context<Self>) {
        *self.engine.lock() = Some(engine.clone());
        self.root_label = root.display().to_string();
        // 恢复最近一次 run 的任务视图。
        // 注意:直接用局部 engine 查询,不要经 self.engine.lock() —— if let 条件里的
        // MutexGuard 临时值会活到整个 if-let 结束,body 内再锁同一把 std Mutex 会同线程死锁。
        if let Some(run) = engine.latest_active_run().ok().flatten() {
            self.active_run = Some(run.id);
            self.run_status = format!("{}(历史)", run.status);
            self.tasks = engine.tasks_of_run(run.id).ok().unwrap_or_default();
        }
        cx.notify();
    }

    pub fn set_on_files_touched(&mut self, cb: impl Fn(&[String], &mut Window, &mut App) + 'static) {
        self.on_files_touched = Some(Box::new(cb));
    }

    fn start_event_pump(&self, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let Some(engine_ref) = engine.lock().clone() else {
                    continue;
                };
                // 非阻塞抽干事件
                let mut batch: Vec<EngineEvent> = Vec::new();
                while let Ok(ev) = engine_ref.events_rx.try_recv() {
                    batch.push(ev);
                    if batch.len() > 200 {
                        break;
                    }
                }
                if !batch.is_empty() {
                    this.update(cx, |p, cx| {
                        p.apply_events(batch, cx);
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    fn apply_events(&mut self, events: Vec<EngineEvent>, cx: &mut Context<Self>) {
        let mut touched: Vec<String> = Vec::new();
        for ev in events {
            match ev {
                EngineEvent::RunStarted(run) => {
                    self.active_run = Some(run.id);
                    self.run_status = "运行中".into();
                    self.tasks.clear();
                    self.logs.push(LogLine {
                        worker: "run".into(),
                        text: format!("目标: {}", run.objective),
                    });
                }
                EngineEvent::TaskCreated(t) => {
                    self.tasks.push(t);
                }
                EngineEvent::TaskStatus(t) => {
                    if let Some(slot) = self.tasks.iter_mut().find(|x| x.id == t.id) {
                        *slot = t;
                    } else {
                        self.tasks.push(t);
                    }
                }
                EngineEvent::WorkerLog { task_id, worker, text } => {
                    self.logs.push(LogLine {
                        worker,
                        text: if task_id > 0 {
                            format!("[#{}] {}", task_id, text)
                        } else {
                            text
                        },
                    });
                    if self.logs.len() > 400 {
                        let drop_n = self.logs.len() - 400;
                        self.logs.drain(0..drop_n);
                    }
                }
                EngineEvent::WorkerTool { task_id, worker, tool, summary } => {
                    self.logs.push(LogLine {
                        worker,
                        text: format!("[#{}] 🔧 {} {}", task_id, tool, summary),
                    });
                    if tool == "fs_write" || tool == "fs_patch" {
                        // 从参数 JSON 提取 path
                        if let Some(p) = extract_path(&summary) {
                            touched.push(p);
                        }
                    }
                }
                EngineEvent::QuestionOpened(q) => {
                    self.open_question = Some(q);
                }
                EngineEvent::QuestionAnswered(_) => {
                    self.open_question = None;
                    self.answer.clear();
                }
                EngineEvent::RunFinished(_, msg) => {
                    self.run_status = msg.clone();
                    self.logs.push(LogLine {
                        worker: "run".into(),
                        text: format!("🏁 {}", msg),
                    });
                }
                EngineEvent::EngineError(e) => {
                    self.logs.push(LogLine {
                        worker: "engine".into(),
                        text: format!("⚠ {}", e),
                    });
                }
            }
        }
        if !touched.is_empty() {
            // 交给 workspace 在 render(有 window)时处理:重载被 agent 修改的编辑器
            self.pending_touched.extend(touched);
            let _ = &self.on_files_touched;
        }
    }

    fn act_start(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.act_start_owned(cx);
    }

    fn act_start_owned(&mut self, cx: &mut Context<Self>) {
        let obj = self.objective.trim().to_string();
        if obj.is_empty() {
            self.run_status = "请输入目标".into();
            cx.notify();
            return;
        }
        let Some(engine) = self.engine.lock().clone() else {
            self.run_status = "先打开项目文件夹".into();
            cx.notify();
            return;
        };
        match engine.start_run(&obj) {
            Ok(_) => {
                self.objective.clear();
                self.logs.clear();
                self.tasks.clear();
                self.run_status = "规划中…".into();
            }
            Err(e) => self.run_status = format!("启动失败: {e}"),
        }
        cx.notify();
    }

    fn act_answer(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(q) = &self.open_question else { return };
        let Some(engine) = self.engine.lock().clone() else { return };
        let ans = self.answer.trim().to_string();
        if ans.is_empty() {
            return;
        }
        let _ = engine.answer_question(q.id, &ans);
        self.answer.clear();
        self.open_question = None;
        cx.notify();
    }

    fn act_unblock(&mut self, task_id: i64, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.lock().clone() else { return };
        let _ = engine.unblock_task(task_id);
        self.run_status = format!("任务 #{} 已重置,等待重新派发", task_id);
        cx.notify();
    }

    fn on_objective_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "enter" {
            self.act_start_owned(cx);
            return;
        }
        if let Some(chars) = event.keystroke.key_char.clone() {
            let printable: String = chars.chars().filter(|c| !c.is_control()).collect();
            if !printable.is_empty() {
                self.objective.push_str(&printable);
                cx.notify();
            }
        } else if event.keystroke.key == "backspace" {
            self.objective.pop();
            cx.notify();
        }
    }

    fn on_answer_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(chars) = event.keystroke.key_char.clone() {
            let printable: String = chars.chars().filter(|c| !c.is_control()).collect();
            if !printable.is_empty() {
                self.answer.push_str(&printable);
                cx.notify();
            }
        } else if event.keystroke.key == "backspace" {
            self.answer.pop();
            cx.notify();
        }
    }

    fn status_color(s: TaskStatus) -> u32 {
        match s {
            TaskStatus::Pending => crate::theme::Theme::FG_FAINT,
            TaskStatus::Ready => crate::theme::Theme::ACCENT,
            TaskStatus::Dispatched => crate::theme::Theme::WARNING,
            TaskStatus::Completed => crate::theme::Theme::SUCCESS,
            TaskStatus::Failed => crate::theme::Theme::DANGER,
            TaskStatus::Blocked => crate::theme::Theme::DANGER,
        }
    }
}

// 挂在结构体外的字段段(pending_touched 需要声明)
impl AgentPanel {
    pub fn take_touched(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_touched)
    }
}

fn extract_path(summary: &str) -> Option<String> {
    // summary 是参数 JSON 前缀,如 {"path":".mf-agent/task-1.md","content":...
    let idx = summary.find("\"path\"")?;
    let rest = &summary[idx + 6..];
    let rest = rest.trim_start_matches(|c: char| c == ' ' || c == ':' || c == '"');
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

impl Focusable for AgentPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.input_focus.clone()
    }
}

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut col = div()
            .id("agent-panel")
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(12.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    // 面板内点击清除全局键处理,避免误触编辑器快捷键
                }),
            );

        // 标题 + 状态
        col = col.child(
            div()
                .h(px(30.))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .border_b_1()
                .border_color(rgb(crate::theme::Theme::BORDER))
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(crate::theme::Theme::FG_FAINT))
                        .child("AGENT"),
                )
                .child(
                    div()
                        .text_color(rgb(crate::theme::Theme::ACCENT))
                        .child(self.run_status.clone()),
                )
                .child(div().ml_auto().text_color(rgb(crate::theme::Theme::FG_FAINT)).child(
                    if self.root_label.is_empty() { "未打开项目".to_string() } else { "mock/GLM".to_string() },
                )),
        );

        // 目标输入 + 启动
        col = col.child(
            div()
                .p_2()
                .flex()
                .flex_col()
                .gap_2()
                .border_b_1()
                .border_color(rgb(crate::theme::Theme::BORDER))
                .child(
                    div()
                        .id("objective-input")
                        .min_h(px(34.))
                        .p_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(crate::theme::Theme::BORDER))
                        .bg(rgb(crate::theme::Theme::BG))
                        .track_focus(&self.input_focus)
                        .on_key_down(cx.listener(Self::on_objective_key))
                        .when(self.input_focus.is_focused(window), |d| {
                            d.border_color(rgb(crate::theme::Theme::ACCENT))
                        })
                        .text_color(rgb(crate::theme::Theme::FG))
                        .child(if self.objective.is_empty() {
                            SharedString::from("输入目标,例如:给 utils 加单元测试并跑通")
                        } else {
                            SharedString::from(self.objective.clone())
                        }),
                )
                .child(
                    div()
                        .id("start-run-btn")
                        .px_3()
                        .py_1()
                        .self_start()
                        .rounded_sm()
                        .bg(rgb(crate::theme::Theme::ACCENT_DIM))
                        .border_1()
                        .border_color(rgb(crate::theme::Theme::ACCENT))
                        .text_color(rgb(crate::theme::Theme::FG))
                        .cursor_pointer()
                        .hover(|d| d.bg(rgb(crate::theme::Theme::BG_ACTIVE)))
                        .child("▶ 启动运行")
                        .on_click(cx.listener(Self::act_start)),
                ),
        );

        // 问答卡片
        if let Some(q) = &self.open_question {
            col = col.child(
                div()
                    .id("question-card")
                    .m_2()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::WARNING))
                    .bg(rgb(crate::theme::Theme::BG_ELEVATED))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_color(rgb(crate::theme::Theme::WARNING))
                            .child(format!("❓ {}", q.question)),
                    )
                    .child(
                        div()
                            .id("answer-input")
                            .min_h(px(28.))
                            .p_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::BORDER))
                            .bg(rgb(crate::theme::Theme::BG))
                            .focusable()
                            .on_key_down(cx.listener(Self::on_answer_key))
                            .child(if self.answer.is_empty() {
                                SharedString::from("输入回答…")
                            } else {
                                SharedString::from(self.answer.clone())
                            }),
                    )
                    .child(
                        div()
                            .id("answer-btn")
                            .px_3()
                            .py_1()
                            .self_start()
                            .rounded_sm()
                            .bg(rgb(crate::theme::Theme::ACCENT_DIM))
                            .cursor_pointer()
                            .child("提交回答")
                            .on_click(cx.listener(Self::act_answer)),
                    ),
            );
        }

        // 任务看板
        let tasks: Vec<(usize, TaskView)> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.clone()))
            .collect();
        let mut board = div()
            .id("task-board")
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(rgb(crate::theme::Theme::BORDER));
        for (_i, t) in tasks {
            let color = Self::status_color(t.status);
            let deps = if t.deps.is_empty() {
                String::new()
            } else {
                format!("← #{}", t.deps.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(" #"))
            };
            let task_id = t.id;
            let blocked = t.status == TaskStatus::Blocked;
            board = board.child(
                div()
                    .id(("task", t.id as u64))
                    .pl_2()
                    .pr_2()
                    .pt_1()
                    .pb_1()
                    .border_b_1()
                    .border_color(rgb(crate::theme::Theme::BORDER))
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
                                    .text_color(rgb(crate::theme::Theme::FG_FAINT))
                                    .child(format!("#{}", t.id)),
                            )
                            .child(
                                div()
                                    .px_1()
                                    .rounded_sm()
                                    .text_color(rgb(color))
                                    .child(t.status.label_cn()),
                            )
                            .child(
                                div()
                                    .text_color(rgb(crate::theme::Theme::FG_FAINT))
                                    .text_size(px(10.))
                                    .child(deps),
                            )
                            .when(blocked, |d| {
                                d.child(
                                    div()
                                        .id(ElementId::Name(format!("unblock-{}", task_id).into()))
                                        .ml_auto()
                                        .px_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(rgb(crate::theme::Theme::DANGER))
                                        .text_color(rgb(crate::theme::Theme::DANGER))
                                        .cursor_pointer()
                                        .child("重置")
                                        .on_click({
                                            cx.listener(move |p: &mut AgentPanel, e, w, cx| {
                                                p.act_unblock(task_id, e, w, cx);
                                            })
                                        }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_color(rgb(crate::theme::Theme::FG))
                            .child(
                                t.spec
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .chars()
                                    .take(90)
                                    .collect::<String>(),
                            ),
                    )
                    .when_some(t.result.clone(), |d, r| {
                        d.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(crate::theme::Theme::FG_FAINT))
                                .child(format!("✓ {}", r.lines().next().unwrap_or("").chars().take(80).collect::<String>())),
                        )
                    }),
            );
        }
        col = col.child(board);

        // 日志
        let log_rows: Vec<(String, String)> = self
            .logs
            .iter()
            .map(|l| (l.worker.clone(), l.text.clone()))
            .collect();
        col.child(
            div()
                .id("agent-log")
                .flex_1()
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(
                    div()
                        .h(px(22.))
                        .flex()
                        .items_center()
                        .px_2()
                        .bg(rgb(crate::theme::Theme::BG_ELEVATED))
                        .text_size(px(10.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(crate::theme::Theme::FG_FAINT))
                        .child("运行日志"),
                )
                .child(uniform_list(
                    "log-list",
                    log_rows.len(),
                    move |range: Range<usize>, _window: &mut Window, _cx: &mut App| {
                        let mut out = Vec::new();
                        for ix in range {
                            if ix >= log_rows.len() {
                                continue;
                            }
                            let (worker, text): (String, String) = log_rows[ix].clone();
                            out.push(
                                div()
                                    .id(("log", ix))
                                    .pl_2()
                                    .pr_2()
                                    .h(px(18.))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .font_family("Consolas")
                                    .text_size(px(11.))
                                    .child(
                                        div()
                                            .w(px(56.))
                                            .text_color(rgb(crate::theme::Theme::ACCENT))
                                            .overflow_hidden()
                                            .child(worker.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_color(rgb(crate::theme::Theme::FG_DIM))
                                            .overflow_hidden()
                                            .child(text.clone()),
                                    ),
                            );
                        }
                        out
                    },
                )
                .flex_1()),
        )
    }
}
