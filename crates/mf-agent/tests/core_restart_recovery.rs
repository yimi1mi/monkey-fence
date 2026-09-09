//! 原计划缺口:真实 Core 重启恢复——待确认输入、已确认未派发、
//! 改图提交前后的状态在重启(重建 Orchestrator/Store)后保持一致。
//! page.reload() 只证明浏览器重载;这里销毁并重建整个进程内 Core 装配。

#[path = "common/mod.rs"]
mod common;
use common::*;
use mf_agent::model::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, WorkflowKernel};
use mf_agent::workflow::*;
use mf_agent::Settlement;
use mf_agent::Store;
use std::sync::Arc;
use std::time::Duration;

fn restart(fx: &mut Fixture, root: &std::path::Path) {
    // 销毁旧调度器(线程/宿主),以同一项目库重建——等价进程内 Core 重启
    fx.orch.stop();
    let store: Arc<Store> = Store::open(&root.join("workflow-v1.db")).unwrap().into();
    let host: Arc<RecordingHost> = Arc::new(RecordingHost::default());
    let host_dyn: Arc<dyn mf_agent::runtime::RuntimeHost> = host.clone();
    let orch = Orchestrator::start_with_routing(
        store,
        root.to_path_buf(),
        mf_agent::Config::default(),
        host_dyn,
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        fx.directory.clone(),
        WorkflowKernel {
            catalog: fx.catalog.clone(),
            pins: Some(fx.pins.clone()),
            instance_resolver: None,
        },
        mf_agent::orchestrator::DirectoryRouting {
            current_pin: Some(plugin_pin("scripted", "hash-scripted")),
            resolver: None,
        },
    )
    .unwrap();
    fx.host = host;
    fx.orch = orch;
}

fn start_ab(review: bool) -> (tempfile::TempDir, Fixture, i64) {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let mut b = node("b", &["a"], "read ${inputs.report}", &fx.instance_id);
    b.require_input_review = review;
    b.input_bindings = vec![InputBinding {
        name: "report".into(),
        source_node_key: "a".into(),
        field_path: "output.path".into(),
        required: true,
        default_value: None,
    }];
    let version = fx.template("restart", vec![node("a", &[], "do A", &fx.instance_id), b]);
    let task = fx.orch.create_task("restart", "probe").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    (tmp, fx, task.id)
}

fn settle_a(fx: &Fixture, task: i64) {
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task, "a"),
            Settlement::Complete {
                summary: "A".into(),
                output: serde_json::json!({"path": "UPSTREAM_VALID.md"}),
            },
        )
        .unwrap();
}

#[test]
fn awaiting_review_input_survives_core_restart_and_still_gates() {
    let (_tmp, mut fx, task) = start_ab(true);
    settle_a(&fx, task);
    let b = fx
        .orch
        .store
        .task_steps(task)
        .unwrap()
        .into_iter()
        .find(|s| s.step_key == "b")
        .unwrap()
        .id;
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch
            .store
            .latest_node_input_of_step(task, b)
            .unwrap()
            .map(|r| r.review_state == "awaiting_review")
            .unwrap_or(false)
    }));

    // 重启:待确认状态与覆盖草稿都在持久库里
    restart(&mut fx, _tmp.path());
    let record = fx
        .orch
        .store
        .latest_node_input_of_step(task, b)
        .unwrap()
        .unwrap();
    assert_eq!(record.review_state, "awaiting_review");
    // 重启后门控仍生效:tick 不派发 b
    std::thread::sleep(Duration::from_millis(800));
    assert!(fx.host.workflow.lock().is_empty(), "重启后确认前不得派发");

    // 确认(用重启前记录的 input_revision)→ 恰好派发一次
    fx.orch
        .store
        .confirm_node_input(record.id, b, record.input_revision)
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "b")
    }));
    std::thread::sleep(Duration::from_millis(300));
    let dispatched = fx
        .host
        .workflow
        .lock()
        .iter()
        .filter(|(spec, _)| spec.node_key == "b")
        .count();
    assert_eq!(dispatched, 1, "重启后确认只派发一次");
    let b_spec = fx
        .host
        .workflow
        .lock()
        .iter()
        .find(|(spec, _)| spec.node_key == "b")
        .map(|(spec, _)| spec.prompt.clone())
        .unwrap();
    assert!(
        b_spec.contains("UPSTREAM_VALID.md"),
        "重启后上游来源仍有效: {}",
        b_spec
    );
    fx.orch.stop();
}

#[test]
fn confirmed_not_yet_dispatched_survives_restart_and_dispatches_once() {
    let (_tmp, mut fx, task) = start_ab(true);
    settle_a(&fx, task);
    let b = fx
        .orch
        .store
        .task_steps(task)
        .unwrap()
        .into_iter()
        .find(|s| s.step_key == "b")
        .unwrap()
        .id;
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch
            .store
            .latest_node_input_of_step(task, b)
            .unwrap()
            .map(|r| r.review_state == "awaiting_review")
            .unwrap_or(false)
    }));
    // 暂停 → 确认(已确认未派发状态) → 重启 → 恢复 → 恰好一次派发
    fx.orch.pause_task(task).unwrap();
    let record = fx
        .orch
        .store
        .latest_node_input_of_step(task, b)
        .unwrap()
        .unwrap();
    fx.orch
        .store
        .confirm_node_input(record.id, b, record.input_revision)
        .unwrap();
    restart(&mut fx, _tmp.path());
    let after = fx
        .orch
        .store
        .latest_node_input_of_step(task, b)
        .unwrap()
        .unwrap();
    assert_eq!(after.review_state, "confirmed", "已确认状态跨重启保持");
    fx.orch.resume_task(task).unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "b")
    }));
    std::thread::sleep(Duration::from_millis(300));
    let count = fx
        .host
        .workflow
        .lock()
        .iter()
        .filter(|(spec, _)| spec.node_key == "b")
        .count();
    assert_eq!(count, 1, "恢复后只派发一次");
    fx.orch.stop();
}

#[test]
fn graph_patch_state_survives_restart_and_resumes_correctly() {
    let (_tmp, mut fx, task) = start_ab(false);
    settle_a(&fx, task);
    // 暂停 → 插入 C(A→C→B) → 重启(提交后、恢复前)→ 恢复只跑 C/B
    fx.orch.pause_task(task).unwrap();
    let nodes = vec![
        node("a", &[], "do A", &fx.instance_id),
        node("c", &["a"], "do C", &fx.instance_id),
        node("b", &["c"], "read nothing", &fx.instance_id),
    ];
    let snapshot = compile(&fx, nodes.clone());
    let digest = workflow_content_digest(&nodes, false);
    fx.orch
        .store
        .with_tx(|tx| Store::create_patched_revision_tx(tx, task, &snapshot, &digest))
        .unwrap();

    restart(&mut fx, _tmp.path());
    // 重启后继承状态保持:A 成功、C 就绪、B 等待 C
    let steps = fx.orch.store.task_steps(task).unwrap();
    let a = steps.iter().find(|s| s.step_key == "a").unwrap();
    let c = steps.iter().find(|s| s.step_key == "c").unwrap();
    let b = steps.iter().find(|s| s.step_key == "b").unwrap();
    assert_eq!(a.status, StepStatus::Succeeded);
    assert_eq!(c.status, StepStatus::Ready);
    assert_eq!(b.status, StepStatus::Pending);
    // 输入历史(R6):A 的输入记录按 key 仍可见
    assert!(fx
        .orch
        .store
        .latest_node_input_of_key(task, "a")
        .unwrap()
        .is_some());

    fx.orch.resume_task(task).unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "c")
    }));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !fx.host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "b"),
        "C 未完成前 B 不派发"
    );
    let a_runs = fx
        .host
        .workflow
        .lock()
        .iter()
        .filter(|(spec, _)| spec.node_key == "a")
        .count();
    assert_eq!(a_runs, 0, "重启后 A 不重跑(新宿主无历史 spec)");
    fx.orch.stop();
}

fn compile(fx: &Fixture, nodes: Vec<WorkflowNodeDraft>) -> WorkflowSnapshot {
    let template = WorkflowTemplateVersion {
        version_id: 0,
        template_key: "restart".into(),
        version: 99,
        nodes,
        created_at: String::new(),
    };
    mf_agent::workflow_compiler::WorkflowCompiler::new()
        .compile(mf_agent::workflow_compiler::CompileInput {
            template: &template,
            directory_provider_isolates: true,
            allow_unsafe_shared_directory: false,
            agent_type_plugins: &plugin_index(),
            resolve_instance: &|reference| {
                fx.catalog
                    .snapshot_agent_instance(reference, None)
                    .map_err(|e| anyhow::anyhow!("{e:#}"))
            },
            directory_provider: Some(plugin_pin("scripted", "hash-scripted")),
        })
        .expect("补丁图必须编译通过")
}
