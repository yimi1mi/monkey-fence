//! I13:join 暂缓的派发门控必须按 (task_id, join_step_key) 精确阻塞
//! —— 一个任务里步骤键 "a" 的租约等待 join 兄弟时,不得阻塞另一个
//! 任务里同样叫 "a" 的步骤的下游。全局裸 step_key 集合会跨任务误伤。

mod common;

use common::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, WorkflowKernel};
use mf_agent::Settlement;
use std::sync::Arc;
use std::time::Duration;

/// 提高并发上限的 fixture(两个任务共 3 个并行节点)。
fn fixture_with_concurrency(dir: &std::path::Path) -> Fixture {
    let catalog = catalog_with_worker_instance();
    let instance_id = fx_instance_id(&catalog);
    let pins = Arc::new(FakePins::default());
    let directory = Arc::new(ScriptedDirectory::new(dir));
    let host = Arc::new(RecordingHost::default());
    let store = mf_agent::store::Store::open(&dir.join("workflow-v1.db")).unwrap();
    let mut config = mf_agent::config::Config::default();
    config.engine.per_project_concurrency = 8;
    config.engine.global_concurrency = 8;
    let orch = Orchestrator::start_with_routing(
        store,
        dir.to_path_buf(),
        config,
        host.clone(),
        empty_profiles(),
        GlobalLimiter::new(8),
        "pipe".into(),
        directory.clone(),
        WorkflowKernel {
            catalog: catalog.clone(),
            pins: Some(pins.clone()),
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
        instance_id,
    }
}

/// 任务 1:A+B→J(A 结算后租约持有等待 B);任务 2 有同名步骤键
/// "a" 的独立链 a→c —— 任务 2 的 c 不得被任务 1 的 "a" 暂缓阻塞。
#[test]
fn same_step_key_deferral_in_one_task_does_not_block_other_task() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture_with_concurrency(tmp.path());
    fx.pins.resolve_ok(true);

    // 任务 1:join 图(键 a/b/j)
    let join_tpl = fx.template(
        "join-graph",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
            node("j", &["a", "b"], "汇合", &fx.instance_id),
        ],
    );
    let task1 = fx.orch.create_task("join 任务", "g1").unwrap();
    fx.assign_and_run(task1.id, &join_tpl);
    assert!(
        wait_until(Duration::from_secs(5), || {
            fx.host.workflow.lock().len() == 2
        }),
        "任务 1 的 a、b 先派发"
    );

    // 任务 1:a 结算 → 租约暂缓等待 b(全局 step_key 集合此时含 "a")
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task1.id, "a"),
            Settlement::complete("A 完成"),
        )
        .unwrap();

    // 任务 2:独立链,步骤键与任务 1 相同("a" → "c")
    let chain_tpl = fx.template(
        "chain-graph",
        vec![
            node("a", &[], "做 A'", &fx.instance_id),
            node("c", &["a"], "做 C", &fx.instance_id),
        ],
    );
    let task2 = fx.orch.create_task("独立链任务", "g2").unwrap();
    fx.assign_and_run(task2.id, &chain_tpl);
    assert!(
        wait_until(Duration::from_secs(5), || fx
            .host
            .workflow
            .lock()
            .iter()
            .any(|(s, _)| s.task_id == task2.id && s.node_key == "a")),
        "任务 2 的 a 应正常派发"
    );

    // 任务 2:a 结算(单租约链,立即汇合释放)→ c 必须照常派发:
    // 任务 1 的同名 "a" 暂缓不得阻塞任务 2 的下游
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task2.id, "a"),
            Settlement::complete("A' 完成"),
        )
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || fx
            .host
            .workflow
            .lock()
            .iter()
            .any(|(s, _)| s.task_id == task2.id && s.node_key == "c")),
        "任务 2 的 c 必须在其 a 结算后派发(不得被任务 1 的同名键暂缓阻塞)"
    );

    // 任务 1 侧语义不变:b 结算后 j 派发
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task1.id, "b"),
            Settlement::complete("B 完成"),
        )
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || fx
            .host
            .workflow
            .lock()
            .iter()
            .any(|(s, _)| s.task_id == task1.id && s.node_key == "j")),
        "任务 1 的 j 必须在 a、b 都终态后派发"
    );
    fx.orch.stop();
}
