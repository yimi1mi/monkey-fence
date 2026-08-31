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
    /// 一次性临时实例编辑器(`+` 菜单「临时实例…」;不落目录库)。
    temp_editor: Option<Entity<TempInstanceComposer>>,
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
            temp_editor: None,
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
        let mut state = TaskComposerState::new(projects, self.foreground.as_ref());
        // 可分配的全局模板(任务本地默认私有,不进选择列表)
        if let Ok(templates) = self.app.catalog_store.list_templates(false) {
            state.set_templates(templates.into_iter().map(|t| (t.key, t.name)).collect());
        }
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
        // 生产分配链:模板选择 → 当前版本 → assign_workflow(编译+pin+Revision)
        let assign_app = self.app.clone();
        // Composer 的并行风险开关随提交持久化(默认 false;模板分配沿用)
        let unsafe_parallel_choice = state.allow_unsafe_parallel();
        let assign =
            move |root: &PathBuf, task_id: i64, choice: &crate::task_composer::WorkflowChoice| {
                let crate::task_composer::WorkflowChoice::Template(key) = choice else {
                    return Ok(()); // 任务本地:画布稍后创建草稿
                };
                let version = assign_app
                    .catalog_store
                    .template_versions(key)?
                    .into_iter()
                    .next_back()
                    .ok_or_else(|| anyhow::anyhow!("模板 {key} 不存在"))?;
                if let Some(orch) = assign_app.orchestrator_of(root) {
                    // 持久化用户本次的显式选择(重启/重分配沿用;默认拒绝)
                    orch.store.set_task_assign_unsafe_parallel(
                        &root.to_string_lossy(),
                        task_id,
                        unsafe_parallel_choice,
                    )?;
                }
                assign_app.assign_workflow(
                    root,
                    task_id,
                    version.version_id,
                    unsafe_parallel_choice,
                )?;
                Ok(())
            };
        match state.submit_with_workflow(move |root| app.orchestrator_of(root), assign) {
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
        let needs_reasons = row.needs_you_reasons.clone();
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
                            .text_size(crate::theme::ui_px(11.5))
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
                            .text_size(crate::theme::ui_px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(row.task.status.label_cn()),
                    )
                    .when(agents > 0, |d| {
                        d.child(
                            div()
                                .text_size(crate::theme::ui_px(9.5))
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
                            .text_size(crate::theme::ui_px(11.))
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
                    .text_size(crate::theme::ui_px(9.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .when(questions > 0, |d| {
                        d.child(
                            div()
                                .text_color(rgb(crate::theme::Theme::warning()))
                                .child(format!("❓{questions}")),
                        )
                    })
                    .when(!needs_reasons.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(crate::theme::ui_px(9.))
                                .text_color(rgb(crate::theme::Theme::warning()))
                                .child(format!("⚑{}", needs_reasons.len())),
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
    pub(crate) fn build_menu(&self) -> Vec<crate::task_cli_menu::MenuEntry> {
        let contributions = self.app.plugins.contributions();
        let types: Vec<crate::agent_instance_editor::AgentTypeInfo> = contributions
            .agent_types()
            .into_iter()
            .map(
                |(full_contribution_id, src, a)| crate::agent_instance_editor::AgentTypeInfo {
                    id: a.id.clone(),
                    full_contribution_id: full_contribution_id.clone(),
                    name: a.name.clone(),
                    plugin_name: src.plugin_full_id.clone(),
                    plugin_version: src.plugin_version.clone(),
                    content_hash: src.content_hash.clone(),
                    config_schema_fields: Vec::new(),
                    detected: mf_plugins::builtin::detect_on_path(&a.command).is_some(),
                    supports_isolated_config: a.supports_isolated_config,
                    default_command: a.command.clone(),
                    adapter: a.adapter.clone(),
                    yolo_args: mf_plugins::builtin::yolo_args_of(&a.id),
                    modes: a
                        .modes
                        .iter()
                        .filter_map(|m| mf_agent::RunMode::parse(m))
                        .collect(),
                },
            )
            .collect();
        let instances = self
            .app
            .catalog_store
            .list_agent_instances(None)
            .map(|rows| {
                rows.into_iter()
                    .map(|row| {
                        let snapshot = self
                            .app
                            .catalog_store
                            .snapshot_agent_instance(&row.id, None)
                            .ok();
                        crate::agent_instances_view::InstanceListInstance {
                            id: row.id.clone(),
                            name: row.name.clone(),
                            agent_type: row.agent_type.clone(),
                            // 类型名从贡献解析(此前误用实例名)
                            type_name: types
                                .iter()
                                .find(|t| t.id == row.agent_type)
                                .map(|t| t.name.clone())
                                .unwrap_or_else(|| row.agent_type.clone()),
                            enabled: row.enabled,
                            current_version: row.current_version,
                            scope: row.scope,
                            executable: snapshot
                                .as_ref()
                                .map(|s| s.executable.clone())
                                .unwrap_or_default(),
                            run_mode: snapshot
                                .as_ref()
                                .map(|s| s.run_mode)
                                .unwrap_or(mf_agent::RunMode::Interactive),
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        crate::task_cli_menu::build_task_cli_menu(&types, &instances)
    }

    /// 启动 `+` 菜单条目(终端/默认 CLI/实例/临时实例)。
    /// 全部走真实生产链:AppCtx → Agent Adapter → LaunchPlan → PTY。
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
                      external_config: bool,
                      slf: &mut TaskSidebar,
                      cx: &mut Context<TaskSidebar>| {
            match slf
                .app
                .create_ad_hoc_session(&root, task_id, &snapshot, mode, external_config)
            {
                Ok(view) => log::info!("离散会话已启动: {}", view.title),
                Err(e) => log::warn!("离散会话启动失败: {e:#}"),
            }
            slf.cli_menu_task = None;
            cx.notify();
        };
        match entry.kind {
            MenuKind::Terminal => {
                // 普通终端同样经 generic-command 适配器编译 LaunchPlan
                let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
                let snapshot =
                    ad_hoc_snapshot_for("generic-command", "任务终端", &shell, Vec::new());
                launch(snapshot, mf_agent::RunMode::Interactive, false, self, cx);
            }
            MenuKind::DefaultCli => {
                let Some(agent_ref) = entry.agent_ref.clone() else {
                    self.cli_menu_task = None;
                    cx.notify();
                    return;
                };
                let command = default_command_of(&self.app, &agent_ref);
                // Orca 式权限物化:全局 yolo 时默认 CLI 附带 yolo 参数
                let argv = self.app.permission_argv_for(&agent_ref);
                let snapshot = ad_hoc_snapshot_for(&agent_ref, &entry.label, &command, argv);
                // Default CLI 显式意图:只读外部已有配置,绝不写入
                launch(snapshot, mf_agent::RunMode::Interactive, true, self, cx);
            }
            MenuKind::TemporaryInstance => {
                // 一次性编辑器:确认后以临时快照启动,不进入全局实例列表
                let composer = cx.new(|cx| {
                    TempInstanceComposer::new(
                        crate::agent_instance_editor::AgentInstanceEditorState::new(
                            temp_generic_type_info(),
                        ),
                        cx,
                    )
                });
                cx.subscribe(&composer, |s, _, ev: &TempComposerUiEvent, cx| match ev {
                    TempComposerUiEvent::Submit => s.launch_temp_instance(cx),
                    TempComposerUiEvent::Cancel => {
                        s.temp_editor = None;
                        s.cli_menu_task = None;
                        cx.notify();
                    }
                })
                .detach();
                self.temp_editor = Some(composer);
                cx.notify();
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
                        // 已保存实例:隔离启动(冻结配置),不受外部配置影响
                        launch(snapshot, mode, false, self, cx);
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

    /// 提交临时实例:一次性快照直启(不落目录库)。
    fn launch_temp_instance(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.temp_editor.clone() else {
            return;
        };
        let (state, valid) = {
            let e = editor.read(cx);
            (e.state.clone(), e.can_launch())
        };
        let Some((root, task_id)) = self.cli_menu_task.clone() else {
            self.temp_editor = None;
            return;
        };
        if !valid {
            return;
        }
        let snapshot =
            state.to_launch_snapshot(&format!("temp-{}", chrono::Utc::now().timestamp()));
        match self
            .app
            .create_ad_hoc_session(&root, task_id, &snapshot, snapshot.run_mode, false)
        {
            Ok(view) => log::info!("临时实例已启动: {}", view.title),
            Err(e) => log::warn!("临时实例启动失败: {e:#}"),
        }
        self.temp_editor = None;
        self.cli_menu_task = None;
        cx.notify();
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
                                    .text_size(crate::theme::ui_px(10.5))
                                    .text_color(rgb(crate::theme::Theme::fg()))
                                    .child(entry.label.clone()),
                            )
                            .child(
                                div()
                                    .text_size(crate::theme::ui_px(8.))
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
                            .text_size(crate::theme::ui_px(11.))
                            .child(format!("任务 {task_id} · 添加 CLI")),
                    )
                    .child(
                        div()
                            .id("cli-menu-close")
                            .text_size(crate::theme::ui_px(9.))
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
                                needs_you_reasons: t.needs_you_reasons.clone(),
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
                            .text_size(crate::theme::ui_px(10.5))
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
                                .text_size(crate::theme::ui_px(9.5))
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
                            .text_size(crate::theme::ui_px(9.5))
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
                    .text_size(crate::theme::ui_px(11.))
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
                            .text_size(crate::theme::ui_px(10.5))
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
                            .text_size(crate::theme::ui_px(10.5))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child("+ 新建任务")
                            .on_click(cx.listener(|s: &mut TaskSidebar, _, _, cx| {
                                s.open_composer(cx);
                            })),
                    ),
            )
            .when_some(self.composer.clone(), |d, composer| d.child(composer))
            .when_some(self.temp_editor.clone(), |d, editor| d.child(editor))
            .when_some(self.composer_error.clone(), |d, err| {
                d.child(
                    div()
                        .id("composer-error")
                        .mx_1p5()
                        .mb_1()
                        .px_2()
                        .text_size(crate::theme::ui_px(10.))
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
/// `argv` 携带按全局权限模式物化的 yolo 参数(终端入口为空)。
fn ad_hoc_snapshot_for(
    agent_type: &str,
    name: &str,
    executable: &str,
    argv: Vec<String>,
) -> mf_agent::AgentInstanceSnapshot {
    mf_agent::AgentInstanceSnapshot {
        id: format!("adhoc-{agent_type}"),
        name: name.to_string(),
        agent_type: agent_type.to_string(),
        version: 0,
        enabled: true,
        run_mode: mf_agent::RunMode::Interactive,
        executable: executable.to_string(),
        argv,
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
        // 菜单引用是完整贡献 ID;短 id 仅兼容显式 legacy 内置引用
        .find(|(full_id, _, a)| full_id == agent_type || a.id == agent_type)
        .map(|(_, _, a)| a.command)
        .unwrap_or_default()
}

// ---------- 一次性临时实例编辑器(`+` 菜单) ----------

/// 临时实例的通用类型投影:任意可执行文件,经 generic-command 适配器
/// 编译 LaunchPlan 后启动;不要求 PATH 检测(executable 由用户填写)。
fn temp_generic_type_info() -> crate::agent_instance_editor::AgentTypeInfo {
    crate::agent_instance_editor::AgentTypeInfo {
        id: "generic-command".into(),
        full_contribution_id: "generic-command".into(),
        name: "临时实例".into(),
        plugin_name: "内置".into(),
        plugin_version: String::new(),
        content_hash: String::new(),
        config_schema_fields: Vec::new(),
        detected: true,
        supports_isolated_config: false,
        default_command: String::new(),
        adapter: "generic-command".into(),
        yolo_args: None,
        modes: vec![mf_agent::RunMode::Interactive, mf_agent::RunMode::OneShot],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TempField {
    Name,
    Executable,
    Argv,
    Env,
}

/// 一次性编辑器:确认后以临时快照直启,不写目录库。
pub struct TempInstanceComposer {
    pub state: crate::agent_instance_editor::AgentInstanceEditorState,
    active_field: TempField,
    focus_handle: FocusHandle,
    pending_focus: bool,
}

pub enum TempComposerUiEvent {
    Submit,
    Cancel,
}

impl gpui::EventEmitter<TempComposerUiEvent> for TempInstanceComposer {}

impl TempInstanceComposer {
    pub fn new(
        state: crate::agent_instance_editor::AgentInstanceEditorState,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            state,
            active_field: TempField::Name,
            focus_handle: cx.focus_handle(),
            pending_focus: true,
        }
    }

    pub fn can_launch(&self) -> bool {
        !self.state.name.trim().is_empty() && !self.state.executable.trim().is_empty()
    }

    fn field_text(&self, f: TempField) -> String {
        match f {
            TempField::Name => self.state.name.clone(),
            TempField::Executable => self.state.executable.clone(),
            TempField::Argv => self.state.argv_text.clone(),
            TempField::Env => self.state.env_text.clone(),
        }
    }

    fn set_field_text(&mut self, f: TempField, text: &str) {
        match f {
            TempField::Name => self.state.set_name(text),
            TempField::Executable => self.state.set_executable(text),
            TempField::Argv => self.state.set_argv(text),
            TempField::Env => self.state.set_env_lines(text),
        }
    }

    fn next_field(&mut self) {
        self.active_field = match self.active_field {
            TempField::Name => TempField::Executable,
            TempField::Executable => TempField::Argv,
            TempField::Argv => TempField::Env,
            TempField::Env => TempField::Name,
        };
    }

    fn handle_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        if ev.keystroke.key.as_str() == "escape" {
            return false; // Cancel
        }
        if ev.keystroke.key.as_str() == "enter" {
            self.next_field();
            return true;
        }
        if ev.keystroke.key.as_str() == "tab" {
            self.next_field();
            return true;
        }
        if ev.keystroke.key.as_str() == "backspace" {
            let mut t = self.field_text(self.active_field);
            t.pop();
            let f = self.active_field;
            self.set_field_text(f, &t);
            return true;
        }
        if let Some(ch) = ev.keystroke.key_char.as_ref() {
            let mut t = self.field_text(self.active_field);
            if ev.keystroke.key.as_str() == "space" && ch != " " {
                t.push(' ');
            } else {
                t.push_str(ch);
            }
            let f = self.active_field;
            self.set_field_text(f, &t);
        }
        true
    }

    fn field_el(
        &self,
        f: TempField,
        id: &'static str,
        placeholder: &str,
        cx: &Context<Self>,
        window: &Window,
    ) -> gpui::Stateful<gpui::Div> {
        let focused = self.focus_handle.is_focused(window) && self.active_field == f;
        let text = self.field_text(f);
        div()
            .id(id)
            .h(px(22.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(rgb(if focused {
                crate::theme::Theme::accent()
            } else {
                crate::theme::Theme::border()
            }))
            .text_size(crate::theme::ui_px(10.5))
            .cursor_pointer()
            .on_click(
                cx.listener(move |c: &mut TempInstanceComposer, _, window, cx| {
                    c.active_field = f;
                    window.focus(&c.focus_handle, cx);
                    cx.notify();
                }),
            )
            .child(if text.is_empty() {
                div()
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child(placeholder.to_string())
            } else {
                div().text_color(rgb(crate::theme::Theme::fg())).child(text)
            })
    }
}

impl Render for TempInstanceComposer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_focus {
            self.pending_focus = false;
            window.focus(&self.focus_handle, cx);
        }
        let valid = self.can_launch();
        div()
            .id("temp-instance-composer")
            .absolute()
            .left(px(56.))
            .top(px(120.))
            .w(px(340.))
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_lg()
            .border_1()
            .border_color(rgb(crate::theme::Theme::accent_dim()))
            .bg(rgb(crate::theme::Theme::bg_elevated()))
            .shadow_md()
            .track_focus(&self.focus_handle)
            .child(
                div()
                    .text_size(crate::theme::ui_px(11.))
                    .child("临时实例(仅本次任务,不保存)"),
            )
            .child(self.field_el(TempField::Name, "temp-name", "名称(必填)", cx, window))
            .child(self.field_el(
                TempField::Executable,
                "temp-executable",
                "可执行文件(必填,如 cmd.exe / claude)",
                cx,
                window,
            ))
            .child(self.field_el(TempField::Argv, "temp-argv", "参数(空格分隔)", cx, window))
            .child(self.field_el(
                TempField::Env,
                "temp-env",
                "环境变量 KEY=VALUE(每行一个)",
                cx,
                window,
            ))
            .child(
                div()
                    .id("temp-run-mode")
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .text_size(crate::theme::ui_px(10.))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .child(format!(
                        "运行模式:{}(点击切换)",
                        match self.state.run_mode {
                            mf_agent::RunMode::Interactive => "交互",
                            mf_agent::RunMode::OneShot => "一次性",
                        }
                    ))
                    .on_click(cx.listener(|c: &mut TempInstanceComposer, _, _, cx| {
                        c.state.run_mode = match c.state.run_mode {
                            mf_agent::RunMode::Interactive => mf_agent::RunMode::OneShot,
                            mf_agent::RunMode::OneShot => mf_agent::RunMode::Interactive,
                        };
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .gap_1()
                    .justify_end()
                    .child(
                        div()
                            .id("temp-cancel")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(crate::theme::ui_px(10.))
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child("取消(Esc)")
                            .on_click(cx.listener(|_: &mut TempInstanceComposer, _, _, cx| {
                                cx.emit(TempComposerUiEvent::Cancel);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("temp-launch")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .text_size(crate::theme::ui_px(10.))
                            .cursor_pointer()
                            .when(valid, |d| {
                                d.bg(rgb(crate::theme::Theme::accent()))
                                    .text_color(rgb(crate::theme::Theme::bg()))
                            })
                            .when(!valid, |d| {
                                d.border_1()
                                    .border_color(rgb(crate::theme::Theme::border()))
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                            })
                            .child("启动")
                            .on_click(cx.listener(|c: &mut TempInstanceComposer, _, _, cx| {
                                if c.can_launch() {
                                    cx.emit(TempComposerUiEvent::Submit);
                                }
                                cx.notify();
                            })),
                    ),
            )
            .on_key_down(cx.listener(
                |c: &mut TempInstanceComposer, ev: &gpui::KeyDownEvent, _, cx| {
                    if !c.handle_key(ev) {
                        cx.emit(TempComposerUiEvent::Cancel);
                    }
                    cx.stop_propagation();
                    cx.notify();
                },
            ))
    }
}
