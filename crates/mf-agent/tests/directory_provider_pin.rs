//! 目录提供器 pin(I7):Revision 冻结提供器身份、Lease 携带 pin、
//! 派发强校验一致(插件升级/换提供器后旧 Revision 不静默换用)。

mod common;

use common::*;
use mf_agent::model::TaskStatus;
use mf_agent::workflow::PluginSourcePin;
use mf_agent::Settlement;
use std::sync::Arc;
use std::time::Duration;

fn pin(v: &str) -> PluginSourcePin {
    PluginSourcePin {
        full_id: format!("vendor.dirs"),
        version: v.into(),
        content_hash: format!("hash-{v}"),
        contribution_id: "scripted".into(),
    }
}

#[test]
fn directory_provider_pin_frozen_in_revision_and_enforced_at_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "pin",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &["a"], "做 B", &fx.instance_id),
        ],
    );

    // pin v1 分配:快照冻结 v1;派发通过,租约元数据携带 pin
    fx.orch.set_directory_provider_pin(Some(pin("1.0")));
    let task1 = fx.orch.create_task("pin v1", "g").unwrap();
    fx.assign_and_run(task1.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .iter()
        .any(|(s, _)| s.node_key == "a")));
    let leases = fx.orch.store.list_execution_leases(task1.id).unwrap();
    assert!(
        leases.iter().any(|l| {
            let meta: serde_json::Value =
                serde_json::from_str(l.metadata_json.as_deref().unwrap_or("{}")).unwrap();
            meta["provider_pin"]["version"] == "1.0"
        }),
        "租约元数据必须携带提供器 pin: {leases:?}"
    );
    // 任务 1 收尾(a → b 都结算)
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task1.id, "a"),
            Settlement::complete("A 完成"),
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .iter()
        .any(|(s, _)| s.task_id == task1.id && s.node_key == "b")));
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task1.id, "b"),
            Settlement::complete("B 完成"),
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || fx
        .orch
        .store
        .task_view(task1.id)
        .unwrap()
        .unwrap()
        .status
        == TaskStatus::Succeeded));

    // pin v2 分配新任务:正常派发(冻结的是各自的 pin)
    fx.orch.set_directory_provider_pin(Some(pin("2.0")));
    let task2 = fx.orch.create_task("pin v2", "g").unwrap();
    fx.assign_and_run(task2.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .iter()
        .any(|(s, _)| s.task_id == task2.id && s.node_key == "a")));
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task2.id, "a"),
            Settlement::complete("A 完成"),
        )
        .unwrap();

    // 关键回归:分配后、确认前更换提供器 pin → 派发必须拒绝
    // (不得静默用新提供器跑旧 Revision)
    fx.orch.set_directory_provider_pin(Some(pin("2.0")));
    let task3 = fx.orch.create_task("pin 漂移", "g").unwrap();
    fx.orch
        .assign_workflow(task3.id, &version, &plugin_index(), false)
        .unwrap();
    fx.orch.set_directory_provider_pin(Some(pin("3.0"))); // 模拟插件升级换提供器
    fx.orch.confirm_and_run(task3.id).unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        !fx.host
            .workflow
            .lock()
            .iter()
            .any(|(s, _)| s.task_id == task3.id),
        "提供器 pin 与 Revision 冻结时不一致:不得派发(需重新分配)"
    );
    // 失败以错误事件如实上报
    let mut saw_pin_error = false;
    while let Ok(ev) = fx.orch.events_rx.try_recv() {
        let text = match &ev {
            mf_agent::SchedulerEvent::Error(text) => Some(text.clone()),
            mf_agent::SchedulerEvent::Log { text, .. } => Some(text.clone()),
            _ => None,
        };
        if text.is_some_and(|t| t.contains("目录提供器 pin")) {
            saw_pin_error = true;
        }
    }
    assert!(saw_pin_error, "pin 不一致必须以错误事件上报");
    fx.orch.stop();
}

/// 无 pin(内核共享目录)→ 快照为 None,与 None 当前 pin 一致照常运行;
/// None ↔ Some 漂移同样拒绝。
#[test]
fn directory_provider_pin_drift_between_none_and_some_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template("np", vec![node("a", &[], "做 A", &fx.instance_id)]);

    fx.orch.set_directory_provider_pin(None); // 分配时无 pin
    let task = fx.orch.create_task("none pin", "g").unwrap();
    fx.orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap();
    fx.orch.set_directory_provider_pin(Some(pin("9.9"))); // 漂移到 Some
    fx.orch.confirm_and_run(task.id).unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        !fx.host
            .workflow
            .lock()
            .iter()
            .any(|(s, _)| s.task_id == task.id),
        "None → Some 漂移必须拒绝派发"
    );
    fx.orch.stop();
}

#[allow(dead_code)]
fn _keep(_: Arc<()>) {}

/// I11:目录提供器 pin 落库失败 → assign 必须失败并回滚
///(不留无 pin 保护的 draft Revision;节点 pin 不残留)。
#[test]
fn directory_pin_persist_failure_fails_assign_and_rolls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    fx.orch.set_directory_provider_pin(Some(pin("9.9")));
    let version = fx.template("pin-fail", vec![node("a", &[], "做 A", &fx.instance_id)]);
    // 注入:目录提供器 pin 持久化失败
    fx.pins.fail_on("vendor.dirs");
    let task = fx.orch.create_task("pin 落库失败", "g").unwrap();
    let err = fx
        .orch
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .err()
        .expect("目录提供器 pin 落库失败必须让 assign 失败");
    assert!(
        format!("{err:#}").contains("目录提供器"),
        "错误必须明示目录提供器 pin 失败: {err:#}"
    );
    assert!(
        fx.orch.store.active_revision(task.id).unwrap().is_none(),
        "失败后不得有激活 Revision"
    );
    let revisions = fx.orch.store.list_revision_ids(task.id).unwrap();
    assert!(
        revisions.is_empty(),
        "失败必须回滚 draft Revision(实际 {revisions:?})"
    );
    assert!(
        fx.pins.pinned.lock().is_empty(),
        "失败后不得残留节点 pin: {:?}",
        fx.pins.pinned.lock()
    );
}
