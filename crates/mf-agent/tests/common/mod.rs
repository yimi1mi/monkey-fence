//! 集成测试共享 fixture:从 workflow_run.rs 抽出的可复用基建
//! (ScriptedDirectory / RecordingHost / FakePins / 工作流模板助手)。

use crossbeam_channel::Sender;
use mf_agent::agent_instance::AgentInstanceDraft;
use mf_agent::catalog_store::CatalogStore;
use mf_agent::execution_directory::{
    ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use mf_agent::orchestrator::{
    GlobalLimiter, Orchestrator, ProfileCatalog, WorkflowKernel, WorkflowPluginPins,
};
use mf_agent::runtime::{RuntimeEvent, RuntimeHost, WorkflowLaunchSpec};
use mf_agent::store::Store;
use mf_agent::workflow::{
    PluginSourcePin, WorkflowNodeDraft, WorkflowTemplateDraft, WorkflowTemplateVersion,
};
use mf_agent::{InstanceScope, LaunchContext, RunMode};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn instance_draft(name: &str, executable: &str) -> AgentInstanceDraft {
    AgentInstanceDraft {
        name: name.into(),
        agent_type: "generic-command".into(),
        scope: InstanceScope::User,
        project_key: None,
        enabled: true,
        run_mode: RunMode::OneShot,
        executable: executable.into(),
        argv: vec!["--do".into()],
        env: vec![("MF_TEST_ENV".into(), "1".into())],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({ "completion": "process-exit" }),
        sealed_secret_ids: vec![],
    }
}

/// 带一个 generic-command worker 实例的内存目录库。
pub fn catalog_with_worker_instance() -> Arc<CatalogStore> {
    let catalog = CatalogStore::memory().unwrap();
    catalog
        .create_agent_instance(instance_draft("worker", "agent.exe"))
        .unwrap();
    catalog
}

/// catalog_with_worker_instance 创建的实例 id。
pub fn fx_instance_id(catalog: &Arc<CatalogStore>) -> String {
    catalog
        .list_agent_instances(None)
        .unwrap()
        .into_iter()
        .find(|i| i.name == "worker")
        .map(|i| i.id)
        .unwrap()
}

pub fn plugin_pin(full_id: &str, hash: &str) -> PluginSourcePin {
    PluginSourcePin {
        full_id: full_id.into(),
        version: "1.2.3".into(),
        content_hash: hash.into(),
        contribution_id: full_id.into(),
    }
}

pub fn plugin_index() -> HashMap<String, PluginSourcePin> {
    let mut map = HashMap::new();
    map.insert(
        "generic-command".into(),
        plugin_pin("builtin.core", "hash-generic"),
    );
    map
}

pub fn node(key: &str, deps: &[&str], instructions: &str, instance: &str) -> WorkflowNodeDraft {
    WorkflowNodeDraft {
        key: key.into(),
        title: format!("节点 {key}"),
        instructions: instructions.into(),
        agent_instance_id: instance.into(),
        deps: deps.iter().map(|s| s.to_string()).collect(),
    }
}

pub struct Fixture {
    pub catalog: Arc<CatalogStore>,
    pub pins: Arc<FakePins>,
    pub directory: Arc<ScriptedDirectory>,
    pub host: Arc<RecordingHost>,
    pub orch: Arc<Orchestrator>,
    pub instance_id: String,
}

impl Fixture {
    pub fn template(&self, key: &str, nodes: Vec<WorkflowNodeDraft>) -> WorkflowTemplateVersion {
        self.catalog
            .save_template(&WorkflowTemplateDraft {
                key: key.into(),
                name: format!("模板 {key}"),
                task_local: false,
                nodes,
            })
            .unwrap()
    }

    pub fn assign_and_run(&self, task_id: i64, version: &WorkflowTemplateVersion) {
        self.orch
            .assign_workflow(task_id, version, &plugin_index(), false)
            .unwrap();
        self.orch.confirm_and_run(task_id).unwrap();
    }
}

pub fn fixture(dir: &Path) -> Fixture {
    let catalog = catalog_with_worker_instance();
    let instance_id = fx_instance_id(&catalog);
    let pins = Arc::new(FakePins::default());
    let directory = Arc::new(ScriptedDirectory::new(dir));
    let host = Arc::new(RecordingHost::default());
    let store = Store::open(&dir.join("workflow-v1.db")).unwrap();
    // F4:测试提供器也携带 pin(dispatch 盖章 provider_pin;
    // 无 pin 的隔离租约会按 Absent 三态拒绝路由)
    let orch = Orchestrator::start_with_routing(
        store,
        dir.to_path_buf(),
        mf_agent::config::Config::default(),
        host.clone(),
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        directory.clone(),
        WorkflowKernel {
            catalog: catalog.clone(),
            pins: Some(pins.clone()),
            instance_resolver: None,
        },
        mf_agent::orchestrator::DirectoryRouting {
            current_pin: Some(plugin_pin("scripted", "hash-scripted")),
            resolver: None,
        },
    )
    .unwrap();
    Fixture {
        catalog,
        pins,
        directory,
        host,
        orch,
        instance_id,
    }
}

/// F4:ScriptedDirectory 测试的标准路由(current pin 一致,无 resolver)。
pub fn scripted_routing() -> mf_agent::orchestrator::DirectoryRouting {
    mf_agent::orchestrator::DirectoryRouting {
        current_pin: Some(plugin_pin("scripted", "hash-scripted")),
        resolver: None,
    }
}

/// F4:任意测试提供器的 pinned 路由。
pub fn pinned_routing(full_id: &str, hash: &str) -> mf_agent::orchestrator::DirectoryRouting {
    let mut pin = plugin_pin(full_id, hash);
    pin.contribution_id = "worktree".into();
    mf_agent::orchestrator::DirectoryRouting {
        current_pin: Some(pin),
        resolver: None,
    }
}

pub fn empty_profiles() -> Arc<RwLock<ProfileCatalog>> {
    Arc::new(RwLock::new(ProfileCatalog::default()))
}

pub fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

/// pin 生命周期假实现:记录调用,可注入 resolve 失败与指定包 pin 失败。
#[derive(Default)]
pub struct FakePins {
    pub pinned: Mutex<Vec<(String, PluginSourcePin)>>,
    pub released: Mutex<Vec<String>>,
    resolve_ok: AtomicBool,
    pub fail_on: Mutex<Option<String>>,
}

impl FakePins {
    pub fn resolve_ok(&self, ok: bool) {
        self.resolve_ok.store(ok, Ordering::SeqCst);
    }
    pub fn fail_on(&self, full_id: &str) {
        *self.fail_on.lock() = Some(full_id.to_string());
    }
}

impl WorkflowPluginPins for FakePins {
    fn pin_for_run(&self, run_key: &str, pin: &PluginSourcePin) -> anyhow::Result<()> {
        if self.fail_on.lock().as_deref() == Some(pin.full_id.as_str()) {
            anyhow::bail!("插件包 {} 不可用(脚本注入失败)", pin.full_id);
        }
        self.pinned.lock().push((run_key.to_string(), pin.clone()));
        Ok(())
    }
    fn resolve_pin(&self, _pin: &PluginSourcePin) -> anyhow::Result<()> {
        if self.resolve_ok.load(Ordering::SeqCst) {
            Ok(())
        } else {
            anyhow::bail!("插件包已被卸载,无法解析 pin")
        }
    }
    fn release_run_pins(&self, run_key: &str) -> anyhow::Result<()> {
        self.released.lock().push(run_key.to_string());
        Ok(())
    }
}

/// 可脚本化目录提供器:acquire 可失败、merge 结果可切换、记录 release
/// 与每次合并的批大小(join 批次语义回归用)。
pub struct ScriptedDirectory {
    root: std::path::PathBuf,
    pub acquire_fails: AtomicBool,
    pub isolates: AtomicBool,
    pub merge_ok: AtomicBool,
    pub merges: AtomicUsize,
    /// 每次调用的批大小(leases.len()),按调用顺序。
    pub merge_batches: Mutex<Vec<usize>>,
    pub released: Mutex<Vec<String>>,
    /// F4:release 可注入失败(验证"release 成功后才标 released")。
    pub release_fails: AtomicBool,
    pub block_merge: AtomicBool,
    pub merge_entered: AtomicBool,
}

impl ScriptedDirectory {
    pub fn new(root: &Path) -> ScriptedDirectory {
        ScriptedDirectory {
            root: root.to_path_buf(),
            acquire_fails: AtomicBool::new(false),
            isolates: AtomicBool::new(true),
            merge_ok: AtomicBool::new(true),
            merges: AtomicUsize::new(0),
            merge_batches: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
            release_fails: AtomicBool::new(false),
            block_merge: AtomicBool::new(false),
            merge_entered: AtomicBool::new(false),
        }
    }

    pub fn set_isolates(&self, v: bool) {
        self.isolates.store(v, Ordering::SeqCst);
    }
}

impl ExecutionDirectoryProvider for ScriptedDirectory {
    fn id(&self) -> &str {
        "scripted"
    }
    fn isolates(&self) -> bool {
        self.isolates.load(Ordering::SeqCst)
    }
    fn acquire(&self, ctx: &LeaseContext) -> anyhow::Result<ExecutionLease> {
        if self.acquire_fails.load(Ordering::SeqCst) {
            anyhow::bail!("目录租约获取失败(脚本)");
        }
        Ok(ExecutionLease {
            id: format!("lease-{}-{}", ctx.task_id, ctx.step_key),
            path: self.root.clone(),
            isolated: true,
            provider: "scripted".into(),
            metadata: serde_json::json!({ "step_key": ctx.step_key }),
        })
    }
    fn merge(&self, leases: &[ExecutionLease]) -> anyhow::Result<MergeOutcome> {
        self.merges.fetch_add(1, Ordering::SeqCst);
        self.merge_batches.lock().push(leases.len());
        self.merge_entered.store(true, Ordering::SeqCst);
        while self.block_merge.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if self.merge_ok.load(Ordering::SeqCst) {
            Ok(MergeOutcome::Merged)
        } else {
            Ok(MergeOutcome::NeedsUser {
                conflicts: vec!["src/conflict.rs(修改者: a 与 b)".into()],
            })
        }
    }
    fn release(&self, lease: &ExecutionLease) -> anyhow::Result<()> {
        if self.release_fails.load(Ordering::SeqCst) {
            anyhow::bail!("释放执行租约失败(脚本注入)");
        }
        self.released.lock().push(lease.id.clone());
        Ok(())
    }
}

/// 宿主:launch_workflow 用真实 GenericCommandAdapter 编译冻结实例。
#[derive(Default)]
pub struct RecordingHost {
    pub workflow: Mutex<Vec<(WorkflowLaunchSpec, mf_agent::LaunchPlan)>>,
    pub senders: Mutex<HashMap<String, Sender<(i64, RuntimeEvent)>>>,
    pub answers: Mutex<Vec<(String, String)>>,
    pub stop_fails: AtomicBool,
}

impl RuntimeHost for RecordingHost {
    fn launch(&self, _spec: mf_agent::LaunchSpec, _events: Sender<(i64, RuntimeEvent)>) {}
    fn launch_workflow(
        &self,
        spec: WorkflowLaunchSpec,
        events: Sender<(i64, RuntimeEvent)>,
    ) -> anyhow::Result<()> {
        let adapter = mf_plugins::builtin::adapter_for("generic-command")
            .ok_or_else(|| anyhow::anyhow!("generic-command 适配器不存在"))?;
        let mut ctx = LaunchContext::new(spec.run_temp.clone(), spec.workdir.clone());
        ctx.prompt = Some(spec.prompt.clone());
        let plan = adapter.compile_launch(&spec.instance, &ctx)?;
        self.senders
            .lock()
            .insert(spec.capability_token.clone(), events.clone());
        let run_id = spec.run_id;
        self.workflow.lock().push((spec, plan));
        let _ = events.send((run_id, RuntimeEvent::Launched));
        Ok(())
    }
    fn send_prompt(
        &self,
        _run_handle: &str,
        _session_handle: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop_run(&self, _run_handle: &str) -> anyhow::Result<()> {
        if self.stop_fails.load(Ordering::SeqCst) {
            anyhow::bail!("会话进程停止未在时限内确认(脚本注入)")
        }
        Ok(())
    }
    fn kill_session(&self, _session_handle: &str) {}
    fn kill_ad_hoc(&self, _display_session_handle: &str) {}
    fn answer_question(&self, run_handle: &str, answer: &str) {
        self.answers
            .lock()
            .push((run_handle.to_string(), answer.to_string()));
    }
    fn launch_ad_hoc(&self, _spec: mf_agent::AdHocLaunchSpec) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 按节点键取最近一次 run 的能力令牌。
pub fn token_of_node(orch: &Orchestrator, task_id: i64, key: &str) -> String {
    let steps = orch.store.task_steps(task_id).unwrap();
    let step = steps.iter().find(|s| s.step_key == key).unwrap();
    orch.store
        .list_runs_of_step(step.id)
        .unwrap()
        .into_iter()
        .rev()
        .next()
        .unwrap()
        .capability_token
}
