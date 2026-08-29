//! Run Monitor 的 DAG 状态投影与渲染宿主(UI 计划 Task 4;设计 §11.4)。
//!
//! 复用 Pipeline 页的任务/步骤数据,把 RunNodeDetails 的动作
//! (继续/重试/跳过/结算/取消)接到 Orchestrator;

pub use crate::run_node_details::{needs_you_reasons, RunAction, RunNodeDetails};

use mf_agent::model::{RunView, Settlement, StepView};
use std::sync::Arc;

/// Run Monitor 视图数据(一次任务的投影)。
pub struct RunMonitorSnapshot {
    pub steps: Vec<StepView>,
    pub runs: Vec<RunView>,
    /// 手工结算输入缓冲(空 = 未输入)。
    pub settle_input: String,
}

impl RunMonitorSnapshot {
    pub fn from_parts(steps: Vec<StepView>, runs: Vec<RunView>) -> RunMonitorSnapshot {
        RunMonitorSnapshot {
            steps,
            runs,
            settle_input: String::new(),
        }
    }

    /// 每个步骤的最新 run 详情(按 step_key)。
    pub fn node_details(&self) -> Vec<RunNodeDetails> {
        self.steps
            .iter()
            .map(|step| {
                let latest = self
                    .runs
                    .iter()
                    .filter(|r| r.step_id == step.id)
                    .max_by_key(|r| r.id);
                match latest {
                    Some(run) => RunNodeDetails::from((run, step)),
                    None => {
                        // 无 run(未派发):仅观察
                        let mut details = RunNodeDetails::from((&placeholder_run(), step));
                        details.actions = vec![RunAction::Observe];
                        details
                    }
                }
            })
            .collect()
    }
}

fn placeholder_run() -> RunView {
    RunView {
        id: 0,
        task_id: 0,
        step_id: 0,
        revision_id: 0,
        session_id: None,
        status: mf_agent::model::RunStatus::Running,
        agent_state: mf_agent::model::AgentState::Idle,
        capability_token: String::new(),
        outcome: None,
        outcome_payload: None,
        started_at: String::new(),
        ended_at: None,
    }
}

/// 执行动作(接线 Orchestrator;返回用户可见结果)。
pub fn execute_action(
    orch: &Arc<mf_agent::Orchestrator>,
    details: &RunNodeDetails,
    action: &RunAction,
    settle_text: &str,
) -> Result<String, String> {
    match action {
        RunAction::Continue => {
            let text = if settle_text.trim().is_empty() {
                "请继续"
            } else {
                settle_text
            };
            orch.send_prompt(details.run_id, text)
                .map_err(|e| format!("{e:#}"))?;
            Ok("已发送提示".into())
        }
        RunAction::FreshRetry => orch
            .retry_step(
                latest_step_id(orch, details),
                mf_agent::RetryMode::FreshSession,
            )
            .map(|_| "已用新会话重试".to_string())
            .map_err(|e| format!("{e:#}")),
        RunAction::Skip => orch
            .skip_step(latest_step_id(orch, details), true)
            .map(|_| "已跳过".to_string())
            .map_err(|e| format!("{e:#}")),
        RunAction::Cancel => {
            if details.run_id == 0 {
                return Ok("尚未派发,无需取消".into());
            }
            // 完整动作:终止进程 + cancelled 结算 + 释放并发槽/租约
            orch.cancel_run(details.run_id)
                .map(|_| "已取消(进程终止,租约释放)".to_string())
                .map_err(|e| format!("{e:#}"))
        }
        RunAction::ManualSettle | RunAction::Settle(_) => {
            let settlement = if settle_text.trim().eq_ignore_ascii_case("fail")
                || settle_text.starts_with("失败:")
            {
                Settlement::Fail {
                    reason: settle_text.trim_start_matches("失败:").to_string(),
                }
            } else {
                Settlement::Complete {
                    summary: if settle_text.trim().is_empty() {
                        "人工确认完成".into()
                    } else {
                        settle_text.to_string()
                    },
                }
            };
            use mf_agent::orchestrator::Orchestrator;
            Orchestrator::settle_run(orch, details.run_id, settlement)
                .map(|_| "已提交结算".to_string())
                .map_err(|e| format!("{e:#}"))
        }
        RunAction::Observe => Ok("继续观察(未知状态不是失败)".into()),
    }
}

fn latest_step_id(orch: &Arc<mf_agent::Orchestrator>, details: &RunNodeDetails) -> i64 {
    orch.store
        .task_steps(details_task(orch, details))
        .ok()
        .and_then(|steps| {
            steps
                .iter()
                .find(|s| s.step_key == details.step_key)
                .map(|s| s.id)
        })
        .unwrap_or(0)
}

fn details_task(orch: &Arc<mf_agent::Orchestrator>, details: &RunNodeDetails) -> i64 {
    orch.store
        .run_view(details.run_id)
        .ok()
        .flatten()
        .map(|r| r.task_id)
        .unwrap_or(0)
}

// ---------- GPUI 视图(真实 Entity,挂载于 AgentWorkspace) ----------

use gpui::prelude::*;
use gpui::{px, rgb, AnyElement, Context, FocusHandle, Window};
use std::path::PathBuf;

/// Run Monitor 页:当前任务的 DAG 运行监控。
/// 动作全部经完整 Orchestrator(继续/重试/跳过/结算/取消);
/// 取消会终止进程并释放执行租约。
pub struct RunMonitor {
    pub app: Arc<crate::app_ctx::AppCtx>,
    task: Option<(PathBuf, i64)>,
    snapshot: RunMonitorSnapshot,
    /// 手工结算/继续输入缓冲。
    input: String,
    input_focused: bool,
    status: String,
    focus_handle: FocusHandle,
}

impl RunMonitor {
    pub fn new(app: Arc<crate::app_ctx::AppCtx>, cx: &mut Context<Self>) -> RunMonitor {
        RunMonitor {
            app,
            task: None,
            snapshot: RunMonitorSnapshot::from_parts(Vec::new(), Vec::new()),
            input: String::new(),
            input_focused: false,
            status: String::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// Workspace 推送当前任务(切换即刷新投影)。
    pub fn set_task(&mut self, task: Option<(PathBuf, i64)>, cx: &mut Context<Self>) {
        self.task = task;
        self.refresh();
        cx.notify();
    }

    /// 概览事件到达时刷新(后台运行持续可见)。
    pub fn refresh_snapshot(&mut self, cx: &mut Context<Self>) {
        self.refresh();
        cx.notify();
    }

    fn refresh(&mut self) {
        let Some((root, task_id)) = self.task.clone() else {
            self.snapshot = RunMonitorSnapshot::from_parts(Vec::new(), Vec::new());
            return;
        };
        let Some(orch) = self.app.orchestrator_of(&root) else {
            return;
        };
        let steps = orch.store.task_steps(task_id).unwrap_or_default();
        let runs = orch.store.list_runs_of_task(task_id).unwrap_or_default();
        // 输入缓冲跨刷新保留(用户正在键入)
        let input = std::mem::take(&mut self.input);
        self.snapshot = RunMonitorSnapshot::from_parts(steps, runs);
        self.input = input;
    }

    fn orchestrator(&self) -> Option<Arc<mf_agent::Orchestrator>> {
        let (root, _) = self.task.as_ref()?;
        self.app.orchestrator_of(root)
    }

    /// 执行节点动作(完整 Orchestrator 链)后刷新投影。
    pub fn run_action(&mut self, idx: usize, action: RunAction, cx: &mut Context<Self>) {
        let details = match self.snapshot.node_details().get(idx) {
            Some(d) => d.clone(),
            None => return,
        };
        let result = match self.orchestrator() {
            Some(orch) => execute_action(&orch, &details, &action, &self.input.clone()),
            None => Err("项目未打开".into()),
        };
        match result {
            Ok(msg) => self.status = msg,
            Err(e) => self.status = e,
        }
        self.refresh();
        cx.notify();
    }

    fn render_nodes(&self, cx: &Context<Self>) -> AnyElement {
        let details = self.snapshot.node_details();
        if details.is_empty() {
            return gpui::div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.))
                .text_color(rgb(crate::theme::Theme::fg_dim()))
                .child("选择任务后监控其工作流运行")
                .into_any_element();
        }
        let mut list = gpui::div().flex().flex_col().gap_1().p_2();
        for (idx, detail) in details.iter().enumerate() {
            let mut row = gpui::div()
                .flex()
                .gap_2()
                .items_center()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(crate::theme::Theme::border()))
                .child(
                    gpui::div()
                        .text_size(px(10.5))
                        .child(format!("{} · {}", detail.step_key, detail.step_title)),
                )
                .child(
                    gpui::div()
                        .text_size(px(9.))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .child(format!(
                            "run {:?} · step {:?}",
                            detail.status, detail.step_status
                        )),
                )
                .child(
                    gpui::div()
                        .text_size(px(9.))
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child(format!(
                            "尝试 {} 次 · run #{}",
                            detail.attempts, detail.run_id
                        )),
                );
            for action in &detail.actions {
                let label = match action {
                    RunAction::Continue => "继续",
                    RunAction::FreshRetry => "重试",
                    RunAction::Skip => "跳过",
                    RunAction::Cancel => "取消",
                    RunAction::ManualSettle => "结算",
                    RunAction::Settle(_) => "结算",
                    RunAction::Observe => "观察",
                };
                let a = action.clone();
                row = row.child(
                    gpui::div()
                        .id(gpui::ElementId::Name(
                            format!("rm-action-{idx}-{label}").into(),
                        ))
                        .px_2()
                        .h(px(18.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(if matches!(a, RunAction::Cancel) {
                            crate::theme::Theme::danger()
                        } else {
                            crate::theme::Theme::accent()
                        }))
                        .text_size(px(9.))
                        .cursor_pointer()
                        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                        .child(label)
                        .on_click(cx.listener(move |monitor: &mut RunMonitor, _ev, _w, cx| {
                            let a2 = match label {
                                "继续" => RunAction::Continue,
                                "重试" => RunAction::FreshRetry,
                                "跳过" => RunAction::Skip,
                                "取消" => RunAction::Cancel,
                                _ => RunAction::ManualSettle,
                            };
                            let _ = &a;
                            monitor.run_action(idx, a2, cx);
                        })),
                );
            }
            list = list.child(row);
        }
        gpui::div()
            .id("rm-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    fn render_input(&self, cx: &Context<Self>) -> AnyElement {
        gpui::div()
            .id("rm-input")
            .flex()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .border_1()
            .border_color(rgb(if self.input_focused {
                crate::theme::Theme::accent()
            } else {
                crate::theme::Theme::border()
            }))
            .rounded_md()
            .text_size(px(10.))
            .cursor_pointer()
            .child(if self.input.is_empty() {
                "继续/结算输入(Enter=结算,「失败:」前缀提交失败)…".to_string()
            } else {
                self.input.clone()
            })
            .on_click(cx.listener(|monitor: &mut RunMonitor, _ev, _w, cx| {
                monitor.input_focused = true;
                cx.notify();
            }))
            .into_any_element()
    }

    pub fn handle_key(&mut self, ev: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "escape" => self.input_focused = false,
            "backspace" => {
                self.input.pop();
            }
            "enter" => {
                // Enter = 手工结算最近一个可结算节点
                let details = self.snapshot.node_details();
                if let Some(idx) = details.iter().rposition(|d| {
                    d.actions
                        .iter()
                        .any(|a| matches!(a, RunAction::ManualSettle))
                }) {
                    self.run_action(idx, RunAction::ManualSettle, cx);
                }
                self.input_focused = false;
            }
            _ => {
                if let Some(ch) = ev.keystroke.key_char.as_ref() {
                    self.input.push_str(ch);
                }
            }
        }
        cx.notify();
    }
}

impl Render for RunMonitor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = match &self.task {
            Some((_, task_id)) => format!("运行监控 · 任务 {task_id}"),
            None => "运行监控".to_string(),
        };
        let nodes = self.render_nodes(cx);
        let input = self.render_input(cx);
        let status = self.status.clone();
        gpui::div()
            .id("run-monitor-page")
            .size_full()
            .flex()
            .flex_col()
            .p_2()
            .gap_2()
            .track_focus(&self.focus_handle)
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(gpui::div().text_size(px(12.)).child(header)),
            )
            .child(nodes)
            .child(input)
            .when(!status.is_empty(), |d| {
                d.child(
                    gpui::div()
                        .text_size(px(9.))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .child(status),
                )
            })
            .on_key_down(cx.listener(
                |monitor: &mut RunMonitor, ev: &gpui::KeyDownEvent, _w, cx| {
                    if monitor.input_focused {
                        monitor.handle_key(ev, cx);
                        cx.stop_propagation();
                    }
                },
            ))
    }
}
