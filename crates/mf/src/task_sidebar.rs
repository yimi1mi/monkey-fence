//! 任务侧边栏:按项目分组展示任务,支持新建任务、选择与归档。
//! 打开/关闭项目的入口在 Workspace(关闭需要确认弹窗,通过 close_intent 传递意图)。

use crate::app_ctx::AppCtx;
use gpui::prelude::*;
use gpui::*;
use gpui::{px, AnyElement, Context, FocusHandle, FontWeight, Window};
use mf_agent::model::{SessionStatus, TaskStatus, TaskView};
use std::path::PathBuf;
use std::sync::Arc;

struct TaskRow {
    task: TaskView,
    active_agents: usize,
    open_questions: usize,
}

struct ProjectAgg {
    root: PathBuf,
    name: String,
    tasks: Vec<TaskRow>,
    active_agents: usize,
}

pub struct TaskSidebar {
    app: Arc<AppCtx>,
    projects: Vec<ProjectAgg>,
    /// (project_root, task_id)
    pub selected: Option<(PathBuf, i64)>,
    /// 用户点击「关闭项目」的意图(Workspace 读取后弹确认框)。
    pub close_intent: Option<PathBuf>,
    new_task_title: String,
    new_task_open: bool,
    active_field: bool,
    focus_handle: FocusHandle,
    pending_focus: bool,
}

impl TaskSidebar {
    pub fn new(app: Arc<AppCtx>, cx: &mut Context<Self>) -> TaskSidebar {
        let sidebar = TaskSidebar {
            app,
            projects: Vec::new(),
            selected: None,
            close_intent: None,
            new_task_title: String::new(),
            new_task_open: false,
            active_field: false,
            focus_handle: cx.focus_handle(),
            pending_focus: false,
        };
        sidebar.start_polling(cx);
        sidebar
    }

    pub fn unread_count(&self) -> usize {
        self.projects
            .iter()
            .flat_map(|p| p.tasks.iter())
            .filter(|t| t.task.unread)
            .count()
    }

    fn start_polling(&self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| loop {
            let app = app.clone();
            let agg = cx
                .background_executor()
                .spawn(async move { poll_projects(&app) })
                .await;
            let alive = this
                .update(cx, |s, cx| {
                    s.projects = agg;
                    cx.notify();
                })
                .is_ok();
            if !alive {
                return;
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(700))
                .await;
        })
        .detach();
    }

    fn create_task(&mut self, cx: &mut Context<Self>) {
        let Some(project_root) = self
            .selected
            .as_ref()
            .map(|(r, _)| r.clone())
            .or_else(|| self.projects.first().map(|p| p.root.clone()))
        else {
            return;
        };
        let title = self.new_task_title.trim().to_string();
        if title.is_empty() {
            return;
        }
        if let Some(orch) = self.app.orchestrator_of(&project_root) {
            if let Ok(task) = orch.create_task(&title, &title) {
                self.selected = Some((project_root, task.id));
            }
            self.new_task_title.clear();
        }
        self.new_task_open = false;
        cx.notify();
    }

    fn archive_task(&self, root: &PathBuf, task_id: i64) {
        if let Some(orch) = self.app.orchestrator_of(root) {
            if let Err(e) = orch.archive_task(task_id) {
                log::warn!("归档失败: {e:#}");
            }
        }
    }

    fn render_task_row(&self, row: &TaskRow, root: &PathBuf, cx: &Context<Self>) -> AnyElement {
        let id = row.task.id;
        let root = root.clone();
        let root_for_archive = root.clone();
        let selected = self
            .selected
            .as_ref()
            .map(|(r, i)| r == &root && *i == id)
            .unwrap_or(false);
        let color = task_color(row.task.status);
        let title: String = row.task.title.chars().take(40).collect();
        let unread = row.task.unread;
        let agents = row.active_agents;
        let questions = row.open_questions;
        div()
            .id(("task-row", id as u64))
            .ml_2()
            .mr_1p5()
            .my_0p5()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |d| d.bg(rgb(crate::theme::Theme::bg_active())))
            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
            .on_click(cx.listener(move |s: &mut TaskSidebar, _, _, cx| {
                s.selected = Some((root.clone(), id));
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(div().size(px(7.)).rounded_full().bg(rgb(color)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(rgb(if selected {
                                crate::theme::Theme::fg()
                            } else {
                                crate::theme::Theme::fg_dim()
                            }))
                            .child(title),
                    )
                    .when(unread, |d| {
                        d.child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(rgb(crate::theme::Theme::warning())),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(row.task.status.label_cn()),
                    )
                    .when(agents > 0, |d| {
                        d.child(
                            div()
                                .text_size(px(9.5))
                                .text_color(rgb(crate::theme::Theme::success()))
                                .child(format!("●{agents}")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .mt_0p5()
                    .text_size(px(9.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .when(questions > 0, |d| {
                        d.child(
                            div()
                                .text_color(rgb(crate::theme::Theme::warning()))
                                .child(format!("❓{questions}")),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id(("task-archive", id as u64))
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child("归档")
                            .on_click(cx.listener(move |s: &mut TaskSidebar, _, _, cx| {
                                cx.stop_propagation();
                                s.archive_task(&root_for_archive, id);
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

fn poll_projects(app: &Arc<AppCtx>) -> Vec<ProjectAgg> {
    // 锁内只取快照(orchestrator 是 Arc),DB 查询在锁外进行,
    // 避免与 UI 线程(active_root 等)争抢 projects 锁
    let snapshot: Vec<(PathBuf, Arc<mf_agent::orchestrator::Orchestrator>)> = {
        let projects = app.projects.lock();
        projects
            .iter()
            .map(|p| (p.root.clone(), p.orchestrator.clone()))
            .collect()
    };
    snapshot
        .iter()
        .map(|(root, orch)| {
            let orch = &*orch;
            let tasks = orch.tasks().unwrap_or_default();
            let runs = orch.store.running_runs().unwrap_or_default();
            let sessions = orch.sessions().unwrap_or_default();
            let rows = tasks
                .into_iter()
                .map(|t| {
                    let active_agents = runs.iter().filter(|r| r.task_id == t.id).count();
                    let open_questions = orch
                        .store
                        .open_questions(Some(t.id))
                        .map(|q| q.len())
                        .unwrap_or(0);
                    TaskRow {
                        task: t,
                        active_agents,
                        open_questions,
                    }
                })
                .collect();
            let active_agents = sessions
                .iter()
                .filter(|s| {
                    matches!(
                        s.status,
                        SessionStatus::Working | SessionStatus::Starting | SessionStatus::Waiting
                    )
                })
                .count();
            ProjectAgg {
                root: root.clone(),
                name: root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| root.display().to_string()),
                tasks: rows,
                active_agents,
            }
        })
        .collect()
}

fn task_color(status: TaskStatus) -> u32 {
    match status {
        TaskStatus::Draft => 0x8a8a8a,
        TaskStatus::Ready => crate::theme::Theme::accent(),
        TaskStatus::Running => crate::theme::Theme::success(),
        TaskStatus::NeedsYou => crate::theme::Theme::warning(),
        TaskStatus::Succeeded => crate::theme::Theme::success(),
        TaskStatus::Failed => crate::theme::Theme::danger(),
        TaskStatus::Cancelled => 0x8a8a8a,
        TaskStatus::Archived => 0x8a8a8a,
    }
}

impl Render for TaskSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_focus {
            self.pending_focus = false;
            window.focus(&self.focus_handle, cx);
        }
        let mut list = div()
            .id("task-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .py_1();
        for project in &self.projects {
            let root = project.root.clone();
            let name = project.name.clone();
            let close_id = name.clone();
            list = list.child(
                div()
                    .id(ElementId::Name(format!("proj-head-{name}").into()))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2p5()
                    .py_1()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child(name.clone()),
                    )
                    .when(project.active_agents > 0, |d| {
                        d.child(
                            div()
                                .text_size(px(9.5))
                                .text_color(rgb(crate::theme::Theme::success()))
                                .child(format!("●{}", project.active_agents)),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id(ElementId::Name(format!("proj-close-{close_id}").into()))
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_size(px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child("✕")
                            .on_click(cx.listener(move |s: &mut TaskSidebar, _, _, cx| {
                                s.close_intent = Some(root.clone());
                                cx.notify();
                            })),
                    ),
            );
            for row in &project.tasks {
                list = list.child(self.render_task_row(row, &project.root, cx));
            }
        }
        if self.projects.is_empty() {
            list = list.child(
                div()
                    .p_3()
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("尚未打开项目;Ctrl+Shift+O 打开文件夹。"),
            );
        }

        div()
            .id("task-sidebar")
            .size_full()
            .flex()
            .flex_col()
            .key_context("TaskSidebar")
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|s: &mut TaskSidebar, ev: &gpui::KeyDownEvent, window, cx| {
                    if !s.new_task_open || !s.active_field || !s.focus_handle.is_focused(window) {
                        return;
                    }
                    match ev.keystroke.key.as_str() {
                        "enter" => s.create_task(cx),
                        "escape" => {
                            s.new_task_open = false;
                            s.new_task_title.clear();
                        }
                        "backspace" => {
                            s.new_task_title.pop();
                        }
                        _ => {
                            if let Some(ch) = ev.keystroke.key_char.as_ref() {
                                s.new_task_title.push_str(ch);
                            }
                        }
                    }
                    cx.notify();
                }),
            )
            .child(
                div().flex().items_center().gap_1().px_1p5().py_1().child(
                    div()
                        .id("new-task-btn")
                        .flex_1()
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(crate::theme::Theme::border()))
                        .cursor_pointer()
                        .text_size(px(10.5))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                        .child("+ 新建任务")
                        .on_click(cx.listener(|s: &mut TaskSidebar, _, _, cx| {
                            s.new_task_open = !s.new_task_open;
                            s.active_field = s.new_task_open;
                            s.pending_focus = s.new_task_open;
                            cx.notify();
                        })),
                ),
            )
            .when(self.new_task_open, |d| {
                d.child(
                    div()
                        .id("new-task-input")
                        .mx_1p5()
                        .mb_1()
                        .h(px(26.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(
                            if self.active_field && self.focus_handle.is_focused(window) {
                                crate::theme::Theme::accent()
                            } else {
                                crate::theme::Theme::border()
                            },
                        ))
                        .text_size(px(11.))
                        .child(if self.new_task_title.is_empty() {
                            div()
                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                .child("任务目标…")
                        } else {
                            div()
                                .text_color(rgb(crate::theme::Theme::fg()))
                                .child(self.new_task_title.clone())
                        }),
                )
            })
            .child(list)
    }
}
