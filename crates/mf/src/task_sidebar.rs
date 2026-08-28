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
    /// `+` 菜单:当前打开的任务 (root, task_id)。
    cli_menu_task: Option<(PathBuf, i64)>,
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
            cli_menu_task: None,
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
        let root2 = root.clone();
        let _ = &root2;
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
                    })
                    .child(
                        div()
                            .id(("task-plus", id as u64))
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(px(11.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .hover(|d| {
                                d.bg(rgb(crate::theme::Theme::bg_hover()))
                                    .text_color(rgb(crate::theme::Theme::fg()))
                            })
                            .child("＋")
                            .on_click(cx.listener(move |s: &mut TaskSidebar, _ev, _w, cx| {
                                s.cli_menu_task = Some((root2.clone(), id));
                                cx.notify();
                            })),
                    ),
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

impl TaskSidebar {
    /// 构建 `+` 菜单条目(插件类型 + 目录实例;设计 §10)。
    fn build_menu(&self) -> Vec<crate::task_cli_menu::MenuEntry> {
        let contributions = self.app.plugins.contributions();
        let types: Vec<crate::agent_instance_editor::AgentTypeInfo> = contributions
            .agent_types()
            .into_iter()
            .map(|(src, a)| crate::agent_instance_editor::AgentTypeInfo {
                id: a.id.clone(),
                name: a.name.clone(),
                plugin_name: src.plugin_full_id.clone(),
                detected: mf_plugins::builtin::detect_on_path(&a.command).is_some(),
                supports_isolated_config: a.supports_isolated_config,
                default_command: a.command.clone(),
                adapter: a.adapter.clone(),
                modes: a
                    .modes
                    .iter()
                    .filter_map(|m| mf_agent::RunMode::parse(m))
                    .collect(),
            })
            .collect();
        let instances = self
            .app
            .catalog_store
            .list_agent_instances(None)
            .map(|rows| {
                rows.into_iter()
                    .map(|row| crate::agent_instances_view::InstanceListInstance {
                        id: row.id.clone(),
                        name: row.name.clone(),
                        agent_type: row.agent_type.clone(),
                        type_name: row.name.clone(),
                        enabled: row.enabled,
                        current_version: row.current_version,
                        scope: row.scope,
                        executable: String::new(),
                        run_mode: mf_agent::RunMode::Interactive,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        crate::task_cli_menu::build_task_cli_menu(&types, &instances)
    }

    /// 启动 `+` 菜单条目(终端/默认 CLI/实例)。
    fn launch_menu_entry(
        &mut self,
        entry: &crate::task_cli_menu::MenuEntry,
        cx: &mut Context<Self>,
    ) {
        use crate::task_cli_menu::MenuKind;
        let Some((root, task_id)) = self.cli_menu_task.clone() else {
            return;
        };
        let launch = |snapshot: mf_agent::AgentInstanceSnapshot,
                      mode: mf_agent::RunMode,
                      slf: &mut TaskSidebar,
                      cx: &mut Context<TaskSidebar>| {
            match slf
                .app
                .create_ad_hoc_session(&root, task_id, &snapshot, mode)
            {
                Ok(view) => log::info!("离散会话已启动: {}", view.title),
                Err(e) => log::warn!("离散会话启动失败: {e:#}"),
            }
            slf.cli_menu_task = None;
            cx.notify();
        };
        match entry.kind {
            MenuKind::Terminal => {
                let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
                let snapshot = ad_hoc_snapshot_for("terminal", "任务终端", &shell);
                launch(snapshot, mf_agent::RunMode::Interactive, self, cx);
            }
            MenuKind::DefaultCli | MenuKind::TemporaryInstance => {
                let Some(agent_ref) = entry.agent_ref.clone() else {
                    self.cli_menu_task = None;
                    cx.notify();
                    return;
                };
                let command = default_command_of(&self.app, &agent_ref);
                let snapshot = ad_hoc_snapshot_for(&agent_ref, &entry.label, &command);
                launch(snapshot, mf_agent::RunMode::Interactive, self, cx);
            }
            MenuKind::AgentInstance => {
                let Some(instance_id) = entry.agent_ref.clone() else {
                    self.cli_menu_task = None;
                    cx.notify();
                    return;
                };
                match self
                    .app
                    .catalog_store
                    .snapshot_agent_instance(&instance_id, None)
                {
                    Ok(snapshot) => {
                        let mode = snapshot.run_mode;
                        launch(snapshot, mode, self, cx);
                    }
                    Err(e) => {
                        log::warn!("读取实例失败: {e:#}");
                        self.cli_menu_task = None;
                        cx.notify();
                    }
                }
            }
        }
    }

    fn render_cli_menu(&self, cx: &Context<Self>) -> AnyElement {
        let Some((_root, task_id)) = &self.cli_menu_task else {
            return div().into_any_element();
        };
        let entries = self.build_menu();
        let mut list = div().flex().flex_col().gap_1();
        for (idx, entry) in entries.iter().enumerate() {
            let owned = entry.clone();
            list = list.child(
                div()
                    .id(("cli-menu-entry", idx as u64))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(rgb(crate::theme::Theme::fg()))
                                    .child(entry.label.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(8.))
                                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                                    .child(entry.note.clone()),
                            ),
                    )
                    .on_click(cx.listener(move |s: &mut TaskSidebar, _ev, _w, cx| {
                        s.launch_menu_entry(&owned, cx);
                    })),
            );
        }
        div()
            .id("cli-menu-popover")
            .absolute()
            .left(px(56.))
            .top(px(120.))
            .w(px(320.))
            .max_h(px(420.))
            .overflow_y_scroll()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_lg()
            .border_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg_elevated()))
            .shadow_md()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(11.))
                            .child(format!("任务 {task_id} · 添加 CLI")),
                    )
                    .child(
                        div()
                            .id("cli-menu-close")
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child("关闭")
                            .on_click(cx.listener(|s: &mut TaskSidebar, _ev, _w, cx| {
                                s.cli_menu_task = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(list)
            .into_any_element()
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
            .when(self.cli_menu_task.is_some(), |d| {
                d.child(self.render_cli_menu(cx))
            })
    }
}

/// 构造离散会话用的最小实例快照(默认 CLI / 终端入口)。
fn ad_hoc_snapshot_for(
    agent_type: &str,
    name: &str,
    executable: &str,
) -> mf_agent::AgentInstanceSnapshot {
    mf_agent::AgentInstanceSnapshot {
        id: format!("adhoc-{agent_type}"),
        name: name.to_string(),
        agent_type: agent_type.to_string(),
        version: 0,
        enabled: true,
        run_mode: mf_agent::RunMode::Interactive,
        executable: executable.to_string(),
        argv: vec![],
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({ "completion": "manual" }),
        sealed_secret_ids: vec![],
    }
}

/// agent type 的默认命令(贡献声明)。
fn default_command_of(app: &std::sync::Arc<crate::app_ctx::AppCtx>, agent_type: &str) -> String {
    app.plugins
        .contributions()
        .agent_types()
        .into_iter()
        .find(|(_, a)| a.id == agent_type)
        .map(|(_, a)| a.command)
        .unwrap_or_default()
}
