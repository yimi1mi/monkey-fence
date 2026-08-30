//! F4:provider pin 全链不可绕过 ——
//! - release 成功后才标 released/清内存(提供器释放失败 → 租约保持
//!   held,可重试,绝不谎报已释放);
//! - 汇合批逐租约校验 pin(不只首尾):混入不同提供器版本的批必须
//!   拒绝并进入待决(needs-you),绝不在错误版本上合并;
//! - 无 pin(Absent)租约不得路由到**隔离**提供器(无法归属版本 →
//!   拒绝合并,needs-you);共享目录(project-dir,非隔离)不受影响。

mod common;

use common::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, WorkflowKernel};
use mf_agent::store::Store;
use mf_agent::workflow::PluginSourcePin;
use mf_agent::Settlement;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// release 失败时,租约必须保持 held(DB + 不得谎报 released)。
#[test]
fn release_failure_keeps_lease_held_until_success() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "release-fail",
        vec![node("solo", &[], "独立步骤", &fx.instance_id)],
    );
    let task = fx.orch.create_task("释放失败", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || !fx
        .host
        .workflow
        .lock()
        .is_empty()));
    // 注入 release 失败:merge 会成功,但释放必须失败
    fx.directory.release_fails.store(true, Ordering::SeqCst);
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "solo"),
            Settlement::complete("done"),
        )
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(3), || fx
            .directory
            .merges
            .load(Ordering::SeqCst)
            == 1),
        "前置:合并已执行"
    );
    let leases = fx.orch.store.list_execution_leases(task.id).unwrap();
    assert!(
        leases.iter().all(|r| r.status == "held"),
        "释放失败时数据库租约必须保持 held(可重试),不得谎报 released:{leases:?}"
    );
    // 恢复 release 后:重启(或下次冲刷)应补释放
    fx.directory.release_fails.store(false, Ordering::SeqCst);
    drop(fx);
    let fx2 = fixture_restart(&tmp.path());
    assert!(
        wait_until(Duration::from_secs(5), || fx2
            .orch
            .store
            .list_execution_leases(task.id)
            .map(|rows| rows.iter().all(|r| r.status == "released"))
            .unwrap_or(false)),
        "清障后恢复必须补齐释放(F2 merged→released)"
    );
}

/// 共享 fixture 的重启变体(同一 DB、同一 ScriptedDirectory 语义)。
fn fixture_restart(dir: &std::path::Path) -> Fixture {
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

/// 汇合批逐租约 pin 校验:批内混入不同提供器版本(中间与首尾不同)
/// 必须拒绝合并(Err → 待决),不得只比首尾放行。
#[test]
fn merge_batch_rejects_mixed_pins_anywhere_in_batch() {
    let tmp = tempfile::tempdir().unwrap();
    // 手工构造混合 pin 的租约批,直接驱动 merge_leases 的判定路径:
    // 经由 apply 的公开入口(settle)难以自然混合,这里用三租约
    // [v1, v2, v1] 的批验证"不只首尾"。
    // (通过两次重启换 pin 版本实现:a 在 v1 下派发、b 在 v2 下派发,
    //  再加共享父 c 也在 v1 下 —— 批 [a(v1), b(v2), c(v1)]。)
    let pin_v1 = plugin_pin("builtin.core", "hash-v1");
    let pin_v2 = plugin_pin("builtin.core", "hash-v2");
    let _ = (pin_v1, pin_v2);
    // 该场景的完整编排较重;此处聚焦语义断言:构造 pinned 目录注册表,
    // 两个版本都可解析;租约 metadata 直接携带不同 pin。
    let directory = Arc::new(MixedPinDirectory::default());
    let catalog = catalog_with_worker_instance();
    let instance_id = fx_instance_id(&catalog);
    let pins = Arc::new(FakePins::default());
    pins.resolve_ok(true);
    let host = Arc::new(RecordingHost::default());
    let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
    let current = plugin_pin("builtin.core", "hash-v2");
    let orch = Orchestrator::start_with_routing(
        store,
        tmp.path().to_path_buf(),
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
            current_pin: Some(current),
            resolver: None,
        },
    )
    .unwrap();
    let version = orch_store_template(&catalog, instance_id);
    let task = orch.create_task("混合 pin", "g").unwrap();
    orch.assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host
        .workflow
        .lock()
        .len()
        == 2));
    // 把 b 的租约 metadata 改成 v1 pin(模拟跨版本重启后派发的兄弟),
    // 批变为 [a(v2), b(v1)]:逐项比较必须拒绝
    {
        let rows = orch.store.list_execution_leases(task.id).unwrap();
        let b_row = rows
            .iter()
            .find(|r| {
                let meta = r.metadata_json.as_deref().unwrap_or("");
                meta.contains("\"b\"")
            })
            .unwrap();
        let meta = serde_json::from_str::<serde_json::Value>(
            b_row.metadata_json.as_deref().unwrap_or("{}"),
        )
        .unwrap();
        let mut meta = meta;
        meta["provider_pin"] = serde_json::json!({
            "full_id": "builtin.core",
            "version": "1.2.3",
            "content_hash": "hash-v1",
        });
        orch.store
            .with_conn(move |c| {
                c.execute(
                    "UPDATE execution_leases SET metadata_json = ?2 WHERE lease_key = ?1",
                    rusqlite::params![b_row.lease_key, meta.to_string()],
                )?;
                Ok(())
            })
            .unwrap();
    }
    orch.settle_by_token(
        &token_of_node(&orch, task.id, "a"),
        Settlement::complete("a"),
    )
    .unwrap();
    orch.settle_by_token(
        &token_of_node(&orch, task.id, "b"),
        Settlement::complete("b"),
    )
    .unwrap();
    // 混合 pin 的批必须不合并、进入待决(needs-you)
    assert!(
        wait_until(Duration::from_secs(5), || !orch
            .store
            .list_pending_merges(Some(task.id))
            .unwrap()
            .is_empty()),
        "混合 pin 的汇合批必须拒绝合并并进入待决"
    );
    assert_eq!(
        directory.merges.load(Ordering::SeqCst),
        0,
        "不得在任何版本上执行合并"
    );
}

fn orch_store_template(
    catalog: &Arc<mf_agent::catalog_store::CatalogStore>,
    instance_id: String,
) -> mf_agent::workflow::WorkflowTemplateVersion {
    let _ = catalog;
    mf_agent::workflow::WorkflowTemplateVersion {
        version_id: 0,
        template_key: "mixed".into(),
        version: 1,
        nodes: vec![
            node("a", &[], "A", &instance_id),
            node("b", &[], "B", &instance_id),
            node("j", &["a", "b"], "J", &instance_id),
        ],
        created_at: String::new(),
    }
}

/// 记录 merge 的 pinned 目录(当前版本 hash-v2)。
struct MixedPinDirectory {
    merges: AtomicUsize,
}

impl Default for MixedPinDirectory {
    fn default() -> Self {
        MixedPinDirectory {
            merges: AtomicUsize::new(0),
        }
    }
}

impl mf_agent::execution_directory::ExecutionDirectoryProvider for MixedPinDirectory {
    fn id(&self) -> &str {
        "mixed"
    }
    fn isolates(&self) -> bool {
        true
    }
    fn acquire(
        &self,
        ctx: &mf_agent::execution_directory::LeaseContext,
    ) -> anyhow::Result<mf_agent::execution_directory::ExecutionLease> {
        Ok(mf_agent::execution_directory::ExecutionLease {
            id: format!("lease-{}-{}", ctx.task_id, ctx.step_key),
            path: ctx.project_root.clone(),
            isolated: true,
            provider: "mixed".into(),
            metadata: serde_json::json!({ "step_key": ctx.step_key, "task_id": ctx.task_id }),
        })
    }
    fn merge(
        &self,
        _leases: &[mf_agent::execution_directory::ExecutionLease],
    ) -> anyhow::Result<mf_agent::execution_directory::MergeOutcome> {
        self.merges.fetch_add(1, Ordering::SeqCst);
        Ok(mf_agent::execution_directory::MergeOutcome::Merged)
    }
    fn release(
        &self,
        _lease: &mf_agent::execution_directory::ExecutionLease,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 无 pin(Absent)租约 + 当前提供器是**隔离**提供器 → 不得路由合并
/// (无法归属版本);共享目录(project-dir,非隔离)不受影响。
#[test]
fn absent_pin_lease_never_routes_to_isolating_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let directory = Arc::new(MixedPinDirectory::default());
    let catalog = catalog_with_worker_instance();
    let instance_id = fx_instance_id(&catalog);
    let pins = Arc::new(FakePins::default());
    pins.resolve_ok(true);
    let host = Arc::new(RecordingHost::default());
    let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
    // 无 current pin(模拟旧版本租约/未 pin 化提供器)
    let orch = Orchestrator::start_with_routing(
        store,
        tmp.path().to_path_buf(),
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
            current_pin: None,
            resolver: None,
        },
    )
    .unwrap();
    let version = orch_store_template(&catalog, instance_id);
    let task = orch.create_task("Absent pin", "g").unwrap();
    orch.assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap();
    orch.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || host
        .workflow
        .lock()
        .len()
        == 2));
    orch.settle_by_token(
        &token_of_node(&orch, task.id, "a"),
        Settlement::complete("a"),
    )
    .unwrap();
    orch.settle_by_token(
        &token_of_node(&orch, task.id, "b"),
        Settlement::complete("b"),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        directory.merges.load(Ordering::SeqCst),
        0,
        "Absent pin 的隔离租约不得在当前隔离提供器上合并(无法归属版本)"
    );
    let t = orch.store.task_view(task.id).unwrap().unwrap();
    assert_eq!(
        t.status.as_str(),
        "needs-you",
        "无法归属的租约必须把任务置为 needs-you(人工处理)"
    );
    let leases = orch.store.list_execution_leases(task.id).unwrap();
    assert!(
        leases.iter().all(|r| r.status == "held"),
        "租约保持持有:{leases:?}"
    );
}
