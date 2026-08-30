//! F2:merge 后处理的可恢复持久阶段(provider_applied → released /
//! needs_user 投影)与非 join 单租约的启动冲刷。
//!
//! 三个崩溃窗口的启动恢复:
//! 1. **merged 未 release**:提供器已合并、租约未释放 —— 恢复必须只
//!    释放(绝不再 merge),批推进 released;
//! 2. **needs_user 未投影**:提供器已判定冲突、pending 行未写 ——
//!    恢复必须按批行持久化的冲突重建投影(任务 → needs-you);
//! 3. **非 join 单租约 settlement 后未 merge**:恢复必须补齐单租约
//!    汇合(merge 一次)并释放。

mod common;

use common::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, WorkflowKernel};
use mf_agent::store::Store;
use mf_agent::Settlement;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn nodes(fx: &Fixture) -> mf_agent::workflow::WorkflowTemplateVersion {
    fx.template(
        "merge-phases",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
            node("j", &["a", "b"], "汇合", &fx.instance_id),
        ],
    )
}

fn restart(dir: &std::path::Path) -> Fixture {
    let catalog = catalog_with_worker_instance();
    let pins = Arc::new(FakePins::default());
    pins.resolve_ok(true);
    let directory = Arc::new(ScriptedDirectory::new(dir));
    let host = Arc::new(RecordingHost::default());
    let store = Store::open(&dir.join("workflow-v1.db")).unwrap();
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
        },
        mf_agent::orchestrator::DirectoryRouting {
            current_pin: Some(plugin_pin("scripted", "hash-scripted")),
            resolver: None,
        },
    )
    .unwrap();
    let instance_id = fx_instance_id(&catalog);
    Fixture {
        catalog,
        pins,
        directory,
        host,
        orch,
        instance_id,
    }
}

/// 场景 1:merged(provider 已合并)但租约未释放 —— 启动恢复只补
/// 释放,不重复 merge,批推进 released。
#[test]
fn startup_recovers_merged_but_unreleased_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let task = fx.orch.create_task("merged 未 release", "g").unwrap();
    fx.assign_and_run(task.id, &nodes(&fx));
    assert!(wait_until(Duration::from_secs(5), || fx.host.workflow.lock().len() == 2));
    fx.orch
        .settle_by_token(&token_of_node(&fx.orch, task.id, "a"), Settlement::complete("a"))
        .unwrap();
    fx.orch
        .settle_by_token(&token_of_node(&fx.orch, task.id, "b"), Settlement::complete("b"))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || fx
            .orch
            .store
            .list_execution_leases(task.id)
            .map(|rows| rows.iter().all(|r| r.status == "released"))
            .unwrap_or(false)),
        "前置:正常路径完成合并与释放"
    );
    let merges_before = fx.directory.merges.load(Ordering::SeqCst);
    assert_eq!(merges_before, 1);
    drop(fx);

    // 模拟崩溃窗口:批停在 merged(provider 已应用),租约仍 held
    {
        let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "UPDATE execution_leases SET status = 'held', released_at = NULL WHERE task_id = ?1",
                    rusqlite::params![task.id],
                )?;
                c.execute(
                    "UPDATE merge_batches SET status = 'merged' WHERE task_id = ?1",
                    rusqlite::params![task.id],
                )?;
                Ok(())
            })
            .unwrap();
    }

    let fx2 = restart(tmp.path());
    assert!(
        wait_until(Duration::from_secs(5), || fx2
            .orch
            .store
            .list_execution_leases(task.id)
            .map(|rows| rows.iter().all(|r| r.status == "released"))
            .unwrap_or(false)),
        "恢复必须释放 merged 批的滞留租约"
    );
    assert_eq!(
        fx2.directory.merges.load(Ordering::SeqCst),
        0,
        "恢复绝不得再次 merge(provider 已应用;新实例计数为 0)"
    );
    assert_eq!(
        fx2.directory.released.lock().len(),
        2,
        "恢复只补释放两个租约"
    );
    let batches = fx2.orch.store.list_merge_batches(task.id).unwrap();
    assert!(
        batches
            .iter()
            .any(|b| b.status == "released" && b.lease_keys.len() == 2),
        "批必须推进到 released:{batches:?}"
    );
}

/// 场景 2:needs_user 已判定但投影(pending 行/needs-you)未写 ——
/// 启动恢复按批行持久化的冲突重建投影。
#[test]
fn startup_projects_unprojected_needs_user_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    fx.directory.merge_ok.store(false, Ordering::SeqCst);
    let task = fx.orch.create_task("needs_user 未投影", "g").unwrap();
    fx.assign_and_run(task.id, &nodes(&fx));
    assert!(wait_until(Duration::from_secs(5), || fx.host.workflow.lock().len() == 2));
    fx.orch
        .settle_by_token(&token_of_node(&fx.orch, task.id, "a"), Settlement::complete("a"))
        .unwrap();
    fx.orch
        .settle_by_token(&token_of_node(&fx.orch, task.id, "b"), Settlement::complete("b"))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || !fx
            .orch
            .store
            .list_pending_merges(Some(task.id))
            .unwrap()
            .is_empty()),
        "前置:冲突已投影为待决汇合"
    );
    drop(fx);

    // 模拟崩溃窗口:批已 needs_user,但投影(pending 行)丢失
    {
        let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
        store
            .with_conn(|c| {
                c.execute("DELETE FROM pending_merges WHERE task_id = ?1", rusqlite::params![task.id])?;
                Ok(())
            })
            .unwrap();
    }

    let fx2 = restart(tmp.path());
    assert!(
        wait_until(Duration::from_secs(5), || fx2
            .orch
            .store
            .list_pending_merges(Some(task.id))
            .map(|rows| rows.len() >= 2)
            .unwrap_or(false)),
        "恢复必须重建待决汇合投影(pending 行)"
    );
    let rows = fx2.orch.store.list_pending_merges(Some(task.id)).unwrap();
    assert!(
        rows.iter().any(|r| r.conflicts.iter().any(|c| c.contains("conflict.rs"))),
        "投影的冲突列表必须来自批行持久化的 conflicts:{rows:?}"
    );
    let t = fx2.orch.store.task_view(task.id).unwrap().unwrap();
    assert_eq!(t.status.as_str(), "needs-you", "任务必须回到 needs-you");
    // 租约保持 held(等待用户解决)
    let leases = fx2.orch.store.list_execution_leases(task.id).unwrap();
    assert!(
        leases.iter().all(|r| r.status == "held"),
        "待决租约必须保持持有:{leases:?}"
    );
}

/// 场景 3:非 join 单租约,结算成功后、汇合前死亡 —— 启动恢复补齐
/// 单租约 merge 并释放。
#[test]
fn startup_flushes_settled_single_lease_without_join() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "single-unmerged",
        vec![node("solo", &[], "独立步骤", &fx.instance_id)],
    );
    let task = fx.orch.create_task("单租约未汇合", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || !fx.host.workflow.lock().is_empty()));
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "solo"),
            Settlement::complete("solo ok"),
        )
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || fx
            .orch
            .store
            .list_execution_leases(task.id)
            .map(|rows| rows.iter().all(|r| r.status == "released"))
            .unwrap_or(false)),
        "前置:正常路径单租约已汇合释放"
    );
    drop(fx);

    // 模拟崩溃窗口:run/step 已成功、批行未建立、租约仍 held
    {
        let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "UPDATE execution_leases SET status = 'held', released_at = NULL WHERE task_id = ?1",
                    rusqlite::params![task.id],
                )?;
                c.execute(
                    "DELETE FROM merge_batches WHERE task_id = ?1",
                    rusqlite::params![task.id],
                )?;
                Ok(())
            })
            .unwrap();
    }

    let fx2 = restart(tmp.path());
    assert!(
        wait_until(Duration::from_secs(5), || fx2
            .orch
            .store
            .list_execution_leases(task.id)
            .map(|rows| rows.iter().all(|r| r.status == "released"))
            .unwrap_or(false)),
        "恢复必须补齐单租约汇合并释放"
    );
    assert_eq!(
        fx2.directory.merges.load(Ordering::SeqCst),
        1,
        "恢复必须执行恰好一次补齐 merge(新实例计数)"
    );
    let batches = fx2.orch.store.list_merge_batches(task.id).unwrap();
    assert!(
        batches
            .iter()
            .any(|b| b.lease_keys.len() == 1 && b.status != "ready"),
        "单租约批必须持久化结论:{batches:?}"
    );
}
