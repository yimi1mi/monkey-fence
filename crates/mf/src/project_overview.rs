//! 统一项目总览快照 + Orchestrator Event Hub。
//!
//! - 每个 GUI Orchestrator 的 `events_rx` 在 attach 后由专职线程持续 drain,
//!   保证状态事件(`send` 阻塞语义)永远不会因 UI 不消费而堵塞调度。
//! - 事件不逐条投影:置 dirty 标记 → 后台整体重建真实快照 → revision +1,
//!   整体替换发布,UI 不会观察到半更新项目集合。
//! - TaskSidebar 与 AgentWorkspace 消费同一份 revisioned snapshot,
//!   不再各自轮询数据库。

use crate::runtime_host::SessionRegistry;
use mf_agent::model::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::pipeline::PipelineDraft;
use mf_plugins::PluginRegistry;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 注意力桶:Agent 卡片的全局看板分列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionBucket {
    NeedsYou,
    Working,
    Done,
    Idle,
}

/// 每 Task 的卡片投影。
pub struct TaskCardOverview {
    pub task: TaskView,
    pub active_runs: usize,
    pub open_questions: usize,
    /// 「需要你」原因(Run Monitor 投影;设计 §11.4)。
    pub needs_you_reasons: Vec<String>,
}

/// 每项目投影。
pub struct ProjectOverview {
    pub root: PathBuf,
    pub name: String,
    pub tasks: Vec<TaskCardOverview>,
    pub active_sessions: usize,
}

/// Agent 卡片投影(覆盖看板 CardData 所需字段)。
#[derive(Clone)]
pub struct AgentCardOverview {
    pub project_root: PathBuf,
    pub project_name: String,
    pub session: SessionView,
    pub run: Option<RunView>,
    pub task_id: Option<i64>,
    pub task_title: Option<String>,
    pub profile_display: String,
    pub tail: Vec<String>,
    pub alive: bool,
    pub bucket: AttentionBucket,
    pub is_http: bool,
}

/// 运行级「需要你」投影(ADR 0004 / Task 7):一次工作流运行
/// (内部 Task)最多贡献一个徽标计数;reason_count 是直接可操作
/// 原因数(仅用于展示,不参与计数),focus_step_id 是优先处理节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunAttention {
    pub project_root: PathBuf,
    pub task_id: i64,
    pub task_title: String,
    pub reason_count: usize,
    /// 优先处理节点(等待输入 → 待结算 → 合并冲突 → 失败/中断;
    /// 同优先级按 Step ID 稳定排序取最小)。
    pub focus_step_id: Option<i64>,
}

/// 直接可操作的关注原因(可由用户动作解除)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectAttentionReason {
    /// Step needs-input。
    NeedsInput,
    /// Step/Run awaiting-outcome(含进程退出/会话死亡未结算)。
    AwaitingOutcome,
    /// 待处理合并冲突。
    MergeConflict,
    /// 失败且可以重试、判定或跳过的节点。
    Failed,
    /// 重启恢复的 interrupted。
    Interrupted,
}

impl DirectAttentionReason {
    /// focus 选择优先级(小值优先)。
    pub fn priority(self) -> u8 {
        match self {
            DirectAttentionReason::NeedsInput => 0,
            DirectAttentionReason::AwaitingOutcome => 1,
            DirectAttentionReason::MergeConflict => 2,
            DirectAttentionReason::Failed | DirectAttentionReason::Interrupted => 3,
        }
    }
}

/// 一个节点的直接关注(纯函数;唯一判定口径)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectAttention {
    pub step_id: i64,
    pub reason: DirectAttentionReason,
}

/// 「需要你」唯一判定口径(ADR 0004 / Task 7):
/// project_overview、RunMonitor 与 WorkflowRunsPage 都复用它,
/// 禁止三处各写一份状态匹配。
///
/// 直接可操作:needs-input / awaiting-outcome(Step 或 Run)/ 失败可
/// 处置 / interrupted / 待处理合并冲突。
/// 不计数:仅因上游失败而 blocked 的后代、pending/ready、
/// 正在自动重试(重新派发后状态不再是 Failed)、已取消/已归档。
pub fn direct_attention_for_step(
    step: &StepView,
    latest_run: Option<&RunView>,
    session_unavailable: bool,
    has_merge_conflict: bool,
) -> Option<DirectAttention> {
    let reason = if step.status == StepStatus::NeedsInput {
        DirectAttentionReason::NeedsInput
    } else if step.status == StepStatus::AwaitingOutcome
        || (step.status == StepStatus::Running
            && matches!(
                latest_run.map(|run| run.status),
                Some(RunStatus::AwaitingOutcome)
            ))
        || (step.status == StepStatus::Running
            && latest_run.is_some_and(|run| run.status == RunStatus::Running)
            && session_unavailable)
    {
        DirectAttentionReason::AwaitingOutcome
    } else if has_merge_conflict {
        // 合并冲突属于工作目录归并阶段，节点可能已 succeeded，也可能仍
        // blocked/pending；归属由 merge batch/lease step key 决定，而非状态猜测。
        DirectAttentionReason::MergeConflict
    } else if step.status == StepStatus::Failed {
        DirectAttentionReason::Failed
    } else if step.status == StepStatus::Running
        && matches!(
            latest_run.map(|run| run.status),
            Some(RunStatus::Interrupted)
        )
    {
        DirectAttentionReason::Interrupted
    } else {
        // 仅因上游失败而 blocked 的后代 / pending / ready /
        // succeeded / skipped / cancelled:非直接可操作
        return None;
    };
    Some(DirectAttention {
        step_id: step.id,
        reason,
    })
}

/// 一次运行的聚合(纯函数):直接原因数 + 优先处理节点。
/// 已取消/已归档运行不产生徽标;一个运行最多贡献一个徽标
/// (调用方按 Task 调用一次,天然去重 blocked 后代)。
pub fn attention_for_task(
    task: &TaskView,
    steps: &[StepView],
    latest_run_by_step: &HashMap<i64, RunView>,
    unavailable_session_steps: &HashSet<i64>,
    merge_conflict_step_ids: &HashSet<i64>,
    has_unmapped_merge_conflict: bool,
    open_questions: usize,
) -> Option<WorkflowRunAttention> {
    if matches!(task.status, TaskStatus::Cancelled | TaskStatus::Archived) {
        return None;
    }
    // 只有存在 Pipeline Revision 的 Task 才是工作流运行
    if task.revision_count == 0 {
        return None;
    }
    let mut directs: Vec<DirectAttention> = steps
        .iter()
        .filter_map(|step| {
            direct_attention_for_step(
                step,
                latest_run_by_step.get(&step.id),
                unavailable_session_steps.contains(&step.id),
                merge_conflict_step_ids.contains(&step.id),
            )
        })
        .collect();
    if has_unmapped_merge_conflict && steps.is_empty() {
        // 无步骤投影但存在待处理冲突(极端恢复态):仍按冲突计数
        return Some(WorkflowRunAttention {
            project_root: PathBuf::new(),
            task_id: task.id,
            task_title: task.title.clone(),
            reason_count: 1,
            focus_step_id: None,
        });
    }
    if directs.is_empty() && !has_unmapped_merge_conflict && open_questions == 0 {
        return None;
    }
    let mut reason_count = directs.len();
    if has_unmapped_merge_conflict
        && !directs
            .iter()
            .any(|d| d.reason == DirectAttentionReason::MergeConflict)
    {
        reason_count += 1;
    }
    if open_questions > 0 {
        reason_count += open_questions;
    }
    // focus:优先级升序,同优先级按 Step ID 稳定排序
    directs.sort_by_key(|d| (d.reason.priority(), d.step_id));
    let focus_step_id = directs.first().map(|d| d.step_id);
    if directs.is_empty() {
        // 只有冲突/问题等运行级原因:focus 无节点
        return Some(WorkflowRunAttention {
            project_root: PathBuf::new(),
            task_id: task.id,
            task_title: task.title.clone(),
            reason_count,
            focus_step_id: None,
        });
    }
    Some(WorkflowRunAttention {
        project_root: PathBuf::new(),
        task_id: task.id,
        task_title: task.title.clone(),
        reason_count,
        focus_step_id,
    })
}

/// 唯一发布物:revision 单调递增,内容整体替换。
pub struct ProjectOverviewSnapshot {
    pub revision: u64,
    pub projects: Vec<ProjectOverview>,
    pub agent_cards: Vec<AgentCardOverview>,
    pub global_active_runs: usize,
    pub templates: Vec<(String, String, PipelineDraft)>,
    /// 运行级「需要你」投影(Task 7):一个运行最多一项。
    pub attention_runs: Vec<WorkflowRunAttention>,
    /// 徽标计数 = attention_runs.len()(按运行数,不按被阻塞节点数)。
    pub attention_run_count: usize,
}

/// Hub 共享的跨项目资源。
pub struct HubCtx {
    pub registry: Arc<SessionRegistry>,
    pub catalog: Arc<RwLock<ProfileCatalog>>,
    pub plugins: Arc<PluginRegistry>,
    pub limiter: Arc<GlobalLimiter>,
    pub keep_awake: Arc<crate::runtime_host::KeepAwake>,
}

struct Attachment {
    orchestrator: Arc<Orchestrator>,
    stop: Arc<AtomicBool>,
    drain_handle: Option<std::thread::JoinHandle<()>>,
}

struct HubState {
    revision: u64,
    attachments: HashMap<PathBuf, Attachment>,
}

pub struct ProjectOverviewHub {
    ctx: Arc<HubCtx>,
    state: Mutex<HubState>,
    snapshot: RwLock<Arc<ProjectOverviewSnapshot>>,
    notify: (
        crossbeam_channel::Sender<()>,
        crossbeam_channel::Receiver<()>,
    ),
}

impl ProjectOverviewHub {
    pub fn new(ctx: Arc<HubCtx>) -> Arc<ProjectOverviewHub> {
        let hub = Arc::new(ProjectOverviewHub {
            ctx,
            state: Mutex::new(HubState {
                revision: 0,
                attachments: HashMap::new(),
            }),
            snapshot: RwLock::new(Arc::new(ProjectOverviewSnapshot {
                revision: 0,
                projects: Vec::new(),
                agent_cards: Vec::new(),
                global_active_runs: 0,
                templates: Vec::new(),
                attention_runs: Vec::new(),
                attention_run_count: 0,
            })),
            // 通知只表达「至少有一次重建需求」；容量 1 + try_send 天然合并，
            // drain 线程永不等待 rebuilder，因此不会把背压传回 Orchestrator。
            notify: crossbeam_channel::bounded(1),
        });
        hub.start_rebuilder();
        hub
    }

    fn start_rebuilder(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("mf-overview-rebuild".into())
            .spawn(move || {
                loop {
                    let Some(hub) = weak.upgrade() else {
                        return;
                    };
                    match hub.notify.1.recv_timeout(Duration::from_millis(200)) {
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                        Ok(()) => {}
                    }
                    // 固定合并窗，而不是等待「连续 50ms 无事件」；高频输出也必须
                    // 在有界时间内发布。后续通知留在容量 1 的槽中触发下一轮。
                    std::thread::sleep(Duration::from_millis(20));
                    while hub.notify.1.try_recv().is_ok() {}
                    hub.rebuild();
                }
            })
            .expect("overview rebuilder 线程启动失败");
    }

    /// attach:为该 Orchestrator 启动持续 drain 线程,并立即构建初始 overview。
    pub fn attach(&self, root: PathBuf, orchestrator: Arc<Orchestrator>) {
        let stop = Arc::new(AtomicBool::new(false));
        let drain_handle = {
            let stop = stop.clone();
            let notify_tx = self.notify.0.clone();
            let root = root.clone();
            let orchestrator = orchestrator.clone();
            std::thread::Builder::new()
                .name(format!("mf-event-drain-{}", root.display()))
                .spawn(move || {
                    loop {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        // 持续消费:比 bounded receiver 更靠近 Orchestrator,
                        // UI 是否读 snapshot 不影响调度正确性。
                        match orchestrator
                            .events_rx
                            .recv_timeout(Duration::from_millis(200))
                        {
                            Ok(_) => {
                                let _ = notify_tx.try_send(());
                            }
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .ok()
        };
        self.state.lock().attachments.insert(
            root.clone(),
            Attachment {
                orchestrator,
                stop,
                drain_handle,
            },
        );
        // 初次 attach 立即构建,不等待第一个事件
        self.request_refresh();
    }

    /// detach:停止 drain 线程并从 snapshot 删除该项目。
    pub fn detach(&self, root: &PathBuf) {
        let removed = self.state.lock().attachments.remove(root);
        if let Some(mut a) = removed {
            a.stop.store(true, Ordering::SeqCst);
            if let Some(h) = a.drain_handle.take() {
                let _ = h.join();
            }
        }
        self.request_refresh();
    }

    /// 标记总览需要从真实 Store/Registry 重建；重复请求被合并且永不阻塞调用方。
    pub fn request_refresh(&self) {
        let _ = self.notify.0.try_send(());
    }

    /// UI 唯一读取口:revision 更新时返回整体快照。
    pub fn snapshot_if_new(&self, last_revision: u64) -> Option<Arc<ProjectOverviewSnapshot>> {
        let snap = self.current();
        (snap.revision > last_revision).then_some(snap)
    }

    pub fn current(&self) -> Arc<ProjectOverviewSnapshot> {
        self.snapshot.read().clone()
    }

    /// 重建全部项目的真实快照(后台线程执行;GPUI render 不做 SQLite 查询)。
    fn rebuild(&self) {
        let orchs: Vec<(PathBuf, Arc<Orchestrator>)> = {
            let state = self.state.lock();
            state
                .attachments
                .iter()
                .map(|(r, a)| (r.clone(), a.orchestrator.clone()))
                .collect()
        };
        let mut projects = Vec::new();
        let mut agent_cards = Vec::new();
        let mut attention_runs = Vec::new();
        for (root, orch) in &orchs {
            let project_name = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.display().to_string());
            let tasks = orch.tasks().unwrap_or_default();
            let running = orch.store.running_runs().unwrap_or_default();
            let sessions = orch.sessions().unwrap_or_default();
            let mut task_cards = Vec::new();
            let mut task_titles: HashMap<i64, String> = HashMap::new();
            for t in &tasks {
                task_titles.insert(t.id, t.title.clone());
                let active_runs = running.iter().filter(|r| r.task_id == t.id).count();
                let open_questions = orch
                    .store
                    .open_questions(Some(t.id))
                    .map(|q| q.len())
                    .unwrap_or(0);
                let steps = orch.store.task_steps(t.id).unwrap_or_default();
                let runs = orch.store.list_runs_of_task(t.id).unwrap_or_default();
                let reasons: Vec<String> = steps
                    .iter()
                    .flat_map(|s| crate::run_node_details::needs_you_reasons(s, None))
                    .collect();
                let _ = &runs;
                task_cards.push(TaskCardOverview {
                    task: t.clone(),
                    active_runs,
                    open_questions,
                    needs_you_reasons: reasons,
                });
                // 运行级「需要你」(Task 7):每运行(内部 Task)最多一项,
                // 徽标按运行数计数,不按被阻塞节点数
                let latest_run_by_step: HashMap<i64, RunView> =
                    runs.iter().fold(HashMap::new(), |mut acc, r| {
                        let entry = acc.entry(r.step_id).or_insert(r.clone());
                        if r.id > entry.id {
                            *entry = r.clone();
                        }
                        acc
                    });
                let sessions_by_id: HashMap<i64, &SessionView> = sessions
                    .iter()
                    .map(|session| (session.id, session))
                    .collect();
                let project_key = root.to_string_lossy().to_string();
                let unavailable_session_steps: HashSet<i64> = latest_run_by_step
                    .iter()
                    .filter_map(|(step_id, run)| {
                        if run.status != RunStatus::Running {
                            return None;
                        }
                        let session_id = run.session_id?;
                        let unavailable = match sessions_by_id.get(&session_id) {
                            None => true,
                            Some(session) => {
                                session.status == SessionStatus::Dead
                                    || (session.status == SessionStatus::Idle
                                        && session.runtime != "http"
                                        && !self
                                            .ctx
                                            .registry
                                            .session_alive(&project_key, session_id))
                            }
                        };
                        unavailable.then_some(*step_id)
                    })
                    .collect();
                let pending_merges = orch
                    .store
                    .list_pending_merges(Some(t.id))
                    .unwrap_or_default();
                let has_merge_conflict = pending_merges.iter().any(|row| !row.conflicts.is_empty());
                let mut merge_conflict_step_ids: HashSet<i64> = HashSet::new();
                if has_merge_conflict {
                    let batches = orch
                        .store
                        .list_merge_batches_for_recovery()
                        .unwrap_or_default();
                    for batch in batches
                        .iter()
                        .filter(|batch| batch.task_id == t.id && batch.status == "needs_user")
                    {
                        let step_key = batch
                            .join_step_key
                            .strip_prefix("__single__:")
                            .unwrap_or(&batch.join_step_key);
                        if let Some(step) = steps.iter().find(|step| step.step_key == step_key) {
                            merge_conflict_step_ids.insert(step.id);
                        }
                    }
                    // 兼容没有 merge_batches 的旧投影：租约 metadata 仍带来源 step_key。
                    if merge_conflict_step_ids.is_empty() {
                        for row in &pending_merges {
                            let step_key = row
                                .lease
                                .metadata
                                .get("step_key")
                                .and_then(|value| value.as_str());
                            if let Some(step) = step_key
                                .and_then(|key| steps.iter().find(|step| step.step_key == key))
                            {
                                merge_conflict_step_ids.insert(step.id);
                            }
                        }
                    }
                }
                let has_unmapped_merge_conflict =
                    has_merge_conflict && merge_conflict_step_ids.is_empty();
                if let Some(mut attention) = attention_for_task(
                    t,
                    &steps,
                    &latest_run_by_step,
                    &unavailable_session_steps,
                    &merge_conflict_step_ids,
                    has_unmapped_merge_conflict,
                    open_questions,
                ) {
                    attention.project_root = root.clone();
                    attention.task_title = t.title.clone();
                    attention_runs.push(attention);
                }
            }
            let active_sessions = sessions
                .iter()
                .filter(|s| {
                    matches!(
                        s.status,
                        SessionStatus::Working | SessionStatus::Starting | SessionStatus::Waiting
                    )
                })
                .count();
            projects.push(ProjectOverview {
                root: root.clone(),
                name: project_name.clone(),
                tasks: task_cards,
                active_sessions,
            });

            // Agent 卡片(与旧 poll_snapshot 相同的聚合规则)
            let mut all_runs = running.clone();
            for t in &tasks {
                for r in orch.runs_of_task(t.id).unwrap_or_default() {
                    if !all_runs.iter().any(|x| x.id == r.id) {
                        all_runs.push(r);
                    }
                }
            }
            for session in &sessions {
                if session.status == SessionStatus::Hidden {
                    continue; // 已确认隐藏的会话不进实时看板
                }
                let run = all_runs
                    .iter()
                    .filter(|r| r.session_id == Some(session.id))
                    .max_by_key(|r| r.id)
                    .cloned();
                let task_id = run.as_ref().map(|r| r.task_id);
                let task_title = run
                    .as_ref()
                    .and_then(|r| task_titles.get(&r.task_id).cloned());
                let profile_display = {
                    let catalog = self.ctx.catalog.read();
                    catalog
                        .specs
                        .get(&session.agent_profile)
                        .map(|s| s.display_name.clone())
                        .unwrap_or_else(|| session.agent_profile.clone())
                };
                let is_http = session.runtime == "http";
                let project_str = root.to_string_lossy().to_string();
                let tail = if is_http {
                    Vec::new()
                } else {
                    self.ctx.registry.pty_tail(&project_str, session.id, 4)
                };
                let alive = self.ctx.registry.session_alive(&project_str, session.id) || is_http;
                let bucket = bucket_of(session, &run, alive);
                agent_cards.push(AgentCardOverview {
                    project_root: root.clone(),
                    project_name: project_name.clone(),
                    session: session.clone(),
                    run,
                    task_id,
                    task_title,
                    profile_display,
                    tail,
                    alive,
                    bucket,
                    is_http,
                });
            }
        }
        let working = agent_cards
            .iter()
            .any(|c| c.bucket == AttentionBucket::Working);
        self.ctx.keep_awake.set_working(working);
        let templates = self.ctx.plugins.templates();
        let mut state = self.state.lock();
        let live_roots: std::collections::HashSet<&PathBuf> = state.attachments.keys().collect();
        projects.retain(|project| live_roots.contains(&project.root));
        agent_cards.retain(|card| live_roots.contains(&card.project_root));
        attention_runs.retain(|run| live_roots.contains(&run.project_root));
        let attention_run_count = attention_runs.len();
        state.revision += 1;
        let revision = state.revision;
        *self.snapshot.write() = Arc::new(ProjectOverviewSnapshot {
            revision,
            projects,
            agent_cards,
            global_active_runs: self.ctx.limiter.active(),
            templates,
            attention_runs,
            attention_run_count,
        });
    }
}

impl Drop for ProjectOverviewHub {
    fn drop(&mut self) {
        let attachments: Vec<Attachment> = self
            .state
            .get_mut()
            .attachments
            .drain()
            .map(|(_, attachment)| attachment)
            .collect();
        for attachment in &attachments {
            attachment.stop.store(true, Ordering::SeqCst);
        }
        for mut attachment in attachments {
            if let Some(handle) = attachment.drain_handle.take() {
                let _ = handle.join();
            }
        }
        self.ctx.keep_awake.set_working(false);
    }
}

/// 看板分桶规则(与旧 column_of 一致)。
pub fn bucket_of(session: &SessionView, run: &Option<RunView>, alive: bool) -> AttentionBucket {
    if let Some(run) = run {
        match run.status {
            RunStatus::Failed | RunStatus::Interrupted | RunStatus::AwaitingOutcome => {
                return AttentionBucket::NeedsYou;
            }
            RunStatus::Running => return AttentionBucket::Working,
            RunStatus::Succeeded => return AttentionBucket::Done, // 成功但未确认 → 已完成
            RunStatus::Cancelled => {}
        }
    }
    match session.status {
        SessionStatus::Waiting | SessionStatus::BlockedState | SessionStatus::Dead => {
            AttentionBucket::NeedsYou
        }
        SessionStatus::Working | SessionStatus::Starting => AttentionBucket::Working,
        SessionStatus::Done => AttentionBucket::Done,
        SessionStatus::Idle => {
            if alive {
                AttentionBucket::Idle
            } else {
                AttentionBucket::NeedsYou
            }
        }
        SessionStatus::Hidden => AttentionBucket::Idle,
    }
}
