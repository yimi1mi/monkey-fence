//! 同一 join 批的并发结算(C1):两个线程(模拟 named-pipe 不同连接)
//! 同时 complete 批的两个父步骤时,批必须只被汇合一次 ——
//! merge 恰好一次、批恰好 [A,B]、每个租约恰好 release 一次、
//! 持久批状态收敛 merged、数据库租约全部 released。
//! 无持久批 claim(ready→merging→merged)时,两个线程都会读到
//! 同一批并各自 merge/release,双推进集成 ref。

mod common;

use common::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, WorkflowKernel};
use mf_agent::store::Store;
use mf_agent::Settlement;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// 并发 complete A/B(barrier 同时放行):必须恰好一次整批判定。
#[test]
fn concurrent_complete_of_join_parents_merges_exactly_once() {
    // 并发窗口极窄,多轮运行保证竞态被真实触发(修复后恒为 1)
    for round in 0..10 {
        let tmp = tempfile::tempdir().unwrap();
        let fx = fixture(tmp.path());
        fx.pins.resolve_ok(true);
        let version = fx.template(
            "join-concurrent",
            vec![
                node("a", &[], "做 A", &fx.instance_id),
                node("b", &[], "做 B", &fx.instance_id),
                node("j", &["a", "b"], "汇合", &fx.instance_id),
            ],
        );
        let task = fx.orch.create_task("并发 join", "g").unwrap();
        fx.assign_and_run(task.id, &version);
        assert!(
            wait_until(Duration::from_secs(5), || fx.host.workflow.lock().len()
                == 2),
            "A、B 先派发(第 {round} 轮)"
        );
        let token_a = token_of_node(&fx.orch, task.id, "a");
        let token_b = token_of_node(&fx.orch, task.id, "b");
        let orch = fx.orch.clone();
        let orch_b = fx.orch.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let barrier_a = barrier.clone();
        let h_a = std::thread::spawn(move || {
            barrier_a.wait();
            orch.settle_by_token(&token_a, Settlement::complete("a ok"))
                .unwrap();
        });
        let h_b = std::thread::spawn(move || {
            barrier.wait();
            orch_b
                .settle_by_token(&token_b, Settlement::complete("b ok"))
                .unwrap();
        });
        h_a.join().unwrap();
        h_b.join().unwrap();

        assert_eq!(
            fx.directory.merges.load(Ordering::SeqCst),
            1,
            "同一 join 批并发 complete 只允许一次 merge(第 {round} 轮,实际 {})",
            fx.directory.merges.load(Ordering::SeqCst)
        );
        assert_eq!(
            fx.directory.merge_batches.lock().as_slice(),
            &[2],
            "唯一的批必须恰好是 [A,B](第 {round} 轮)"
        );
        let released = fx.directory.released.lock().clone();
        assert_eq!(
            released.len(),
            2,
            "两个租约各恰好释放一次(第 {round} 轮,实际 {released:?})"
        );
        let rows = fx.orch.store.list_execution_leases(task.id).unwrap();
        assert!(
            rows.iter().all(|r| r.status == "released"),
            "数据库租约全部 released(第 {round} 轮,实际 {rows:?})"
        );
        let deferrals = fx.orch.store.list_join_deferrals(Some(task.id)).unwrap();
        assert!(
            deferrals.is_empty(),
            "批汇合后 join 暂缓行必须清除(第 {round} 轮)"
        );
    }
}

/// 持久批状态:并发结算后恰好一行 merged 批;崩溃窗口中的 merging 行
/// 重启后被重置为 ready(可重冲)。
#[test]
fn merge_batch_state_is_persisted_and_recovered_on_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "join-batch-state",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
            node("j", &["a", "b"], "汇合", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("批状态", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 2));
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "a"),
            Settlement::complete("a"),
        )
        .unwrap();
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "b"),
            Settlement::complete("b"),
        )
        .unwrap();
    let batches = fx.orch.store.list_merge_batches(task.id).unwrap();
    assert_eq!(batches.len(), 1, "join 批持久化恰好一行: {batches:?}");
    assert_eq!(batches[0].join_step_key, "j");
    assert_eq!(batches[0].status, "merged");
    assert_eq!(batches[0].lease_keys.len(), 2);

    // 模拟崩溃窗口:手动把批置回 merging,重启恢复必须重置为 ready
    fx.orch
        .store
        .force_merge_batch_merging(task.id, "j", batches[0].revision_id)
        .unwrap();
    let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
    // 重启路径(restore)触发重置
    let orch2 = Orchestrator::start_with(
        store,
        tmp.path().to_path_buf(),
        mf_agent::config::Config::default(),
        fx.host.clone(),
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        fx.directory.clone(),
        WorkflowKernel {
            catalog: fx.catalog.clone(),
            pins: Some(fx.pins.clone()),
        },
    )
    .unwrap();
    let batches = orch2.store.list_merge_batches(task.id).unwrap();
    assert!(
        batches.iter().all(|b| b.status != "merging"),
        "重启后 merging 批必须被重置(实际 {batches:?})"
    );
}
