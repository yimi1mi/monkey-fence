//! 工作流运行链(独立复审阻塞项 1/4/10):
//! Task Composer 之外的生产链 —— WorkflowCompiler 冻结插件 pin、
//! create_workflow_revision 原子投影 Step、Orchestrator 从冻结
//! Agent Instance 派发(真实 Agent Adapter 编译 LaunchPlan)、
//! `${nodes.*}` 变量替换、Task goal 与上游 Handoff 注入、
//! 目录租约失败不留 Running、隔离租约汇合冲突 → needs-you。

mod common;

use common::*;

use crossbeam_channel::Sender;
use mf_agent::agent_instance::AgentInstanceDraft;
use mf_agent::catalog_store::CatalogStore;
use mf_agent::config::Config;
use mf_agent::execution_directory::{
    ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use mf_agent::model::*;
use mf_agent::orchestrator::{
    GlobalLimiter, Orchestrator, ProfileCatalog, WorkflowKernel, WorkflowPluginPins,
};
use mf_agent::runtime::{RuntimeEvent, RuntimeHost, WorkflowLaunchSpec};
use mf_agent::store::Store;
use mf_agent::workflow::{
    PluginSourcePin, WorkflowNodeDraft, WorkflowTemplateDraft, WorkflowTemplateVersion,
};
use mf_agent::workflow_compiler::{CompileInput, WorkflowCompiler};
use mf_agent::{InstanceScope, LaunchContext, RunMode};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------- Fixture ----------

fn instance_draft(name: &str, executable: &str) -> AgentInstanceDraft {
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

fn plugin_pin(full_id: &str, hash: &str) -> PluginSourcePin {
    PluginSourcePin {
        full_id: full_id.into(),
        version: "1.2.3".into(),
        content_hash: hash.into(),
        contribution_id: full_id.into(),
    }
}

/// 可用 Agent Type → 插件包 pin(生产:AppCtx 从插件贡献构建)。
fn plugin_index() -> HashMap<String, PluginSourcePin> {
    let mut map = HashMap::new();
    map.insert(
        "generic-command".into(),
        plugin_pin("builtin.core", "hash-generic"),
    );
    map
}

fn node(key: &str, deps: &[&str], instructions: &str, instance: &str) -> WorkflowNodeDraft {
    WorkflowNodeDraft {
        key: key.into(),
        title: format!("节点 {key}"),
        instructions: instructions.into(),
        agent_instance_id: instance.into(),
        deps: deps.iter().map(|s| s.to_string()).collect(),
    }
}

struct Fixture {
    catalog: Arc<CatalogStore>,
    pins: Arc<FakePins>,
    directory: Arc<ScriptedDirectory>,
    host: Arc<RecordingHost>,
    orch: Arc<Orchestrator>,
    instance_id: String,
}

impl Fixture {
    fn new(dir: &std::path::Path) -> Fixture {
        let catalog = CatalogStore::memory().unwrap();
        let instance = catalog
            .create_agent_instance(instance_draft("worker", "agent.exe"))
            .unwrap();
        let pins = Arc::new(FakePins::default());
        let directory = Arc::new(ScriptedDirectory::new(dir));
        let host = Arc::new(RecordingHost::default());
        let store = Store::open(&dir.join("workflow-v1.db")).unwrap();
        let orch = Orchestrator::start_with_routing(
            store,
            dir.to_path_buf(),
            Config::default(),
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
            scripted_routing(),
        )
        .unwrap();
        Fixture {
            catalog,
            pins,
            directory,
            host,
            orch,
            instance_id: instance.id,
        }
    }

    fn template(&self, key: &str, nodes: Vec<WorkflowNodeDraft>) -> WorkflowTemplateVersion {
        self.catalog
            .save_template(&WorkflowTemplateDraft {
                key: key.into(),
                name: format!("模板 {key}"),
                task_local: false,
                nodes,
            })
            .unwrap()
    }

    fn assign_and_run(&self, task_id: i64, version: &WorkflowTemplateVersion) {
        self.orch
            .assign_workflow(task_id, version, &plugin_index(), false)
            .unwrap();
        self.orch.confirm_and_run(task_id).unwrap();
    }
}

fn empty_profiles() -> Arc<RwLock<ProfileCatalog>> {
    Arc::new(RwLock::new(ProfileCatalog::default()))
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
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
struct FakePins {
    pinned: Mutex<Vec<(String, PluginSourcePin)>>,
    released: Mutex<Vec<String>>,
    resolve_ok: AtomicBool,
    /// 命中 full_id 的 pin_for_run 失败(部分失败回滚测试)。
    fail_on: Mutex<Option<String>>,
}

impl FakePins {
    fn resolve_ok(&self, ok: bool) {
        self.resolve_ok.store(ok, Ordering::SeqCst);
    }
    fn fail_on(&self, full_id: &str) {
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
struct ScriptedDirectory {
    root: PathBuf,
    acquire_fails: AtomicBool,
    /// 隔离能力(编译器输入;默认 true)。
    isolates: AtomicBool,
    /// false → 返回 NeedsUser(冲突);true → Merged。
    merge_ok: AtomicBool,
    merges: AtomicUsize,
    /// 每次调用的批大小(leases.len()),按调用顺序。
    merge_batches: Mutex<Vec<usize>>,
    released: Mutex<Vec<String>>,
    release_fails: AtomicBool,
}

impl ScriptedDirectory {
    fn new(root: &std::path::Path) -> ScriptedDirectory {
        ScriptedDirectory {
            root: root.to_path_buf(),
            acquire_fails: AtomicBool::new(false),
            isolates: AtomicBool::new(true),
            merge_ok: AtomicBool::new(true),
            merges: AtomicUsize::new(0),
            merge_batches: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
            release_fails: AtomicBool::new(false),
        }
    }

    fn set_isolates(&self, v: bool) {
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
struct RecordingHost {
    workflow: Mutex<Vec<(WorkflowLaunchSpec, mf_agent::LaunchPlan)>>,
    senders: Mutex<HashMap<String, Sender<(i64, RuntimeEvent)>>>,
    /// true → stop_run 返回 Err(注入"停止未确认"场景)。
    stop_fails: AtomicBool,
}

impl RuntimeHost for RecordingHost {
    fn launch(&self, _spec: mf_agent::LaunchSpec, _events: Sender<(i64, RuntimeEvent)>) {}
    fn launch_workflow(
        &self,
        spec: WorkflowLaunchSpec,
        events: Sender<(i64, RuntimeEvent)>,
    ) -> anyhow::Result<()> {
        // 真实适配器路径:与生产一致的 Adapter → LaunchPlan 编译
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
    fn answer_question(&self, _run_handle: &str, _answer: &str) {}
    fn launch_ad_hoc(&self, _spec: mf_agent::AdHocLaunchSpec) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------- 编译器冻结插件 pin ----------

#[test]
fn compiler_freezes_plugin_pin_per_node() {
    let catalog = CatalogStore::memory().unwrap();
    let instance = catalog
        .create_agent_instance(instance_draft("worker", "agent.exe"))
        .unwrap();
    let template = WorkflowTemplateVersion {
        version_id: 0,
        template_key: "t".into(),
        version: 1,
        created_at: String::new(),
        nodes: vec![node("a", &[], "做 A", &instance.id)],
    };
    let pin = plugin_pin("builtin.core", "hash-generic");
    let mut plugins = HashMap::new();
    plugins.insert("generic-command".to_string(), pin.clone());
    let snapshot = WorkflowCompiler::new()
        .compile(CompileInput {
            template: &template,
            directory_provider: None,
            directory_provider_isolates: true,
            allow_unsafe_shared_directory: false,
            agent_type_plugins: &plugins,
            resolve_instance: &|id| catalog.snapshot_agent_instance(id, None),
        })
        .unwrap();
    assert_eq!(snapshot.nodes[0].plugin.as_ref(), Some(&pin));
}

#[test]
fn compiler_rejects_agent_type_without_plugin() {
    let catalog = CatalogStore::memory().unwrap();
    let instance = catalog
        .create_agent_instance(instance_draft("worker", "agent.exe"))
        .unwrap();
    let template = WorkflowTemplateVersion {
        version_id: 0,
        template_key: "t".into(),
        version: 1,
        created_at: String::new(),
        nodes: vec![node("a", &[], "做 A", &instance.id)],
    };
    let empty: HashMap<String, PluginSourcePin> = HashMap::new();
    let errors = WorkflowCompiler::new()
        .compile(CompileInput {
            template: &template,
            directory_provider: None,
            directory_provider_isolates: true,
            allow_unsafe_shared_directory: false,
            agent_type_plugins: &empty,
            resolve_instance: &|id| catalog.snapshot_agent_instance(id, None),
        })
        .unwrap_err();
    assert!(
        errors.iter().any(|e| e.code == "plugin-missing"),
        "{errors:?}"
    );
}

// ---------- Revision 原子投影 ----------

#[test]
fn workflow_revision_projects_steps_with_deps() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    let version = fx.template(
        "t",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &["a"], "看 ${nodes.a.output.summary}", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("标题", "目标").unwrap();
    let snapshot = mf_agent::workflow::freeze_workflow(&fx.catalog, &version).unwrap();
    let rev = fx
        .orch
        .store
        .create_workflow_revision(task.id, &snapshot, None)
        .unwrap();

    // 投影的 Step 与快照节点一致(键/标题/说明/依赖),
    // 且 revision 快照可整体读回(同一原子事务写入)
    let steps = fx.orch.store.revision_steps(rev.id).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].step_key, "a");
    assert_eq!(steps[1].step_key, "b");
    assert_eq!(steps[1].title, "节点 b");
    assert!(steps[1].instructions.contains("${nodes.a.output.summary}"));
    let dep_step = steps.iter().find(|s| s.step_key == "a").unwrap();
    assert!(steps[1].deps.contains(&dep_step.id));
    // 工作流 Step 的 agent_profile 投影为冻结实例的 Agent Type
    assert_eq!(steps[0].agent_profile, "generic-command");
    assert_eq!(
        fx.orch.store.revision_snapshot(rev.id).unwrap().unwrap(),
        snapshot
    );
}

// ---------- assign_workflow:编译 + pin + Revision ----------

#[test]
fn assign_workflow_compiles_pins_and_creates_active_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    let version = fx.template("t", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let task = fx.orch.create_task("标题", "").unwrap();

    let rev = fx
        .orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap();
    // 插件 pin 已按任务 run_key 固定(目录提供器 pin 随快照一并固定)
    let pinned = fx.pins.pinned.lock().clone();
    let run_key = mf_agent::orchestrator::workflow_pin_key(tmp.path(), task.id, rev.id);
    let agent_pins: Vec<_> = pinned
        .iter()
        .filter(|(k, p)| *k == run_key && p.full_id == "builtin.core")
        .collect();
    assert_eq!(agent_pins.len(), 1, "去重后每个插件只 pin 一次:{pinned:?}");
    assert_eq!(
        agent_pins[0].0, run_key,
        "pin key 必须含规范化 project+task+revision"
    );
    assert_eq!(agent_pins[0].1.content_hash, "hash-generic");
    // Step 已投影,快照含插件 pin
    let steps = fx.orch.store.revision_steps(rev.id).unwrap();
    assert_eq!(steps.len(), 1);
    let snapshot = fx.orch.store.revision_snapshot(rev.id).unwrap().unwrap();
    assert_eq!(
        snapshot.nodes[0].plugin.as_ref().unwrap().full_id,
        "builtin.core"
    );
}

#[test]
fn assign_workflow_rejects_invalid_template_without_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    // 环:a → b → a
    let version = fx.template(
        "t",
        vec![
            node("a", &["b"], "做 A", &fx.instance_id),
            node("b", &["a"], "做 B", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("标题", "").unwrap();
    let err = fx
        .orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap_err();
    assert!(err.to_string().contains("cycle"), "{err:#}");
    assert!(fx.pins.pinned.lock().is_empty(), "编译失败不得写 pin");
    assert!(
        fx.orch.store.active_revision(task.id).unwrap().is_none(),
        "编译失败不得创建 Revision"
    );
}

#[test]
fn parallel_without_isolation_defaults_to_reject() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    // 共享目录提供器(ScriptedDirectory 声明隔离,先关掉隔离能力)
    fx.directory.set_isolates(false);
    let version = fx.template(
        "t",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("标题", "").unwrap();
    let err = fx
        .orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap_err();
    assert!(err.to_string().contains("unsafe-parallel"), "{err:#}");
    // 显式风险开关后放行
    fx.orch
        .assign_workflow(task.id, &version, &plugin_index(), true)
        .unwrap();
}

// ---------- 两节点端到端(冻结实例派发 + goal/Handoff 注入 + 变量替换) ----------

#[test]
fn two_node_workflow_dispatches_frozen_instance_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "two",
        vec![
            node("a", &[], "先完成 A", &fx.instance_id),
            node(
                "b",
                &["a"],
                "阅读 ${nodes.a.output.summary} 后执行 B",
                &fx.instance_id,
            ),
        ],
    );
    let task = fx
        .orch
        .create_task("发布预检", "把发布检查清单跑一遍")
        .unwrap();
    fx.assign_and_run(task.id, &version);

    // 节点 a 派发:冻结实例 + goal 注入
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 1
    }));
    {
        let guard = fx.host.workflow.lock();
        let (spec, plan) = &guard[0];
        assert_eq!(spec.node_key, "a");
        assert_eq!(spec.instance.id, fx.instance_id, "必须派发冻结实例");
        assert_eq!(spec.instance.executable, "agent.exe");
        assert_eq!(spec.instance.argv, vec!["--do".to_string()]);
        assert!(
            spec.prompt.contains("把发布检查清单跑一遍"),
            "Task goal 未注入"
        );
        assert!(spec.prompt.contains("先完成 A"));
        assert!(spec.prompt.contains("mfctl"), "结算纪律必须在提示中");
        // 真实适配器编译出的计划:可执行文件与实例一致,prompt 进入输入注入
        assert_eq!(plan.executable.to_string_lossy(), "agent.exe");
        assert!(
            matches!(&plan.input, mf_agent::InputInjection::Argv(text) if text.contains("先完成 A")),
            "prompt 必须经 LaunchPlan 输入注入进入真实 CLI"
        );
    }

    // 结算 a(带 summary,落 Handoff)→ 解锁 b
    let token_a = {
        let guard = fx.host.workflow.lock();
        guard[0].0.capability_token.clone()
    };
    fx.orch
        .settle_by_token(
            &token_a,
            Settlement::Complete {
                summary: "A 的检查结论全部通过".into(),
                output: Default::default(),
            },
        )
        .unwrap();

    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 2
    }));
    {
        let guard = fx.host.workflow.lock();
        let (spec_b, _) = &guard[1];
        assert_eq!(spec_b.node_key, "b");
        assert!(
            spec_b.prompt.contains("A 的检查结论全部通过"),
            "上游 Handoff 未注入/变量未替换: {}",
            spec_b.prompt
        );
        assert!(
            !spec_b.prompt.contains("${nodes."),
            "变量引用必须被替换: {}",
            spec_b.prompt
        );
        assert!(spec_b.prompt.contains("上游交接"), "应显式携带上游交接段落");
    }

    // 结算 b → 任务收敛成功;成功后释放插件 pin
    let token_b = {
        let guard = fx.host.workflow.lock();
        guard[1].0.capability_token.clone()
    };
    fx.orch
        .settle_by_token(
            &token_b,
            Settlement::Complete {
                summary: "B 完成".into(),
                output: Default::default(),
            },
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch.store.task_view(task.id).unwrap().unwrap().status == TaskStatus::Succeeded
    }));
    let released_expected = {
        // 任务全部 Revision 的 pin key 都应释放
        let revs = fx.orch.store.list_revision_ids(task.id).unwrap();
        revs.into_iter()
            .map(|r| mf_agent::orchestrator::workflow_pin_key(tmp.path(), task.id, r))
            .collect::<Vec<_>>()
    };
    assert!(
        wait_until(Duration::from_secs(5), || {
            let released = fx.pins.released.lock();
            released_expected.iter().all(|k| released.contains(k))
        }),
        "任务成功后应释放全部 Revision 的插件 pin"
    );
}

#[test]
fn unresolvable_plugin_pin_fails_run_without_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(false); // 插件包被卸载
    let version = fx.template("t", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let task = fx.orch.create_task("标题", "").unwrap();
    fx.assign_and_run(task.id, &version);

    // 派发时 resolve_pin 失败 → run 失败结算(不是永远 Running)
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch
            .store
            .list_runs_of_task(task.id)
            .unwrap()
            .iter()
            .any(|r| r.status == RunStatus::Failed)
    }));
    assert!(
        fx.host.workflow.lock().is_empty(),
        "pin 解析失败不得启动进程"
    );
    let task_view = fx.orch.store.task_view(task.id).unwrap().unwrap();
    assert!(
        matches!(task_view.status, TaskStatus::NeedsYou | TaskStatus::Failed),
        "任务应进入需要人工处理的状态,实际 {:?}",
        task_view.status
    );
}

// ---------- 目录租约失败不留 Running ----------

#[test]
fn directory_acquire_failure_settles_run_not_running() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    fx.directory.acquire_fails.store(true, Ordering::SeqCst);
    let version = fx.template("t", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let task = fx.orch.create_task("标题", "").unwrap();
    fx.assign_and_run(task.id, &version);

    // acquire 失败必须结算为 Failed(含原因),不得留下 Running 行
    assert!(wait_until(Duration::from_secs(5), || {
        let runs = fx.orch.store.list_runs_of_task(task.id).unwrap();
        let task_status = fx.orch.store.task_view(task.id).unwrap().unwrap().status;
        runs.iter().any(|r| r.status == RunStatus::Failed)
            && matches!(task_status, TaskStatus::NeedsYou | TaskStatus::Failed)
    }));
    let runs = fx.orch.store.list_runs_of_task(task.id).unwrap();
    assert!(
        runs.iter().all(|r| r.status != RunStatus::Running),
        "不得留下 Running 的孤儿 run"
    );
    assert!(fx.host.workflow.lock().is_empty());
    let task_view = fx.orch.store.task_view(task.id).unwrap().unwrap();
    assert!(
        matches!(task_view.status, TaskStatus::NeedsYou | TaskStatus::Failed),
        "任务应进入需要人工处理的状态,实际 {:?}",
        task_view.status
    );
    // 派发失败必须归还全局并发槽(tick 预占的槽不得泄漏)
    let failed_run = runs.iter().find(|r| r.status == RunStatus::Failed).unwrap();
    assert_eq!(
        fx.orch.global_limiter().active(),
        0,
        "派发失败后全局并发槽必须归还"
    );
    // 派发失败必须收口 Session(不得留 Working 的悬挂会话)
    if let Some(session_id) = failed_run.session_id {
        let session = fx.orch.store.session_view(session_id).unwrap().unwrap();
        assert!(
            matches!(session.status, mf_agent::model::SessionStatus::Dead),
            "失败 run 的会话应收口为 Dead,实际 {:?}",
            session.status
        );
    }
}

// ---------- 隔离租约汇合:冲突 → needs-you → 解决后释放 ----------

#[test]
fn isolated_merge_conflict_moves_task_to_needs_you_then_release() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template("t", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let task = fx.orch.create_task("标题", "").unwrap();
    fx.assign_and_run(task.id, &version);

    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() == 1
    }));
    let token = {
        let guard = fx.host.workflow.lock();
        guard[0].0.capability_token.clone()
    };

    // 汇合冲突:成功结算不直接收敛,任务进入 needs-you,租约保持持有
    fx.directory.merge_ok.store(false, Ordering::SeqCst);
    fx.orch
        .settle_by_token(
            &token,
            Settlement::Complete {
                summary: "A 完成".into(),
                output: Default::default(),
            },
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch.store.task_view(task.id).unwrap().unwrap().status == TaskStatus::NeedsYou
    }));
    assert_eq!(
        fx.directory.merges.load(Ordering::SeqCst),
        1,
        "必须调用汇合"
    );
    assert!(
        fx.directory.released.lock().is_empty(),
        "冲突时不得释放隔离租约"
    );
    // 步骤已成功,但任务不得被收敛覆盖为 Succeeded
    let steps = fx.orch.store.task_steps(task.id).unwrap();
    assert_eq!(steps[0].status, StepStatus::Succeeded);

    // 用户解决冲突后重试汇合 → 合并成功、释放租约、任务收敛
    fx.directory.merge_ok.store(true, Ordering::SeqCst);
    fx.orch.resolve_pending_merges(task.id).unwrap();
    assert_eq!(fx.directory.released.lock().len(), 1, "解决后释放租约");
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch.store.task_view(task.id).unwrap().unwrap().status == TaskStatus::Succeeded
    }));
}

#[test]
fn merge_conflicts_persist_across_restart_and_resolve() {
    let tmp = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template("t", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let task = fx.orch.create_task("标题", "").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() == 1
    }));
    let token = {
        let guard = fx.host.workflow.lock();
        guard[0].0.capability_token.clone()
    };
    // 汇合冲突 → needs-you + 持久化待决行
    fx.directory.merge_ok.store(false, Ordering::SeqCst);
    fx.orch
        .settle_by_token(
            &token,
            Settlement::Complete {
                summary: "A 完成".into(),
                output: Default::default(),
            },
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch.store.task_view(task.id).unwrap().unwrap().status == TaskStatus::NeedsYou
    }));
    assert_eq!(
        fx.orch
            .store
            .list_pending_merges(Some(task.id))
            .unwrap()
            .len(),
        1
    );

    // 重启:同一 Store/目录提供器重建 Orchestrator,待决汇合恢复
    fx.orch.stop();
    let store = mf_agent::store::Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
    let orch2 = Orchestrator::start_with_routing(
        store,
        tmp.path().to_path_buf(),
        Config::default(),
        Arc::new(RecordingHost::default()),
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        fx.directory.clone(),
        WorkflowKernel {
            catalog: fx.catalog.clone(),
            pins: Some(fx.pins.clone()),
            instance_resolver: None,
        },
        scripted_routing(),
    )
    .unwrap();
    assert_eq!(
        orch2.pending_merge_conflicts(task.id).len(),
        1,
        "重启后待决冲突必须可查"
    );
    // 恢复的待决汇合把任务留在 needs-you(不被收敛覆盖)
    assert_eq!(
        orch2.store.task_view(task.id).unwrap().unwrap().status,
        TaskStatus::NeedsYou
    );

    // 用户解决后重试:整批汇合、释放租约、任务收敛、持久化行清空
    fx.directory.merge_ok.store(true, Ordering::SeqCst);
    let remaining = orch2.resolve_pending_merges(task.id).unwrap();
    assert!(remaining.is_empty(), "仍冲突: {remaining:?}");
    assert!(fx.directory.released.lock().len() >= 1);
    assert!(orch2
        .store
        .list_pending_merges(Some(task.id))
        .unwrap()
        .is_empty());
    assert!(wait_until(Duration::from_secs(5), || {
        orch2.store.task_view(task.id).unwrap().unwrap().status == TaskStatus::Succeeded
    }));
    orch2.stop();

    // 再次重启:无待决行,任务保持 Succeeded(不复活)
    let store3 = mf_agent::store::Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
    let orch3 = Orchestrator::start_with_routing(
        store3,
        tmp.path().to_path_buf(),
        Config::default(),
        Arc::new(RecordingHost::default()),
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        fx.directory.clone(),
        WorkflowKernel {
            catalog: fx.catalog.clone(),
            pins: Some(fx.pins.clone()),
            instance_resolver: None,
        },
        scripted_routing(),
    )
    .unwrap();
    assert!(orch3.pending_merge_conflicts(task.id).is_empty());
    assert_eq!(
        orch3.store.task_view(task.id).unwrap().unwrap().status,
        TaskStatus::Succeeded
    );
    orch3.stop();
}

// ---------- pin 生命周期:重分配 / 部分失败 / 归档 ----------

#[test]
fn reassign_holds_old_pins_until_runs_settle_then_releases() {
    // I4:旧 active Revision 有存活 run 时不能立刻 release;
    // 仅 superseded 且无 active/retryable Runs 才释放。
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let v1 = fx.template("v1", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let v2 = fx.template("v2", vec![node("x", &[], "做 X", &fx.instance_id)]);
    let task = fx.orch.create_task("标题", "").unwrap();

    let rev1 = fx
        .orch
        .assign_workflow(task.id, &v1, &plugin_index(), false)
        .unwrap();
    fx.orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() == 1
    }));
    // 运行中重新分配必须先暂停
    assert!(fx
        .orch
        .assign_workflow(task.id, &v2, &plugin_index(), false)
        .is_err());
    fx.orch.pause_task(task.id).unwrap();
    let rev2 = fx
        .orch
        .assign_workflow(task.id, &v2, &plugin_index(), false)
        .unwrap();
    assert_ne!(rev1.id, rev2.id);
    let rev1_key = mf_agent::orchestrator::workflow_pin_key(tmp.path(), task.id, rev1.id);
    // rev1 仍有存活 run(运行中的 a 节点):重分配不得立刻释放其 pin
    assert!(
        !fx.pins.released.lock().contains(&rev1_key),
        "旧 active Revision 有存活 run 时不得释放 pin(重试仍需解析 pin):{:?}",
        fx.pins.released.lock()
    );
    assert!(
        fx.pins
            .pinned
            .lock()
            .iter()
            .any(|(k, _)| *k
                == mf_agent::orchestrator::workflow_pin_key(tmp.path(), task.id, rev2.id)),
        "新 Revision 应有自己的 pin"
    );

    // 确认新 Revision(rev1 → superseded)且旧 run 全部成功收口后:
    // rev1 的 pin 才释放(确认/结算钩子触发延迟释放)。
    // 若以失败收口,failed 步骤视为可重试 → pin 继续保持(保守不泄漏)。
    let token = { fx.host.workflow.lock()[0].0.capability_token.clone() };
    fx.orch
        .settle_by_token(&token, Settlement::complete("旧流程成功收口"))
        .unwrap();
    fx.orch.confirm_and_run(task.id).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            fx.pins.released.lock().contains(&rev1_key)
        }),
        "superseded 且无存活/可重试 run 后应释放旧 Revision pin:{:?}",
        fx.pins.released.lock()
    );
    fx.orch.stop();
}

#[test]
fn stale_draft_revision_pins_do_not_leak() {
    // I4:未激活的旧 draft Revision(被更新的 draft 取代)pin 不得泄漏
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    let v1 = fx.template("v1", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let v2 = fx.template("v2", vec![node("b", &[], "做 B", &fx.instance_id)]);
    let task = fx.orch.create_task("标题", "").unwrap();
    let rev1 = fx
        .orch
        .assign_workflow(task.id, &v1, &plugin_index(), false)
        .unwrap();
    let rev2 = fx
        .orch
        .assign_workflow(task.id, &v2, &plugin_index(), false)
        .unwrap();
    assert_ne!(rev1.id, rev2.id);
    let rev1_key = mf_agent::orchestrator::workflow_pin_key(tmp.path(), task.id, rev1.id);
    assert!(
        fx.pins.released.lock().contains(&rev1_key),
        "被更新 draft 取代的旧 draft pin 必须立即释放(永不运行):{:?}",
        fx.pins.released.lock()
    );
    // 最新 draft 保持 pin(即将被确认运行)
    assert!(
        fx.pins
            .pinned
            .lock()
            .iter()
            .any(|(k, _)| *k
                == mf_agent::orchestrator::workflow_pin_key(tmp.path(), task.id, rev2.id)),
        "最新 draft 应保持 pin"
    );
    fx.orch.stop();
}

#[test]
fn pin_partial_failure_rolls_back_revision_and_pins() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    // 两个不同 Agent Type 的实例 → 两个插件包 pin;第二个失败
    let second = {
        let mut draft = instance_draft("worker-2", "agent2.exe");
        draft.agent_type = "other-type".into();
        fx.catalog.create_agent_instance(draft).unwrap()
    };
    let mut index = plugin_index();
    index.insert(
        "other-type".into(),
        plugin_pin("builtin.other", "hash-other"),
    );
    fx.pins.fail_on("builtin.other");
    let version = fx.template(
        "t",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &second.id),
        ],
    );
    let task = fx.orch.create_task("标题", "").unwrap();
    let err = fx
        .orch
        .assign_workflow(task.id, &version, &index, false)
        .unwrap_err();
    assert!(err.to_string().contains("builtin.other"), "{err:#}");
    // 部分失败精确回滚:已 pin 的引用被释放、draft Revision 被删除
    assert_eq!(fx.pins.released.lock().len(), 1, "必须回收本次 run_key");
    assert!(
        fx.pins.released.lock()[0].contains("rev-"),
        "回滚的是本次 revision key"
    );
    assert!(
        fx.orch.store.list_revision_ids(task.id).unwrap().is_empty(),
        "pin 失败不得留下 draft Revision"
    );
    fx.orch.stop();
}

#[test]
fn archive_task_releases_all_revision_pins() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template("t", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let task = fx.orch.create_task("标题", "").unwrap();
    let rev = fx
        .orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap();
    fx.orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    let token = { fx.host.workflow.lock()[0].0.capability_token.clone() };
    // 失败结算收口活动 run 后归档
    fx.orch
        .settle_by_token(
            &token,
            Settlement::Fail {
                reason: "收口".into(),
            },
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        !fx.orch.store.task_has_active_runs(task.id).unwrap()
    }));
    fx.orch.archive_task(task.id).unwrap();
    assert!(
        fx.pins
            .released
            .lock()
            .contains(&mf_agent::orchestrator::workflow_pin_key(
                tmp.path(),
                task.id,
                rev.id
            )),
        "归档必须释放 Revision pin:{:?}",
        fx.pins.released.lock()
    );
    fx.orch.stop();
}

// ---------- 全部祖先 Handoff + output.report_path 精确替换 ----------

#[test]
fn transitive_ancestor_output_report_path_resolves_in_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "chain",
        vec![
            node("a", &[], "产出报告", &fx.instance_id),
            node("b", &["a"], "阅读 ${nodes.a.output}", &fx.instance_id),
            node(
                "c",
                &["b"],
                // a 是传递祖先(非直接依赖):引用必须可解析
                "按 ${nodes.a.output.report_path} 归档,摘要 ${nodes.a.output.summary_note}",
                &fx.instance_id,
            ),
        ],
    );
    let task = fx.orch.create_task("链式", "g").unwrap();
    fx.assign_and_run(task.id, &version);

    // a 结算:结构化 output 精确写入 Handoff
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    let token_a = { fx.host.workflow.lock()[0].0.capability_token.clone() };
    fx.orch
        .settle_by_token(
            &token_a,
            Settlement::complete_with_output(
                "A 完成",
                serde_json::json!({
                    "report_path": "reports/a.md",
                    "summary_note": "全部检查通过"
                }),
            ),
        )
        .unwrap();
    // Handoff 完整性:output 原样落库,raw_log_ref 指向 run
    {
        let rows = fx.orch.store.list_handoff_rows(task.id).unwrap();
        assert_eq!(rows.len(), 1);
        let handoff = &rows[0].handoff;
        assert_eq!(handoff.output["report_path"], "reports/a.md");
        assert_eq!(handoff.output["summary_note"], "全部检查通过");
        assert!(
            handoff
                .raw_log_ref
                .as_deref()
                .unwrap_or("")
                .starts_with("agent-run:"),
            "raw_log_ref 必须引用 run: {:?}",
            handoff.raw_log_ref
        );
    }

    // b 结算后 c 派发:提示中 a 的传递引用被精确替换
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 2));
    let token_b = { fx.host.workflow.lock()[1].0.capability_token.clone() };
    fx.orch
        .settle_by_token(&token_b, Settlement::complete("B 完成"))
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 3));
    let prompt_c = {
        let guard = fx.host.workflow.lock();
        guard
            .iter()
            .find(|(spec, _)| spec.node_key == "c")
            .map(|(spec, _)| spec.prompt.clone())
            .expect("c 未派发")
    };
    assert!(
        prompt_c.contains("reports/a.md"),
        "传递祖先 output.report_path 必须精确替换: {prompt_c}"
    );
    assert!(
        prompt_c.contains("全部检查通过"),
        "嵌套 output 键必须精确替换: {prompt_c}"
    );
    assert!(
        !prompt_c.contains("${nodes.a.output"),
        "不得残留未替换变量: {prompt_c}"
    );
    fx.orch.stop();
}

// ---------- 任务本地工作流:项目 Store 草稿直接冻结 + unsafe-parallel 持久化 ----------

#[test]
fn task_local_workflow_assigns_from_project_store_draft() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let task = fx.orch.create_task("任务本地", "g").unwrap();
    let draft = mf_agent::workflow::WorkflowTemplateDraft {
        key: format!("task-{}", task.id),
        name: "任务工作流".into(),
        task_local: true,
        nodes: vec![node("a", &[], "做 A", &fx.instance_id)],
    };
    fx.orch
        .store
        .save_task_workflow(&tmp.path().to_string_lossy(), task.id, &draft, false)
        .unwrap();

    // 编译检查(不写库):无 Revision 产生
    let snapshot = fx
        .orch
        .compile_task_local_workflow(task.id, &plugin_index())
        .unwrap();
    assert_eq!(snapshot.nodes.len(), 1);
    assert!(fx.orch.store.list_revision_ids(task.id).unwrap().is_empty());

    // 分配:直接冻结项目 Store 草稿
    let rev = fx
        .orch
        .assign_task_local_workflow(task.id, &plugin_index())
        .unwrap();
    let steps = fx.orch.store.revision_steps(rev.id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].step_key, "a");
    assert!(
        fx.pins
            .pinned
            .lock()
            .iter()
            .any(|(k, _)| *k
                == mf_agent::orchestrator::workflow_pin_key(tmp.path(), task.id, rev.id)),
        "任务本地分配同样按 Revision pin"
    );
    fx.orch.stop();
}

#[test]
fn task_local_unsafe_parallel_flag_is_persisted_and_honored() {
    let tmp = tempfile::tempdir().unwrap();
    // ScriptedDirectory 关闭隔离能力 → 模拟非 Git 共享目录
    let fx = Fixture::new(tmp.path());
    fx.directory.set_isolates(false);
    fx.pins.resolve_ok(true);
    let task = fx.orch.create_task("并行任务", "g").unwrap();
    let draft = mf_agent::workflow::WorkflowTemplateDraft {
        key: format!("task-{}", task.id),
        name: "任务工作流".into(),
        task_local: true,
        nodes: vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
        ],
    };
    // 开关关闭(默认):并行编译被拒
    fx.orch
        .store
        .save_task_workflow(&tmp.path().to_string_lossy(), task.id, &draft, false)
        .unwrap();
    let err = fx
        .orch
        .assign_task_local_workflow(task.id, &plugin_index())
        .unwrap_err();
    assert!(err.to_string().contains("unsafe-parallel"), "{err:#}");
    assert!(fx.orch.store.list_revision_ids(task.id).unwrap().is_empty());

    // 开关持久化后:同一草稿可分配(用户显式接受风险)
    fx.orch
        .store
        .save_task_workflow(&tmp.path().to_string_lossy(), task.id, &draft, true)
        .unwrap();
    assert!(fx
        .orch
        .store
        .task_workflow_unsafe_parallel(&tmp.path().to_string_lossy(), task.id)
        .unwrap());
    let rev = fx
        .orch
        .assign_task_local_workflow(task.id, &plugin_index())
        .unwrap();
    assert_eq!(fx.orch.store.revision_steps(rev.id).unwrap().len(), 2);
    fx.orch.stop();
}

// ---------- 真实 worktree:并行兄弟改同一文件 → needs-you、零覆盖 ----------

#[test]
fn parallel_siblings_same_file_conflict_needs_user_with_real_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("project");
    std::fs::create_dir_all(&repo_root).unwrap();
    {
        // 真实 Git 仓库(基线提交含 shared.txt)
        let repo = git2::Repository::init(&repo_root).unwrap();
        let sig = git2::Signature::now("mf", "mf@test").unwrap();
        std::fs::write(repo_root.join("shared.txt"), "base\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
    }
    let catalog = CatalogStore::memory().unwrap();
    let instance = catalog
        .create_agent_instance(instance_draft("worker", "agent.exe"))
        .unwrap();
    let pins = Arc::new(FakePins::default());
    pins.resolve_ok(true);
    let directory = Arc::new(
        mf_plugins::git_worktree_provider::GitWorktreeProvider::new(repo_root.clone()).unwrap(),
    );
    assert!(directory.isolates());
    let db_dir = repo_root.join(".mf-agent");
    std::fs::create_dir_all(&db_dir).unwrap();
    let store = Store::open(&db_dir.join("workflow-v1.db")).unwrap();
    let orch = Orchestrator::start_with_routing(
        store,
        repo_root.clone(),
        Config::default(),
        Arc::new(RecordingHost::default()),
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        directory.clone(),
        WorkflowKernel {
            catalog: catalog.clone(),
            pins: Some(pins.clone()),
            instance_resolver: None,
        },
        pinned_routing("builtin.core", "hash-worktree"),
    )
    .unwrap();

    let version = {
        let mut nodes = catalog
            .save_template(&mf_agent::workflow::WorkflowTemplateDraft {
                key: "parallel".into(),
                name: "并行".into(),
                task_local: false,
                nodes: vec![
                    WorkflowNodeDraft {
                        key: "a".into(),
                        title: "A".into(),
                        instructions: "做 A".into(),
                        agent_instance_id: instance.id.clone(),
                        deps: vec![],
                    },
                    WorkflowNodeDraft {
                        key: "b".into(),
                        title: "B".into(),
                        instructions: "做 B".into(),
                        agent_instance_id: instance.id.clone(),
                        deps: vec![],
                    },
                ],
            })
            .unwrap();
        let _ = &mut nodes;
        nodes
    };
    let task = orch.create_task("并行冲突", "g").unwrap();
    orch.assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();

    // 两个并行节点都被派发,租约都持有
    assert!(
        wait_until(Duration::from_secs(5), || {
            orch.store
                .list_execution_leases(task.id)
                .map(|ls| ls.iter().filter(|l| l.status == "held").count() == 2)
                .unwrap_or(false)
        }),
        "两个并行节点的租约都应持有"
    );
    let lease_path = |key: &str| {
        orch.store
            .list_execution_leases(task.id)
            .unwrap()
            .into_iter()
            .find(|l| {
                let meta: serde_json::Value =
                    serde_json::from_str(l.metadata_json.as_deref().unwrap_or("{}")).unwrap();
                meta["step_key"].as_str() == Some(key)
            })
            .map(|l| std::path::PathBuf::from(l.path))
            .unwrap()
    };
    let path_a = lease_path("a");
    let path_b = lease_path("b");
    // B 的基线原始字节(从它自己的 worktree 读,含检出时的换行形态)
    let baseline_bytes = std::fs::read(path_b.join("shared.txt")).unwrap();
    // 两个 worktree 都改 shared.txt
    std::fs::write(path_a.join("shared.txt"), "from-a\n").unwrap();
    std::fs::write(path_b.join("shared.txt"), "from-b\n").unwrap();

    let token_for_step = |key: &str| {
        let steps = orch.store.task_steps(task.id).unwrap();
        let step = steps.iter().find(|s| s.step_key == key).unwrap();
        let run = orch
            .store
            .list_runs_of_step(step.id)
            .unwrap()
            .into_iter()
            .rev()
            .next()
            .unwrap();
        run.capability_token
    };

    // A 先结算:单独汇合成功
    orch.settle_by_token(&token_for_step("a"), Settlement::complete("A 完成"))
        .unwrap();
    let norm = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap().replace("\r\n", "\n");
    assert_eq!(norm(repo_root.join("shared.txt")), "from-a\n");

    // B 后结算:与已汇合的 A 修改重叠 → NeedsUser,任务 needs-you,
    // 持久化待决行,项目目录零覆盖
    orch.settle_by_token(&token_for_step("b"), Settlement::complete("B 完成"))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            orch.store.task_view(task.id).unwrap().unwrap().status == TaskStatus::NeedsYou
        }),
        "后完成者与已汇合修改重叠必须进入 needs-you,实际 {:?}",
        orch.store.task_view(task.id).unwrap().map(|t| t.status)
    );
    assert_eq!(norm(repo_root.join("shared.txt")), "from-a\n", "零覆盖");
    let pending = orch.store.list_pending_merges(Some(task.id)).unwrap();
    assert_eq!(pending.len(), 1, "待决冲突必须持久化: {pending:?}");
    assert!(
        pending[0]
            .conflicts
            .iter()
            .any(|c| c.contains("shared.txt")),
        "{:?}",
        pending[0].conflicts
    );

    // 用户解决:B 放弃修改(恢复基线字节)→ 重试合并 → 收敛成功
    std::fs::write(path_b.join("shared.txt"), &baseline_bytes).unwrap();
    let remaining = orch.resolve_pending_merges(task.id).unwrap();
    assert!(remaining.is_empty(), "{remaining:?}");
    assert!(
        wait_until(Duration::from_secs(5), || {
            orch.store.task_view(task.id).unwrap().unwrap().status == TaskStatus::Succeeded
        }),
        "解决后任务应收敛,实际 {:?}",
        orch.store.task_view(task.id).unwrap().map(|t| t.status)
    );
    assert_eq!(norm(repo_root.join("shared.txt")), "from-a\n");
    // 租约清理:B 的 worktree 释放
    assert!(!path_b.exists(), "解决后 B 的 worktree 应释放");
    orch.stop();
}

// ---------- join 汇合批:并行父分支作为完整批次合并(C1 / I10 / I11) ----------

/// 按节点键取最近一次 run 的能力令牌。
fn token_of_node(orch: &Orchestrator, task_id: i64, key: &str) -> String {
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

#[test]
fn join_parents_merge_as_one_complete_batch_after_both_settle() {
    // a、b 并行 → join(c 依赖两者):a 先结算不得单独汇合/释放/推进基线,
    // b 结算后才以完整父批次(2 个租约)一次合并,下游 c 才被派发
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "join",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
            node("c", &["a", "b"], "汇合两者", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("并行汇合", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 2));

    // a 先成功结算:join 批不完整 → 零汇合、零释放、c 不派发
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "a"),
            Settlement::complete("A 完成"),
        )
        .unwrap();
    assert_eq!(
        fx.directory.merges.load(Ordering::SeqCst),
        0,
        "join 批未完成:不得用单 Lease 汇合"
    );
    assert!(
        fx.directory.released.lock().is_empty(),
        "join 批未完成:不得释放父租约"
    );
    assert!(
        fx.host
            .workflow
            .lock()
            .iter()
            .all(|(spec, _)| spec.node_key != "c"),
        "join 批未完成:下游不得派发(基线未含全部上游)"
    );

    // b 结算:完整父批次一次合并(2 个租约),随后 c 派发
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "b"),
            Settlement::complete("B 完成"),
        )
        .unwrap();
    assert_eq!(
        fx.directory.merges.load(Ordering::SeqCst),
        1,
        "b 结算必须恰好触发一次汇合"
    );
    assert_eq!(
        fx.directory.merge_batches.lock()[0],
        2,
        "汇合必须是完整父租约批次(实际 {:?})",
        fx.directory.merge_batches.lock()
    );
    assert_eq!(
        fx.directory.released.lock().len(),
        2,
        "批成功后两个父租约都释放"
    );
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "c")
    }));
    fx.orch.stop();
}

#[test]
fn join_batch_conflict_pends_both_leases_and_resolve_resumes_scheduling() {
    // join 批冲突:两个父租约都持久化为待决、保持持有;用户解决后
    // 任务恢复 Running,下游继续派发(不是停在 needs-you)
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "join",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
            node("c", &["a", "b"], "汇合两者", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("并行冲突", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 2));

    fx.directory.merge_ok.store(false, Ordering::SeqCst);
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "a"),
            Settlement::complete("A 完成"),
        )
        .unwrap();
    assert_eq!(
        fx.orch.store.task_view(task.id).unwrap().unwrap().status,
        TaskStatus::Running,
        "批未完成时无冲突判定:任务保持运行"
    );
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "b"),
            Settlement::complete("B 完成"),
        )
        .unwrap();
    assert_eq!(
        fx.directory.merges.load(Ordering::SeqCst),
        1,
        "b 结算触发整批判定"
    );
    assert_eq!(fx.directory.merge_batches.lock()[0], 2);
    assert_eq!(
        fx.orch.store.task_view(task.id).unwrap().unwrap().status,
        TaskStatus::NeedsYou,
        "整批冲突必须进入 needs-you"
    );
    // 两个父租约都持久化为待决、保持持有
    let pending = fx.orch.store.list_pending_merges(Some(task.id)).unwrap();
    assert_eq!(
        pending.len(),
        2,
        "完整批次的每个父租约都要持久化: {pending:?}"
    );
    assert!(fx.directory.released.lock().is_empty(), "冲突时零释放");
    assert!(
        fx.host
            .workflow
            .lock()
            .iter()
            .all(|(spec, _)| spec.node_key != "c"),
        "needs-you 期间下游不派发"
    );

    // 用户解决后重试时，如果任一 release 失败，必须保持整批待决，
    // 不得清 pending / 恢复下游调度。
    fx.directory.merge_ok.store(true, Ordering::SeqCst);
    fx.directory.release_fails.store(true, Ordering::SeqCst);
    assert!(
        fx.orch.resolve_pending_merges(task.id).is_err(),
        "release 失败必须向调用方返回错误"
    );
    assert_eq!(
        fx.orch
            .store
            .list_pending_merges(Some(task.id))
            .unwrap()
            .len(),
        2,
        "release 失败时完整 pending 批必须保留"
    );
    assert_eq!(
        fx.orch.store.task_view(task.id).unwrap().unwrap().status,
        TaskStatus::NeedsYou,
        "release 失败时任务必须保持 needs-you"
    );
    assert!(
        fx.orch
            .store
            .list_execution_leases(task.id)
            .unwrap()
            .iter()
            .all(|row| row.status == "held"),
        "release 失败时租约必须保持 held"
    );

    // 清障后再次重试:整批合并成功 → 双租约释放、任务回 Running、c 派发
    fx.directory.release_fails.store(false, Ordering::SeqCst);
    let remaining = fx.orch.resolve_pending_merges(task.id).unwrap();
    assert!(remaining.is_empty(), "{remaining:?}");
    assert_eq!(fx.directory.released.lock().len(), 2);
    assert_eq!(
        fx.orch.store.task_view(task.id).unwrap().unwrap().status,
        TaskStatus::Running,
        "待决汇合解决且注意力清空后必须恢复 Running"
    );
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "c")
    }));

    // c 结算 → 任务收敛成功
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "c"),
            Settlement::complete("汇合完成"),
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch.store.task_view(task.id).unwrap().unwrap().status == TaskStatus::Succeeded
    }));
    fx.orch.stop();
}

#[test]
fn archive_task_with_pending_join_merge_releases_worktrees() {
    // 待决汇合的 join 批(两父租约)在归档时必须释放 worktree,
    // 不能只清 DB/内存映射留下磁盘痕迹
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "join",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
            node("c", &["a", "b"], "汇合两者", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("归档待决", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 2));
    fx.directory.merge_ok.store(false, Ordering::SeqCst);
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "a"),
            Settlement::complete("A 完成"),
        )
        .unwrap();
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "b"),
            Settlement::complete("B 完成"),
        )
        .unwrap();
    assert_eq!(
        fx.orch.store.task_view(task.id).unwrap().unwrap().status,
        TaskStatus::NeedsYou
    );
    // 无活动 run(两个父 run 都已成功结算)→ 允许归档;
    // 归档必须释放待决的两个父租约(复用取消清理语义)
    fx.orch.archive_task(task.id).unwrap();
    let released_after_archive = fx.directory.released.lock().clone();
    assert_eq!(
        released_after_archive.len(),
        2,
        "归档必须释放待决汇合的全部父租约:{released_after_archive:?}"
    );
    assert!(
        fx.orch
            .store
            .list_pending_merges(Some(task.id))
            .unwrap()
            .is_empty(),
        "归档后待决行清空"
    );
    fx.orch.stop();
}

// ---------- C3:停止未确认 → Interrupted/needs-you,不标 Cancelled ----------

#[test]
fn cancel_run_without_stop_confirmation_marks_interrupted_not_cancelled() {
    // stop_run 未确认进程终止:run 不得标 Cancelled、不得释放执行租约;
    // 进入 Interrupted,任务 needs-you(进程可能仍在写隔离目录)
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template("t", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let task = fx.orch.create_task("取消未确认", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    let run_id = {
        let runs = fx.orch.store.list_runs_of_task(task.id).unwrap();
        runs.iter()
            .find(|r| r.status == RunStatus::Running)
            .unwrap()
            .id
    };

    fx.host.stop_fails.store(true, Ordering::SeqCst);
    let err = fx.orch.cancel_run(run_id).unwrap_err();
    assert!(err.to_string().contains("未确认"), "{err:#}");
    let run = fx.orch.store.run_view(run_id).unwrap().unwrap();
    assert_eq!(
        run.status,
        RunStatus::Interrupted,
        "未确认终止必须 Interrupted,不能谎称 Cancelled"
    );
    assert!(
        fx.directory.released.lock().is_empty(),
        "未确认终止不得释放执行租约(进程可能仍在写)"
    );
    assert_eq!(
        fx.orch.store.task_view(task.id).unwrap().unwrap().status,
        TaskStatus::NeedsYou,
        "任务应进入需要人工处理状态"
    );
    fx.orch.stop();
}

#[test]
fn cancel_task_without_stop_confirmation_keeps_task_recoverable() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template("t", vec![node("a", &[], "做 A", &fx.instance_id)]);
    let task = fx.orch.create_task("任务取消未确认", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    let run_id = {
        let runs = fx.orch.store.list_runs_of_task(task.id).unwrap();
        runs.iter()
            .find(|r| r.status == RunStatus::Running)
            .unwrap()
            .id
    };

    fx.host.stop_fails.store(true, Ordering::SeqCst);
    let err = fx.orch.cancel_task(task.id).unwrap_err();
    assert!(err.to_string().contains("未确认"), "{err:#}");
    assert_eq!(
        fx.orch.store.run_view(run_id).unwrap().unwrap().status,
        RunStatus::Interrupted
    );
    assert_eq!(
        fx.orch.store.task_view(task.id).unwrap().unwrap().status,
        TaskStatus::NeedsYou,
        "停止未确认时任务不得标 Cancelled(保留可恢复状态)"
    );
    assert!(fx.directory.released.lock().is_empty());
    fx.orch.stop();
}

// ---------- I13:assign-then-confirm 不重复 assign ----------

#[test]
fn assign_and_confirm_task_local_reuses_unchanged_draft_and_refreezes_on_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Fixture::new(tmp.path());
    fx.pins.resolve_ok(true);
    let task = fx.orch.create_task("任务本地", "g").unwrap();
    let save_draft = |nodes: Vec<mf_agent::workflow::WorkflowNodeDraft>| {
        fx.orch
            .store
            .save_task_workflow(
                &tmp.path().to_string_lossy(),
                task.id,
                &mf_agent::workflow::WorkflowTemplateDraft {
                    key: format!("task-{}", task.id),
                    name: "任务工作流".into(),
                    task_local: true,
                    nodes,
                },
                false,
            )
            .unwrap();
    };
    save_draft(vec![node("a", &[], "做 A", &fx.instance_id)]);

    // 第一次:冻结 + 确认(一个 draft Revision → active)
    let t = fx
        .orch
        .assign_and_confirm_task_local(task.id, &plugin_index())
        .unwrap();
    assert_eq!(t.status, TaskStatus::Running);
    assert_eq!(fx.orch.store.list_revision_ids(task.id).unwrap().len(), 1);

    // 草稿未变再次确认:不得重复 assign(active_revision 判断会误判
    // "仍 none"而重复冻结 —— 这里幂等复用同一 Revision)
    let t = fx
        .orch
        .assign_and_confirm_task_local(task.id, &plugin_index())
        .unwrap();
    assert_eq!(t.status, TaskStatus::Running);
    assert_eq!(
        fx.orch.store.list_revision_ids(task.id).unwrap().len(),
        1,
        "草稿未变时不得重复冻结 Revision"
    );

    // 草稿被再次保存(内容变化/updated_at 更新)→ 重新冻结新 draft 并确认
    fx.orch.pause_task(task.id).unwrap();
    save_draft(vec![
        node("a", &[], "做 A", &fx.instance_id),
        node("b", &[], "做 B", &fx.instance_id),
    ]);
    let t = fx
        .orch
        .assign_and_confirm_task_local(task.id, &plugin_index())
        .unwrap();
    assert_eq!(t.status, TaskStatus::Running);
    let revisions = fx.orch.store.list_revision_ids(task.id).unwrap();
    assert!(
        revisions.len() >= 2,
        "草稿变化后必须重新冻结(实际 {revisions:?})"
    );
    fx.orch.stop();
}
