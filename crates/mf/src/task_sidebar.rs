//! 任务侧边栏:按项目分组展示任务,支持新建任务、选择与归档。
//! 打开/关闭项目的入口在 Workspace(关闭需要确认弹窗,通过 close_intent 传递意图)。

use crate::app_ctx::AppCtx;
use crate::project_context::{normalize_project_path, ActivationTarget};
use crate::project_overview::{ProjectOverviewSnapshot, TaskCardOverview};
use crate::task_composer::{ComposerUiEvent, TaskComposer, TaskComposerState};
use gpui::prelude::*;
use gpui::*;
use gpui::{px, AnyElement, Context, EventEmitter, FocusHandle, FontWeight, Window};
use mf_agent::model::TaskStatus;
use std::path::PathBuf;
use std::sync::Arc;

/// TaskSidebar → Workspace 的意图事件(Workspace 统一走 activation seam)。
pub enum TaskSidebarEvent {
    /// 打开系统目录选择器，登记并激活一个新 Project。
    AddProjectRequested,
    Activate(ActivationTarget),
    /// 用户点击「关闭项目」。
    CloseRequested(PathBuf),
    /// 任务已归档:清除上下文中的残留选择。
    TaskArchived(PathBuf, i64),
}

impl EventEmitter<TaskSidebarEvent> for TaskSidebar {}

pub struct TaskSidebar {
    app: Arc<AppCtx>,
    overview: Option<Arc<ProjectOverviewSnapshot>>,
    /// (project_root, task_id);由 Workspace 通过 activation seam 设置。
    pub selected: Option<(PathBuf, i64)>,
    /// 当前前台项目根(用于 header 高亮);由 Workspace 设置。
    foreground: Option<PathBuf>,
    /// 显式新建任务 Composer(打开时存在;Project 必选,无隐式归属)。
    composer: Option<Entity<TaskComposer>>,
    /// Composer 提交失败的用户可见提示。
    composer_error: Option<String>,
    focus_handle: FocusHandle,
}

impl TaskSidebar {
    pub fn new(app: Arc<AppCtx>, cx: &mut Context<Self>) -> TaskSidebar {
        TaskSidebar {
            app,
            overview: None,
            selected: None,
            foreground: None,
            composer: None,
            composer_error: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Workspace 在 activation outcome 后同步选择与前台高亮。
    pub fn set_selection(&mut self, sel: Option<(PathBuf, i64)>, cx: &mut Context<Self>) {
        self.selected = sel;
        cx.notify();
    }

    pub fn set_foreground(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.foreground = root;
        cx.notify();
    }

    pub fn unread_count(&self) -> usize {
        self.overview
            .as_ref()
            .map(|o| {
                o.projects
                    .iter()
                    .flat_map(|p| p.tasks.iter())
                    .filter(|t| t.task.unread)
                    .count()
            })
            .unwrap_or(0)
    }

    /// Workspace 泵推送统一快照(同一 revision,TaskSidebar 不再自己轮询)。
    pub fn set_overview(&mut self, snapshot: Arc<ProjectOverviewSnapshot>, cx: &mut Context<Self>) {
        self.overview = Some(snapshot);
        cx.notify();
    }

    /// 打开显式 Composer:Project 必选(默认当前项目,可更改),Title/Goal 必填。
    fn open_composer(&mut self, cx: &mut Context<Self>) {
        if self.composer.is_some() {
            self.composer = None;
            self.composer_error = None;
            cx.notify();
            return;
        }
        self.composer_error = None;
        let projects: Vec<(PathBuf, String)> = self
            .overview
            .as_ref()
            .map(|o| {
                o.projects
                    .iter()
                    .map(|p| (p.root.clone(), p.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let state = TaskComposerState::new(projects, self.foreground.as_ref());
        let composer = cx.new(|cx| TaskComposer::new(state, cx));
        cx.subscribe(&composer, |s, _, ev: &ComposerUiEvent, cx| match ev {
            ComposerUiEvent::Submit => s.submit_composer(cx),
            ComposerUiEvent::Cancel => {
                s.composer = None;
                s.composer_error = None;
                cx.notify();
            }
        })
        .detach();
        self.composer = Some(composer);
        cx.notify();
    }

    /// 创建任务:只写入 Composer 显式选择的项目,激活 B + 新 task。
    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let Some(composer) = self.composer.clone() else {
            return;
        };
        let state = composer.read(cx).state.clone();
        let app = self.app.clone();
        match state.submit(move |root| app.orchestrator_of(root)) {
            Ok(target) => {
                self.composer = None;
                self.composer_error = None;
                cx.emit(TaskSidebarEvent::Activate(target));
            }
            Err(e) => {
                // 用户可见失败:保留 Composer,标题栏提示
                log::warn!("创建任务失败: {e:#}");
                self.composer_error = Some(format!("{e:#}"));
            }
        }
        cx.notify();
    }

    fn archive_task(&self, root: &PathBuf, task_id: i64, cx: &mut Context<Self>) {
        if let Some(orch) = self.app.orchestrator_of(root) {
            match orch.archive_task(task_id) {
                Ok(()) => cx.emit(TaskSidebarEvent::TaskArchived(root.clone(), task_id)),
                Err(e) => log::warn!("归档失败: {e:#}"),
            }
        }
    }

    fn render_task_row(
        &self,
        row: &TaskCardOverview,
        root: &PathBuf,
        cx: &Context<Self>,
    ) -> AnyElement {
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
        let agents = row.active_runs;
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
            .on_click(cx.listener(move |_s: &mut TaskSidebar, _, _, cx| {
                // 一次点击 = 激活 Task 所属 Project + 选择 Task(由 Workspace 统一联动)
                let (pid, _) = normalize_project_path(&root);
                cx.emit(TaskSidebarEvent::Activate(ActivationTarget::Task {
                    project: pid,
                    task_id: id,
                }));
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
                                s.archive_task(&root_for_archive, id, cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("task-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .py_1();
        let projects: Vec<crate::project_overview::ProjectOverview> = self
            .overview
            .as_ref()
            .map(|o| {
                o.projects
                    .iter()
                    .map(|p| crate::project_overview::ProjectOverview {
                        root: p.root.clone(),
                        name: p.name.clone(),
                        tasks: p
                            .tasks
                            .iter()
                            .map(|t| TaskCardOverview {
                                task: t.task.clone(),
                                active_runs: t.active_runs,
                                open_questions: t.open_questions,
                            })
                            .collect(),
                        active_sessions: p.active_sessions,
                    })
                    .collect()
            })
            .unwrap_or_default();
        for project in &projects {
            let root = project.root.clone();
            let root_close = project.root.clone();
            let name = project.name.clone();
            let close_id = name.clone();
            let is_foreground = self.foreground.as_ref() == Some(&project.root);
            list = list.child(
                div()
                    .id(ElementId::Name(format!("proj-head-{name}").into()))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2p5()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(is_foreground, |d| {
                        d.bg(rgb(crate::theme::Theme::bg_active()))
                    })
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .on_click(cx.listener(move |_s: &mut TaskSidebar, _, _, cx| {
                        // Project header 点击 = 原子激活该项目
                        let (pid, _) = normalize_project_path(&root);
                        cx.emit(TaskSidebarEvent::Activate(ActivationTarget::Project(pid)));
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(if is_foreground {
                                crate::theme::Theme::accent()
                            } else {
                                crate::theme::Theme::fg()
                            }))
                            .child(name.clone()),
                    )
                    .when(project.active_sessions > 0, |d| {
                        d.child(
                            div()
                                .text_size(px(9.5))
                                .text_color(rgb(crate::theme::Theme::success()))
                                .child(format!("●{}", project.active_sessions)),
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
                            .on_click(cx.listener(move |_s: &mut TaskSidebar, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(TaskSidebarEvent::CloseRequested(root_close.clone()));
                                cx.notify();
                            })),
                    ),
            );
            for row in &project.tasks {
                list = list.child(self.render_task_row(row, &project.root, cx));
            }
        }
        if projects.is_empty() {
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .py_1()
                    .child(
                        div()
                            .id("add-project-btn")
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
                            .child("+ 添加项目")
                            .on_click(cx.listener(|_s: &mut TaskSidebar, _, _, cx| {
                                cx.emit(TaskSidebarEvent::AddProjectRequested);
                            })),
                    )
                    .child(
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
                                s.open_composer(cx);
                            })),
                    ),
            )
            .when_some(self.composer.clone(), |d, composer| d.child(composer))
            .when_some(self.composer_error.clone(), |d, err| {
                d.child(
                    div()
                        .id("composer-error")
                        .mx_1p5()
                        .mb_1()
                        .px_2()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::Theme::danger()))
                        .child(err),
                )
            })
            .child(list)
    }
}
