//! 工作流运行页(ADR 0004 / Task 7):以「一次运行(内部 Task)」为单位
//! 的运行列表 + 右侧 RunMonitor 复用。
//!
//! - 左侧过滤:需要你 / 运行中 / 最近完成;
//! - 只把存在 Pipeline Revision 的 Task 投影为工作流运行;
//! - 选中 attention 项时定位 RunMonitor 到优先处理节点;
//! - 徽标/计数完全来自统一 overview 快照(attention_runs),
//!   运行完成或人工动作后由事件 → 重建统一更新,不手工减计数。

use crate::project_overview::{ProjectOverviewSnapshot, WorkflowRunAttention};
use gpui::prelude::*;
use gpui::*;
use gpui::{px, AnyElement, Context, FocusHandle, Window};
use mf_agent::TaskStatus;
use std::path::PathBuf;
use std::sync::Arc;

/// 左侧过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFilter {
    /// 需要你(attention_runs 命中的运行)。
    NeedsYou,
    /// 运行中。
    Running,
    /// 最近完成(成功/失败/取消,按更新时间倒序)。
    RecentlyCompleted,
}

pub enum WorkflowRunsPageEvent {
    ActivateRun {
        project_root: PathBuf,
        task_id: i64,
        focus_step_id: Option<i64>,
    },
    OpenSession {
        project_root: PathBuf,
        task_id: i64,
        session_id: i64,
        run_id: i64,
        is_http: bool,
    },
}

impl EventEmitter<WorkflowRunsPageEvent> for WorkflowRunsPage {}

impl RunFilter {
    pub fn label(self) -> &'static str {
        match self {
            RunFilter::NeedsYou => "需要你",
            RunFilter::Running => "运行中",
            RunFilter::RecentlyCompleted => "最近完成",
        }
    }
}

/// 列表项(一次运行)。
#[derive(Debug, Clone)]
pub struct WorkflowRunItem {
    pub project_root: PathBuf,
    pub project_name: String,
    pub task_id: i64,
    pub task_title: String,
    pub status: TaskStatus,
    /// 该运行的「需要你」投影(命中过滤用)。
    pub attention: Option<WorkflowRunAttention>,
    pub updated_at: String,
}

/// 工作流运行页(独立 GPUI 实体;Runs 页签的宿主)。
pub struct WorkflowRunsPage {
    pub app: Arc<crate::app_ctx::AppCtx>,
    pub filter: RunFilter,
    pub items: Vec<WorkflowRunItem>,
    /// 当前选中 (project_root, task_id)。
    pub selected: Option<(PathBuf, i64)>,
    /// 右侧 RunMonitor(复用 DAG 与节点动作)。
    pub monitor: gpui::Entity<crate::run_monitor::RunMonitor>,
    status: String,
    focus_handle: FocusHandle,
}

impl WorkflowRunsPage {
    pub fn new(app: Arc<crate::app_ctx::AppCtx>, cx: &mut Context<Self>) -> WorkflowRunsPage {
        let monitor = cx.new(|cx| crate::run_monitor::RunMonitor::new(app.clone(), cx));
        cx.subscribe(
            &monitor,
            |_page, _, event: &crate::run_monitor::RunMonitorEvent, cx| match event {
                crate::run_monitor::RunMonitorEvent::OpenSession {
                    project_root,
                    task_id,
                    session_id,
                    run_id,
                    is_http,
                } => cx.emit(WorkflowRunsPageEvent::OpenSession {
                    project_root: project_root.clone(),
                    task_id: *task_id,
                    session_id: *session_id,
                    run_id: *run_id,
                    is_http: *is_http,
                }),
            },
        )
        .detach();
        WorkflowRunsPage {
            app,
            filter: RunFilter::NeedsYou,
            items: Vec::new(),
            selected: None,
            monitor,
            status: String::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// 统一快照到达:重建列表(保持选择;消失则回落第一项)。
    pub fn set_overview(&mut self, snapshot: Arc<ProjectOverviewSnapshot>, cx: &mut Context<Self>) {
        let attention_of: std::collections::HashMap<(PathBuf, i64), WorkflowRunAttention> =
            snapshot
                .attention_runs
                .iter()
                .map(|a| ((a.project_root.clone(), a.task_id), a.clone()))
                .collect();
        let mut items = Vec::new();
        for project in &snapshot.projects {
            for card in &project.tasks {
                // 只把存在 Pipeline Revision 的 Task 投影为工作流运行
                if card.task.revision_count == 0 {
                    continue;
                }
                items.push(WorkflowRunItem {
                    project_root: project.root.clone(),
                    project_name: project.name.clone(),
                    task_id: card.task.id,
                    task_title: card.task.title.clone(),
                    status: card.task.status,
                    attention: attention_of
                        .get(&(project.root.clone(), card.task.id))
                        .cloned(),
                    updated_at: card.task.updated_at.clone(),
                });
            }
        }
        self.items = items;
        if !self
            .filtered_items()
            .iter()
            .any(|i| Some((i.project_root.clone(), i.task_id)) == self.selected)
        {
            let first = self.filtered_items().first().cloned();
            if let Some(item) = first {
                self.select_run(&item.project_root, item.task_id, None, cx);
            } else {
                self.selected = None;
                self.monitor.update(cx, |m, cx| m.set_task(None, cx));
            }
        }
        // 后台事件持续可见:右侧监控同步刷新
        self.monitor.update(cx, |m, cx| m.refresh_snapshot(cx));
        cx.notify();
    }

    /// 当前过滤下的列表项(稳定排序:需要你优先,再按更新时间倒序)。
    pub fn filtered_items(&self) -> Vec<WorkflowRunItem> {
        let mut items: Vec<WorkflowRunItem> = self
            .items
            .iter()
            .filter(|i| match self.filter {
                RunFilter::NeedsYou => i.attention.is_some(),
                RunFilter::Running => {
                    matches!(i.status, TaskStatus::Running | TaskStatus::NeedsYou)
                }
                RunFilter::RecentlyCompleted => matches!(
                    i.status,
                    TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
                ),
            })
            .cloned()
            .collect();
        items.sort_by(|a, b| {
            let key = |i: &WorkflowRunItem| {
                (
                    i.attention.is_none(),
                    std::cmp::Reverse(i.updated_at.clone()),
                )
            };
            key(a).cmp(&key(b)).then_with(|| {
                (a.project_root.clone(), a.task_id).cmp(&(b.project_root.clone(), b.task_id))
            })
        });
        items
    }

    /// 选中一次运行;attention 命中时定位优先处理节点。
    pub fn select_run(
        &mut self,
        project_root: &PathBuf,
        task_id: i64,
        focus_step_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some((project_root.clone(), task_id));
        self.monitor.update(cx, |m, cx| {
            m.set_task(Some((project_root.clone(), task_id)), cx)
        });
        if let Some(step_id) = focus_step_id {
            self.monitor.update(cx, |m, cx| m.focus_step(step_id, cx));
        }
        cx.notify();
    }

    /// 「需要你」直达:选中该运行并定位优先处理节点。
    pub fn open_attention(&mut self, attention: &WorkflowRunAttention, cx: &mut Context<Self>) {
        self.filter = RunFilter::NeedsYou;
        self.select_run(
            &attention.project_root,
            attention.task_id,
            attention.focus_step_id,
            cx,
        );
        self.status = format!("已定位「{}」的优先处理节点", attention.task_title);
    }

    /// 切换过滤(切到空过滤时回落第一项)。
    pub fn set_filter(&mut self, filter: RunFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        let first = self.filtered_items().first().cloned();
        if let Some(item) = first {
            self.select_run(&item.project_root, item.task_id, None, cx);
        } else {
            self.selected = None;
            self.monitor
                .update(cx, |monitor, cx| monitor.set_task(None, cx));
        }
        cx.notify();
    }

    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// 任务推送(Workspace 上下文;与列表选择协同)。
    pub fn set_task(&mut self, task: Option<(PathBuf, i64)>, cx: &mut Context<Self>) {
        self.monitor
            .update(cx, |m, cx| m.set_task(task.clone(), cx));
        if let Some((root, id)) = task {
            if self.selected.is_none() {
                self.selected = Some((root, id));
            }
        }
        cx.notify();
    }

    fn render_list(&self, cx: &Context<Self>) -> AnyElement {
        let items = self.filtered_items();
        let mut list = gpui::div().flex().flex_col().gap_1();
        if items.is_empty() {
            list = list.child(
                gpui::div()
                    .text_size(crate::theme::ui_px(9.5))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child(match self.filter {
                        RunFilter::NeedsYou => "没有需要你处理的运行",
                        RunFilter::Running => "没有进行中的运行",
                        RunFilter::RecentlyCompleted => "还没有已完成的运行",
                    }),
            );
        }
        for item in &items {
            let selected = self.selected == Some((item.project_root.clone(), item.task_id));
            let (root, task_id, attention) = (
                item.project_root.clone(),
                item.task_id,
                item.attention.clone(),
            );
            let badge = attention
                .as_ref()
                .map(|a| format!("需要你 ×{}", a.reason_count.max(1)));
            let status_label = status_label(item.status);
            let title = item.task_title.clone();
            let project_name = item.project_name.clone();
            list = list.child(
                gpui::div()
                    .id(gpui::ElementId::Name(
                        format!("wf-run-{}-{}", project_name, task_id).into(),
                    ))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if selected {
                        crate::theme::Theme::accent()
                    } else {
                        crate::theme::Theme::border()
                    }))
                    .bg(rgb(if selected {
                        crate::theme::Theme::bg_active()
                    } else {
                        crate::theme::Theme::bg_elevated()
                    }))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .child(
                        gpui::div()
                            .text_size(crate::theme::ui_px(10.5))
                            .child(title),
                    )
                    .child(
                        gpui::div()
                            .flex()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .text_size(crate::theme::ui_px(8.5))
                                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                                    .child(format!("{project_name} · {status_label}")),
                            )
                            .children(badge.map(|b| {
                                gpui::div()
                                    .text_size(crate::theme::ui_px(8.5))
                                    .text_color(rgb(crate::theme::Theme::warning()))
                                    .child(b)
                            })),
                    )
                    .on_click(
                        cx.listener(move |_page: &mut WorkflowRunsPage, _ev, _w, cx| {
                            let focus = attention.as_ref().and_then(|a| a.focus_step_id);
                            cx.emit(WorkflowRunsPageEvent::ActivateRun {
                                project_root: root.clone(),
                                task_id,
                                focus_step_id: focus,
                            });
                        }),
                    ),
            );
        }
        gpui::div()
            .id("wf-runs-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }
}

fn status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Draft => "草稿",
        TaskStatus::Ready => "就绪",
        TaskStatus::Running => "运行中",
        TaskStatus::NeedsYou => "需要你",
        TaskStatus::Succeeded => "成功",
        TaskStatus::Failed => "失败",
        TaskStatus::Cancelled => "已取消",
        TaskStatus::Archived => "已归档",
    }
}

impl Render for WorkflowRunsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filters: Vec<RunFilter> = vec![
            RunFilter::NeedsYou,
            RunFilter::Running,
            RunFilter::RecentlyCompleted,
        ];
        let mut filter_row = gpui::div().flex().gap_1().px_2().py_1();
        for f in filters {
            let active = self.filter == f;
            filter_row = filter_row.child(
                gpui::div()
                    .id(gpui::ElementId::Name(
                        format!("wf-runs-filter-{}", f.label()).into(),
                    ))
                    .px_2()
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if active {
                        crate::theme::Theme::accent()
                    } else {
                        crate::theme::Theme::border()
                    }))
                    .text_size(crate::theme::ui_px(9.))
                    .text_color(rgb(if active {
                        crate::theme::Theme::accent()
                    } else {
                        crate::theme::Theme::fg_dim()
                    }))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .child(f.label())
                    .on_click(
                        cx.listener(move |page: &mut WorkflowRunsPage, _ev, _w, cx| {
                            page.set_filter(f, cx);
                        }),
                    ),
            );
        }
        let list = self.render_list(cx);
        let monitor = self.monitor.clone();
        let status = self.status.clone();
        let _ = &self.focus_handle;
        gpui::div()
            .id("workflow-runs-page")
            .size_full()
            .flex()
            .gap_1()
            .p_2()
            .child(
                gpui::div()
                    .w(px(280.))
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        gpui::div()
                            .text_size(crate::theme::ui_px(11.))
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child("运行"),
                    )
                    .child(filter_row)
                    .child(list),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(monitor)
                    .when(!status.is_empty(), |d| {
                        d.child(
                            gpui::div()
                                .text_size(crate::theme::ui_px(9.))
                                .text_color(rgb(crate::theme::Theme::fg_dim()))
                                .child(status),
                        )
                    }),
            )
    }
}
