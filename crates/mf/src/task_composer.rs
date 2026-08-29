//! 显式新建任务 Composer:Project 必选、Title/Goal 必填,
//! 取代旧的「已选项目,否则第一个项目」隐式归属规则(ADR 0001:Task 不绑定 VCS)。
//!
//! 纯逻辑(`TaskComposerState`)与渲染分离,便于自动化测试。

use crate::project_context::{normalize_project_path, ActivationTarget};
use gpui::prelude::*;
use gpui::*;
use gpui::{px, AnyElement, Context, FocusHandle, Window};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerField {
    Title,
    Goal,
}

/// Composer 状态机(纯逻辑,可测试)。
#[derive(Clone)]
pub struct TaskComposerState {
    /// (root, 显示名)
    projects: Vec<(PathBuf, String)>,
    selected_project: Option<usize>,
    title: String,
    goal: String,
    /// goal 是否仍跟随 title(用户未手工编辑)
    goal_follows_title: bool,
    /// 工作流分配选择(模板键或任务本地新建;设计 §11.3)。
    workflow_choice: WorkflowChoice,
    /// 可分配的全局模板 (key, 名称);由宿主从目录库注入。
    templates: Vec<(String, String)>,
}

/// 任务创建的工作流分配。
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowChoice {
    /// 任务本地工作流(默认;私有,可另存为模板)。
    TaskLocal,
    /// 分配已有全局模板(template_key)。
    Template(String),
}

impl TaskComposerState {
    pub fn new(projects: Vec<(PathBuf, String)>, default_project: Option<&PathBuf>) -> Self {
        let selected_project =
            default_project.and_then(|d| projects.iter().position(|(r, _)| r == d));
        Self {
            projects,
            selected_project,
            title: String::new(),
            goal: String::new(),
            goal_follows_title: true,
            workflow_choice: WorkflowChoice::TaskLocal,
            templates: Vec::new(),
        }
    }

    /// 注入可分配模板(渲染选项;提交校验只认已注入的模板)。
    pub fn set_templates(&mut self, templates: Vec<(String, String)>) {
        // 选择失效时回落任务本地
        if let WorkflowChoice::Template(key) = &self.workflow_choice {
            if !templates.iter().any(|(k, _)| k == key) {
                self.workflow_choice = WorkflowChoice::TaskLocal;
            }
        }
        self.templates = templates;
    }

    /// 渲染标签(工作流行)。
    pub fn workflow_choice_label(&self) -> String {
        match &self.workflow_choice {
            WorkflowChoice::TaskLocal => "工作流:任务本地(新建,默认私有)".into(),
            WorkflowChoice::Template(key) => {
                let name = self
                    .templates
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| key.clone());
                format!("工作流:模板 {name}")
            }
        }
    }

    /// 点击循环:任务本地 → 各模板 → 任务本地。
    pub fn cycle_workflow(&mut self) {
        self.workflow_choice = match &self.workflow_choice {
            WorkflowChoice::TaskLocal => self
                .templates
                .first()
                .map(|(k, _)| WorkflowChoice::Template(k.clone()))
                .unwrap_or(WorkflowChoice::TaskLocal),
            WorkflowChoice::Template(key) => {
                match self.templates.iter().position(|(k, _)| k == key) {
                    Some(i) if i + 1 < self.templates.len() => {
                        WorkflowChoice::Template(self.templates[i + 1].0.clone())
                    }
                    _ => WorkflowChoice::TaskLocal,
                }
            }
        };
    }

    pub fn select_next_project(&mut self) {
        if self.projects.is_empty() {
            return;
        }
        self.selected_project = Some(match self.selected_project {
            Some(i) if i + 1 < self.projects.len() => i + 1,
            _ => 0,
        });
    }

    pub fn selected_project(&self) -> Option<&PathBuf> {
        self.selected_project
            .and_then(|i| self.projects.get(i))
            .map(|(r, _)| r)
    }

    pub fn selected_project_name(&self) -> &str {
        self.selected_project
            .and_then(|i| self.projects.get(i))
            .map(|(_, n)| n.as_str())
            .unwrap_or("(无项目)")
    }

    pub fn set_title(&mut self, title: &str) {
        self.title = title.to_string();
        if self.goal_follows_title {
            self.goal = self.title.clone();
        }
    }

    pub fn set_goal(&mut self, goal: &str) {
        self.goal = goal.to_string();
        self.goal_follows_title = false;
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn goal(&self) -> &str {
        &self.goal
    }

    /// 提交校验:Project 必选、Title/Goal 必填(无当前 Project 时不能提交)。
    pub fn workflow_choice(&self) -> &WorkflowChoice {
        &self.workflow_choice
    }

    /// 选择工作流:模板键空串 = 任务本地。
    pub fn select_workflow(&mut self, template_key: &str) {
        self.workflow_choice = if template_key.is_empty() {
            WorkflowChoice::TaskLocal
        } else {
            WorkflowChoice::Template(template_key.to_string())
        };
    }

    /// 分配选项列表(供渲染)。
    pub fn workflow_options(templates: &[(String, bool)]) -> Vec<String> {
        templates
            .iter()
            .filter(|(_, local)| !local)
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn can_submit(&self) -> bool {
        self.selected_project.is_some()
            && !self.title.trim().is_empty()
            && !self.goal.trim().is_empty()
    }

    /// 创建任务并返回「目标项目 + 新 task id」的 ActivationTarget。
    /// orchestrator 由调用方解析(AppCtx 在 GUI,测试注入直连)。
    pub fn submit<F>(&self, resolve: F) -> anyhow::Result<ActivationTarget>
    where
        F: Fn(&PathBuf) -> Option<Arc<mf_agent::orchestrator::Orchestrator>>,
    {
        self.submit_with_workflow(resolve, |_, _, _| Ok(()))
    }

    /// 创建任务后按工作流选择持久化:`Template(key)` 时由 assign 回调
    /// 解析模板当前版本并分配(编译 + 插件 pin + Revision);
    /// `TaskLocal` 完全不触发分配(画布稍后创建任务本地草稿)。
    pub fn submit_with_workflow<F, A>(
        &self,
        resolve: F,
        mut assign: A,
    ) -> anyhow::Result<ActivationTarget>
    where
        F: Fn(&PathBuf) -> Option<Arc<mf_agent::orchestrator::Orchestrator>>,
        A: FnMut(&PathBuf, i64, &WorkflowChoice) -> anyhow::Result<()>,
    {
        let Some(root) = self.selected_project() else {
            anyhow::bail!("必须先选择项目");
        };
        let title = self.title.trim();
        let goal = self.goal.trim();
        if title.is_empty() || goal.is_empty() {
            anyhow::bail!("标题与目标均为必填");
        }
        let orch =
            resolve(root).ok_or_else(|| anyhow::anyhow!("项目未打开: {}", root.display()))?;
        let task = orch.create_task(title, goal)?;
        // 模板分配失败 → 回滚:删除刚建的任务,不留无人认领的 Draft
        if let WorkflowChoice::Template(_) = &self.workflow_choice {
            if let Err(e) = assign(root, task.id, &self.workflow_choice) {
                if let Err(discard_err) = orch.discard_task(task.id) {
                    log::warn!("分配失败后清理 Draft 任务失败: {discard_err:#}");
                }
                return Err(e);
            }
        }
        let (pid, _) = normalize_project_path(root);
        Ok(ActivationTarget::Task {
            project: pid,
            task_id: task.id,
        })
    }
}

/// GPUI 渲染宿主:按键编辑字段,Enter 提交(通过回调),Esc 取消。
pub struct TaskComposer {
    pub state: TaskComposerState,
    active_field: ComposerField,
    focus_handle: FocusHandle,
    pending_focus: bool,
}

impl TaskComposer {
    pub fn new(state: TaskComposerState, cx: &mut Context<Self>) -> Self {
        Self {
            state,
            active_field: ComposerField::Title,
            focus_handle: cx.focus_handle(),
            pending_focus: true,
        }
    }

    pub fn take_pending_focus(&mut self, window: &mut Window, cx: &mut App) -> bool {
        if self.pending_focus {
            self.pending_focus = false;
            window.focus(&self.focus_handle, cx);
            true
        } else {
            false
        }
    }

    pub fn handle_key(&mut self, ev: &gpui::KeyDownEvent, window: &Window) -> ComposerKeyResult {
        if ev.keystroke.key.as_str() == "escape" {
            return ComposerKeyResult::Cancel;
        }
        if ev.keystroke.key.as_str() == "enter" && self.active_field == ComposerField::Title {
            // Title 回车 → 转到 Goal(显式确认才提交)
            self.active_field = ComposerField::Goal;
            return ComposerKeyResult::Consumed;
        }
        if ev.keystroke.key.as_str() == "enter" {
            return if self.state.can_submit() {
                ComposerKeyResult::Submit
            } else {
                ComposerKeyResult::Consumed
            };
        }
        if ev.keystroke.key.as_str() == "backspace" {
            match self.active_field {
                ComposerField::Title => {
                    let mut t = self.state.title().to_string();
                    t.pop();
                    self.state.set_title(&t);
                }
                ComposerField::Goal => {
                    let mut g = self.state.goal().to_string();
                    g.pop();
                    self.state.set_goal(&g);
                }
            }
            return ComposerKeyResult::Consumed;
        }
        if let Some(ch) = ev.keystroke.key_char.as_ref() {
            match self.active_field {
                ComposerField::Title => {
                    let mut t = self.state.title().to_string();
                    t.push_str(ch);
                    self.state.set_title(&t);
                }
                ComposerField::Goal => {
                    let mut g = self.state.goal().to_string();
                    g.push_str(ch);
                    self.state.set_goal(&g);
                }
            }
        }
        let _ = window;
        ComposerKeyResult::Consumed
    }

    pub fn render_inner(&self, cx: &Context<Self>, window: &Window) -> AnyElement {
        let field_focused =
            |f: ComposerField| self.focus_handle.is_focused(window) && self.active_field == f;
        let valid = self.state.can_submit();
        let project_label = format!("项目:{}", self.state.selected_project_name());
        div()
            .id("task-composer")
            .mx_1p5()
            .mb_1()
            .p_1p5()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(crate::theme::Theme::accent_dim()))
            .bg(rgb(crate::theme::Theme::bg_elevated()))
            .track_focus(&self.focus_handle)
            .child(
                div()
                    .id("composer-project")
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .text_size(px(10.5))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .child(project_label)
                    .on_click(cx.listener(|c: &mut TaskComposer, _, _, cx| {
                        c.state.select_next_project();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("composer-title")
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if field_focused(ComposerField::Title) {
                        crate::theme::Theme::accent()
                    } else {
                        crate::theme::Theme::border()
                    }))
                    .text_size(px(11.))
                    .cursor_pointer()
                    .on_click(cx.listener(|c: &mut TaskComposer, _, window, cx| {
                        c.active_field = ComposerField::Title;
                        window.focus(&c.focus_handle, cx);
                        cx.notify();
                    }))
                    .child(if self.state.title().is_empty() {
                        div()
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("标题(必填)…")
                    } else {
                        div()
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child(self.state.title().to_string())
                    }),
            )
            .child(
                div()
                    .id("composer-workflow")
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .text_size(px(10.5))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .child(self.state.workflow_choice_label())
                    .on_click(cx.listener(|c: &mut TaskComposer, _, _, cx| {
                        c.state.cycle_workflow();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("composer-goal")
                    .min_h(px(38.))
                    .p_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if field_focused(ComposerField::Goal) {
                        crate::theme::Theme::accent()
                    } else {
                        crate::theme::Theme::border()
                    }))
                    .text_size(px(11.))
                    .cursor_pointer()
                    .on_click(cx.listener(|c: &mut TaskComposer, _, window, cx| {
                        c.active_field = ComposerField::Goal;
                        window.focus(&c.focus_handle, cx);
                        cx.notify();
                    }))
                    .child(if self.state.goal().is_empty() {
                        div()
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("目标(必填,默认跟随标题,可编辑)…")
                    } else {
                        div()
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child(self.state.goal().to_string())
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_1()
                    .justify_end()
                    .child(
                        div()
                            .id("composer-cancel")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(px(10.))
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child("取消(Esc)")
                            .on_click(cx.listener(|_c: &mut TaskComposer, _, _, cx| {
                                cx.emit(ComposerUiEvent::Cancel);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("composer-submit")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .text_size(px(10.))
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
                            .child("创建任务")
                            .on_click(cx.listener(|c: &mut TaskComposer, _, _, cx| {
                                if c.state.can_submit() {
                                    cx.emit(ComposerUiEvent::Submit);
                                }
                                cx.notify();
                            })),
                    ),
            )
            .on_key_down(cx.listener(
                |c: &mut TaskComposer, ev: &gpui::KeyDownEvent, window, cx| {
                    match c.handle_key(ev, window) {
                        ComposerKeyResult::Submit => cx.emit(ComposerUiEvent::Submit),
                        ComposerKeyResult::Cancel => cx.emit(ComposerUiEvent::Cancel),
                        ComposerKeyResult::Consumed => {}
                    }
                    cx.stop_propagation();
                    cx.notify();
                },
            ))
            .into_any_element()
    }
}

pub enum ComposerKeyResult {
    Consumed,
    Submit,
    Cancel,
}

/// Composer → 宿主(TaskSidebar)的 UI 事件。
pub enum ComposerUiEvent {
    Submit,
    Cancel,
}

impl EventEmitter<ComposerUiEvent> for TaskComposer {}

impl Render for TaskComposer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.take_pending_focus(window, cx);
        self.render_inner(cx, window)
    }
}
