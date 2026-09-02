//! Run Monitor 的 DAG 状态投影与渲染宿主(UI 计划 Task 4;设计 §11.4)。
//!
//! Issue #26 只读迁移:节点/运行/会话/Handoff/租约/待决冲突等业务事实
//! 来自 Core Kernel 的 WorkflowRunSnapshot(Store 只读定位提供 rowid/
//! 时间戳等接线身份)。legacy Store 回退只在 `cfg(test)` 编译，生产
//! Core 未装配时 fail-visible，绝不维持第二事实源。

pub use crate::run_node_details::{needs_you_reasons, RunAction, RunNodeDetails};

use mf_agent::model::{HandoffRow, RunView, SessionView, Settlement, StepView};
use mf_agent::orchestrator::Orchestrator;
use std::collections::HashMap;
use std::path::Path;
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
    /// 任务的执行租约(Kernel 投影不含目录路径)。
    pub leases: Vec<RunLeaseView>,
    /// 待决汇合冲突(持久化行投影;空 = 无冲突)。
    pub pending_conflicts: Vec<String>,
    /// 手工结算输入缓冲(空 = 未输入)。
    pub settle_input: String,
}

/// 执行租约的 UI 投影:Kernel `ExecutionLeaseSnapshot` 只暴露提供器/
/// 隔离/状态,目录路径与 metadata 不越过 Core。
#[derive(Debug, Clone)]
pub struct RunLeaseView {
    pub step_id: i64,
    pub run_id: Option<i64>,
    pub provider: String,
    pub isolated: bool,
    pub status: String,
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

    /// 从 Orchestrator 收集完整投影(回退路径:无 Core Kernel 源的测试
    /// 回滚模式使用;生产读经 [`Self::collect_via_kernel`])。
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
            .unwrap_or_default()
            .into_iter()
            .map(|row| RunLeaseView {
                step_id: row.step_id,
                run_id: row.run_id,
                provider: row.provider,
                isolated: row.isolated,
                status: row.status,
            })
            .collect();
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

    /// 经 Core Kernel 读取一次 Workflow Run 的权威投影(Issue #26)。
    /// Step/Run/Session 状态、Handoff、冲突与原因等业务事实全部来自
    /// `WorkflowRunSnapshotData`;Store 只读定位(`*_view_by_handle`)提供
    /// rowid/时间戳/能力令牌等 UI 接线身份,与 cancel/retry/settle 写路径
    /// 的 handle 解析同一模式。
    ///
    /// - `None`:仅当 Core 未装配或项目未登记(测试回滚模式)→ 调用方回退
    ///   旧投影;生产 open_project 成功后此分支不可达;
    /// - `Some(Err(..))`:Core 投影链上的读取/解析失败(生产 fail-visible,
    ///   不回退)。
    pub fn collect_via_kernel(
        app: &crate::app_ctx::AppCtx,
        root: &Path,
        task_id: i64,
    ) -> Option<Result<RunMonitorSnapshot, String>> {
        let orch = app.orchestrator_of(root)?;
        // Core 可用性先于一切数据读取:None 只表示回滚模式,生产不会
        // 因数据问题(Store 错误/handle 损坏)悄悄降级到旧 Store 投影。
        if !app.workflow_run_projection_ready(root) {
            return None;
        }
        let task = match orch.store.task_view(task_id) {
            Ok(Some(task)) => task,
            // 任务不存在:新旧投影一致为空,回退由调用方兜底
            Ok(None) => return None,
            Err(error) => return Some(Err(format!("Store 只读定位失败:{error}"))),
        };
        let handle = match mf_kernel::handles::WorkflowRunHandle::parse(task.public_handle.clone())
        {
            Ok(handle) => handle,
            Err(error) => return Some(Err(format!("Workflow Run handle 损坏:{error}"))),
        };
        let envelope = match app.workflow_run_snapshot_via_kernel(root, &handle) {
            // 可用性已验证;此处 None 只可能是关闭竞态,按回退处理
            None => return None,
            Some(result) => result,
        }
        .map_err(|error| format!("{error}"));
        let data = match envelope {
            Ok(envelope) => match envelope.data {
                mf_kernel::projection::SnapshotData::WorkflowRun(data) => data,
                _ => return Some(Err("Core 返回了错误的 Snapshot 类型".into())),
            },
            Err(error) => return Some(Err(error)),
        };
        Some(join_kernel_snapshot(&orch, task_id, data).map_err(|error| format!("{error:#}")))
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
                        // Kernel 租约投影不含目录路径(不越过 Core)
                        extras.lease = Some(format!(
                            "{} [{}]{}",
                            lease.provider,
                            lease.status,
                            if lease.isolated { " · 隔离" } else { "" }
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

    pub fn session_target_for_step(&self, step_id: i64) -> Option<(i64, i64, bool)> {
        let run = self
            .runs
            .iter()
            .filter(|run| run.step_id == step_id)
            .max_by_key(|run| run.id)?;
        let session_id = run.session_id?;
        let session = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)?;
        Some((session_id, run.id, session.runtime == "http"))
    }
}

/// Kernel Workflow Run 快照 → RunMonitorSnapshot(自由函数)。
/// 业务事实(状态/标题/未读/Handoff/冲突/租约状态)取自 Kernel;
/// rowid/时间戳/能力令牌等 Kernel 未投影的接线字段由 Store 只读定位补齐。
fn join_kernel_snapshot(
    orch: &Arc<Orchestrator>,
    task_id: i64,
    data: mf_kernel::projection::WorkflowRunSnapshotData,
) -> anyhow::Result<RunMonitorSnapshot> {
    use mf_agent::model::{AgentState, RunStatus, SessionStatus, StepStatus};
    use mf_kernel::handles::{AgentRunHandle, AgentSessionHandle, StepHandle};

    // Store 只读定位:handle → 身份行(时间戳/rowid/能力令牌)
    let mut steps_by_handle: HashMap<String, StepView> = orch
        .store
        .task_steps(task_id)?
        .into_iter()
        .map(|step| (step.public_handle.clone(), step))
        .collect();
    // Agent Run 可能引用旧 Revision 的历史 Step:补齐身份行
    for run in &data.agent_runs {
        if !steps_by_handle.contains_key(run.step.as_str()) {
            if let Some(view) = orch
                .store
                .step_view_by_handle(run.step.as_str())
                .ok()
                .flatten()
            {
                steps_by_handle.insert(view.public_handle.clone(), view);
            }
        }
    }
    let runs_by_handle: HashMap<String, RunView> = orch
        .store
        .list_runs_of_task(task_id)?
        .into_iter()
        .map(|run| (run.public_handle.clone(), run))
        .collect();
    let sessions_by_handle: HashMap<String, SessionView> = orch
        .store
        .list_sessions()?
        .into_iter()
        .map(|session| (session.public_handle.clone(), session))
        .collect();
    let step_id_of = |handle: &StepHandle| steps_by_handle.get(handle.as_str()).map(|s| s.id);
    let run_id_of = |handle: &AgentRunHandle| runs_by_handle.get(handle.as_str()).map(|r| r.id);
    let session_id_of =
        |handle: &AgentSessionHandle| sessions_by_handle.get(handle.as_str()).map(|s| s.id);

    let steps = data
        .steps
        .iter()
        .map(|snapshot| {
            let identity = steps_by_handle.get(snapshot.step.as_str());
            StepView {
                id: identity.map(|s| s.id).unwrap_or(0),
                public_handle: snapshot.step.as_str().to_owned(),
                revision: i64::try_from(snapshot.revision.revision).unwrap_or(0),
                revision_id: identity.map(|s| s.revision_id).unwrap_or(0),
                task_id,
                step_key: snapshot.key.clone(),
                title: snapshot.title.clone(),
                instructions: snapshot.instructions.clone(),
                agent_profile: snapshot.agent_instance_ref.clone(),
                session_policy: snapshot.session_policy.clone(),
                status: StepStatus::parse(&snapshot.status)
                    .or(identity.map(|s| s.status))
                    .unwrap_or(StepStatus::Pending),
                attempts: snapshot.attempts,
                auto_retry: snapshot.auto_retry,
                result: snapshot.result.clone(),
                started_at: identity.and_then(|s| s.started_at.clone()),
                ended_at: identity.and_then(|s| s.ended_at.clone()),
                deps: snapshot
                    .dependencies
                    .iter()
                    .filter_map(step_id_of)
                    .collect(),
            }
        })
        .collect();
    let runs = data
        .agent_runs
        .iter()
        .map(|snapshot| {
            let identity = runs_by_handle.get(snapshot.agent_run.as_str());
            RunView {
                id: identity.map(|r| r.id).unwrap_or(0),
                public_handle: snapshot.agent_run.as_str().to_owned(),
                revision: i64::try_from(snapshot.revision.revision).unwrap_or(0),
                task_id,
                step_id: step_id_of(&snapshot.step).unwrap_or(0),
                revision_id: identity.map(|r| r.revision_id).unwrap_or(0),
                session_id: snapshot.agent_session.as_ref().and_then(session_id_of),
                status: RunStatus::parse(&snapshot.status)
                    .or(identity.map(|r| r.status))
                    .unwrap_or(RunStatus::Running),
                agent_state: AgentState::parse(&snapshot.agent_state)
                    .or(identity.map(|r| r.agent_state))
                    .unwrap_or(AgentState::Idle),
                // UI 投影不持有认证材料；所有动作经 opaque handles +
                // Core command 路由，capability token 只存在于 Runtime env。
                capability_token: String::new(),
                outcome: snapshot.outcome.clone(),
                outcome_payload: snapshot.outcome_payload.clone(),
                started_at: identity.map(|r| r.started_at.clone()).unwrap_or_default(),
                ended_at: identity.and_then(|r| r.ended_at.clone()),
            }
        })
        .collect();
    let sessions = data
        .agent_sessions
        .iter()
        .map(|snapshot| {
            let identity = sessions_by_handle.get(snapshot.agent_session.as_str());
            SessionView {
                id: identity.map(|s| s.id).unwrap_or(0),
                public_handle: snapshot.agent_session.as_str().to_owned(),
                revision: i64::try_from(snapshot.revision.revision).unwrap_or(0),
                session_key: identity.and_then(|s| s.session_key.clone()),
                runtime: snapshot.runtime.clone(),
                agent_profile: identity
                    .map(|s| s.agent_profile.clone())
                    .unwrap_or_default(),
                title: snapshot.title.clone(),
                status: SessionStatus::parse(&snapshot.status)
                    .or(identity.map(|s| s.status))
                    .unwrap_or(SessionStatus::Idle),
                last_instruction: identity.and_then(|s| s.last_instruction.clone()),
                last_reply: identity.and_then(|s| s.last_reply.clone()),
                unread: snapshot.unread,
                created_at: identity.map(|s| s.created_at.clone()).unwrap_or_default(),
                updated_at: identity.map(|s| s.updated_at.clone()).unwrap_or_default(),
            }
        })
        .collect();
    let handoffs = data
        .handoffs
        .iter()
        .enumerate()
        .map(|(index, snapshot)| HandoffRow {
            // Kernel Handoff 投影不含行号;展示只按 step/run 归属使用
            id: i64::try_from(index + 1).unwrap_or(0),
            step_id: snapshot.step.as_ref().and_then(step_id_of),
            run_id: snapshot.agent_run.as_ref().and_then(run_id_of),
            handoff: snapshot.handoff.clone(),
        })
        .collect();
    let leases = data
        .execution_leases
        .iter()
        .map(|lease| RunLeaseView {
            step_id: step_id_of(&lease.step).unwrap_or(0),
            run_id: lease.agent_run.as_ref().and_then(run_id_of),
            provider: lease.provider.clone(),
            isolated: lease.isolated,
            status: lease.status.clone(),
        })
        .collect();
    let pending_conflicts = data
        .pending_merges
        .iter()
        .flat_map(|pending| pending.conflicts.iter().cloned())
        .collect();
    Ok(RunMonitorSnapshot {
        steps,
        runs,
        sessions,
        handoffs,
        leases,
        pending_conflicts,
        settle_input: String::new(),
    })
}

fn placeholder_run() -> RunView {
    RunView {
        id: 0,
        public_handle: String::new(),
        revision: 0,
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
        RunAction::Cancel => {
            "确认取消整个 Workflow Run?将终止全部活动 Agent 并释放执行租约(不可恢复)。".into()
        }
        _ => "确认执行该操作?".into(),
    }
}

/// 经 CoreKernel 取消 Workflow Run,对 `RevisionConflict` 做有界重读重试。
///
/// 已知窗口:cancel 的 prepare 阶段会停止进程(mf-agent
/// `prepare_cancel_runs`)→ RuntimeEvent::Exit → `enter_awaiting_outcome`
/// 可能在命令 L-CMD 复验之前推进 run revision,使首次 CAS 失败。取消的
/// 用户语义是收敛到 Cancelled,按乐观并发惯例重读快照重试;根因
/// (client 在 prepare 之后未随命令重读 expected)在 mf-kernel facade,
/// 已登记为跨任务 blocker,不在 UI 层修复。重试不放大冲突:最多 3 次,
/// 非 CAS 错误原样上抛。
pub(crate) fn cancel_workflow_run_via_kernel_retrying_conflicts(
    app: &crate::app_ctx::AppCtx,
    project: &Path,
    task_id: i64,
) -> Result<String, String> {
    const MAX_ATTEMPTS: usize = 3;
    let mut conflict: Option<String> = None;
    for _ in 0..MAX_ATTEMPTS {
        match app.cancel_workflow_run_via_kernel(project, task_id) {
            Ok(_) => return Ok("已取消 Workflow Run".to_string()),
            Err(mf_kernel::kernel::KernelProblem::RevisionConflict) => {
                conflict = Some("revision_conflict".to_string());
            }
            Err(error) => return Err(format!("{error:#}")),
        }
    }
    Err(format!(
        "取消与并发状态变更冲突(重试 {MAX_ATTEMPTS} 次后仍失败):{}",
        conflict.expect("RevisionConflict 分支必已记录")
    ))
}

fn execute_action_via_kernel(
    app: &crate::app_ctx::AppCtx,
    project: &std::path::Path,
    task_id: i64,
    details: &RunNodeDetails,
    action: &RunAction,
    settle_text: &str,
) -> Result<String, String> {
    match action {
        RunAction::FreshRetry => app
            .retry_workflow_step_via_kernel(
                project,
                details.step_id,
                mf_agent::RetryMode::FreshSession,
            )
            .map(|_| "已用新会话重试".to_string())
            .map_err(|error| format!("{error:#}")),
        RunAction::Cancel => {
            cancel_workflow_run_via_kernel_retrying_conflicts(app, project, task_id)
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
            app.settle_agent_run_via_kernel(project, details.run_id, settlement)
                .map(|_| "已提交结算".to_string())
                .map_err(|error| format!("{error:#}"))
        }
        RunAction::Continue => app
            .retry_workflow_step_via_kernel(
                project,
                details.step_id,
                mf_agent::RetryMode::ContinueSession,
            )
            .map(|_| "已通过 Core 继续会话".to_string())
            .map_err(|error| format!("{error:#}")),
        RunAction::Skip => app
            .skip_workflow_step_via_kernel(project, details.step_id)
            .map(|_| "已跳过".to_string())
            .map_err(|error| format!("{error:#}")),
        RunAction::Observe => Ok("继续观察(未知状态不是失败)".into()),
    }
}

// ---------- GPUI 视图(真实 Entity,挂载于 AgentWorkspace) ----------

use gpui::prelude::*;
use gpui::{px, rgb, AnyElement, Context, EventEmitter, FocusHandle, Window};
use std::path::PathBuf;

pub enum RunMonitorEvent {
    OpenSession {
        project_root: PathBuf,
        task_id: i64,
        session_id: i64,
        run_id: i64,
        is_http: bool,
    },
}

impl EventEmitter<RunMonitorEvent> for RunMonitor {}

/// Run Monitor 页:当前任务的 DAG 运行监控。
/// 读:Workflow Run 业务事实经 Core Kernel Snapshot;写:Run 生命周期
/// (cancel/retry/settle)经 CoreKernel,终端提示/Skip 仍走 Orchestrator
/// 的既有内部动作(后续独立命令族迁移)。
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
    /// 「需要你」直达定位的步骤(Task 7;渲染高亮)。
    focused_step_id: Option<i64>,
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
            focused_step_id: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Workspace 推送当前任务(切换即刷新投影)。
    pub fn set_task(&mut self, task: Option<(PathBuf, i64)>, cx: &mut Context<Self>) {
        self.task = task;
        self.focused_step_id = None;
        self.refresh();
        cx.notify();
    }

    /// 定位到优先处理节点(Task 7「需要你」直达):刷新投影并高亮该步骤。
    pub fn focus_step(&mut self, step_id: i64, cx: &mut Context<Self>) {
        self.focused_step_id = Some(step_id);
        self.refresh();
        cx.notify();
    }

    /// 当前定位的节点(测试/诊断)。
    pub fn focused_step(&self) -> Option<i64> {
        self.focused_step_id
    }

    /// 投影节点数(测试/诊断)。
    pub fn snapshot_node_count(&self) -> usize {
        self.snapshot.node_details().len()
    }

    /// 状态栏文本(测试/诊断)。
    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// 是否存在待确认的危险动作(测试/诊断)。
    pub fn has_pending_confirm(&self) -> bool {
        self.pending_confirm.is_some()
    }

    /// 投影节点详情(测试/诊断)。
    pub fn node_details_for_test(&self) -> Vec<crate::run_node_details::RunNodeDetails> {
        self.snapshot.node_details()
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
        let input = std::mem::take(&mut self.input);
        // 完整投影 kernel-first:Core Snapshot 是唯一业务事实源;
        // None = Core 未装配；测试可保留旧投影做回归对照，生产必须
        // fail-visible，不能静默回退成第二事实源。
        self.snapshot = match RunMonitorSnapshot::collect_via_kernel(&self.app, &root, task_id) {
            Some(Ok(snapshot)) => snapshot,
            Some(Err(error)) => {
                log::error!("Core Workflow Run 快照读取失败: {error}");
                self.status = format!("Core 快照读取失败:{error}");
                RunMonitorSnapshot::from_parts(Vec::new(), Vec::new())
            }
            None => {
                #[cfg(test)]
                {
                    match self.app.orchestrator_of(&root) {
                        Some(orch) => RunMonitorSnapshot::collect(&orch, task_id),
                        None => RunMonitorSnapshot::from_parts(Vec::new(), Vec::new()),
                    }
                }
                #[cfg(not(test))]
                {
                    self.status = "Core Workflow Run 快照不可用".into();
                    RunMonitorSnapshot::from_parts(Vec::new(), Vec::new())
                }
            }
        };
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
        let result = match self.task.as_ref() {
            Some((project, task_id)) => execute_action_via_kernel(
                &self.app,
                project,
                *task_id,
                &details,
                &action,
                &self.input.clone(),
            ),
            None => Err("项目未打开".into()),
        };
        match result {
            Ok(msg) => self.status = msg,
            Err(e) => self.status = e,
        }
        // CoreKernel 写路径不产生 Orchestrator 调度事件(Store 写在 L-CMD
        // 事务内,不经 events_rx):主动 nudge 统一快照重建,运行列表/
        // 徽标不等待下一个无关事件才更新。失败也 nudge——prepare 阶段的
        // 进程停止可能已改变事实。
        self.app.overview().request_refresh();
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
                .text_size(crate::theme::ui_px(11.))
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
                            .text_size(crate::theme::ui_px(10.))
                            .text_color(rgb(crate::theme::Theme::warning()))
                            .child("隔离目录汇合冲突(租约保持持有,解决后重试合并)"),
                    )
                    .children(self.snapshot.pending_conflicts.iter().map(|c| {
                        gpui::div()
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child(format!("• {c}"))
                    }))
                    .child(
                        gpui::div()
                            .id("rm-merge-retry")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::warning()))
                            .text_size(crate::theme::ui_px(9.))
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
            let focused = self.focused_step_id == Some(detail.step_id);
            let session_target = self.snapshot.session_target_for_step(detail.step_id);
            let task_target = self.task.clone();
            let mut row = gpui::div()
                .flex()
                .gap_2()
                .items_center()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(if focused {
                    crate::theme::Theme::accent()
                } else {
                    crate::theme::Theme::border()
                }))
                .when(focused, |d| {
                    d.bg(rgb(crate::theme::Theme::bg_active())).child(
                        gpui::div()
                            .text_size(crate::theme::ui_px(8.))
                            .text_color(rgb(crate::theme::Theme::accent()))
                            .child("★ 优先处理"),
                    )
                })
                .child(
                    gpui::div()
                        .text_size(crate::theme::ui_px(10.5))
                        .child(format!("{} · {}", detail.step_key, detail.step_title)),
                )
                .child(
                    gpui::div()
                        .text_size(crate::theme::ui_px(9.))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .child(format!(
                            "run {:?} · step {:?}",
                            detail.status, detail.step_status
                        )),
                )
                .child(
                    gpui::div()
                        .text_size(crate::theme::ui_px(9.))
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
                        .text_size(crate::theme::ui_px(8.5))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .child(line.clone()),
                );
            }
            if let (Some((session_id, run_id, is_http)), Some((project_root, task_id))) =
                (session_target, task_target)
            {
                row = row.child(
                    gpui::div()
                        .id(gpui::ElementId::Name(
                            format!("rm-open-session-{idx}").into(),
                        ))
                        .px_2()
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(crate::theme::Theme::accent()))
                        .text_size(crate::theme::ui_px(9.))
                        .cursor_pointer()
                        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                        .child(if is_http {
                            "打开 transcript"
                        } else {
                            "打开终端"
                        })
                        .on_click(cx.listener(move |_monitor, _ev, _w, cx| {
                            cx.emit(RunMonitorEvent::OpenSession {
                                project_root: project_root.clone(),
                                task_id,
                                session_id,
                                run_id,
                                is_http,
                            });
                        })),
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
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(if matches!(a, RunAction::Cancel) {
                            crate::theme::Theme::danger()
                        } else {
                            crate::theme::Theme::accent()
                        }))
                        .text_size(crate::theme::ui_px(9.))
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
            .text_size(crate::theme::ui_px(10.))
            .text_color(rgb(crate::theme::Theme::warning()))
            .child(prompt)
            .child(
                gpui::div()
                    .id("rm-confirm-yes")
                    .px_2()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::danger()))
                    .text_size(crate::theme::ui_px(9.))
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
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .text_size(crate::theme::ui_px(9.))
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
            .text_size(crate::theme::ui_px(10.))
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
                gpui::div().flex().items_center().gap_2().child(
                    gpui::div()
                        .text_size(crate::theme::ui_px(12.))
                        .child(header),
                ),
            )
            .child(nodes)
            .child(confirm)
            .child(input)
            .when(!status.is_empty(), |d| {
                d.child(
                    gpui::div()
                        .text_size(crate::theme::ui_px(9.))
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
