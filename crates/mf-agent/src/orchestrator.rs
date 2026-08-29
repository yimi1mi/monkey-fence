//! v2 调度器(Orchestrator):自动派发 ready Step、并发限制、会话串行化、
//! 显式结算驱动状态机、失败阻塞后代、暂停/编辑 Revision。
//!
//! 生命周期规则见 `CONTEXT.md`;与 Runtime 的边界见 `runtime.rs`。

use crate::config::Config;
use crate::execution_directory::{
    ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use crate::model::*;
use crate::pipeline::{PipelineDraft, ProfileIndex, SessionPolicy};
use crate::runtime::{AgentProfileSpec, LaunchSpec, RuntimeEvent, RuntimeHost, WorkflowLaunchSpec};
use crate::store::Store;
use crate::workflow::{PluginSourcePin, WorkflowTemplateVersion};
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// 工作流运行期的插件 pin 生命周期(生产实现由插件宿主提供:
/// pin 保护包不被卸载,resolve 校验内容哈希,release 归还引用)。
pub trait WorkflowPluginPins: Send + Sync {
    /// 冻结 Revision 时固定插件包(重复 pin 幂等)。
    fn pin_for_run(&self, run_key: &str, pin: &PluginSourcePin) -> Result<()>;
    /// 派发前验证 pin 仍可解析(插件在位且内容哈希一致)。
    fn resolve_pin(&self, pin: &PluginSourcePin) -> Result<()>;
    /// 任务终态后释放该 run_key 的全部 pin。
    fn release_run_pins(&self, run_key: &str) -> Result<()>;
}

/// 工作流内核依赖:实例解析(目录库)与插件 pin。
/// `pins` 为 None 时跳过 pin 校验(旧项目库/无插件宿主场景)。
pub struct WorkflowKernel {
    pub catalog: Arc<crate::catalog_store::CatalogStore>,
    pub pins: Option<Arc<dyn WorkflowPluginPins>>,
}

impl WorkflowKernel {
    /// 无 pin 校验的内核(目录库必须提供;pin 可缺省)。
    pub fn new(catalog: Arc<crate::catalog_store::CatalogStore>) -> WorkflowKernel {
        WorkflowKernel {
            catalog,
            pins: None,
        }
    }
}

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
    /// 执行目录提供器(默认项目目录;worktree 等由宿主注入插件实现)。
    directory: Arc<dyn ExecutionDirectoryProvider>,
    /// run → 持有中的租约(终态释放;未知状态保持)。
    held_leases: Mutex<HashMap<i64, ExecutionLease>>,
    /// step → 租约(自动重试复用同一目录,保留文件修改)。
    step_leases: Mutex<HashMap<i64, ExecutionLease>>,
    /// 工作流内核(实例目录库 + 插件 pin)。
    workflow: WorkflowKernel,
    /// task → 汇合冲突待处理的隔离租约(NeedsUser;解决前任务保持 needs-you)。
    pending_merges: Mutex<HashMap<i64, Vec<ExecutionLease>>>,
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
        directory: Arc<dyn ExecutionDirectoryProvider>,
    ) -> Result<Arc<Orchestrator>> {
        Self::start_with(
            store,
            root,
            config,
            host,
            profiles,
            global,
            pipe_name,
            directory,
            WorkflowKernel::new(crate::catalog_store::CatalogStore::memory()?),
        )
    }

    /// 完整启动(带工作流内核:实例目录库 + 插件 pin 生命周期)。
    #[allow(clippy::too_many_arguments)]
    pub fn start_with(
        store: Arc<Store>,
        root: PathBuf,
        config: Config,
        host: Arc<dyn RuntimeHost>,
        profiles: Arc<RwLock<ProfileCatalog>>,
        global: Arc<GlobalLimiter>,
        pipe_name: String,
        directory: Arc<dyn ExecutionDirectoryProvider>,
        workflow: WorkflowKernel,
    ) -> Result<Arc<Orchestrator>> {
        // 异常退出恢复(设计 §13):宿主确认存活的会话重连;
        // 未知状态 → interrupted + awaiting-outcome(不判失败)
        let root_str_for_probe = root.to_string_lossy().to_string();
        let host_for_probe = host.clone();
        let recovered = store.recover_interrupted_with(&|session_id| {
            host_for_probe.is_session_alive(&root_str_for_probe, session_id)
        })?;
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
            directory,
            held_leases: Mutex::new(HashMap::new()),
            step_leases: Mutex::new(HashMap::new()),
            workflow,
            pending_merges: Mutex::new(HashMap::new()),
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
        // 待决汇合恢复:持久化的冲突行装回内存,任务保持 needs-you
        orch.restore_pending_merges();
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
        // 停止所有在途运行(会话进程交给 Session Registry 决定是否保留);
        // 停止未确认只记录(关闭路径不因个别进程阻塞而失败)
        if let Ok(runs) = self.store.running_runs() {
            for run in runs {
                if let Err(e) = self.host.stop_run(&self.root_str, run.id) {
                    log::warn!("关闭时停止 run {} 未确认: {e:#}", run.id);
                }
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

    /// 丢弃任务(Composer 分配失败的回滚):无活动 Run 才允许,
    /// 连带清理 Revision/Step/待决汇合;成功后发 TaskRemoved。
    pub fn discard_task(&self, task_id: i64) -> Result<()> {
        if self.store.delete_task_if_unused(task_id)? {
            self.emit(SchedulerEvent::TaskRemoved(task_id));
            Ok(())
        } else {
            anyhow::bail!("任务 {task_id} 已有 Agent Run,不能丢弃");
        }
    }

    pub fn archive_task(&self, task_id: i64) -> Result<()> {
        if self.task_has_active_runs(task_id)? {
            anyhow::bail!("任务仍有活动 Agent Run,请先停止");
        }
        // 先释放全部执行租约痕迹(待决汇合批 + join 暂缓 + 仍持有的 run
        // 租约;含数据库兜底行)。任一失败 → 阻止归档,保留可恢复状态。
        let errors = self.release_all_task_leases(task_id);
        if !errors.is_empty() {
            anyhow::bail!(
                "任务 {task_id} 存在无法释放的执行租约,已阻止归档(解决后可重试):{}",
                errors.join("; ")
            );
        }
        self.release_workflow_pins(task_id);
        let _ = self.store.clear_pending_merges(task_id)?;
        if let Err(e) = self.directory.discard_task_baselines(task_id) {
            log::warn!("清理任务 {task_id} 集成基线失败: {e:#}");
        }
        self.store.set_task_status(task_id, TaskStatus::Archived)?;
        self.emit(SchedulerEvent::TaskRemoved(task_id));
        Ok(())
    }

    /// 释放任务的全部执行租约痕迹:待决汇合批(pending_merges 映射 +
    /// 持久化行)、join 暂缓的父租约、仍持有的 run 租约(进程内映射 +
    /// 数据库兜底行)。成功的租约从全部状态中移除;失败的保持持有
    /// (待决映射保留,可重试)。返回失败描述(空 = 全部成功)。
    fn release_all_task_leases(&self, task_id: i64) -> Vec<String> {
        let mut errors: Vec<String> = Vec::new();
        let mut done: HashSet<String> = HashSet::new();
        // 1) 待决汇合批(取消/归档路径复用同一清理语义)
        let pending: Vec<ExecutionLease> = self
            .pending_merges
            .lock()
            .get(&task_id)
            .cloned()
            .unwrap_or_default();
        for lease in pending {
            if !done.insert(lease.id.clone()) {
                continue;
            }
            match self.directory.release(&lease) {
                Ok(()) => {
                    let _ = self.store.release_execution_lease(&lease.id);
                    self.held_leases.lock().retain(|_, l| l.id != lease.id);
                    self.step_leases.lock().retain(|_, l| l.id != lease.id);
                }
                Err(e) => {
                    log::warn!("释放执行租约 `{}` 失败: {e:#}", lease.id);
                    errors.push(format!("{}: {e:#}", lease.id));
                }
            }
        }
        if errors.is_empty() {
            self.pending_merges.lock().remove(&task_id);
        }
        // 2) 仍持有的 run 租约(join 暂缓的父租约挂在其成功 run 下)
        let runs = self.store.list_runs_of_task(task_id).unwrap_or_default();
        for run in runs {
            let lease = self.held_leases.lock().get(&run.id).cloned();
            let Some(lease) = lease else { continue };
            if !done.insert(lease.id.clone()) {
                continue;
            }
            if let Err(e) = self.directory.release(&lease) {
                log::warn!("释放执行租约 `{}` 失败: {e:#}", lease.id);
                errors.push(format!("{}: {e:#}", lease.id));
                continue; // 保持持有
            }
            let _ = self.store.release_execution_lease(&lease.id);
            self.held_leases.lock().remove(&run.id);
            self.step_leases.lock().retain(|_, l| l.id != lease.id);
        }
        // 3) 数据库兜底(重启恢复后内存映射为空的持有行)
        if errors.is_empty() {
            if let Ok(rows) = self.store.list_execution_leases(task_id) {
                for row in rows.into_iter().filter(|r| r.status == "held") {
                    let lease = lease_from_row(&row);
                    if done.contains(&lease.id) {
                        continue;
                    }
                    if let Err(e) = self.directory.release(&lease) {
                        log::warn!("释放执行租约 `{}` 失败: {e:#}", lease.id);
                        errors.push(format!("{}: {e:#}", lease.id));
                        continue;
                    }
                    let _ = self.store.release_execution_lease(&lease.id);
                }
            }
        }
        errors
    }

    fn task_has_active_runs(&self, task_id: i64) -> Result<bool> {
        self.store.task_has_active_runs(task_id)
    }

    /// 释放 run 持有的执行租约(终态结算/取消;未知状态不调用)。
    /// 重启恢复后的 run 不在进程内映射中,按数据库兜底查找。
    fn release_lease_of_run(&self, run_id: i64) {
        if let Some(lease) = self.held_leases.lock().remove(&run_id) {
            if let Err(e) = self.directory.release(&lease) {
                log::warn!("释放执行租约 `{}` 失败: {e:#}", lease.id);
            }
            let _ = self.store.release_execution_lease(&lease.id);
            self.step_leases.lock().retain(|_, l| l.id != lease.id);
            return;
        }
        if let Ok(Some(row)) = self.store.held_lease_of_run(run_id) {
            let lease = lease_from_row(&row);
            if let Err(e) = self.directory.release(&lease) {
                log::warn!("释放执行租约 `{}` 失败: {e:#}", lease.id);
            }
            let _ = self.store.release_execution_lease(&row.lease_key);
        }
    }

    /// 注入 Runtime 事件(测试与外部管道用;与宿主事件同队列)。
    pub fn push_runtime_event(&self, run_id: i64, ev: RuntimeEvent) {
        let _ = self.runtime_tx.send((run_id, ev));
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

    /// 终止任务:停止在途运行(等待真实终止确认),未终结 Step →
    /// cancelled,Task → cancelled。任一运行停止未确认 → 该 run 转入
    /// Interrupted、任务进入 needs-you 并返回 Err(不标 Cancelled,
    /// 保留可恢复状态;进程可能仍在写隔离目录)。
    pub fn cancel_task(&self, task_id: i64) -> Result<TaskView> {
        let mut stop_failures: Vec<String> = Vec::new();
        for run in self.store.running_runs()? {
            if run.task_id != task_id {
                continue;
            }
            if let Err(e) = self.host.stop_run(&self.root_str, run.id) {
                stop_failures.push(format!("run {}: {e:#}", run.id));
                if let Some(r) = self.store.set_run_status(run.id, RunStatus::Interrupted)? {
                    self.release_slot(run.id);
                    self.emit(SchedulerEvent::RunUpdated(r));
                }
                continue; // 未确认终止:不标 Cancelled、不释放租约
            }
            if let Some(r) = self.store.set_run_status(run.id, RunStatus::Cancelled)? {
                self.release_slot(run.id);
                self.release_lease_of_run(run.id);
                self.emit(SchedulerEvent::RunUpdated(r));
            }
        }
        if !stop_failures.is_empty() {
            if let Some(t) = self.store.task_view(task_id)? {
                if !t.status.terminal() {
                    if let Some(t) = self.store.set_task_status(t.id, TaskStatus::NeedsYou)? {
                        self.store.set_task_unread(t.id, true).ok();
                        self.emit(SchedulerEvent::TaskUpdated(t));
                    }
                }
            }
            anyhow::bail!(
                "任务 {task_id} 部分运行停止未确认,已标记 interrupted;\
                 任务进入 needs-you(不释放执行租约):{}",
                stop_failures.join("; ")
            );
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
        // 取消:待决汇合与仍持有的租约(join 暂缓等)一并释放 —— 与归档
        // 复用同一清理;取消语义放行(失败仅记录,不阻塞任务取消)
        let lease_errors = self.release_all_task_leases(task_id);
        if !lease_errors.is_empty() {
            log::warn!("取消任务 {task_id} 时部分租约释放失败: {lease_errors:?}");
        }
        let _ = self.store.clear_pending_merges(task_id)?;
        self.release_workflow_pins(task_id);
        if let Err(e) = self.directory.discard_task_baselines(task_id) {
            log::warn!("清理任务 {task_id} 集成基线失败: {e:#}");
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
        // 激活把旧 active 置为 superseded:尝试释放其 pin
        // (仍有存活/可重试 run 的旧 Revision 由结算钩子延迟释放)
        self.release_stale_revision_pins(task_id);
        self.emit(SchedulerEvent::TaskUpdated(t.clone()));
        Ok(t)
    }

    // ---------- 工作流分配(模板 → 编译 → 冻结 Revision) ----------

    /// 运行 Workflow Compiler(纯函数;错误格式化为用户可读列表)。
    fn compile_version(
        &self,
        version: &WorkflowTemplateVersion,
        agent_type_plugins: &HashMap<String, PluginSourcePin>,
        allow_unsafe_shared_directory: bool,
    ) -> std::result::Result<crate::workflow::WorkflowSnapshot, anyhow::Error> {
        crate::workflow_compiler::WorkflowCompiler::new()
            .compile(crate::workflow_compiler::CompileInput {
                template: version,
                directory_provider_isolates: self.directory.isolates(),
                allow_unsafe_shared_directory,
                agent_type_plugins,
                resolve_instance: &|id| self.workflow.catalog.snapshot_agent_instance(id, None),
            })
            .map_err(|errors| {
                anyhow::anyhow!(
                    "工作流编译失败:
{}",
                    errors
                        .iter()
                        .map(|e| format!("- [{}] {}: {}", e.code, e.node, e.message))
                        .collect::<Vec<_>>()
                        .join(
                            "
"
                        )
                )
            })
    }

    /// 把工作流模板版本分配给任务:运行 Workflow Compiler 全量校验,
    /// 冻结实例快照与插件 pin,原子写入 Revision(快照 + Step 投影)。
    /// `agent_type_plugins` 是当前可用插件贡献的 Agent Type → 插件包身份。
    pub fn assign_workflow(
        &self,
        task_id: i64,
        version: &WorkflowTemplateVersion,
        agent_type_plugins: &HashMap<String, PluginSourcePin>,
        allow_unsafe_shared_directory: bool,
    ) -> Result<RevisionView> {
        if let Some(t) = self.store.task_view(task_id)? {
            if t.status == TaskStatus::Running && !t.paused {
                anyhow::bail!("运行中的任务必须先暂停才能重新分配工作流");
            }
        } else {
            anyhow::bail!("任务 {task_id} 不存在");
        }
        let snapshot = crate::workflow_compiler::WorkflowCompiler::new()
            .compile(crate::workflow_compiler::CompileInput {
                template: version,
                directory_provider_isolates: self.directory.isolates(),
                allow_unsafe_shared_directory,
                agent_type_plugins,
                resolve_instance: &|id| self.workflow.catalog.snapshot_agent_instance(id, None),
            })
            .map_err(|errors| {
                anyhow::anyhow!(
                    "工作流编译失败:\n{}",
                    errors
                        .iter()
                        .map(|e| format!("- [{}] {}: {}", e.code, e.node, e.message))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })?;
        // 预编译通过才写库(pin key 含 revision id,必须先建 Revision);
        // pin 失败 → 精确回滚:释放本次 run_key 已 pin 引用 + 删除刚建的 draft Revision
        let rev = self.store.create_workflow_revision(task_id, &snapshot)?;
        let run_key = workflow_pin_key(&self.root, task_id, rev.id);
        if let Some(pins) = &self.workflow.pins {
            let mut pinned: Vec<PluginSourcePin> = Vec::new();
            let mut failure: Option<anyhow::Error> = None;
            for node in &snapshot.nodes {
                let Some(pin) = &node.plugin else {
                    continue;
                };
                if pinned.contains(pin) {
                    continue;
                }
                match pins.pin_for_run(&run_key, pin) {
                    Ok(()) => pinned.push(pin.clone()),
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = failure {
                // 部分失败精确释放:只回收本次 run_key(其他 Revision 的 pin 不受影响)
                if let Err(release_err) = pins.release_run_pins(&run_key) {
                    log::warn!("回滚 pin 失败(可能留下悬挂引用): {release_err:#}");
                }
                if let Err(del) = self.store.delete_draft_revision(rev.id) {
                    log::warn!("回滚 draft Revision 失败: {del:#}");
                }
                return Err(e);
            }
        }
        // 重分配:清理陈旧 Revision 的 pin(旧 active 在确认新 Revision、
        // 其 run 全部收口前保持 —— 存活 run 的重试仍需解析 pin;
        // 被更新 draft 取代的 stale draft 立即释放,永不运行不泄漏)
        self.release_stale_revision_pins(task_id);
        self.emit(SchedulerEvent::RevisionCreated(rev.clone()));
        if let Some(t) = self.store.task_view(task_id)? {
            self.emit(SchedulerEvent::TaskUpdated(t));
        }
        Ok(rev)
    }

    /// 释放任务的陈旧 Revision pin:
    /// - stale draft(非最新 draft 的未激活 Revision)→ 立即释放(永不运行);
    /// - superseded 且无存活 run、无可重试步骤 → 释放;
    /// - active / 最新 draft / 仍有存活或可重试 run 的 → 保持
    ///   (确认激活与结算/跳过钩子会再次尝试)。
    fn release_stale_revision_pins(&self, task_id: i64) {
        let Ok(revisions) = self.store.revision_statuses(task_id) else {
            return;
        };
        let latest_draft = revisions
            .iter()
            .filter(|(_, status)| status == "draft")
            .map(|(id, _)| *id)
            .max();
        for (revision_id, status) in revisions {
            let releasable = match status.as_str() {
                "draft" => latest_draft != Some(revision_id),
                "superseded" => self
                    .store
                    .revision_pins_release_safe(revision_id)
                    .unwrap_or(false),
                _ => false,
            };
            if releasable {
                self.release_revision_pins(revision_id);
            }
        }
    }

    /// 编译任务本地工作流(项目 Store 草稿;只校验,不写库)。
    /// 返回冻结快照;错误信息含全部编译错误(格式化列表)。
    pub fn compile_task_local_workflow(
        &self,
        task_id: i64,
        agent_type_plugins: &HashMap<String, PluginSourcePin>,
    ) -> Result<crate::workflow::WorkflowSnapshot> {
        let version = self.task_local_version(task_id)?;
        let unsafe_parallel = self
            .store
            .task_workflow_unsafe_parallel(&self.root_str, task_id)?;
        self.compile_version(&version, agent_type_plugins, unsafe_parallel)
    }

    /// 把任务本地工作流(项目 Store 草稿)分配冻结为 Revision:
    /// 草稿 + 持久化的 unsafe-parallel 开关作为编译输入,直接冻结存储的
    /// 草稿(不是 UI 瞬时状态)。
    pub fn assign_task_local_workflow(
        &self,
        task_id: i64,
        agent_type_plugins: &HashMap<String, PluginSourcePin>,
    ) -> Result<RevisionView> {
        let version = self.task_local_version(task_id)?;
        let unsafe_parallel = self
            .store
            .task_workflow_unsafe_parallel(&self.root_str, task_id)?;
        self.assign_workflow(task_id, &version, agent_type_plugins, unsafe_parallel)
    }

    /// 任务本地工作流的「分配并确认运行」原子路径(I13):
    /// - 无 draft Revision、或草稿在上次冻结后又保存过
    ///   (task_workflows.updated_at > draft created_at)→ 重新编译冻结;
    /// - 已有与当前草稿一致的未激活 draft → 直接确认,不重复 assign
    ///   (assign 只建 draft,active_revision 要到确认才激活 —— 用
    ///   active_revision 判断会误判为"未分配"而重复冻结);
    /// 然后激活并开始调度。
    pub fn assign_and_confirm_task_local(
        &self,
        task_id: i64,
        agent_type_plugins: &HashMap<String, PluginSourcePin>,
    ) -> Result<TaskView> {
        if !self.task_local_draft_is_frozen(task_id) {
            self.assign_task_local_workflow(task_id, agent_type_plugins)?;
        }
        self.confirm_and_run(task_id)
    }

    /// 任务本地草稿是否已被冻结成 Revision(draft 待确认或 active 均算):
    /// 以草稿最近保存时间与 Revision 冻结时间比较 —— 草稿在冻结后又
    /// 保存过即视为已变化,需要重新冻结;未变化则复用既有 Revision。
    fn task_local_draft_is_frozen(&self, task_id: i64) -> bool {
        let Ok(Some(saved_at)) = self.store.task_workflow_saved_at(&self.root_str, task_id) else {
            return false;
        };
        if let Ok(Some((_, created))) = self.store.latest_draft_revision(task_id) {
            if created >= saved_at {
                return true; // 冻结于最近保存之后:draft 一致,待确认
            }
        }
        if let Ok(Some(rev)) = self.store.active_revision(task_id) {
            if rev.created_at >= saved_at {
                return true; // 当前活动 Revision 即最新草稿
            }
        }
        false
    }

    /// 项目 Store 里的任务本地草稿 → 模板版本形态(无草稿报错)。
    fn task_local_version(&self, task_id: i64) -> Result<WorkflowTemplateVersion> {
        let Some(draft) = self.store.load_task_workflow(&self.root_str, task_id)? else {
            anyhow::bail!("任务 {task_id} 没有本地工作流草稿;先在画布保存");
        };
        Ok(WorkflowTemplateVersion {
            version_id: 0,
            template_key: draft.key,
            version: 1,
            nodes: draft.nodes,
            created_at: String::new(),
        })
    }

    /// 用户解决汇合冲突后重试:以持久化行为源整批重新汇合(批量预检 +
    /// 原子应用);全部合并成功则释放租约、删除待决行并重新收敛任务。
    /// 返回仍存在的冲突(空 = 已全部解决)。
    pub fn resolve_pending_merges(&self, task_id: i64) -> Result<Vec<String>> {
        let rows = self.store.list_pending_merges(Some(task_id))?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let leases: Vec<ExecutionLease> = rows.iter().map(|r| r.lease.clone()).collect();
        let conflicts = self.merge_leases(&leases)?;
        if conflicts.is_empty() {
            for lease in &leases {
                self.release_lease_of_run_by_key(&lease.id);
            }
            self.store.clear_pending_merges(task_id)?;
            self.pending_merges.lock().remove(&task_id);
            // 待决清除后恢复调度(不能只做收敛检查):重新计算就绪步骤,
            // 注意力全部消除 → 任务回 Running,下游继续自动派发
            if let Some(rev) = self.store.active_revision(task_id)? {
                if let Ok(promoted) = self.store.with_tx(|c| Store::promote_ready_tx(c, rev.id)) {
                    for s in promoted {
                        self.emit(SchedulerEvent::StepUpdated(s));
                    }
                }
            }
            if let Some(t) = self.store.task_view(task_id)? {
                if t.status == TaskStatus::NeedsYou && self.task_attention_cleared(task_id) {
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
            self.check_convergence(task_id)?;
        } else {
            // 仍有冲突:刷新持久化冲突列表,租约继续持有等待再次处理
            for row in &rows {
                self.store
                    .update_pending_merge_conflicts(row.id, &conflicts)?;
            }
            self.pending_merges.lock().insert(task_id, leases);
        }
        Ok(conflicts)
    }

    /// 任务当前的待决汇合冲突(持久化行投影;Run Monitor 展示用)。
    pub fn pending_merge_conflicts(&self, task_id: i64) -> Vec<String> {
        self.store
            .list_pending_merges(Some(task_id))
            .map(|rows| rows.into_iter().flat_map(|r| r.conflicts).collect())
            .unwrap_or_default()
    }

    /// 重启恢复:把持久化的待决汇合装回内存映射,任务回到 needs-you。
    fn restore_pending_merges(&self) {
        let Ok(rows) = self.store.list_pending_merges(None) else {
            return;
        };
        let mut by_task: HashMap<i64, Vec<ExecutionLease>> = HashMap::new();
        for row in rows {
            if let Some(t) = self.store.task_view(row.task_id).ok().flatten() {
                if !t.status.terminal() {
                    if let Some(t) = self
                        .store
                        .set_task_status(row.task_id, TaskStatus::NeedsYou)
                        .ok()
                        .flatten()
                    {
                        self.store.set_task_unread(row.task_id, true).ok();
                        self.emit(SchedulerEvent::TaskUpdated(t));
                    }
                }
            }
            by_task.entry(row.task_id).or_default().push(row.lease);
        }
        *self.pending_merges.lock() = by_task;
    }

    /// 汇合一批隔离租约;返回冲突列表(空 = 全部合并成功)。
    fn merge_leases(&self, leases: &[ExecutionLease]) -> Result<Vec<String>> {
        let isolated: Vec<ExecutionLease> = leases.iter().filter(|l| l.isolated).cloned().collect();
        if isolated.is_empty() {
            return Ok(Vec::new());
        }
        Ok(match self.directory.merge(&isolated)? {
            MergeOutcome::NeedsUser { conflicts } => conflicts,
            MergeOutcome::Merged | MergeOutcome::NotRequired => Vec::new(),
        })
    }

    fn release_lease_of_run_by_key(&self, lease_id: &str) {
        // 先取出并释放锁(if let 条件里的临时 guard 会活到块末,持锁再锁会死锁)
        let lease = self
            .held_leases
            .lock()
            .values()
            .find(|l| l.id == lease_id)
            .cloned();
        if let Some(lease) = lease {
            self.release_lease_of_lease(&lease);
            return;
        }
        // 重启恢复的待决汇合不在内存映射:按租约键从数据库兜底
        if let Ok(Some(row)) = self.store.held_lease_by_key(lease_id) {
            let lease = lease_from_row(&row);
            self.release_lease_of_lease(&lease);
        }
    }

    /// 释放具体租约对象(进程内映射 + 数据库行)。
    fn release_lease_of_lease(&self, lease: &ExecutionLease) {
        if let Err(e) = self.directory.release(lease) {
            log::warn!("释放执行租约 `{}` 失败: {e:#}", lease.id);
        }
        let _ = self.store.release_execution_lease(&lease.id);
        self.held_leases.lock().retain(|_, l| l.id != lease.id);
        self.step_leases.lock().retain(|_, l| l.id != lease.id);
    }

    /// 终止单个 Agent Run(完整动作):请求宿主停止进程、**等待真实终止
    /// 确认**后结算为 cancelled、释放并发槽与执行租约。幂等;未知 run
    /// 报错。停止未确认 → run 转入 Interrupted、任务 needs-you、返回 Err
    /// (不标 Cancelled、不释放租约:进程可能仍在写隔离目录)。
    pub fn cancel_run(&self, run_id: i64) -> Result<RunView> {
        let run = self
            .store
            .run_view(run_id)?
            .ok_or_else(|| anyhow::anyhow!("Run {run_id} 不存在"))?;
        if matches!(
            run.status,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
        ) {
            return Ok(run); // 终态幂等
        }
        // stop_run 真终止进程并等待停止确认;确认后才结算/释放租约
        if let Err(stop_err) = self.host.stop_run(&self.root_str, run.id) {
            // 未确认终止:不标 Cancelled、不释放租约/不动会话状态 ——
            // 进程可能仍在运行;转入 Interrupted 等待人工处理
            self.release_slot(run.id);
            if let Some(r) = self.store.set_run_status(run.id, RunStatus::Interrupted)? {
                self.emit(SchedulerEvent::RunUpdated(r));
            }
            if let Some(t) = self.store.task_view(run.task_id)? {
                if !t.status.terminal() {
                    if let Some(t) = self.store.set_task_status(t.id, TaskStatus::NeedsYou)? {
                        self.store.set_task_unread(t.id, true).ok();
                        self.emit(SchedulerEvent::TaskUpdated(t));
                    }
                }
            }
            anyhow::bail!(
                "Run {run_id} 停止未确认,已标记为 interrupted\
                 (不释放执行租约): {stop_err:#}"
            );
        }
        if let Some(session_id) = run.session_id {
            if !matches!(run.status, RunStatus::Interrupted) {
                // 进程已被 stop_run 终止:会话如实标记 Dead(不再是"可复用存活")
                if let Some(s) =
                    self.store
                        .update_session(session_id, Some(SessionStatus::Dead), None, None)?
                {
                    self.emit(SchedulerEvent::SessionUpdated(s));
                }
            }
        }
        let cancelled = self
            .store
            .set_run_status(run.id, RunStatus::Cancelled)?
            .ok_or_else(|| anyhow::anyhow!("Run {run_id} 状态写入失败"))?;
        self.release_slot(run.id);
        self.release_lease_of_run(run.id);
        // run 级取消不改写 Step/Task 状态(由重试/跳过/终止任务决定),
        // 但 awaiting-outcome 的注意力语义需要重新评估收敛
        if let Some(step) = self.store.step_view(run.step_id).ok().flatten() {
            self.emit(SchedulerEvent::StepUpdated(step));
        }
        self.emit(SchedulerEvent::RunUpdated(cancelled.clone()));
        Ok(cancelled)
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
        // 跳过可能补齐某个 join 组(本父步骤已终态):先汇合已完整的
        // 父批次,再 promote 下游 —— 下游从含全部已汇合上游的基线检出
        self.flush_completed_join_batches(step.task_id);
        if let Some(rev) = self.store.active_revision(step.task_id)? {
            for s in self.store.with_tx(|c| Store::promote_ready_tx(c, rev.id))? {
                self.emit(SchedulerEvent::StepUpdated(s));
            }
        }
        self.emit(SchedulerEvent::StepUpdated(s.clone()));
        // 跳过让可重试状态消失:尝试释放被推迟的旧 Revision pin
        self.release_stale_revision_pins(step.task_id);
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
        // 成功结算的租约在下方 Complete 分支汇合后释放;
        // 失败结算的释放延迟到自动重试判定之后
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
                // 隔离租约先汇合回项目目录;冲突 → needs-you(租约保持持有)
                self.merge_or_pend(run);
                // join 批未完整时暂缓 promote:下游必须从含全部上游的
                // 汇合基线检出,不能在父分支半途派发
                if !self.has_incomplete_join_deferral(run.task_id) {
                    if let Some(rev) = self.store.active_revision(run.task_id).ok().flatten() {
                        if let Ok(promoted) =
                            self.store.with_tx(|c| Store::promote_ready_tx(c, rev.id))
                        {
                            for s in promoted {
                                self.emit(SchedulerEvent::StepUpdated(s));
                            }
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
                    // 自动重试路径:保留租约与文件修改,不检查收敛(节点回到 ready)
                    return;
                }
                self.release_lease_of_run(run.id);
                if let Ok(blocked) = self.store.block_descendants(run.step_id) {
                    for s in blocked {
                        self.emit(SchedulerEvent::StepUpdated(s));
                    }
                }
                // 失败可能补齐某个 join 组(本父步骤已终态):
                // 汇合其余成功父节点的暂缓批次,不让其无限期滞留
                self.flush_completed_join_batches(run.task_id);
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
        // run 收口可能让被推迟的旧 Revision pin 变为可释放(延迟释放钩子)
        self.release_stale_revision_pins(run.task_id);
        if let Err(e) = self.check_convergence(run.task_id) {
            self.emit(SchedulerEvent::Error(format!("收敛检查失败: {e:#}")));
        }
    }

    /// 成功结算后的隔离租约汇合:合并成功 → 释放;
    /// 冲突/合并出错 → 租约保持持有并进入待决列表,任务 → needs-you。
    /// 成功结算后的隔离租约汇合(join 感知,设计 §9.4):
    /// - 该步骤是某个多父 join 节点的父节点 → 等组内全部父步骤终态后,
    ///   把全部成功父节点的租约作为**一个完整批次**交给提供器按稳定
    ///   拓扑/键序合并;批完整前不合并、不释放、不推进集成基线
    ///   (并行父分支不得静默覆盖/丢修改,下游必须看到全部上游);
    /// - 不参与任何 join → 单租约立即汇合(串行链语义不变);
    /// - 批次成功 → 释放全部;冲突/出错 → 全部保持持有并进入待决列表,
    ///   任务 → needs-you。
    fn merge_or_pend(&self, run: &RunView) {
        let lease = self.held_leases.lock().get(&run.id).cloned();
        let Some(lease) = lease else {
            self.release_lease_of_run(run.id);
            return;
        };
        if !lease.isolated {
            self.release_lease_of_lease(&lease);
            return;
        }
        match self.join_merge_batch(run, &lease) {
            Some(batch) => self.apply_merge_batch(run.task_id, batch, run.id),
            None => {
                // join 批未完整:租约保持持有,等待兄弟节点终态;
                // 下游 promote 由 has_incomplete_join_deferral 门控
            }
        }
    }

    /// join 批判定:`Some(batch)` = 可立即汇合的完整租约批(单租约,或
    /// 组内全部父步骤终态时全部成功父节点的租约);`None` = 本步骤属于
    /// 某个 join 组且组内仍有父步骤未终态(暂缓,不得提前汇合/释放)。
    fn join_merge_batch(&self, run: &RunView, own: &ExecutionLease) -> Option<Vec<ExecutionLease>> {
        let steps = self.store.revision_steps(run.revision_id).ok()?;
        let me = steps.iter().find(|s| s.id == run.step_id)?;
        // 直接子节点中的 join(多父)节点:本步骤是其父之一
        let join_children: Vec<&StepView> = steps
            .iter()
            .filter(|s| s.deps.contains(&me.id) && s.deps.len() > 1)
            .collect();
        if join_children.is_empty() {
            return Some(vec![own.clone()]); // 串行链/无下游:单租约立即汇合
        }
        // 组 = 这些 join 子节点的全部父步骤;全部终态批才完整
        let mut parent_ids: HashSet<i64> = HashSet::new();
        for child in &join_children {
            parent_ids.extend(child.deps.iter().copied());
        }
        let parents: Vec<&StepView> = steps
            .iter()
            .filter(|s| parent_ids.contains(&s.id))
            .collect();
        if parents.iter().any(|p| !p.status.terminal()) {
            return None;
        }
        // 批 = 全部成功父节点仍持有的租约(失败/跳过的父节点无产出可汇)
        let step_leases = self.step_leases.lock();
        let batch: Vec<ExecutionLease> = parents
            .iter()
            .filter(|p| p.status == StepStatus::Succeeded)
            .filter_map(|p| step_leases.get(&p.id).cloned())
            .collect();
        Some(batch)
    }

    /// 把完整租约批汇合回项目目录:成功 → 释放全部租约;冲突/出错 →
    /// 全部保持持有、逐条持久化为待决、任务 → needs-you(重启可恢复)。
    fn apply_merge_batch(&self, task_id: i64, leases: Vec<ExecutionLease>, trigger_run: i64) {
        if leases.is_empty() {
            return;
        }
        let conflicts = match self.merge_leases(&leases) {
            Ok(conflicts) => conflicts,
            Err(e) => vec![format!("汇合执行失败: {e:#}")],
        };
        if conflicts.is_empty() {
            for lease in &leases {
                self.release_lease_of_lease(lease);
            }
            return;
        }
        // 冲突持久化(重启后仍可恢复:任务保持 needs-you,租约保持持有)
        for lease in &leases {
            if let Err(e) = self.store.insert_pending_merge(task_id, lease, &conflicts) {
                log::error!("持久化待决汇合失败: {e:#}");
            }
        }
        self.pending_merges
            .lock()
            .entry(task_id)
            .or_default()
            .extend(leases.iter().cloned());
        if let Some(t) = self.store.task_view(task_id).ok().flatten() {
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
            run_id: trigger_run,
            text: format!(
                "隔离目录汇合存在冲突,等待用户处理:
{}",
                conflicts.join(
                    "
"
                )
            ),
        });
    }

    /// 任务是否存在"已成功、租约暂缓等待 join 兄弟"的父节点:
    /// 暂缓期间不得 promote 下游(下游必须从含全部上游的汇合基线检出)。
    fn has_incomplete_join_deferral(&self, task_id: i64) -> bool {
        let Some(rev) = self.store.active_revision(task_id).ok().flatten() else {
            return false;
        };
        let Ok(steps) = self.store.revision_steps(rev.id) else {
            return false;
        };
        let step_has_lease = |id: i64| self.step_leases.lock().contains_key(&id);
        for child in steps.iter().filter(|s| s.deps.len() > 1) {
            if child
                .deps
                .iter()
                .all(|pid| steps.iter().any(|s| s.id == *pid && s.status.terminal()))
            {
                continue; // 批已完整(已汇合或进入待决)
            }
            if child.deps.iter().any(|pid| {
                steps.iter().any(|s| {
                    s.id == *pid && s.status == StepStatus::Succeeded && step_has_lease(s.id)
                })
            }) {
                return true;
            }
        }
        false
    }

    /// join 组因跳过/失败等事件补齐(全部父步骤终态)时,立即汇合
    /// 已完整的父批次。幂等:无完整组或无持有租约则不动作。
    fn flush_completed_join_batches(&self, task_id: i64) {
        let Some(rev) = self.store.active_revision(task_id).ok().flatten() else {
            return;
        };
        let Ok(steps) = self.store.revision_steps(rev.id) else {
            return;
        };
        for child in steps.iter().filter(|s| s.deps.len() > 1) {
            let parents: Vec<&StepView> = steps
                .iter()
                .filter(|s| child.deps.contains(&s.id))
                .collect();
            if parents.iter().any(|p| !p.status.terminal()) {
                continue;
            }
            let step_leases = self.step_leases.lock();
            let batch: Vec<ExecutionLease> = parents
                .iter()
                .filter(|p| p.status == StepStatus::Succeeded)
                .filter_map(|p| step_leases.get(&p.id).cloned())
                .collect();
            drop(step_leases);
            if !batch.is_empty() {
                self.apply_merge_batch(task_id, batch, 0);
            }
        }
    }

    /// 释放任务的插件 pin(成功收敛/取消/归档后;幂等)。
    /// pin key 按 Revision 维护:遍历任务全部 Revision 逐个释放。
    fn release_workflow_pins(&self, task_id: i64) {
        let Ok(revisions) = self.store.list_revision_ids(task_id) else {
            return;
        };
        for revision_id in revisions {
            self.release_revision_pins(revision_id);
        }
    }

    /// 释放单个 Revision 的插件 pin(幂等)。
    fn release_revision_pins(&self, revision_id: i64) {
        let task_id = self
            .store
            .task_of_revision(revision_id)
            .ok()
            .flatten()
            .unwrap_or(0);
        if let Some(pins) = &self.workflow.pins {
            let run_key = workflow_pin_key(&self.root, task_id, revision_id);
            if let Err(e) = pins.release_run_pins(&run_key) {
                log::warn!("释放 Revision {revision_id} 的插件 pin 失败: {e:#}");
            }
        }
    }

    fn check_convergence(&self, task_id: i64) -> Result<()> {
        // 取消/归档的任务不被后续收敛改写;failed 任务在人工 skip 后允许重新收敛
        if let Some(t) = self.store.task_view(task_id)? {
            if matches!(t.status, TaskStatus::Cancelled | TaskStatus::Archived) {
                return Ok(());
            }
        }
        // 汇合冲突未解决:结果尚未回到项目目录,任务不算完成
        if self
            .pending_merges
            .lock()
            .get(&task_id)
            .map_or(false, |v| !v.is_empty())
        {
            return Ok(());
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
                self.release_workflow_pins(task_id);
                if let Err(e) = self.directory.discard_task_baselines(task_id) {
                    log::warn!("清理任务 {task_id} 集成基线失败: {e:#}");
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
        // 展示会话行(Agents 卡片/终端交互的既有通道);离散会话
        // 不建 Step / Agent Run,不参与任务成功判定(设计 §4.7)
        let display =
            self.store
                .create_session(None, "pty", &instance_snapshot.agent_type, &view.title)?;
        let view = self
            .store
            .attach_display_session(view.id, display.id)?
            .unwrap_or(view);
        let launch = self.host.launch_ad_hoc(crate::runtime::AdHocLaunchSpec {
            task_id,
            session_id: view.id,
            display_session_id: display.id,
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
            if let Some(dead_session) =
                self.store
                    .update_session(display.id, Some(SessionStatus::Dead), None, None)?
            {
                self.emit(SchedulerEvent::SessionUpdated(dead_session));
            }
            return Err(anyhow::anyhow!("离散会话 {} 启动失败: {error:#}", view.id));
        }
        // 启动成功但 DB 写失败:必须补偿杀进程,不留孤儿 CLI
        let launched = match self.store.mark_ad_hoc_launched(view.id) {
            Ok(Some(view)) => view,
            Ok(None) => anyhow::bail!("离散会话 {} 启动后读取失败", view.id),
            Err(db_error) => {
                // 进程注册在展示会话键下 —— 补偿 kill 必须用 display ID
                self.host.kill_ad_hoc(&self.root_str, display.id);
                // 二次状态写入失败不遮蔽主错误(kill 已执行,必须如实上报)
                match self.store.set_ad_hoc_status(view.id, SessionStatus::Dead) {
                    Ok(Some(dead)) => {
                        self.emit(SchedulerEvent::AdHocSessionUpdated(dead));
                    }
                    Ok(None) => {}
                    Err(secondary) => {
                        log::warn!("离散会话 {} 补偿置 Dead 失败: {secondary:#}", view.id);
                    }
                }
                return Err(db_error.context(format!(
                    "离散会话 {} 启动后状态写入失败,已终止进程",
                    view.id
                )));
            }
        };
        if let Some(working) =
            self.store
                .update_session(display.id, Some(SessionStatus::Working), None, None)?
        {
            self.emit(SchedulerEvent::SessionUpdated(working));
        }
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
            // 同步展示会话状态(卡片与终端视图)
            if let Some(display_id) = final_view.display_session_id {
                if let Some(updated) =
                    self.store
                        .update_session(display_id, Some(status), None, None)?
                {
                    self.emit(SchedulerEvent::SessionUpdated(updated));
                }
            }
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
        // 工作流 Revision:Step 对应快照节点(冻结实例);否则走旧 Profile 路径
        let snapshot_node = self
            .store
            .revision_snapshot(step.revision_id)?
            .and_then(|s| s.nodes.into_iter().find(|n| n.key == step.step_key));
        let agent_label = snapshot_node
            .as_ref()
            .map(|n| n.instance.agent_type.clone())
            .unwrap_or_else(|| step.agent_profile.clone());
        let legacy_profile = if snapshot_node.is_some() {
            None
        } else {
            let catalog = self.profiles.read();
            Some(
                catalog
                    .specs
                    .get(&step.agent_profile)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Agent Profile `{}` 未注册", step.agent_profile)
                    })?,
            )
        };
        // 会话运行时:工作流节点恒为 PTY(Adapter 编译 LaunchPlan);
        // 旧路径沿用 Profile 声明的运行时
        let session_runtime = snapshot_node
            .as_ref()
            .map(|_| "pty".to_string())
            .or_else(|| {
                legacy_profile
                    .as_ref()
                    .map(|p| p.runtime.as_str().to_string())
            })
            .unwrap_or_else(|| "pty".to_string());
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
                    self.store
                        .create_session(None, &session_runtime, &agent_label, &step.title)?,
                    false,
                ),
            }
        } else {
            match policy {
                SessionPolicy::Fresh => (
                    self.store
                        .create_session(None, &session_runtime, &agent_label, &step.title)?,
                    false,
                ),
                SessionPolicy::Reuse { key } => {
                    match self.store.find_reusable_session(key, &agent_label)? {
                        Some(s)
                            if !matches!(s.status, SessionStatus::Dead | SessionStatus::Hidden) =>
                        {
                            (s, true)
                        }
                        _ => (
                            self.store.create_session(
                                Some(key),
                                &session_runtime,
                                &agent_label,
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
        // 并发槽登记先于租约获取:后续任何失败路径(租约/持久化/pin/启动)
        // 经失败结算归还全局槽,不会泄漏 tick 预占的额度
        self.active_dispatches.lock().insert(run.id);
        // 执行位置租约:同 step 复用(自动重试保留文件修改),新 step acquire。
        // acquire/持久化失败立即失败结算 —— 绝不留下 Running 的孤儿 run。
        let lease = {
            let mut step_leases = self.step_leases.lock();
            if let Some(existing) = step_leases.get(&step.id) {
                existing.clone()
            } else {
                // 上游节点键(worktree 合并的拓扑顺序;工作流节点取快照 deps,
                // 旧流水线把 step 依赖 id 映射回节点键)
                let deps = match &snapshot_node {
                    Some(node) => node.deps.clone(),
                    None => {
                        let all = self.store.task_steps(task.id).unwrap_or_default();
                        step.deps
                            .iter()
                            .filter_map(|dep_id| {
                                all.iter()
                                    .find(|s| s.id == *dep_id)
                                    .map(|s| s.step_key.clone())
                            })
                            .collect()
                    }
                };
                let ctx = LeaseContext {
                    task_id: task.id,
                    step_id: step.id,
                    revision_id: step.revision_id,
                    attempt: (step.attempts as u32) + 1,
                    project_root: self.root.clone(),
                    step_key: step.step_key.clone(),
                    deps,
                };
                let lease = match self.directory.acquire(&ctx) {
                    Ok(lease) => lease,
                    Err(e) => {
                        drop(step_leases);
                        self.settle_dispatch_failure(&run, format!("获取执行目录租约失败: {e:#}"));
                        return Ok(());
                    }
                };
                step_leases.insert(step.id, lease.clone());
                lease
            }
        };
        if let Err(e) = self
            .store
            .insert_execution_lease(&lease, Some(run.id), step.id, task.id)
        {
            // 数据库行写入失败:先直接释放刚 acquire 的租约,再失败结算
            self.step_leases.lock().remove(&step.id);
            let _ = self.directory.release(&lease);
            self.settle_dispatch_failure(&run, format!("持久化执行租约失败: {e:#}"));
            return Ok(());
        }
        self.held_leases.lock().insert(run.id, lease.clone());
        self.emit(SchedulerEvent::StepUpdated(StepView {
            status: StepStatus::Running,
            ..step.clone()
        }));
        self.emit(SchedulerEvent::RunUpdated(run.clone()));

        if let Some(node) = snapshot_node {
            // 工作流路径:插件 pin 校验 → 冻结实例经宿主真实 Adapter 编译启动
            if let (Some(pin), Some(pins)) = (&node.plugin, &self.workflow.pins) {
                if let Err(e) = pins.resolve_pin(pin) {
                    self.settle_dispatch_failure(
                        &run,
                        format!("插件包 pin 无法解析({}): {e:#}", pin.full_id),
                    );
                    return Ok(());
                }
            }
            let upstream = self.upstream_handoffs(step, &node.deps);
            let prompt = build_workflow_prompt(task, &node, &run.capability_token, &upstream);
            let run_temp = trusted_run_temp(run.id);
            let spec = WorkflowLaunchSpec {
                project_root: self.root.clone(),
                run_id: run.id,
                step_id: step.id,
                task_id: task.id,
                session_id: session.id,
                session_key: policy.session_key().map(str::to_string),
                attach_existing_session: attach_existing,
                node_key: node.key.clone(),
                step_title: step.title.clone(),
                instance: node.instance.clone(),
                plugin: node.plugin.clone(),
                prompt,
                capability_token: run.capability_token.clone(),
                pipe_name: self.pipe_name.clone(),
                mfctl_hint: Some(format!(
                    "mfctl step complete --summary \"...\" / mfctl step fail --reason \"...\""
                )),
                workdir: lease.path.clone(),
                run_temp: run_temp.clone(),
            };
            let tx = self.runtime_tx.clone();
            if let Err(e) = self.host.launch_workflow(spec, tx) {
                self.settle_dispatch_failure(&run, format!("工作流节点启动失败: {e:#}"));
            }
            return Ok(());
        }

        let prompt = build_prompt(task, step, &run.capability_token);
        let spec_profile = legacy_profile.expect("非工作流路径必然解析出 Profile");
        let spec = LaunchSpec {
            project_root: self.root.clone(),
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
            workdir: lease.path.clone(),
        };
        let tx = self.runtime_tx.clone();
        self.host.launch(spec, tx);
        Ok(())
    }

    /// 派发失败补偿:收口会话(无进程存活,不留 Working 悬挂行)、
    /// 立即失败结算(归还并发槽/租约,任务进入 needs-you)。
    fn settle_dispatch_failure(&self, run: &RunView, reason: String) {
        self.emit(SchedulerEvent::Log {
            run_id: run.id,
            text: reason.clone(),
        });
        if let Some(session_id) = run.session_id {
            if let Ok(Some(s)) =
                self.store
                    .update_session(session_id, Some(SessionStatus::Dead), None, None)
            {
                self.emit(SchedulerEvent::SessionUpdated(s));
            }
        }
        if let Err(e) = self.settle_run(run.id, Settlement::Fail { reason }) {
            self.emit(SchedulerEvent::Error(format!("派发失败结算出错: {e:#}")));
        }
    }

    /// 上游节点(按节点键)最近一次 Handoff。加载**全部传递祖先**
    /// (不只直接依赖):深链节点(如 a→b→c 中的 a)的输出对 c 可见,
    /// `${nodes.a.output.report_path}` 等传递引用才能解析。
    fn upstream_handoffs(
        &self,
        step: &StepView,
        deps: &[String],
    ) -> HashMap<String, crate::handoff::Handoff> {
        let mut out = HashMap::new();
        let Ok(steps) = self.store.revision_steps(step.revision_id) else {
            return out;
        };
        let Ok(rows) = self.store.list_handoff_rows(step.task_id) else {
            return out;
        };
        // 依赖图(id → 键)展开:从直接依赖 BFS 收集全部祖先键
        let id_to_key: HashMap<i64, &str> =
            steps.iter().map(|s| (s.id, s.step_key.as_str())).collect();
        let deps_of: HashMap<&str, Vec<&str>> = steps
            .iter()
            .map(|s| {
                (
                    s.step_key.as_str(),
                    s.deps
                        .iter()
                        .filter_map(|d| id_to_key.get(d).copied())
                        .collect(),
                )
            })
            .collect();
        let mut ancestors: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut queue: Vec<&str> = deps.iter().map(String::as_str).collect();
        while let Some(key) = queue.pop() {
            if !seen.insert(key) {
                continue;
            }
            ancestors.push(key.to_string());
            if let Some(upstream) = deps_of.get(key) {
                queue.extend(upstream.iter().copied());
            }
        }
        for dep_key in &ancestors {
            let Some(dep_step) = steps.iter().find(|s| s.step_key == *dep_key) else {
                continue;
            };
            // 同一上游多次尝试:取最新一行(handoffs 按 id 升序)
            if let Some(row) = rows
                .iter()
                .filter(|r| r.step_id == Some(dep_step.id))
                .next_back()
            {
                out.insert(dep_key.clone(), row.handoff.clone());
            }
        }
        out
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
                // 启动失败:无进程存活,会话收口为 Dead(不留 Working 悬挂行)
                if let Some(session_id) = run.session_id {
                    if let Some(s) = self.store.update_session(
                        session_id,
                        Some(SessionStatus::Dead),
                        None,
                        None,
                    )? {
                        self.emit(SchedulerEvent::SessionUpdated(s));
                    }
                }
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
        if self
            .pending_merges
            .lock()
            .get(&task_id)
            .map_or(false, |v| !v.is_empty())
        {
            return false;
        }
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

/// 工作流节点的 run-temp 可信物化根(调度器独占分配;宿主不得改写)。
fn trusted_run_temp(run_id: i64) -> PathBuf {
    std::env::temp_dir()
        .join("monkeyfence")
        .join("steps")
        .join(format!("{}-{run_id}", std::process::id()))
}

/// 任务级插件 pin run_key:规范化 project + task + revision。
/// pin 生命周期实际按 Revision 维护(重分配各持各的 key);
/// 跨项目同 task id 不会碰撞到同一把 pin。
pub fn workflow_pin_key(project_root: &std::path::Path, task_id: i64, revision_id: i64) -> String {
    format!(
        "wf/{}/task-{}/rev-{}",
        normalize_project_for_pin(project_root),
        task_id,
        revision_id
    )
}

/// 项目根规范化:统一分隔符为 `/`、去尾斜杠;Windows 大小写不敏感。
fn normalize_project_for_pin(project_root: &std::path::Path) -> String {
    let mut text = project_root.to_string_lossy().replace('\\', "/");
    while text.ends_with('/') && text.len() > 1 {
        text.pop();
    }
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// 从数据库行重建租约对象(重启恢复路径)。
fn lease_from_row(row: &ExecutionLeaseRow) -> ExecutionLease {
    ExecutionLease {
        id: row.lease_key.clone(),
        path: PathBuf::from(&row.path),
        isolated: row.isolated,
        provider: row.provider.clone(),
        metadata: row
            .metadata_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or(serde_json::Value::Null),
    }
}

/// 工作流节点初始提示:Task goal + 上游 Handoff 注入 + `${nodes.*}` 变量替换
/// + mfctl 结算纪律(与旧路径同一纪律文本)。
pub fn build_workflow_prompt(
    task: &TaskView,
    node: &crate::workflow::WorkflowNodeSnapshot,
    token: &str,
    upstream: &HashMap<String, crate::handoff::Handoff>,
) -> String {
    let instructions = substitute_node_references(&node.instructions, upstream);
    let mut sections = vec![format!(
        "你在 MonkeyFence 中执行工作流节点「{}」(任务: {})。",
        node.title, task.title
    )];
    if !task.goal.trim().is_empty() {
        sections.push(format!("任务目标:\n{}", task.goal));
    }
    if !upstream.is_empty() {
        let mut keys: Vec<&String> = upstream.keys().collect();
        keys.sort();
        let lines: Vec<String> = std::iter::once("上游交接:".to_string())
            .chain(keys.into_iter().map(|key| {
                let handoff = &upstream[key];
                format!(
                    "- {key}: {}",
                    if handoff.summary.trim().is_empty() {
                        "(无摘要)"
                    } else {
                        &handoff.summary
                    }
                )
            }))
            .collect();
        sections.push(lines.join("\n"));
    }
    sections.push(format!(
        "工作说明:\n{}",
        if instructions.trim().is_empty() {
            "(无补充说明)".to_string()
        } else {
            instructions
        }
    ));
    format!(
        "{}\n\n完成后必须显式结算(在你的 shell 中运行以下命令之一,--token 参数必须原样保留):\n- 成功:mfctl --token {token} step complete --summary \"一句话总结\"\n- 失败:mfctl --token {token} step fail --reason \"失败原因\"\n\n规则:\n- 不要提交、推送或搁置任何版本控制变更。\n- 需要用户决策时,直接在终端中说明并等待。\n- 令牌仅对本步骤有效;重复提交相同结算是幂等的,提交冲突结算会被拒绝。",
        sections.join("\n\n")
    )
}

/// 替换 `${nodes.<key>.output...}` 变量:引用上游节点最近一次 Handoff。
/// 支持的路径:``(整个 Handoff 的 JSON)、`.summary`、`.status`、
/// `.changed_files`、`.artifacts`、`.blockers`、`.recommendations`、
/// `.verification`、`.output`(自定义 JSON)与其下的嵌套键。
/// 上游无输出(跳过/未结算)替换为占位说明,不保留原始变量。
pub fn substitute_node_references(
    text: &str,
    upstream: &HashMap<String, crate::handoff::Handoff>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("${nodes.") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "${nodes.".len()..];
        // 取到最近的 `}` 作为引用结束(引用语法内不嵌套花括号)
        let Some(end_rel) = after.find('}') else {
            out.push_str("${nodes.");
            out.push_str(after);
            return out;
        };
        let reference = &after[..end_rel];
        let key: String = reference
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if key.is_empty() {
            out.push_str(&rest[at..at + "${nodes.".len() + end_rel + 1]);
            rest = &after[end_rel + 1..];
            continue;
        }
        let path = reference[key.len()..].trim_start_matches('.');
        match upstream.get(&key) {
            Some(handoff) => out.push_str(&resolve_handoff_path(handoff, path)),
            None => out.push_str(&format!("(上游节点 `{key}` 暂无交接输出)")),
        }
        rest = &after[end_rel + 1..];
    }
    out.push_str(rest);
    out
}

/// 解析 Handoff 输出路径(`output.` 之后的部分)。
fn resolve_handoff_path(handoff: &crate::handoff::Handoff, path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        return serde_json::to_string_pretty(handoff).unwrap_or_default();
    }
    match path {
        "summary" => return handoff.summary.clone(),
        "status" => return handoff.status.clone(),
        "changed_files" => return handoff.changed_files.join("\n"),
        "artifacts" => return handoff.artifacts.join("\n"),
        "blockers" => return handoff.blockers.join("\n"),
        "recommendations" => return handoff.recommendations.join("\n"),
        _ => {}
    }
    // `output` 或 `output.<嵌套键...>`:走自定义 JSON
    let is_output = path == "output" || path.starts_with("output.");
    let json_path = if is_output {
        path["output".len()..].trim_start_matches('.')
    } else {
        path
    };
    let mut value = if is_output {
        handoff.output.clone()
    } else {
        serde_json::to_value(handoff).unwrap_or(serde_json::Value::Null)
    };
    if json_path.is_empty() {
        return serde_json::to_string_pretty(&value).unwrap_or_default();
    }
    for segment in json_path.split('.') {
        if let serde_json::Value::Object(map) = &value {
            value = map.get(segment).cloned().unwrap_or(serde_json::Value::Null);
        } else {
            value = serde_json::Value::Null;
        }
    }
    match &value {
        serde_json::Value::Null => format!("(交接输出无字段 `{path}`)"),
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod pin_key_tests {
    use super::*;

    #[test]
    fn pin_key_includes_normalized_project_task_revision() {
        use std::path::Path;
        let a = workflow_pin_key(Path::new("D:/work/Alpha"), 3, 7);
        // 分隔符与大小写规范化(Windows):反斜杠/大小写差异是同一项目
        let b = workflow_pin_key(Path::new("D:\\WORK\\alpha\\"), 3, 7);
        assert_eq!(a, b, "同项目不同写法必须得到同一 pin key");
        // 跨项目同 task/rev 不碰撞
        let other = workflow_pin_key(Path::new("D:/work/Beta"), 3, 7);
        assert_ne!(a, other);
        // 同任务不同 revision 不互相覆盖(重分配各自 pin)
        let next_rev = workflow_pin_key(Path::new("D:/work/Alpha"), 3, 8);
        assert_ne!(a, next_rev);
        assert!(a.starts_with("wf/d:/work/alpha/task-3/rev-7") || a.contains("task-3/rev-7"));
    }
}
