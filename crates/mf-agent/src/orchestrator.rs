//! v2 调度器(Orchestrator):自动派发 ready Step、并发限制、会话串行化、
//! 显式结算驱动状态机、失败阻塞后代、暂停/编辑 Revision。
//!
//! 生命周期规则见 `CONTEXT.md`;与 Runtime 的边界见 `runtime.rs`。

use crate::config::Config;
use crate::model::*;
use crate::pipeline::{PipelineDraft, ProfileIndex, SessionPolicy};
use crate::runtime::{AgentProfileSpec, LaunchSpec, RuntimeEvent, RuntimeHost};
use crate::store::Store;
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// 跨项目全局并发限制(默认 4,可在设置中修改)。
pub struct GlobalLimiter {
    max: AtomicUsize,
    active: AtomicUsize,
}

impl GlobalLimiter {
    pub fn new(max: usize) -> Arc<GlobalLimiter> {
        Arc::new(GlobalLimiter {
            max: AtomicUsize::new(max.max(1)),
            active: AtomicUsize::new(0),
        })
    }
    pub fn set_max(&self, max: usize) {
        self.max.store(max.max(1), Ordering::SeqCst);
    }
    pub fn max(&self) -> usize {
        self.max.load(Ordering::SeqCst)
    }
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
    fn try_begin(&self) -> bool {
        loop {
            let a = self.active.load(Ordering::SeqCst);
            if a >= self.max.load(Ordering::SeqCst) {
                return false;
            }
            if self
                .active
                .compare_exchange(a, a + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
    }
    fn end(&self) {
        loop {
            let a = self.active.load(Ordering::SeqCst);
            if a == 0 {
                return;
            }
            if self
                .active
                .compare_exchange(a, a - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return;
            }
        }
    }
}

/// 可刷新的 Agent Profile 目录(插件注册表投影)。
#[derive(Default)]
pub struct ProfileCatalog {
    pub index: ProfileIndex,
    pub specs: HashMap<String, AgentProfileSpec>,
}

pub struct Orchestrator {
    pub store: Arc<Store>,
    pub root: PathBuf,
    /// 项目根(字符串形式,作为 RuntimeHost 作用域键)。
    root_str: String,
    pub pipe_name: String,
    host: Arc<dyn RuntimeHost>,
    config: Config,
    profiles: Arc<RwLock<ProfileCatalog>>,
    global: Arc<GlobalLimiter>,
    events_tx: Sender<SchedulerEvent>,
    pub events_rx: Receiver<SchedulerEvent>,
    runtime_tx: Sender<(i64, RuntimeEvent)>,
    runtime_rx: Receiver<(i64, RuntimeEvent)>,
    stop: Arc<AtomicBool>,
    /// 本调度器派发、仍占用并发槽的 run。
    active_dispatches: Mutex<HashSet<i64>>,
    /// 手动"继续会话"重试:step → 存活会话 id(一次性,dispatch 消费)。
    continue_sessions: Mutex<HashMap<i64, i64>>,
}

impl Orchestrator {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        store: Arc<Store>,
        root: PathBuf,
        config: Config,
        host: Arc<dyn RuntimeHost>,
        profiles: Arc<RwLock<ProfileCatalog>>,
        global: Arc<GlobalLimiter>,
        pipe_name: String,
    ) -> Result<Arc<Orchestrator>> {
        // 异常退出恢复:未结算 Agent Run → interrupted,Task → needs-you
        let recovered = store.recover_interrupted()?;
        let orphan_steps = store.repair_orphan_steps()?;
        if !orphan_steps.is_empty() {
            log::warn!(
                "崩溃恢复:修复 {} 个无活动 run 的孤儿 step",
                orphan_steps.len()
            );
        }
        let (events_tx, events_rx) = crossbeam_channel::bounded(8192);
        let (runtime_tx, runtime_rx) = crossbeam_channel::bounded(8192);
        let stop = Arc::new(AtomicBool::new(false));
        let root_str = root.to_string_lossy().to_string();
        let orch = Arc::new(Orchestrator {
            store,
            root,
            root_str,
            pipe_name,
            host,
            config,
            profiles,
            global,
            events_tx,
            events_rx,
            runtime_tx,
            runtime_rx,
            stop: stop.clone(),
            active_dispatches: Mutex::new(HashSet::new()),
            continue_sessions: Mutex::new(HashMap::new()),
        });
        for run in &recovered {
            if let Some(t) = orch.store.task_view(run.task_id)? {
                orch.emit(SchedulerEvent::TaskUpdated(t));
            }
            orch.emit(SchedulerEvent::RunUpdated(run.clone()));
        }
        if !recovered.is_empty() {
            orch.emit(SchedulerEvent::Log {
                run_id: 0,
                text: format!(
                    "崩溃恢复:{} 个未结算 Agent Run 已标记为 interrupted",
                    recovered.len()
                ),
            });
        }
        std::thread::Builder::new()
            .name("mf-dispatch".into())
            .spawn({
                let orch = orch.clone();
                move || loop {
                    if orch.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Err(e) = orch.tick() {
                        orch.emit(SchedulerEvent::Error(format!("调度错误: {e:#}")));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            })?;
        std::thread::Builder::new()
            .name("mf-runtime-pump".into())
            .spawn({
                let orch = orch.clone();
                move || loop {
                    if orch.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match orch
                        .runtime_rx
                        .recv_timeout(std::time::Duration::from_millis(200))
                    {
                        Ok((run_id, ev)) => {
                            if let Err(e) = orch.apply_runtime_event(run_id, ev) {
                                orch.emit(SchedulerEvent::Error(format!("运行时事件错误: {e:#}")));
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
        Ok(orch)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // 停止所有在途运行(会话进程交给 Session Registry 决定是否保留)
        if let Ok(runs) = self.store.running_runs() {
            for run in runs {
                self.host.stop_run(&self.root_str, run.id);
            }
        }
    }

    pub fn host(&self) -> Arc<dyn RuntimeHost> {
        self.host.clone()
    }

    pub fn profiles(&self) -> Arc<RwLock<ProfileCatalog>> {
        self.profiles.clone()
    }

    pub fn global_limiter(&self) -> Arc<GlobalLimiter> {
        self.global.clone()
    }

    fn emit(&self, ev: SchedulerEvent) {
        // 状态转换事件必须送达;高频事件(日志/transcript)满时丢弃,避免 UI 停顿阻塞调度线程
        match &ev {
            SchedulerEvent::Log { .. } | SchedulerEvent::Transcript { .. } => {
                let _ = self.events_tx.try_send(ev);
            }
            _ => {
                let _ = self.events_tx.send(ev);
            }
        }
    }

    fn release_slot(&self, run_id: i64) {
        if self.active_dispatches.lock().remove(&run_id) {
            self.global.end();
        }
    }

    // ---------- Task 生命周期 ----------

    pub fn create_task(&self, title: &str, goal: &str) -> Result<TaskView> {
        let t = self.store.create_task(title, goal)?;
        self.emit(SchedulerEvent::TaskUpdated(t.clone()));
        Ok(t)
    }

    pub fn update_task_meta(&self, task_id: i64, title: &str, goal: &str) -> Result<TaskView> {
        let t = self
            .store
            .update_task_meta(task_id, title, goal)?
            .ok_or_else(|| anyhow::anyhow!("任务 {task_id} 不存在"))?;
        self.emit(SchedulerEvent::TaskUpdated(t.clone()));
        Ok(t)
    }

    pub fn archive_task(&self, task_id: i64) -> Result<()> {
        if self.task_has_active_runs(task_id)? {
            anyhow::bail!("任务仍有活动 Agent Run,请先停止");
        }
        self.store.set_task_status(task_id, TaskStatus::Archived)?;
        self.emit(SchedulerEvent::TaskRemoved(task_id));
        Ok(())
    }

    fn task_has_active_runs(&self, task_id: i64) -> Result<bool> {
        self.store.task_has_active_runs(task_id)
    }

    /// Step 最近一次 run 关联的存活会话(继续会话重试的前提)。
    fn live_session_of_step(&self, step_id: i64) -> Result<Option<SessionView>> {
        let session_id = self
            .store
            .list_runs_of_step(step_id)?
            .into_iter()
            .rev()
            .find_map(|run| run.session_id);
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        Ok(self
            .store
            .session_view(session_id)?
            .filter(|s| !matches!(s.status, SessionStatus::Dead | SessionStatus::Hidden)))
    }

    pub fn pause_task(&self, task_id: i64) -> Result<TaskView> {
        let t = self
            .store
            .set_task_paused(task_id, true)?
            .ok_or_else(|| anyhow::anyhow!("任务 {task_id} 不存在"))?;
        self.emit(SchedulerEvent::TaskUpdated(t.clone()));
        Ok(t)
    }

    pub fn resume_task(&self, task_id: i64) -> Result<TaskView> {
        let mut t = self
            .store
            .set_task_paused(task_id, false)?
            .ok_or_else(|| anyhow::anyhow!("任务 {task_id} 不存在"))?;
        if t.status == TaskStatus::Ready {
            t = self
                .store
                .set_task_status(task_id, TaskStatus::Running)?
                .unwrap_or(t);
        }
        self.emit(SchedulerEvent::TaskUpdated(t.clone()));
        Ok(t)
    }

    /// 终止任务:停止在途运行,未终结 Step → cancelled,Task → cancelled。
    pub fn cancel_task(&self, task_id: i64) -> Result<TaskView> {
        for run in self.store.running_runs()? {
            if run.task_id != task_id {
                continue;
            }
            self.host.stop_run(&self.root_str, run.id);
            if let Some(r) = self.store.set_run_status(run.id, RunStatus::Cancelled)? {
                self.release_slot(run.id);
                self.emit(SchedulerEvent::RunUpdated(r));
            }
        }
        for step in self.store.task_steps(task_id)? {
            if !step.status.terminal() {
                if let Some(s) = self.store.set_step_status(step.id, StepStatus::Cancelled)? {
                    self.emit(SchedulerEvent::StepUpdated(s));
                }
            }
        }
        if let Some(rev) = self.store.active_revision(task_id)? {
            self.store.with_tx(|c| {
                c.execute(
                    "UPDATE pipeline_revisions SET status = 'cancelled' WHERE id = ?1",
                    rusqlite::params![rev.id],
                )?;
                Ok(())
            })?;
        }
        let t = self
            .store
            .set_task_status(task_id, TaskStatus::Cancelled)?
            .ok_or_else(|| anyhow::anyhow!("任务 {task_id} 不存在"))?;
        self.emit(SchedulerEvent::TaskUpdated(t.clone()));
        Ok(t)
    }

    // ---------- 流水线 ----------

    /// 校验并保存草案(初始草案 / Planner 提案 / 暂停后的编辑)。
    /// 对已有活动 revision 的任务,编辑只能改动尚未启动的 Step。
    pub fn save_pipeline(&self, task_id: i64, draft: &PipelineDraft) -> Result<Vec<String>> {
        if let Some(t) = self.store.task_view(task_id)? {
            if t.status == TaskStatus::Running && !t.paused {
                anyhow::bail!("运行中修改 DAG 必须先暂停");
            }
        }
        {
            let catalog = self.profiles.read();
            let errs = draft.validate(&catalog.index);
            if !errs.is_empty() {
                return Err(anyhow::anyhow!("流水线校验失败:\n{}", errs.join("\n")));
            }
        }
        let has_active = self.store.active_revision(task_id)?.is_some();
        let rev = if has_active {
            self.store.save_edited_revision(task_id, draft)?
        } else {
            self.store.create_draft_revision(task_id, draft)?
        };
        // 重新计算 ready 队列(编辑产生的 Revision 继承已终结 Step 状态)
        if has_active {
            self.store.with_tx(|c| Store::promote_ready_tx(c, rev.id))?;
        }
        self.emit(SchedulerEvent::RevisionCreated(rev));
        let t = self.store.task_view(task_id)?;
        if let Some(t) = t {
            self.emit(SchedulerEvent::TaskUpdated(t));
        }
        Ok(Vec::new())
    }

    /// Planner 提案:只保存草案,必须经用户确认后才会运行(不得绕过确认)。
    pub fn planner_propose(&self, task_id: i64, draft: &PipelineDraft) -> Result<()> {
        if !self
            .store
            .task_view(task_id)?
            .map(|t| {
                matches!(
                    t.status,
                    TaskStatus::Draft | TaskStatus::NeedsYou | TaskStatus::Failed
                )
            })
            .unwrap_or(false)
        {
            anyhow::bail!("任务当前状态不接受 Planner 提案");
        }
        let errs = {
            let catalog = self.profiles.read();
            draft.validate(&catalog.index)
        };
        if !errs.is_empty() {
            anyhow::bail!("Planner 草案校验失败:\n{}", errs.join("\n"));
        }
        self.store.create_draft_revision(task_id, draft)?;
        self.store.set_task_status(task_id, TaskStatus::Draft)?;
        let t = self.store.task_view(task_id)?;
        if let Some(t) = t {
            self.emit(SchedulerEvent::TaskUpdated(t));
        }
        Ok(())
    }

    /// 确认当前草案并开始运行(用户显式动作)。
    pub fn confirm_and_run(&self, task_id: i64) -> Result<TaskView> {
        let t = self
            .store
            .activate_revision(task_id)?
            .ok_or_else(|| anyhow::anyhow!("任务 {task_id} 不存在"))?;
        let t = self
            .store
            .set_task_status(task_id, TaskStatus::Running)?
            .unwrap_or(t);
        self.emit(SchedulerEvent::TaskUpdated(t.clone()));
        Ok(t)
    }

    /// 失败节点操作:重试。`RetryMode::ContinueSession` 只对存活会话合法,
    /// 其余场景必须显式 `FreshSession`(设计 §9.6)。
    pub fn retry_step(&self, step_id: i64, mode: RetryMode) -> Result<StepView> {
        let step = self
            .store
            .step_view(step_id)?
            .ok_or_else(|| anyhow::anyhow!("Step {step_id} 不存在"))?;
        if !matches!(
            step.status,
            StepStatus::Failed
                | StepStatus::Blocked
                | StepStatus::Cancelled
                | StepStatus::AwaitingOutcome
        ) {
            anyhow::bail!("仅失败/阻塞/待结算的 Step 可以重试");
        }
        // blocked 的根因是依赖失败:重试前确认依赖已成功/跳过(不绕过失败阻塞语义)
        let deps_ok = step
            .deps
            .iter()
            .filter_map(|d| self.store.step_view(*d).ok().flatten())
            .all(|d| matches!(d.status, StepStatus::Succeeded | StepStatus::Skipped));
        if !deps_ok {
            anyhow::bail!("上游依赖尚未成功/跳过;请先重试失败的上游节点");
        }
        // 继续会话只对存活的会话合法;否则必须显式选择新会话
        let continue_session = match mode {
            RetryMode::FreshSession => None,
            RetryMode::ContinueSession => {
                let live = self.live_session_of_step(step_id)?;
                let Some(session) = live else {
                    anyhow::bail!(
                        "Step {step_id} 没有存活的会话可继续;请使用 FreshSession 创建新会话"
                    );
                };
                Some(session.id)
            }
        };
        // 先取消该 step 的活动 run(否则 awaiting-outcome run 永远占用
        // task_attention_cleared 与 busy_keys,needs-you 无法清除、reuse 键死锁)
        if let Some(active) = self.store.active_run_of_step(step_id)? {
            if let Some(r) = self.store.set_run_status(active.id, RunStatus::Cancelled)? {
                self.release_slot(r.id);
                self.emit(SchedulerEvent::RunUpdated(r));
            }
        }
        if let Some(session_id) = continue_session {
            self.continue_sessions.lock().insert(step_id, session_id);
        } else {
            self.continue_sessions.lock().remove(&step_id);
        }
        let s = self
            .store
            .set_step_status(step_id, StepStatus::Ready)?
            .ok_or_else(|| anyhow::anyhow!("Step {step_id} 不存在"))?;
        // 后代从 blocked 恢复为 pending 由 promote 重新计算
        if let Some(rev) = self.store.active_revision(step.task_id)? {
            self.store.with_tx(|c| Store::promote_ready_tx(c, rev.id))?;
        }
        let task = self.store.task_view(step.task_id)?;
        if let Some(t) = task {
            if matches!(
                t.status,
                TaskStatus::Failed | TaskStatus::NeedsYou | TaskStatus::Ready | TaskStatus::Draft
            ) {
                self.store.set_task_status(t.id, TaskStatus::Running)?;
            }
            if t.paused {
                self.store.set_task_paused(t.id, false)?;
            }
            if let Some(t) = self.store.task_view(t.id)? {
                self.emit(SchedulerEvent::TaskUpdated(t));
            }
        }
        self.emit(SchedulerEvent::StepUpdated(s.clone()));
        Ok(s)
    }

    /// 失败节点操作:跳过(必须人工确认)。
    pub fn skip_step(&self, step_id: i64, confirmed: bool) -> Result<StepView> {
        if !confirmed {
            anyhow::bail!("跳过必须人工确认");
        }
        let step = self
            .store
            .step_view(step_id)?
            .ok_or_else(|| anyhow::anyhow!("Step {step_id} 不存在"))?;
        // 跳过同样先取消活动 run(原因同 retry)
        if let Some(active) = self.store.active_run_of_step(step_id)? {
            if let Some(r) = self.store.set_run_status(active.id, RunStatus::Cancelled)? {
                self.release_slot(r.id);
                self.emit(SchedulerEvent::RunUpdated(r));
            }
        }
        let s = self
            .store
            .set_step_status(step_id, StepStatus::Skipped)?
            .ok_or_else(|| anyhow::anyhow!("Step {step_id} 不存在"))?;
        if let Some(rev) = self.store.active_revision(step.task_id)? {
            for s in self.store.with_tx(|c| Store::promote_ready_tx(c, rev.id))? {
                self.emit(SchedulerEvent::StepUpdated(s));
            }
        }
        self.emit(SchedulerEvent::StepUpdated(s.clone()));
        self.check_convergence(step.task_id)?;
        Ok(s)
    }

    /// 失败节点操作:替换 Agent(产生新 Revision;该 Step 重置为 pending)。
    pub fn replace_agent(&self, step_id: i64, profile_id: &str) -> Result<RevisionView> {
        {
            let catalog = self.profiles.read();
            if !catalog.index.is_usable(profile_id) {
                anyhow::bail!("Agent Profile `{profile_id}` 不可用");
            }
        }
        let step = self
            .store
            .step_view(step_id)?
            .ok_or_else(|| anyhow::anyhow!("Step {step_id} 不存在"))?;
        let all = self.store.task_steps(step.task_id)?;
        let id_of: HashMap<&str, i64> = all.iter().map(|s| (s.step_key.as_str(), s.id)).collect();
        let mut draft = PipelineDraft {
            steps: all
                .iter()
                .map(|s| crate::pipeline::StepDraft {
                    key: s.step_key.clone(),
                    title: s.title.clone(),
                    instructions: s.instructions.clone(),
                    agent_profile: s.agent_profile.clone(),
                    session_policy: SessionPolicy::parse_db(&s.session_policy),
                    deps: s
                        .deps
                        .iter()
                        .filter_map(|d| all.iter().find(|x| x.id == *d).map(|x| x.step_key.clone()))
                        .collect(),
                })
                .collect(),
        };
        if let Some(target) = draft
            .steps
            .iter_mut()
            .find(|s| id_of.get(s.key.as_str()) == Some(&step_id))
        {
            target.agent_profile = profile_id.to_string();
        } else {
            anyhow::bail!("Step 不在当前 Revision 中");
        }
        let rev = self.store.save_edited_revision(step.task_id, &draft)?;
        // 目标 Step 重置为 pending(替换 Agent 视为重新开始该节点)
        let new_steps = self.store.revision_steps(rev.id)?;
        if let Some(ns) = new_steps.iter().find(|s| s.step_key == step.step_key) {
            self.store.set_step_status(ns.id, StepStatus::Pending)?;
        }
        self.store.with_tx(|c| Store::promote_ready_tx(c, rev.id))?;
        self.emit(SchedulerEvent::RevisionCreated(rev.clone()));
        for s in self.store.revision_steps(rev.id)? {
            self.emit(SchedulerEvent::StepUpdated(s));
        }
        if let Some(t) = self.store.task_view(step.task_id)? {
            if t.status != TaskStatus::Running {
                if let Some(t) = self.store.set_task_status(t.id, TaskStatus::Running)? {
                    self.emit(SchedulerEvent::TaskUpdated(t));
                }
            } else {
                self.emit(SchedulerEvent::TaskUpdated(t));
            }
        }
        Ok(rev)
    }

    // ---------- 结算 ----------

    /// 通过能力令牌结算(mfctl 管道路径)。
    pub fn settle_by_token(
        &self,
        token: &str,
        settlement: Settlement,
    ) -> std::result::Result<SettleOutcome, SettleError> {
        let (run, outcome) = self.store.settle_run_by_token(token, settlement.clone())?;
        if outcome == SettleOutcome::Applied {
            self.after_settlement(&run, &settlement);
        }
        Ok(outcome)
    }

    /// 手工结算(用户在「需要你」中的判定)。
    pub fn settle_run(
        &self,
        run_id: i64,
        settlement: Settlement,
    ) -> std::result::Result<SettleOutcome, SettleError> {
        let (run, outcome) = self.store.settle_run_by_id(run_id, settlement.clone())?;
        if outcome == SettleOutcome::Applied {
            self.after_settlement(&run, &settlement);
        }
        Ok(outcome)
    }

    fn after_settlement(&self, run: &RunView, settlement: &Settlement) {
        self.release_slot(run.id);
        // needs-you 的任务在「需要你」的诱因全部消除后回到 running,下游继续自动派发
        if let Some(t) = self.store.task_view(run.task_id).ok().flatten() {
            if t.status == TaskStatus::NeedsYou && self.task_attention_cleared(run.task_id) {
                if let Some(t) = self
                    .store
                    .set_task_status(t.id, TaskStatus::Running)
                    .ok()
                    .flatten()
                {
                    self.emit(SchedulerEvent::TaskUpdated(t));
                }
            }
        }
        self.emit(SchedulerEvent::RunUpdated(run.clone()));
        if let Some(step) = self.store.step_view(run.step_id).ok().flatten() {
            self.emit(SchedulerEvent::StepUpdated(step));
        }
        match settlement {
            Settlement::Complete { .. } => {
                if let Some(rev) = self.store.active_revision(run.task_id).ok().flatten() {
                    if let Ok(promoted) = self.store.with_tx(|c| Store::promote_ready_tx(c, rev.id))
                    {
                        for s in promoted {
                            self.emit(SchedulerEvent::StepUpdated(s));
                        }
                    }
                }
            }
            Settlement::Fail { .. } => {
                // 有限自动重试(设计 §9.6):attempts 计入已消耗尝试,
                // 未超过上限时以全新会话重跑(保留文件修改),
                // 不阻塞下游、不进入 needs-you。
                let step = self.store.step_view(run.step_id).ok().flatten();
                let auto_retry_left = step
                    .as_ref()
                    .map(|s| s.attempts <= s.auto_retry)
                    .unwrap_or(false);
                if auto_retry_left {
                    if let Some(s) = self
                        .store
                        .set_step_status(run.step_id, StepStatus::Ready)
                        .ok()
                        .flatten()
                    {
                        self.emit(SchedulerEvent::StepUpdated(s));
                    }
                    if let Some(t) = self.store.task_view(run.task_id).ok().flatten() {
                        if matches!(
                            t.status,
                            TaskStatus::NeedsYou | TaskStatus::Failed | TaskStatus::Ready
                        ) {
                            if let Some(t) = self
                                .store
                                .set_task_status(t.id, TaskStatus::Running)
                                .ok()
                                .flatten()
                            {
                                self.emit(SchedulerEvent::TaskUpdated(t));
                            }
                        }
                    }
                    self.emit(SchedulerEvent::Log {
                        run_id: run.id,
                        text: format!(
                            "自动重试:第 {} 次尝试失败,上限内以新会话重跑",
                            step.map(|s| s.attempts).unwrap_or(0)
                        ),
                    });
                    // 自动重试路径不检查收敛(节点回到 ready)
                    return;
                }
                if let Ok(blocked) = self.store.block_descendants(run.step_id) {
                    for s in blocked {
                        self.emit(SchedulerEvent::StepUpdated(s));
                    }
                }
                // 失败需要人工决策(重试/跳过/替换/终止);独立分支继续运行
                if let Some(t) = self.store.task_view(run.task_id).ok().flatten() {
                    if !t.status.terminal() {
                        if let Some(t) = self
                            .store
                            .set_task_status(t.id, TaskStatus::NeedsYou)
                            .ok()
                            .flatten()
                        {
                            self.store.set_task_unread(t.id, true).ok();
                            self.emit(SchedulerEvent::TaskUpdated(t));
                        }
                    }
                }
            }
        }
        if let Err(e) = self.check_convergence(run.task_id) {
            self.emit(SchedulerEvent::Error(format!("收敛检查失败: {e:#}")));
        }
    }

    fn check_convergence(&self, task_id: i64) -> Result<()> {
        // 取消/归档的任务不被后续收敛改写;failed 任务在人工 skip 后允许重新收敛
        if let Some(t) = self.store.task_view(task_id)? {
            if matches!(t.status, TaskStatus::Cancelled | TaskStatus::Archived) {
                return Ok(());
            }
        }
        let Some(rev) = self.store.active_revision(task_id)? else {
            return Ok(());
        };
        match self.store.revision_converged(rev.id)? {
            None => {}
            Some(true) => {
                if let Some(t) = self.store.set_task_status(task_id, TaskStatus::Succeeded)? {
                    self.emit(SchedulerEvent::TaskUpdated(t));
                }
            }
            Some(false) => {
                if let Some(t) = self.store.set_task_status(task_id, TaskStatus::Failed)? {
                    self.emit(SchedulerEvent::TaskUpdated(t));
                }
            }
        }
        Ok(())
    }

    // ---------- 人在环 ----------

    /// 继续发送提示:awaiting-outcome 的 Run 回到 running。
    pub fn send_prompt(&self, run_id: i64, text: &str) -> Result<()> {
        let run = self
            .store
            .run_view(run_id)?
            .ok_or_else(|| anyhow::anyhow!("Run {run_id} 不存在"))?;
        if !matches!(run.status, RunStatus::Running | RunStatus::AwaitingOutcome) {
            anyhow::bail!("Run 已终结,不能再发送提示");
        }
        if let Some(session_id) = run.session_id {
            self.host
                .send_prompt(&self.root_str, run_id, session_id, text);
        }
        if run.status == RunStatus::AwaitingOutcome {
            if let Some(r) = self.store.set_run_status(run_id, RunStatus::Running)? {
                self.emit(SchedulerEvent::RunUpdated(r));
            }
            if let Some(s) = self.store.step_view(run.step_id)? {
                if s.status == StepStatus::AwaitingOutcome {
                    if let Some(s) = self
                        .store
                        .set_step_status(run.step_id, StepStatus::Running)?
                    {
                        self.emit(SchedulerEvent::StepUpdated(s));
                    }
                }
            }
            if let Some(t) = self.store.task_view(run.task_id)? {
                if t.status == TaskStatus::NeedsYou {
                    if let Some(t) = self.store.set_task_status(t.id, TaskStatus::Running)? {
                        self.emit(SchedulerEvent::TaskUpdated(t));
                    }
                }
            }
            // 重新占用并发槽:拿不到就保持 awaiting-outcome(不无槽运行)
            let acquired = {
                let mut active = self.active_dispatches.lock();
                if active.contains(&run_id) {
                    true
                } else if self.global.try_begin() {
                    active.insert(run_id);
                    true
                } else {
                    false
                }
            };
            if !acquired {
                anyhow::bail!(
                    "全局并发已满({}/{}),稍后重试或调高设置",
                    self.global.active(),
                    self.global.max()
                );
            }
        }
        if let Some(session_id) = run.session_id {
            if let Some(s) = self
                .store
                .update_session(session_id, None, Some(text), None)?
            {
                self.emit(SchedulerEvent::SessionUpdated(s));
            }
        }
        Ok(())
    }

    pub fn answer_question(&self, question_id: i64, answer: &str) -> Result<()> {
        let q = self
            .store
            .open_questions(None)?
            .into_iter()
            .find(|q| q.id == question_id)
            .ok_or_else(|| anyhow::anyhow!("问题 {question_id} 不存在或已回答"))?;
        let answered = self
            .store
            .answer_question(question_id, answer)?
            .ok_or_else(|| anyhow::anyhow!("回答失败"))?;
        if let Some(run_id) = q.run_id {
            self.host.answer_question(&self.root_str, run_id, answer);
            if let Some(run) = self.store.run_view(run_id)? {
                self.emit(SchedulerEvent::RunUpdated(run));
            }
        }
        if let Some(step_id) = q.step_id {
            // 只有 needs-input 的 step 回到 running(迟到回答不得覆盖终态)
            if let Some(cur) = self.store.step_view(step_id)? {
                if cur.status == StepStatus::NeedsInput {
                    if let Some(s) = self.store.set_step_status(step_id, StepStatus::Running)? {
                        self.emit(SchedulerEvent::StepUpdated(s));
                    }
                }
            }
        }
        if let Some(t) = self.store.task_view(q.task_id)? {
            if t.status == TaskStatus::NeedsYou && self.task_attention_cleared(q.task_id) {
                if let Some(t) = self.store.set_task_status(t.id, TaskStatus::Running)? {
                    self.emit(SchedulerEvent::TaskUpdated(t));
                }
            }
        }
        self.emit(SchedulerEvent::QuestionAnswered(answered));
        Ok(())
    }

    // ---------- 离散 CLI 会话 ----------

    /// 在任务下创建离散 CLI 会话(设计 §4.7 / §10):
    /// 不属于 Revision、没有 Step / Agent Run、绝不改变 Task 状态。
    /// 路由键是 ad_hoc_sessions 行号,宿主以 (project, session_id) 定位进程。
    pub fn create_ad_hoc_session(
        &self,
        task_id: i64,
        instance_snapshot: &crate::agent_instance::AgentInstanceSnapshot,
        launch_mode: crate::model::RunMode,
        trusted_run_temp: PathBuf,
        plan: crate::agent_adapter::LaunchPlan,
    ) -> Result<AdHocSessionView> {
        if plan.run_temp != trusted_run_temp {
            anyhow::bail!("Agent Adapter 试图改写可信 run-temp,已拒绝启动");
        }
        self.store
            .task_view(task_id)?
            .ok_or_else(|| anyhow::anyhow!("任务 {task_id} 不存在"))?;
        let view = self
            .store
            .insert_ad_hoc_session(task_id, instance_snapshot)?;
        let launch = self.host.launch_ad_hoc(crate::runtime::AdHocLaunchSpec {
            task_id,
            session_id: view.id,
            title: view.title.clone(),
            run_mode: launch_mode,
            plan,
            run_temp: trusted_run_temp,
            workdir: self.root.clone(),
            events: self.runtime_tx.clone(),
        });
        if let Err(error) = launch {
            if let Some(dead) = self.store.set_ad_hoc_status(view.id, SessionStatus::Dead)? {
                self.emit(SchedulerEvent::AdHocSessionUpdated(dead));
            }
            return Err(anyhow::anyhow!("离散会话 {} 启动失败: {error:#}", view.id));
        }
        // 启动成功但 DB 写失败:必须补偿杀进程,不留孤儿 CLI
        let launched = match self.store.mark_ad_hoc_launched(view.id) {
            Ok(Some(view)) => view,
            Ok(None) => anyhow::bail!("离散会话 {} 启动后读取失败", view.id),
            Err(db_error) => {
                self.host.kill_ad_hoc(&self.root_str, view.id);
                if let Some(dead) = self.store.set_ad_hoc_status(view.id, SessionStatus::Dead)? {
                    self.emit(SchedulerEvent::AdHocSessionUpdated(dead));
                }
                return Err(db_error.context(format!(
                    "离散会话 {} 启动后状态写入失败,已终止进程",
                    view.id
                )));
            }
        };
        self.emit(SchedulerEvent::AdHocSessionUpdated(launched.clone()));
        // 显式不触碰 Task 状态:离散会话不参与成功判定
        Ok(launched)
    }

    /// 离散 CLI 会话退出处理(宿主经 `RuntimeEvent::AdHocExited` 上报;
    /// mfctl/测试也可直接调用)。按完成契约与退出码分类终态:
    /// - oneshot + process-exit:退出码 0 → Done,否则 Dead;
    /// - oneshot + stdout-marker / result-file:以标记/文件为准;
    /// - interactive / manual:退出不判成功 → Dead(不误报 Done)。
    /// 迟到事件(已终结/已提交 Handoff 的行)不复活、不改写。
    pub fn handle_ad_hoc_exit(
        &self,
        session_id: i64,
        exit_code: Option<i32>,
        marker_seen: bool,
        result_file_present: bool,
    ) {
        if let Err(error) =
            self.apply_ad_hoc_exit(session_id, exit_code, marker_seen, result_file_present)
        {
            self.emit(SchedulerEvent::Error(format!(
                "离散会话退出处理失败: {error:#}"
            )));
        }
    }

    fn apply_ad_hoc_exit(
        &self,
        session_id: i64,
        exit_code: Option<i32>,
        marker_seen: bool,
        result_file_present: bool,
    ) -> Result<()> {
        let Some(view) = self.store.ad_hoc_session_view(session_id)? else {
            return Ok(());
        };
        if matches!(
            view.status,
            SessionStatus::Done | SessionStatus::Dead | SessionStatus::Hidden
        ) {
            return Ok(()); // 迟到事件:人工已收口,不复活
        }
        let contract = crate::agent_adapter::ExecutionContract::parse(&view.snapshot).ok();
        let status = match view.snapshot.run_mode {
            crate::model::RunMode::Interactive => SessionStatus::Dead,
            crate::model::RunMode::OneShot => {
                let mode = contract
                    .map(|c| c.completion)
                    .unwrap_or(crate::agent_adapter::CompletionMode::ProcessExit);
                use crate::agent_adapter::CompletionMode as M;
                match mode {
                    M::ProcessExit => {
                        if exit_code == Some(0) {
                            SessionStatus::Done
                        } else {
                            SessionStatus::Dead
                        }
                    }
                    M::StdoutMarker => {
                        if marker_seen {
                            SessionStatus::Done
                        } else {
                            SessionStatus::Dead
                        }
                    }
                    M::ResultFile => {
                        if result_file_present {
                            SessionStatus::Done
                        } else {
                            SessionStatus::Dead
                        }
                    }
                    M::Manual => SessionStatus::Dead,
                }
            }
        };
        if let Some(final_view) = self.store.set_ad_hoc_status(session_id, status)? {
            self.emit(SchedulerEvent::AdHocSessionUpdated(final_view));
        }
        Ok(())
    }

    /// 用户显式把离散会话输出提交为 Handoff(可多次提交,以最后一次为准)。
    pub fn submit_ad_hoc_handoff(
        &self,
        session_id: i64,
        handoff: &crate::agent_adapter::HandoffDraft,
    ) -> Result<AdHocSessionView> {
        let json = serde_json::to_string(handoff)?;
        let view = self
            .store
            .submit_ad_hoc_handoff(session_id, &json)?
            .ok_or_else(|| anyhow::anyhow!("离散会话 {session_id} 不存在"))?;
        self.emit(SchedulerEvent::AdHocSessionUpdated(view.clone()));
        Ok(view)
    }

    pub fn list_ad_hoc_sessions(&self, task_id: i64) -> Result<Vec<AdHocSessionView>> {
        self.store.list_ad_hoc_sessions(task_id)
    }

    // ---------- 调度 ----------

    fn tick(&self) -> Result<()> {
        let running_count = {
            let active = self.active_dispatches.lock();
            active.len()
        };
        if running_count >= self.per_project_limit() {
            return Ok(());
        }
        if !self.global.try_begin_probe() {
            return Ok(());
        }
        // 收集可派发 Step(按任务 → Step id 顺序)
        let mut candidates: Vec<(TaskView, StepView)> = Vec::new();
        for task in self.store.list_tasks(false)? {
            if task.status != TaskStatus::Running || task.paused {
                continue;
            }
            let Some(rev) = self.store.active_revision(task.id)? else {
                continue;
            };
            for step in self.store.revision_steps(rev.id)? {
                if step.status == StepStatus::Ready {
                    candidates.push((task.clone(), step));
                }
            }
        }
        // 会话键占用表:running run 的 session key
        let mut busy_keys: HashSet<String> = HashSet::new();
        for run in self.store.running_runs()? {
            if let Some(sid) = run.session_id {
                if let Some(Some(key)) = self.store.session_view(sid)?.map(|s| s.session_key) {
                    busy_keys.insert(key);
                }
            }
        }
        for (task, step) in candidates {
            if self.active_dispatches.lock().len() >= self.per_project_limit() {
                break;
            }
            if !self.global.try_begin() {
                break;
            }
            let policy = SessionPolicy::parse_db(&step.session_policy);
            if let Some(key) = policy.session_key() {
                if busy_keys.contains(key) {
                    self.global.end(); // 串行化:同 key 不能并行(含同 tick 内刚派发的)
                    continue;
                }
            }
            match self.dispatch(&task, &step, &policy) {
                Ok(()) => {
                    // 同一 tick 内串行化:刚派发的 key 立即占用
                    if let Some(key) = policy.session_key().map(str::to_string) {
                        busy_keys.insert(key);
                    }
                }
                Err(e) => {
                    self.global.end();
                    self.emit(SchedulerEvent::Error(format!("派发失败: {e:#}")));
                }
            }
        }
        Ok(())
    }

    fn per_project_limit(&self) -> usize {
        self.config.engine.per_project_concurrency.max(1)
    }

    fn dispatch(&self, task: &TaskView, step: &StepView, policy: &SessionPolicy) -> Result<()> {
        let (spec_profile, runtime) = {
            let catalog = self.profiles.read();
            let spec = catalog
                .specs
                .get(&step.agent_profile)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Agent Profile `{}` 未注册", step.agent_profile))?;
            let runtime = spec.runtime;
            (spec, runtime)
        };
        // 会话:fresh → 新建;reuse → 复用活的,否则新建;
        // 手动"继续会话"重试 → 固定复用指定存活会话(一次性映射)
        let (session, attach_existing) = if let Some(session_id) =
            self.continue_sessions.lock().remove(&step.id)
        {
            let live = self
                .store
                .session_view(session_id)?
                .filter(|s| !matches!(s.status, SessionStatus::Dead | SessionStatus::Hidden));
            match live {
                Some(s) => (s, true),
                None => (
                    self.store.create_session(
                        None,
                        runtime.as_str(),
                        &step.agent_profile,
                        &step.title,
                    )?,
                    false,
                ),
            }
        } else {
            match policy {
                SessionPolicy::Fresh => (
                    self.store.create_session(
                        None,
                        runtime.as_str(),
                        &step.agent_profile,
                        &step.title,
                    )?,
                    false,
                ),
                SessionPolicy::Reuse { key } => {
                    match self.store.find_reusable_session(key, &step.agent_profile)? {
                        Some(s)
                            if !matches!(s.status, SessionStatus::Dead | SessionStatus::Hidden) =>
                        {
                            (s, true)
                        }
                        _ => (
                            self.store.create_session(
                                Some(key),
                                runtime.as_str(),
                                &step.agent_profile,
                                &step.title,
                            )?,
                            false,
                        ),
                    }
                }
            }
        };
        if !attach_existing {
            if let Some(s) =
                self.store
                    .update_session(session.id, Some(SessionStatus::Working), None, None)?
            {
                self.emit(SchedulerEvent::SessionUpdated(s));
            }
        }
        // 原子派发:bump attempts + step→running + 建 run 同事务(崩溃窗口不留孤儿)
        let run = self
            .store
            .dispatch_run(task.id, step.id, step.revision_id, session.id)?;
        self.emit(SchedulerEvent::StepUpdated(StepView {
            status: StepStatus::Running,
            ..step.clone()
        }));
        self.active_dispatches.lock().insert(run.id);
        self.emit(SchedulerEvent::RunUpdated(run.clone()));

        let prompt = build_prompt(task, step, &run.capability_token);
        let spec = LaunchSpec {
            run_id: run.id,
            step_id: step.id,
            task_id: task.id,
            session_id: session.id,
            session_key: policy.session_key().map(str::to_string),
            attach_existing_session: attach_existing,
            profile: spec_profile,
            step_title: step.title.clone(),
            prompt,
            capability_token: run.capability_token.clone(),
            pipe_name: self.pipe_name.clone(),
            mfctl_hint: Some(format!(
                "mfctl step complete --summary \"...\" / mfctl step fail --reason \"...\""
            )),
            workdir: self.root.clone(),
        };
        let tx = self.runtime_tx.clone();
        self.host.launch(spec, tx);
        Ok(())
    }

    fn apply_runtime_event(&self, run_id: i64, ev: RuntimeEvent) -> Result<()> {
        // 离散会话事件:tag 是 ad_hoc 行号,不走 run 状态机
        if let RuntimeEvent::AdHocExited {
            session_id,
            exit_code,
            marker_seen,
            result_file_present,
        } = ev
        {
            self.apply_ad_hoc_exit(session_id, exit_code, marker_seen, result_file_present)?;
            return Ok(());
        }
        let run = match self.store.run_view(run_id)? {
            Some(r) => r,
            None => return Ok(()),
        };
        // 终态 run 的迟到事件(取消后的退出/提问/状态)一律丢弃,防止复活
        let run_terminal = matches!(
            run.status,
            RunStatus::Succeeded
                | RunStatus::Failed
                | RunStatus::Cancelled
                | RunStatus::Interrupted
        );
        if run_terminal && !matches!(ev, RuntimeEvent::Settled(_)) {
            return Ok(());
        }
        match ev {
            RuntimeEvent::AdHocExited { .. } => unreachable!("已在函数入口分流"),
            RuntimeEvent::Launched => {
                if let Some(session_id) = run.session_id {
                    if let Some(s) = self.store.update_session(
                        session_id,
                        Some(SessionStatus::Working),
                        None,
                        None,
                    )? {
                        self.emit(SchedulerEvent::SessionUpdated(s));
                    }
                }
            }
            RuntimeEvent::SpawnError(msg) => {
                self.settle_run(
                    run.id,
                    Settlement::Fail {
                        reason: format!("启动失败: {msg}"),
                    },
                )
                .ok();
            }
            RuntimeEvent::AgentState(state) => {
                self.handle_agent_state_report(run.id, state);
            }
            RuntimeEvent::TuiIdle(idle) => {
                // tui-idle 仅用于展示,不参与结算
                let _ = idle;
            }
            RuntimeEvent::Output => {
                if let Some(session_id) = run.session_id {
                    self.store.set_session_unread(session_id, true)?;
                    if let Some(session) = self.store.session_view(session_id)? {
                        self.emit(SchedulerEvent::SessionUpdated(session));
                    }
                }
                if let Some(task) = self.store.task_view(run.task_id)? {
                    self.store.set_task_unread(task.id, true)?;
                    if let Some(task) = self.store.task_view(task.id)? {
                        self.emit(SchedulerEvent::TaskUpdated(task));
                    }
                }
            }
            RuntimeEvent::Transcript { role, text } => {
                if let Some(session_id) = run.session_id {
                    let is_user = role == "user";
                    let _ = self
                        .store
                        .push_event("transcript", &serde_json::json!({ "session_id": session_id, "role": role, "text": text }).to_string());
                    if let Some(s) = self.store.update_session(
                        session_id,
                        None,
                        is_user.then_some(text.as_str()),
                        (!is_user).then_some(text.as_str()),
                    )? {
                        self.emit(SchedulerEvent::SessionUpdated(s));
                    }
                    self.emit(SchedulerEvent::Transcript {
                        session_id,
                        role,
                        text,
                    });
                }
            }
            RuntimeEvent::Question(text) => {
                let q =
                    self.store
                        .ask_question(run.task_id, Some(run.step_id), Some(run.id), &text)?;
                // 只有非终态 step 才进入 needs-input(迟到提问不得覆盖已成功的步骤)
                if let Some(cur) = self.store.step_view(run.step_id)? {
                    if !cur.status.terminal() {
                        if let Some(s) = self
                            .store
                            .set_step_status(run.step_id, StepStatus::NeedsInput)?
                        {
                            self.emit(SchedulerEvent::StepUpdated(s));
                        }
                    }
                }
                if let Some(t) = self.store.task_view(run.task_id)? {
                    if t.status == TaskStatus::Running {
                        if let Some(t) = self.store.set_task_status(t.id, TaskStatus::NeedsYou)? {
                            self.emit(SchedulerEvent::TaskUpdated(t));
                        }
                    }
                    self.store.set_task_unread(t.id, true)?;
                }
                self.emit(SchedulerEvent::QuestionOpened(q));
            }
            RuntimeEvent::Exited { .. } => {
                if run.outcome.is_none() {
                    self.enter_awaiting_outcome(&run, "运行结束但未显式结算");
                }
            }
            RuntimeEvent::Settled(settlement) => {
                self.settle_run(run.id, settlement).ok();
            }
        }
        Ok(())
    }

    /// 外部(状态钩子/管道)上报 Agent 状态:
    /// working/waiting/blocked/done;done 无显式结算 → awaiting-outcome + 需要你。
    pub fn handle_agent_state_report(&self, run_id: i64, state: AgentState) {
        let Ok(Some(run)) = self.store.run_view(run_id) else {
            return;
        };
        if let Ok(Some(r)) = self.store.set_run_agent_state(run_id, state) {
            self.emit(SchedulerEvent::RunUpdated(r));
        }
        if let Some(session_id) = run.session_id {
            let session_status = match state {
                AgentState::Starting => Some(SessionStatus::Starting),
                AgentState::Working => Some(SessionStatus::Working),
                AgentState::Waiting => Some(SessionStatus::Waiting),
                AgentState::BlockedState => Some(SessionStatus::BlockedState),
                AgentState::Done => Some(SessionStatus::Done),
                AgentState::Idle => Some(SessionStatus::Idle),
                AgentState::Dead => Some(SessionStatus::Dead),
            };
            if let Some(st) = session_status {
                if let Ok(Some(s)) = self.store.update_session(session_id, Some(st), None, None) {
                    self.emit(SchedulerEvent::SessionUpdated(s));
                }
            }
        }
        let outcome_none = self
            .store
            .run_view(run_id)
            .map(|r| r.and_then(|r| r.outcome).is_none())
            .unwrap_or(true);
        if state == AgentState::Done && outcome_none {
            self.enter_awaiting_outcome(&run, "Agent 上报 done 但未显式结算");
        }
        if state == AgentState::Dead && outcome_none {
            self.enter_awaiting_outcome(&run, "会话已死亡,等待用户选择重建或结算");
        }
    }

    /// 「需要你」诱因是否已消除:无失败/阻塞/待结算步骤,无开放问题,无中断运行。
    fn task_attention_cleared(&self, task_id: i64) -> bool {
        let Ok(steps) = self.store.task_steps(task_id) else {
            return false;
        };
        if steps.iter().any(|s| {
            matches!(
                s.status,
                StepStatus::Failed
                    | StepStatus::Blocked
                    | StepStatus::AwaitingOutcome
                    | StepStatus::NeedsInput
            )
        }) {
            return false;
        }
        if self
            .store
            .open_questions(Some(task_id))
            .map(|q| !q.is_empty())
            .unwrap_or(true)
        {
            return false;
        }
        !self
            .store
            .list_runs_of_task(task_id)
            .map(|runs| {
                runs.iter().any(|r| {
                    matches!(
                        r.status,
                        RunStatus::Interrupted | RunStatus::AwaitingOutcome
                    )
                })
            })
            .unwrap_or(true)
    }

    /// done / 退出但无结算 → awaiting-outcome,看板「需要你」。
    fn enter_awaiting_outcome(&self, run: &RunView, why: &str) {
        // 只对活动(running/awaiting)run 生效:迟到的退出事件不得复活已取消的 run
        let Ok(Some(current)) = self.store.run_view(run.id) else {
            return;
        };
        if !matches!(
            current.status,
            RunStatus::Running | RunStatus::AwaitingOutcome
        ) {
            return;
        }
        if let Some(r) = self
            .store
            .set_run_status(run.id, RunStatus::AwaitingOutcome)
            .ok()
            .flatten()
        {
            self.release_slot(run.id);
            self.emit(SchedulerEvent::RunUpdated(r));
        }
        if let Some(s) = self
            .store
            .set_step_status(run.step_id, StepStatus::AwaitingOutcome)
            .ok()
            .flatten()
        {
            self.emit(SchedulerEvent::StepUpdated(s));
        }
        if let Some(t) = self.store.task_view(run.task_id).ok().flatten() {
            if !t.status.terminal() {
                if let Some(t) = self
                    .store
                    .set_task_status(t.id, TaskStatus::NeedsYou)
                    .ok()
                    .flatten()
                {
                    self.store.set_task_unread(t.id, true).ok();
                    self.emit(SchedulerEvent::TaskUpdated(t));
                }
            }
        }
        self.emit(SchedulerEvent::Log {
            run_id: run.id,
            text: why.to_string(),
        });
    }

    // ---------- 查询(UI 投影) ----------

    pub fn tasks(&self) -> Result<Vec<TaskView>> {
        self.store.list_tasks(false)
    }

    pub fn task_detail(
        &self,
        task_id: i64,
    ) -> Result<Option<(TaskView, Vec<StepView>, Vec<StepQuestionView>)>> {
        let Some(t) = self.store.task_view(task_id)? else {
            return Ok(None);
        };
        let steps = self.store.task_steps(task_id)?;
        let questions = self.store.open_questions(Some(task_id))?;
        Ok(Some((t, steps, questions)))
    }

    pub fn sessions(&self) -> Result<Vec<SessionView>> {
        self.store.list_sessions()
    }

    pub fn runs_of_task(&self, task_id: i64) -> Result<Vec<RunView>> {
        self.store.list_runs_of_task(task_id)
    }

    pub fn mark_session_read(&self, session_id: i64) -> Result<()> {
        self.store.set_session_unread(session_id, false)?;
        if let Some(s) = self.store.session_view(session_id)? {
            self.emit(SchedulerEvent::SessionUpdated(s));
        }
        Ok(())
    }

    /// 清除任务未读(用户打开了对应上下文);更新 Store 后 emit,保证快照最终一致。
    pub fn mark_task_read(&self, task_id: i64) -> Result<()> {
        self.store.set_task_unread(task_id, false)?;
        if let Some(t) = self.store.task_view(task_id)? {
            self.emit(SchedulerEvent::TaskUpdated(t));
        }
        Ok(())
    }

    pub fn hide_session(&self, session_id: i64) -> Result<()> {
        self.set_session_status(session_id, SessionStatus::Hidden)
    }

    /// 看板确认/隐藏/终止:统一经 Orchestrator 更新并 emit(UI 不直接写 Store)。
    pub fn set_session_status(&self, session_id: i64, status: SessionStatus) -> Result<()> {
        if let Some(s) = self
            .store
            .update_session(session_id, Some(status), None, None)?
        {
            self.emit(SchedulerEvent::SessionUpdated(s));
        }
        Ok(())
    }
}

impl GlobalLimiter {
    /// 探测全局是否还有余量(不占用)。
    fn try_begin_probe(&self) -> bool {
        self.active.load(Ordering::SeqCst) < self.max.load(Ordering::SeqCst)
    }
}

/// 初始 prompt:工作说明 + mfctl 结算纪律。
pub fn build_prompt(task: &TaskView, step: &StepView, token: &str) -> String {
    format!(
        "你在 MonkeyFence 中执行流水线步骤「{title}」(任务: {task_title})。

         工作说明:
{instructions}

         完成后必须显式结算(在你的 shell 中运行以下命令之一,--token 参数必须原样保留):
         - 成功:mfctl --token {token} step complete --summary \"一句话总结\"
         - 失败:mfctl --token {token} step fail --reason \"失败原因\"

         规则:
         - 不要提交、推送或搁置任何版本控制变更。
         - 需要用户决策时,直接在终端中说明并等待。
         - 令牌仅对本步骤有效;重复提交相同结算是幂等的,提交冲突结算会被拒绝。",
        title = step.title,
        task_title = task.title,
        token = token,
        instructions = if step.instructions.is_empty() {
            "(无补充说明)"
        } else {
            &step.instructions
        },
    )
}
