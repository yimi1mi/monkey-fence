//! Run Monitor 的 DAG 状态投影与渲染宿主(UI 计划 Task 4;设计 §11.4)。
//!
//! 复用 Pipeline 页的任务/步骤数据,把 RunNodeDetails 的动作
//! (继续/重试/跳过/结算/取消)接到 Orchestrator;

pub use crate::run_node_details::{needs_you_reasons, RunAction, RunNodeDetails};

use mf_agent::model::{ExecutionLeaseRow, HandoffRow, RunView, SessionView, Settlement, StepView};
use mf_agent::orchestrator::Orchestrator;
use std::collections::HashMap;
use std::sync::Arc;

/// Run Monitor 视图数据(一次任务的投影):
/// 步骤/运行之外还携带 Session、Handoff、执行租约与待决汇合冲突,
/// 节点行与冲突面板都从这里渲染。
pub struct RunMonitorSnapshot {
    pub steps: Vec<StepView>,
    pub runs: Vec<RunView>,
    /// 任务关联的会话(按 run.session_id 收集)。
    pub sessions: Vec<SessionView>,
    /// 每个步骤最近一次 Handoff(含 files/verification/output)。
    pub handoffs: Vec<HandoffRow>,
    /// 任务的执行租约(路径/提供器/持有状态)。
    pub leases: Vec<ExecutionLeaseRow>,
    /// 待决汇合冲突(持久化行投影;空 = 无冲突)。
    pub pending_conflicts: Vec<String>,
    /// 手工结算输入缓冲(空 = 未输入)。
    pub settle_input: String,
}

/// 结构化 output 的显示文本(I12):除 null 外的任意合法 JSON
/// (object/array/string/number/bool,含空对象)都显示。
pub fn handoff_output_text(output: &serde_json::Value) -> Option<String> {
    if output.is_null() {
        return None;
    }
    Some(serde_json::to_string(output).unwrap_or_default())
}

impl RunMonitorSnapshot {
    pub fn from_parts(steps: Vec<StepView>, runs: Vec<RunView>) -> RunMonitorSnapshot {
        RunMonitorSnapshot {
            steps,
            runs,
            sessions: Vec::new(),
            handoffs: Vec::new(),
            leases: Vec::new(),
            pending_conflicts: Vec::new(),
            settle_input: String::new(),
        }
    }

    /// 从 Orchestrator 收集完整投影(Store 查询 + 待决冲突)。
    pub fn collect(orch: &Arc<Orchestrator>, task_id: i64) -> RunMonitorSnapshot {
        let steps = orch.store.task_steps(task_id).unwrap_or_default();
        let runs = orch.store.list_runs_of_task(task_id).unwrap_or_default();
        let session_ids: Vec<i64> = runs.iter().filter_map(|r| r.session_id).collect();
        let sessions = orch
            .store
            .list_sessions()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| session_ids.contains(&s.id))
            .collect();
        // 每个步骤最近一次 Handoff(handoffs 按 id 升序,倒序取第一个)
        let mut handoffs: Vec<HandoffRow> = Vec::new();
        for step in &steps {
            if let Some(row) = orch
                .store
                .list_handoff_rows(task_id)
                .unwrap_or_default()
                .into_iter()
                .rev()
                .find(|r| r.step_id == Some(step.id))
            {
                handoffs.push(row);
            }
        }
        let leases = orch
            .store
            .list_execution_leases(task_id)
            .unwrap_or_default();
        let pending_conflicts = orch.pending_merge_conflicts(task_id);
        RunMonitorSnapshot {
            steps,
            runs,
            sessions,
            handoffs,
            leases,
            pending_conflicts,
            settle_input: String::new(),
        }
    }

    /// 每个步骤的最新 run 详情(按 step_key;附 Session/Handoff/租约投影)。
    pub fn node_details(&self) -> Vec<RunNodeDetails> {
        let sessions_by_id: HashMap<i64, &SessionView> =
            self.sessions.iter().map(|s| (s.id, s)).collect();
        let handoff_by_step: HashMap<i64, &HandoffRow> = self
            .handoffs
            .iter()
            .map(|h| (h.step_id.unwrap_or(0), h))
            .collect();
        self.steps
            .iter()
            .map(|step| {
                let latest = self
                    .runs
                    .iter()
                    .filter(|r| r.step_id == step.id)
                    .max_by_key(|r| r.id);
                let mut extras = crate::run_node_details::NodeExtras::default();
                if let Some(run) = latest {
                    if let Some(session) = run.session_id.and_then(|id| sessions_by_id.get(&id)) {
                        extras.session_status = Some(format!("{:?}", session.status));
                    }
                    if let Some(row) = handoff_by_step.get(&step.id) {
                        extras.handoff_summary = Some(row.handoff.summary.clone());
                        extras.handoff_files = row.handoff.changed_files.clone();
                        extras.handoff_verification =
                            row.handoff.verification.as_ref().map(|v| v.to_string());
                        extras.handoff_artifacts = row.handoff.artifacts.clone();
                        extras.handoff_blockers = row.handoff.blockers.clone();
                        extras.handoff_recommendations = row.handoff.recommendations.clone();
                        extras.handoff_output = handoff_output_text(&row.handoff.output);
                        extras.log_ref = row.handoff.raw_log_ref.clone();
                    }
                    if let Some(lease) = self
                        .leases
                        .iter()
                        .rev()
                        .find(|l| l.run_id == Some(run.id) || l.step_id == step.id)
                    {
                        extras.lease = Some(format!(
                            "{} {} [{}]",
                            lease.provider, lease.path, lease.status
                        ));
                    }
                }
                match latest {
                    Some(run) => RunNodeDetails {
                        extras,
                        ..RunNodeDetails::from((run, step))
                    },
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

/// 危险动作的显式确认意图(I14:不能一键直接执行)。
#[derive(Debug, Clone, PartialEq)]
pub enum PendingConfirm {
    /// 跳过节点(放弃该步骤的执行与产出)。
    Skip { node_index: usize },
    /// 取消运行(终止进程、释放执行租约)。
    CancelRun { node_index: usize },
    /// 重试待决汇合(把隔离租约的变更合并回项目目录)。
    MergeRetry,
}

/// 是否需要二次确认(Skip 放弃步骤 / Cancel 终止进程;
/// 合并重试经 PendingConfirm::MergeRetry 显式确认)。
pub fn requires_confirmation(action: &RunAction) -> bool {
    matches!(action, RunAction::Skip | RunAction::Cancel)
}

/// 确认提示文案(说明后果,用户显式选择)。
pub fn confirmation_prompt(action: &RunAction) -> String {
    match action {
        RunAction::Skip => "确认跳过该节点?跳过后本步骤不再执行,产出被放弃。".into(),
        RunAction::Cancel => "确认取消该运行?将终止 Agent 进程并释放执行租约(不可恢复)。".into(),
        _ => "确认执行该操作?".into(),
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
                    output: Default::default(),
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
    /// 危险动作的待确认意图(显式确认后才执行)。
    pending_confirm: Option<PendingConfirm>,
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
            pending_confirm: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Workspace 推送当前任务(切换即刷新投影)。
    pub fn set_task(&mut self, task: Option<(PathBuf, i64)>, cx: &mut Context<Self>) {
        self.task = task;
        self.refresh();
        cx.notify();
    }

    /// 投影节点数(测试/诊断)。
    pub fn snapshot_node_count(&self) -> usize {
        self.snapshot.node_details().len()
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
        // 完整投影:Session/Handoff/租约/待决冲突(冲突面板 + 节点富显示)
        let input = std::mem::take(&mut self.input);
        self.snapshot = RunMonitorSnapshot::collect(&orch, task_id);
        self.input = input;
    }

    /// 用户点击「重试合并」:先进入确认状态(I14 危险动作,
    /// 不得一键直接合并);确认后执行 resolve_pending_merge_confirmed。
    pub fn resolve_pending_merge(&mut self, cx: &mut Context<Self>) {
        self.pending_confirm = Some(PendingConfirm::MergeRetry);
        self.status = "确认重试合并?将把隔离租约的全部变更合并回项目目录。".into();
        cx.notify();
    }

    fn resolve_pending_merge_confirmed(&mut self, cx: &mut Context<Self>) {
        let result = match self.orchestrator() {
            Some(orch) => {
                let (_, task_id) = self.task.clone().expect("orchestrator 存在则任务存在");
                orch.resolve_pending_merges(task_id)
                    .map(|remaining| {
                        if remaining.is_empty() {
                            "汇合完成:冲突全部解决,租约已释放".to_string()
                        } else {
                            format!("仍存在冲突:{} ", remaining.join("; "))
                        }
                    })
                    .map_err(|e| format!("{e:#}"))
            }
            None => Err("项目未打开".into()),
        };
        match result {
            Ok(msg) => self.status = msg,
            Err(e) => self.status = e,
        }
        self.refresh();
        cx.notify();
    }

    fn orchestrator(&self) -> Option<Arc<mf_agent::Orchestrator>> {
        let (root, _) = self.task.as_ref()?;
        self.app.orchestrator_of(root)
    }

    /// 执行节点动作(完整 Orchestrator 链)后刷新投影。
    /// 危险动作(Skip/Cancel)先进入确认状态 —— 不得一键直接执行。
    pub fn run_action(&mut self, idx: usize, action: RunAction, cx: &mut Context<Self>) {
        if requires_confirmation(&action) {
            self.pending_confirm = Some(match action {
                RunAction::Skip => PendingConfirm::Skip { node_index: idx },
                RunAction::Cancel => PendingConfirm::CancelRun { node_index: idx },
                _ => unreachable!("requires_confirmation 只放行 Skip/Cancel"),
            });
            self.status = confirmation_prompt(&action);
            cx.notify();
            return;
        }
        self.execute_confirmed(idx, action, cx);
    }

    fn execute_confirmed(&mut self, idx: usize, action: RunAction, cx: &mut Context<Self>) {
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

    /// 用户显式确认待决危险动作后执行。
    pub fn confirm_pending(&mut self, cx: &mut Context<Self>) {
        match self.pending_confirm.take() {
            Some(PendingConfirm::Skip { node_index }) => {
                self.execute_confirmed(node_index, RunAction::Skip, cx);
            }
            Some(PendingConfirm::CancelRun { node_index }) => {
                self.execute_confirmed(node_index, RunAction::Cancel, cx);
            }
            Some(PendingConfirm::MergeRetry) => {
                self.resolve_pending_merge_confirmed(cx);
            }
            None => {}
        }
    }

    /// 放弃待确认动作。
    pub fn dismiss_pending(&mut self, cx: &mut Context<Self>) {
        if self.pending_confirm.take().is_some() {
            self.status = "已取消操作".into();
            cx.notify();
        }
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
        // 待决汇合冲突面板:列出冲突 + 重试合并(merge pending 可恢复)
        if !self.snapshot.pending_conflicts.is_empty() {
            list = list.child(
                gpui::div()
                    .id("rm-merge-conflicts")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::warning()))
                    .child(
                        gpui::div()
                            .text_size(px(10.))
                            .text_color(rgb(crate::theme::Theme::warning()))
                            .child("隔离目录汇合冲突(租约保持持有,解决后重试合并)"),
                    )
                    .children(self.snapshot.pending_conflicts.iter().map(|c| {
                        gpui::div()
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child(format!("• {c}"))
                    }))
                    .child(
                        gpui::div()
                            .id("rm-merge-retry")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::warning()))
                            .text_size(px(9.))
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child("重试合并")
                            .on_click(cx.listener(|monitor: &mut RunMonitor, _ev, _w, cx| {
                                monitor.resolve_pending_merge(cx);
                            })),
                    ),
            );
        }
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
            // 富显示行实际渲染(Session/Handoff 摘要与文件/产物/阻塞/
            // 建议/结构化输出/租约/日志引用)—— 已收集的投影必须可见
            let extra_lines = detail.extra_lines();
            for (line_idx, line) in extra_lines.iter().enumerate() {
                row = row.child(
                    gpui::div()
                        .id(gpui::ElementId::Name(
                            format!("rm-extra-{idx}-{line_idx}").into(),
                        ))
                        .text_size(px(8.5))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .child(line.clone()),
                );
            }
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

    /// 危险动作确认面板(显式「确认执行/取消」;Esc 取消)。
    fn render_confirm(&self, cx: &Context<Self>) -> AnyElement {
        let prompt = match &self.pending_confirm {
            Some(_) => self.status.clone(),
            None => return gpui::div().into_any_element(),
        };
        gpui::div()
            .id("rm-confirm-panel")
            .flex()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(crate::theme::Theme::danger()))
            .text_size(px(10.))
            .text_color(rgb(crate::theme::Theme::warning()))
            .child(prompt)
            .child(
                gpui::div()
                    .id("rm-confirm-yes")
                    .px_2()
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::danger()))
                    .text_size(px(9.))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .child("确认执行")
                    .on_click(cx.listener(|monitor: &mut RunMonitor, _ev, _w, cx| {
                        monitor.confirm_pending(cx);
                    })),
            )
            .child(
                gpui::div()
                    .id("rm-confirm-no")
                    .px_2()
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .text_size(px(9.))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .child("取消")
                    .on_click(cx.listener(|monitor: &mut RunMonitor, _ev, _w, cx| {
                        monitor.dismiss_pending(cx);
                    })),
            )
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
            "escape" => {
                if self.pending_confirm.is_some() {
                    self.dismiss_pending(cx);
                }
                self.input_focused = false;
            }
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
        let confirm = self.render_confirm(cx);
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
            .child(confirm)
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
