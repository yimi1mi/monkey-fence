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
            instance_resolver: None,
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
                c.execute(
                    "DELETE FROM pending_merges WHERE task_id = ?1",
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
            .list_pending_merges(Some(task.id))
            .map(|rows| rows.len() >= 2)
            .unwrap_or(false)),
        "恢复必须重建待决汇合投影(pending 行)"
    );
    let rows = fx2.orch.store.list_pending_merges(Some(task.id)).unwrap();
    assert!(
        rows.iter()
            .any(|r| r.conflicts.iter().any(|c| c.contains("conflict.rs"))),
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
    assert!(wait_until(Duration::from_secs(5), || !fx
        .host
        .workflow
        .lock()
        .is_empty()));
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

/// 新实例启动不能把另一个仍存活实例正在处理的 `merging` 批抢回
/// `ready`。否则两个实例会分别执行同一批的 provider merge / release。
#[test]
fn startup_does_not_reclaim_a_fresh_merging_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "fresh-merging-owner",
        vec![node("solo", &[], "独立步骤", &fx.instance_id)],
    );
    let task = fx.orch.create_task("活跃 merging 不得被抢", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || !fx
        .host
        .workflow
        .lock()
        .is_empty()));
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "solo"),
            Settlement::complete("done"),
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || fx
        .orch
        .store
        .list_merge_batches(task.id)
        .map(|rows| {
            rows.iter()
                .any(|b| matches!(b.status.as_str(), "merged" | "released"))
        })
        .unwrap_or(false)));
    let batch = fx.orch.store.list_merge_batches(task.id).unwrap().remove(0);
    fx.orch
        .store
        .force_merge_batch_active(task.id, &batch.join_step_key, batch.revision_id)
        .unwrap();

    let fx2 = restart(tmp.path());
    std::thread::sleep(Duration::from_millis(300));
    let batches = fx2.orch.store.list_merge_batches(task.id).unwrap();
    assert_eq!(
        batches[0].status, "merging",
        "新实例不得重置仍在有效 owner 租期内的批:{batches:?}"
    );
    fx.orch.stop();
    fx2.orch.stop();
}

/// provider 已应用后，如果 `merging -> merged` 的持久化提交失败，
/// 后处理必须停止：租约仍 held，不能先释放真实目录再留下 merging 行。
#[test]
fn merge_conclusion_persist_failure_does_not_release_lease() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "persist-before-release",
        vec![node("solo", &[], "独立步骤", &fx.instance_id)],
    );
    let task = fx.orch.create_task("结论持久化失败", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || !fx
        .host
        .workflow
        .lock()
        .is_empty()));
    fx.orch
        .store
        .with_conn(|c| {
            c.execute_batch(
                "CREATE TRIGGER reject_merge_conclusion
                 BEFORE UPDATE ON merge_batches
                 WHEN NEW.status IN ('merged', 'needs_user')
                 BEGIN SELECT RAISE(ABORT, 'injected conclusion failure'); END;",
            )?;
            Ok(())
        })
        .unwrap();

    let error = fx
        .orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "solo"),
            Settlement::complete("done"),
        )
        .expect_err("merge 结论无法持久化时必须把 post-commit 错误返回调用方");
    assert!(
        matches!(error, mf_agent::SettleError::Db(_)),
        "外部 action 错误应保留为可重试的结算错误:{error:?}"
    );

    let leases = fx.orch.store.list_execution_leases(task.id).unwrap();
    assert!(
        leases.iter().all(|row| row.status == "held"),
        "结论未提交时不得释放租约:{leases:?}"
    );
    assert!(
        fx.directory.released.lock().is_empty(),
        "结论未提交时不得调用 provider.release"
    );
    assert!(
        fx.orch
            .store
            .list_pending_merges(Some(task.id))
            .unwrap()
            .is_empty(),
        "needs_user 结论未提交时也不得提前写投影"
    );
    fx.orch.stop();
}

/// 即使调度 tick 已停止，领取批的外部 provider 调用也必须由独立 guard
/// 续租；第二实例不得在慢 merge 期间回收并重复执行。
#[test]
fn blocked_merge_renews_owner_without_scheduler_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "owner-heartbeat-guard",
        vec![node("solo", &[], "独立步骤", &fx.instance_id)],
    );
    let task = fx.orch.create_task("慢 merge owner", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || !fx
        .host
        .workflow
        .lock()
        .is_empty()));
    let token = token_of_node(&fx.orch, task.id, "solo");
    fx.directory.block_merge.store(true, Ordering::SeqCst);
    fx.orch.stop(); // 关闭 tick，外部调用不能依赖调度线程续租
    let orch1 = fx.orch.clone();
    let settle = std::thread::spawn(move || {
        orch1
            .settle_by_token(&token, Settlement::complete("done"))
            .unwrap();
    });
    assert!(wait_until(Duration::from_secs(5), || fx
        .directory
        .merge_entered
        .load(Ordering::SeqCst)));
    fx.orch
        .store
        .with_conn(|c| {
            c.execute(
                "UPDATE merge_batches SET owner_expires_at = '1970-01-01T00:00:00+00:00'
                 WHERE task_id = ?1 AND status = 'merging'",
                rusqlite::params![task.id],
            )?;
            Ok(())
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(700));

    let (tx, rx) = std::sync::mpsc::channel();
    let db = tmp.path().join("workflow-v1.db");
    let root = tmp.path().to_path_buf();
    let catalog = fx.catalog.clone();
    let pins = fx.pins.clone();
    let directory = fx.directory.clone();
    let host = fx.host.clone();
    let starter = std::thread::spawn(move || {
        let orch = Orchestrator::start_with_routing(
            Store::open(&db).unwrap(),
            root,
            mf_agent::config::Config::default(),
            host,
            empty_profiles(),
            GlobalLimiter::new(4),
            "pipe-2".into(),
            directory,
            WorkflowKernel {
                catalog,
                pins: Some(pins),
                instance_resolver: None,
            },
            scripted_routing(),
        )
        .unwrap();
        let _ = tx.send(orch);
    });
    let started_quickly = rx.recv_timeout(Duration::from_millis(800)).ok();
    let was_quick = started_quickly.is_some();
    fx.directory.block_merge.store(false, Ordering::SeqCst);
    settle.join().unwrap();
    let orch2 = match started_quickly {
        Some(orch) => orch,
        None => rx.recv_timeout(Duration::from_secs(5)).unwrap(),
    };
    starter.join().unwrap();
    assert!(
        was_quick,
        "第二实例启动被重复 merge 阻塞，说明活跃 owner 已被错误回收"
    );
    assert_eq!(
        fx.directory.merges.load(Ordering::SeqCst),
        1,
        "慢 merge 必须只执行一次"
    );
    orch2.stop();
}

/// 同一任务可因并行在途步骤形成多个独立批。`merged` 只补 release，
/// `needs_user` 必须逐批重新 merge，绝不能把 task 全部 pending 合成一批。
#[test]
fn resolve_groups_mixed_batch_states_without_remerging_merged_leases() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "mixed-resolution-batches",
        vec![
            node("a", &[], "A", &fx.instance_id),
            node("b", &[], "B", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("混合批状态", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 2));
    fx.directory.release_fails.store(true, Ordering::SeqCst);
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "a"),
            Settlement::complete("a"),
        )
        .unwrap();
    assert!(fx
        .orch
        .store
        .list_merge_batches(task.id)
        .unwrap()
        .iter()
        .any(|batch| batch.join_step_key == "__single__:a" && batch.status == "merged"));
    fx.directory.release_fails.store(false, Ordering::SeqCst);
    fx.directory.merge_ok.store(false, Ordering::SeqCst);
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "b"),
            Settlement::complete("b"),
        )
        .unwrap();
    assert_eq!(
        fx.orch
            .store
            .list_pending_merges(Some(task.id))
            .unwrap()
            .len(),
        2
    );
    fx.directory.merge_ok.store(true, Ordering::SeqCst);
    let remaining = fx.orch.resolve_pending_merges(task.id).unwrap();
    assert!(remaining.is_empty(), "{remaining:?}");
    let sizes = fx.directory.merge_batches.lock().clone();
    assert_eq!(
        sizes,
        vec![1, 1, 1],
        "前两次是各自冲突；resolve 只能重合并 needs_user 的 b，不能把已 merged 的 a 合入:{sizes:?}"
    );
    assert!(fx
        .orch
        .store
        .list_pending_merges(Some(task.id))
        .unwrap()
        .is_empty());
    fx.orch.stop();
}

#[test]
fn join_authority_uses_step_id_and_never_silently_drops_corrupt_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "corrupt-parent-metadata",
        vec![
            node("a", &[], "A", &fx.instance_id),
            node("b", &[], "B", &fx.instance_id),
            node("j", &["a", "b"], "J", &fx.instance_id),
        ],
    );
    let task = fx
        .orch
        .create_task("损坏 metadata 的完整父集", "g")
        .unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 2));
    let a_step = fx
        .orch
        .store
        .task_steps(task.id)
        .unwrap()
        .into_iter()
        .find(|step| step.step_key == "a")
        .unwrap();
    fx.orch
        .store
        .with_conn(|c| {
            c.execute(
                "UPDATE execution_leases SET metadata_json = '{broken'
                 WHERE task_id = ?1 AND step_id = ?2",
                rusqlite::params![task.id, a_step.id],
            )?;
            Ok(())
        })
        .unwrap();
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
    assert!(
        batches
            .iter()
            .any(|batch| batch.join_step_key == "j" && batch.lease_keys.len() == 2),
        "损坏 metadata 不能让权威父租约集静默缩水:{batches:?}"
    );
    assert_eq!(
        fx.directory.merges.load(Ordering::SeqCst),
        0,
        "pin metadata 损坏时应整批 NeedsYou，不能执行半批 merge"
    );
    fx.orch.stop();
}
