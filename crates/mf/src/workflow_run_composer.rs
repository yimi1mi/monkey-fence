//! 工作流运行 Composer(ADR 0004 / Task 4):
//! 从项目工作流直接发起运行前的唯一输入面 —— 本次目标。
//!
//! 纯逻辑(`WorkflowRunComposerState`)与渲染宿主(`WorkflowRunComposer`)分离,
//! 模式同 `task_composer`。提交动作不直接落在渲染回调里:状态机只产出
//! 意图,由 `AgentWorkspace` 调用 `AppCtx::run_project_workflow` 执行。

use crate::app_ctx::WorkflowRunTarget;
use gpui::prelude::*;
use gpui::*;
use gpui::{px, AnyElement, Context, FocusHandle, Window};
use std::path::PathBuf;

/// 按键编辑结果(宿主转发提交/取消意图)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerKeyResult {
    Consumed,
    Submit,
    Cancel,
}

/// Composer 状态机(纯逻辑,可测试)。
/// 只负责:项目/工作流摘要(只读)、本次目标(唯一主输入)、
/// 可折叠高级选项摘要(首版只展示工作流已保存的并行策略)、
/// 提交中/错误/取消。
#[derive(Clone)]
pub struct WorkflowRunComposerState {
    pub project_root: PathBuf,
    pub project_name: String,
    pub workflow_key: String,
    pub workflow_name: String,
    /// 工作流已保存的并行策略(只读展示,不复制编辑入口)。
    pub allow_unsafe_parallel: bool,
    pub node_count: usize,
    goal: String,
    advanced_open: bool,
    submitting: bool,
    error: Option<String>,
    cancelled: bool,
}

impl WorkflowRunComposerState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project_root: PathBuf,
        project_name: String,
        workflow_key: String,
        workflow_name: String,
        allow_unsafe_parallel: bool,
        node_count: usize,
    ) -> Self {
        Self {
            project_root,
            project_name,
            workflow_key,
            workflow_name,
            allow_unsafe_parallel,
            node_count,
            goal: String::new(),
            advanced_open: false,
            submitting: false,
            error: None,
            cancelled: false,
        }
    }

    pub fn set_goal(&mut self, goal: &str) {
        self.goal = goal.to_string();
        if self.error.is_some() {
            self.error = None;
        }
    }

    pub fn goal(&self) -> &str {
        &self.goal
    }

    pub fn toggle_advanced(&mut self) {
        self.advanced_open = !self.advanced_open;
    }

    pub fn advanced_open(&self) -> bool {
        self.advanced_open
    }

    /// 高级选项摘要(首版:工作流已保存的并行策略)。
    pub fn advanced_summary(&self) -> String {
        if self.allow_unsafe_parallel {
            "共享目录并行:已开启(沿用工作流保存的风险开关)".to_string()
        } else {
            "共享目录并行:关闭(非隔离目录下并行将被编译器拒绝)".to_string()
        }
    }

    /// 空 goal 不能提交;提交中不允许重复提交。
    pub fn can_submit(&self) -> bool {
        !self.submitting && !self.goal.trim().is_empty()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, message: String) {
        self.submitting = false;
        self.error = Some(message);
    }

    pub fn is_submitting(&self) -> bool {
        self.submitting
    }

    /// 提交:执行回调并管理提交中/错误状态。
    /// 运行本体由调用方注入(`AppCtx::run_project_workflow`),
    /// 状态机不触 GPUI、不持 AppCtx。
    pub fn submit<R>(&mut self, run: R) -> anyhow::Result<WorkflowRunTarget>
    where
        R: FnOnce(&PathBuf, &str, &str) -> anyhow::Result<WorkflowRunTarget>,
    {
        if self.goal.trim().is_empty() {
            self.error = Some("运行目标不能为空".to_string());
            anyhow::bail!("运行目标不能为空");
        }
        self.submitting = true;
        self.error = None;
        let result = run(&self.project_root, &self.workflow_key, self.goal.trim());
        match &result {
            Ok(_) => self.submitting = false,
            Err(e) => self.set_error(format!("{e:#}")),
        }
        result
    }
}

/// GPUI 渲染宿主:按键编辑目标,Enter 提交(意图交宿主),Esc 取消。
pub struct WorkflowRunComposer {
    pub state: WorkflowRunComposerState,
    focus_handle: FocusHandle,
    pending_focus: bool,
}

impl WorkflowRunComposer {
    pub fn new(state: WorkflowRunComposerState, cx: &mut Context<Self>) -> Self {
        Self {
            state,
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

    pub fn handle_key(&mut self, ev: &gpui::KeyDownEvent, _window: &Window) -> ComposerKeyResult {
        if ev.keystroke.key.as_str() == "escape" {
            self.state.cancel();
            return ComposerKeyResult::Cancel;
        }
        if ev.keystroke.key.as_str() == "enter" {
            return if self.state.can_submit() {
                ComposerKeyResult::Submit
            } else {
                ComposerKeyResult::Consumed
            };
        }
        if ev.keystroke.key.as_str() == "backspace" {
            let mut g = self.state.goal().to_string();
            g.pop();
            self.state.set_goal(&g);
            return ComposerKeyResult::Consumed;
        }
        if let Some(ch) = ev.keystroke.key_char.as_ref() {
            let mut g = self.state.goal().to_string();
            g.push_str(ch);
            self.state.set_goal(&g);
        }
        ComposerKeyResult::Consumed
    }

    pub fn render_inner(&self, cx: &Context<Self>, window: &Window) -> AnyElement {
        let focused = self.focus_handle.is_focused(window);
        let valid = self.state.can_submit();
        let goal_display: String = self.state.goal().replace('\n', " ⏎ ");
        let error_line = self.state.error().map(|e| {
            div()
                .id("workflow-run-composer-error")
                .text_size(crate::theme::ui_px(10.))
                .text_color(rgb(crate::theme::Theme::danger()))
                .child(e.to_string())
        });
        let mut advanced = div()
            .id("workflow-run-composer-advanced-toggle")
            .h(px(20.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .text_size(crate::theme::ui_px(10.))
            .cursor_pointer()
            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
            .child(if self.state.advanced_open() {
                "▾ 高级选项"
            } else {
                "▸ 高级选项"
            })
            .on_click(cx.listener(|c: &mut WorkflowRunComposer, _, _, cx| {
                c.state.toggle_advanced();
                cx.notify();
            }));
        if self.state.advanced_open() {
            advanced = advanced.child(
                div()
                    .id("workflow-run-composer-advanced-summary")
                    .text_size(crate::theme::ui_px(10.))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child(self.state.advanced_summary()),
            );
        }
        div()
            .id("workflow-run-composer")
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
                    .text_size(crate::theme::ui_px(10.5))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child(format!(
                        "运行工作流「{}」· 项目 {} · {} 个节点",
                        self.state.workflow_name, self.state.project_name, self.state.node_count
                    )),
            )
            .child(
                div()
                    .id("workflow-run-composer-goal")
                    .min_h(px(22.))
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if focused {
                        crate::theme::Theme::accent()
                    } else {
                        crate::theme::Theme::border()
                    }))
                    .text_size(crate::theme::ui_px(11.))
                    .cursor_pointer()
                    .on_click(cx.listener(|c: &mut WorkflowRunComposer, _, window, cx| {
                        window.focus(&c.focus_handle, cx);
                        cx.notify();
                    }))
                    .child(if goal_display.is_empty() {
                        div()
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("本次目标(必填):要达成什么…")
                    } else {
                        div().child(goal_display)
                    }),
            )
            .child(advanced)
            .children(error_line)
            .child(
                div()
                    .id("workflow-run-composer-hint")
                    .text_size(crate::theme::ui_px(9.5))
                    .text_color(rgb(if valid {
                        crate::theme::Theme::fg_dim()
                    } else {
                        crate::theme::Theme::fg_faint()
                    }))
                    .child(if self.state.is_submitting() {
                        "正在启动运行…"
                    } else {
                        "Enter 开始运行 · Esc 取消"
                    }),
            )
            .into_any_element()
    }
}

impl Render for WorkflowRunComposer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let inner = self.render_inner(cx, window);
        div()
            .id("workflow-run-composer-page")
            .w(px(520.))
            .child(inner)
            .into_any_element()
    }
}
