//! F1:跨 Orchestrator/跨进程的 join 汇合不得领取半批。
//!
//! 两个 Orchestrator 共享同一数据库,各自只在自己的进程内缓存了
//! 一个父租约(实例 A 派发 a、实例 B 派发 b)。两者同时 complete 时,
//! Store 必须在同一事务内从活动修订的 join 父步骤 + held 租约**权威
//! 收集完整父集**:merge 恰好一次、批恰好 [a,b]、两个租约都释放、
//! 持久批状态收敛 —— 调用方内存中的半批集合绝不能成为合并批。

mod common;

use common::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, WorkflowKernel};
use mf_agent::runtime::RuntimeHost;
use mf_agent::workflow::{WorkflowNodeDraft, WorkflowTemplateDraft, WorkflowTemplateVersion};

/// 共享宿主:记录行为与 RecordingHost 一致,但会话存活探测恒真
///(两个实例共享同一进程内的测试 CLI 语义,不存在真实进程死亡)。
struct SharedHost(RecordingHost);

impl RuntimeHost for SharedHost {
    fn launch(&self, spec: mf_agent::LaunchSpec, events: crossbeam_channel::Sender<(i64, mf_agent::RuntimeEvent)>) {
        self.0.launch(spec, events)
    }
    fn launch_workflow(
        &self,
        spec: mf_agent::runtime::WorkflowLaunchSpec,
        events: crossbeam_channel::Sender<(i64, mf_agent::RuntimeEvent)>,
    ) -> anyhow::Result<()> {
        self.0.launch_workflow(spec, events)
    }
    fn send_prompt(&self, project: &str, run_id: i64, session_id: i64, text: &str) {
        self.0.send_prompt(project, run_id, session_id, text)
    }
    fn stop_run(&self, project: &str, run_id: i64) -> anyhow::Result<()> {
        self.0.stop_run(project, run_id)
    }
    fn is_session_alive(&self, _project: &str, _session_id: i64) -> bool {
        true
    }
    fn kill_session(&self, project: &str, session_id: i64) {
        self.0.kill_session(project, session_id)
    }
    fn kill_ad_hoc(&self, project: &str, display_session_id: i64) {
        self.0.kill_ad_hoc(project, display_session_id)
    }
    fn answer_question(&self, project: &str, run_id: i64, answer: &str) {
        self.0.answer_question(project, run_id, answer)
    }
    fn launch_ad_hoc(&self, spec: mf_agent::AdHocLaunchSpec) -> anyhow::Result<()> {
        self.0.launch_ad_hoc(spec)
    }
}
use mf_agent::store::Store;
use mf_agent::Settlement;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// 共享 ScriptedDirectory + 共享 DB 文件 + 共享宿主;两个 Orchestrator
/// 实例先后启动:A 先派发 a(a 转 running 后 B 才启动,只会派发 b),
/// 内存租约缓存天然各持一半。
/// 共享 ScriptedDirectory + 共享 DB 文件 + 存活探测恒真的共享宿主。
struct CrossFixture {
    catalog: Arc<mf_agent::catalog_store::CatalogStore>,
    pins: Arc<FakePins>,
    directory: Arc<ScriptedDirectory>,
    host: Arc<SharedHost>,
    orch: Arc<Orchestrator>,
    instance_id: String,
}

fn start_instance(
    db: &std::path::Path,
    dir: &std::path::Path,
    catalog: &Arc<mf_agent::catalog_store::CatalogStore>,
    pins: &Arc<FakePins>,
    directory: &Arc<ScriptedDirectory>,
    host: &Arc<SharedHost>,
) -> Arc<Orchestrator> {
    let store = Store::open(db).unwrap();
    let mut config = mf_agent::config::Config::default();
    // 每实例并发 1:A 只能派发 a;a 转 running 后 B 启动只能派发 b
    // —— 两个实例的内存租约缓存天然各持一半(半批场景前提)
    config.engine.per_project_concurrency = 1;
    Orchestrator::start_with_routing(
        store,
        dir.to_path_buf(),
        config,
        host.clone(),
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        directory.clone(),
        WorkflowKernel {
            catalog: catalog.clone(),
            pins: Some(pins.clone()),
        },
        mf_agent::orchestrator::DirectoryRouting {
            current_pin: Some(plugin_pin("scripted", "hash-scripted")),
            resolver: None,
        },
    )
    .unwrap()
}

fn cross_instance_fixture(dir: &std::path::Path) -> (CrossFixture, std::path::PathBuf) {
    let catalog = catalog_with_worker_instance();
    let instance_id = fx_instance_id(&catalog);
    let pins = Arc::new(FakePins::default());
    let directory = Arc::new(ScriptedDirectory::new(dir));
    let host: Arc<SharedHost> = Arc::new(SharedHost(RecordingHost::default()));
    let db = dir.join("workflow-v1.db");
    let orch = start_instance(&db, dir, &catalog, &pins, &directory, &host);
    (
        CrossFixture {
            catalog,
            pins,
            directory,
            host,
            orch,
            instance_id,
        },
        db,
    )
}

impl CrossFixture {
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

#[test]
fn cross_instance_complete_merges_full_batch_exactly_once() {
    for round in 0..5 {
        let tmp = tempfile::tempdir().unwrap();
        let (fx, db) = cross_instance_fixture(tmp.path());
        let orch_a = fx.orch.clone();
        fx.pins.resolve_ok(true);
        let version = fx.template(
            "join-cross-instance",
            vec![
                node("a", &[], "做 A", &fx.instance_id),
                node("b", &[], "做 B", &fx.instance_id),
                node("j", &["a", "b"], "汇合", &fx.instance_id),
            ],
        );
        let task = fx.orch.create_task("跨实例 join", "g").unwrap();
        fx.assign_and_run(task.id, &version);
        // A 派发 a(并发槽内只有 a ready;先于 B 存在)
        assert!(
            wait_until(Duration::from_secs(5), || fx.host.0.workflow.lock().len() == 1),
            "实例 A 先派发 a(第 {round} 轮)"
        );
        // B 现在才启动:只会看到 b ready → 派发 b —— 两个实例的
        // 内存租约缓存各持一个父租约(半批场景前提)
        let orch_b = {
            let store = Store::open(&db).unwrap();
            let mut config = mf_agent::config::Config::default();
            config.engine.per_project_concurrency = 1;
            Orchestrator::start_with_routing(
                store,
                tmp.path().to_path_buf(),
                config,
                fx.host.clone(),
                empty_profiles(),
                GlobalLimiter::new(4),
                "pipe".into(),
                fx.directory.clone(),
                WorkflowKernel {
                    catalog: fx.catalog.clone(),
                    pins: Some(fx.pins.clone()),
                },
                mf_agent::orchestrator::DirectoryRouting {
                    current_pin: Some(plugin_pin("scripted", "hash-scripted")),
                    resolver: None,
                },
            )
            .unwrap()
        };
        if !wait_until(Duration::from_secs(5), || fx.host.0.workflow.lock().len() == 2) {
            let steps = fx.orch.store.task_steps(task.id).unwrap();
            for st in &steps {
                eprintln!("[diag] step {} status={}", st.step_key, st.status.as_str());
            }
            let runs = fx.orch.store.list_runs_of_task(task.id).unwrap();
            for r in &runs {
                eprintln!("[diag] run {} step_id={} status={}", r.id, r.step_id, r.status.as_str());
            }
            panic!("实例 B 派发 b 失败(第 {round} 轮)");
        }
        let steps = fx.orch.store.task_steps(task.id).unwrap();
        assert!(
            steps
                .iter()
                .filter(|s| matches!(s.step_key.as_str(), "a" | "b"))
                .all(|s| s.status.as_str() == "running"),
            "a、b 都在运行(第 {round} 轮)"
        );
        // 同时 complete(两实例各自的 token)
        let token_a = token_of_node(&fx.orch, task.id, "a");
        let token_b = token_of_node(&fx.orch, task.id, "b");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b_a = barrier.clone();
        let h_a = std::thread::spawn(move || {
            b_a.wait();
            orch_a
                .settle_by_token(&token_a, Settlement::complete("a ok"))
                .unwrap();
        });
        let b_b = barrier.clone();
        let h_b = std::thread::spawn(move || {
            b_b.wait();
            orch_b
                .settle_by_token(&token_b, Settlement::complete("b ok"))
                .unwrap();
        });
        h_a.join().unwrap();
        h_b.join().unwrap();

        // 断言收敛(等待后台 flush 完成)
        assert!(
            wait_until(Duration::from_secs(5), || {
                fx.orch.store.list_execution_leases(task.id)
                    .map(|rows| rows.iter().all(|r| r.status == "released"))
                    .unwrap_or(false)
            }),
            "两个实例的租约都必须由唯一领取者释放(第 {round} 轮)"
        );
        assert_eq!(
            fx.directory.merges.load(Ordering::SeqCst),
            1,
            "跨实例 join 批必须恰好合并一次(第 {round} 轮,实际 {})",
            fx.directory.merges.load(Ordering::SeqCst)
        );
        assert_eq!(
            fx.directory.merge_batches.lock().as_slice(),
            &[2],
            "唯一的合并批必须是完整 [A,B] 批,不得是半批(第 {round} 轮)"
        );
        let released = fx.directory.released.lock().clone();
        assert_eq!(
            released.len(),
            2,
            "两个租约各恰好释放一次(第 {round} 轮,实际 {released:?})"
        );
        let batches = fx.orch.store.list_merge_batches(task.id).unwrap();
        assert!(
            batches
                .iter()
                .any(|b| b.lease_keys.len() == 2 && b.status != "ready"),
            "持久批行必须记录完整双租约集合并离开 ready(第 {round} 轮,{batches:?})"
        );
        let deferrals = fx.orch.store.list_join_deferrals(Some(task.id)).unwrap();
        assert!(
            deferrals.is_empty(),
            "批汇合后 join 暂缓行必须清除(第 {round} 轮)"
        );
    }
}
