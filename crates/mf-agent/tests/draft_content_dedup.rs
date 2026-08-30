//! 草稿内容身份去重(I8):同内容保存不刷新 updated_at,
//! 常规「分配→确认运行」不因 save 刷新时间戳而重复冻结 Revision/pin。

mod common;

use common::*;
use mf_agent::workflow::{WorkflowNodeDraft, WorkflowTemplateDraft};
use std::time::Duration;

fn draft(task_id: i64, marker: &str) -> WorkflowTemplateDraft {
    WorkflowTemplateDraft {
        key: format!("task-{task_id}"),
        name: "本地".into(),
        task_local: true,
        nodes: vec![WorkflowNodeDraft {
            key: "a".into(),
            title: format!("A {marker}"),
            instructions: "做 A".into(),
            agent_instance_id: "inst".into(),
            deps: vec![],
        }],
    }
}

#[test]
fn saving_unchanged_draft_does_not_refresh_updated_at() {
    let tmp = tempfile::tempdir().unwrap();
    let store = mf_agent::store::Store::open(&tmp.path().join("db.sqlite")).unwrap();
    let key = tmp.path().to_string_lossy().to_string();
    store
        .save_task_workflow(&key, 1, &draft(1, "v1"), false)
        .unwrap();
    let t0 = store.task_workflow_saved_at(&key, 1).unwrap().unwrap();

    // 时间戳为秒级:跨秒后保存相同内容 → updated_at 必须不变
    std::thread::sleep(Duration::from_millis(1100));
    store
        .save_task_workflow(&key, 1, &draft(1, "v1"), false)
        .unwrap();
    let t1 = store.task_workflow_saved_at(&key, 1).unwrap().unwrap();
    assert_eq!(t0, t1, "同内容保存不得刷新 updated_at(内容身份去重)");

    // 内容变化 → 时间戳推进
    std::thread::sleep(Duration::from_millis(1100));
    store
        .save_task_workflow(&key, 1, &draft(1, "v2"), false)
        .unwrap();
    let t2 = store.task_workflow_saved_at(&key, 1).unwrap().unwrap();
    assert_ne!(t1, t2, "内容变化必须刷新 updated_at");

    // 仅风险开关变化也算内容变化
    std::thread::sleep(Duration::from_millis(1100));
    store
        .save_task_workflow(&key, 1, &draft(1, "v2"), true)
        .unwrap();
    let t3 = store.task_workflow_saved_at(&key, 1).unwrap().unwrap();
    assert_ne!(t2, t3, "风险开关变化必须刷新 updated_at");
}

#[test]
fn confirm_after_noop_save_reuses_frozen_revision() {
    // UI 常规路径:task-local 草稿保存 → 确认运行;再保存同内容 →
    // 再次确认。草稿未变时不得因保存时间戳刷新而重复冻结 Revision
    //(重复 Revision/pin 回归)。
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let task = fx.orch.create_task("去重", "g").unwrap();
    let root = tmp.path().to_string_lossy().to_string();
    let saved = draft(task.id, "v1");
    // UI 先保存草稿(节点引用 fixture 实例)
    let saved = WorkflowTemplateDraft {
        nodes: vec![node("a", &[], "做 A", &fx.instance_id)],
        ..saved
    };
    fx.orch
        .store
        .save_task_workflow(&root, task.id, &saved, false)
        .unwrap();

    // 确认运行 #1:冻结 Revision 并派发
    fx.orch
        .assign_and_confirm_task_local(task.id, &plugin_index())
        .unwrap();
    assert_eq!(fx.orch.store.list_revision_ids(task.id).unwrap().len(), 1);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .iter()
        .any(|(s, _)| s.node_key == "a")));

    // UI 再次保存(同内容,幂等)→ 再次「分配并确认」
    fx.orch
        .store
        .save_task_workflow(&root, task.id, &saved, false)
        .unwrap();
    fx.orch
        .assign_and_confirm_task_local(task.id, &plugin_index())
        .unwrap();
    let revisions = fx.orch.store.list_revision_ids(task.id).unwrap();
    assert_eq!(
        revisions.len(),
        1,
        "同内容保存后确认运行必须复用已冻结 Revision(实际 {revisions:?})"
    );
    fx.orch.stop();
}

/// I12:内容身份判定不得依赖时间戳 —— 系统时钟回拨(saved_at 落在
/// 冻结之后/未来)时,同内容仍必须复用已冻结 Revision。
#[test]
fn clock_rollback_same_content_still_reuses_frozen_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let task = fx.orch.create_task("时钟回拨", "g").unwrap();
    let root = tmp.path().to_string_lossy().to_string();
    let saved = WorkflowTemplateDraft {
        key: format!("task-{}", task.id),
        name: "本地".into(),
        task_local: true,
        nodes: vec![node("a", &[], "做 A", &fx.instance_id)],
    };
    fx.orch
        .store
        .save_task_workflow(&root, task.id, &saved, false)
        .unwrap();
    fx.orch
        .assign_and_confirm_task_local(task.id, &plugin_index())
        .unwrap();
    assert_eq!(fx.orch.store.list_revision_ids(task.id).unwrap().len(), 1);

    // 模拟时钟回拨:草稿 updated_at 被写到未来(比 Revision created_at 晚)
    fx.orch
        .store
        .with_conn(|c| {
            c.execute(
                "UPDATE task_workflows SET updated_at = '2999-01-01T00:00:00+00:00'
                 WHERE project_key = ?1 AND task_id = ?2",
                rusqlite::params![root, task.id],
            )?;
            Ok(())
        })
        .unwrap();
    fx.orch
        .assign_and_confirm_task_local(task.id, &plugin_index())
        .unwrap();
    let revisions = fx.orch.store.list_revision_ids(task.id).unwrap();
    assert_eq!(
        revisions.len(),
        1,
        "同内容 + 时钟回拨必须按内容身份复用 Revision(实际 {revisions:?})"
    );
    fx.orch.stop();
}

/// I12:时间戳相同(甚至草稿 updated_at 早于冻结)但内容不同 →
/// 必须重新冻结,不得误复用旧 Revision。
#[test]
fn earlier_timestamp_different_content_must_refreeze() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let task = fx.orch.create_task("同时间不同内容", "g").unwrap();
    let root = tmp.path().to_string_lossy().to_string();
    let v1 = WorkflowTemplateDraft {
        key: format!("task-{}", task.id),
        name: "本地".into(),
        task_local: true,
        nodes: vec![node("a", &[], "做 A", &fx.instance_id)],
    };
    fx.orch
        .store
        .save_task_workflow(&root, task.id, &v1, false)
        .unwrap();
    fx.orch
        .assign_and_confirm_task_local(task.id, &plugin_index())
        .unwrap();
    assert_eq!(fx.orch.store.list_revision_ids(task.id).unwrap().len(), 1);

    // 内容改为双节点;updated_at 被压回冻结之前(时间戳会误判"未变化")
    let v2 = WorkflowTemplateDraft {
        nodes: vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
        ],
        ..v1
    };
    fx.orch
        .store
        .save_task_workflow(&root, task.id, &v2, false)
        .unwrap();
    fx.orch
        .store
        .with_conn(|c| {
            c.execute(
                "UPDATE task_workflows SET updated_at = '2000-01-01T00:00:00+00:00'
                 WHERE project_key = ?1 AND task_id = ?2",
                rusqlite::params![root, task.id],
            )?;
            Ok(())
        })
        .unwrap();
    fx.orch.store.set_task_paused(task.id, true).unwrap(); // 运行中需先暂停
    fx.orch
        .assign_and_confirm_task_local(task.id, &plugin_index())
        .unwrap();
    let revisions = fx.orch.store.list_revision_ids(task.id).unwrap();
    assert_eq!(
        revisions.len(),
        2,
        "不同内容必须重新冻结 Revision(时间戳不得参与判定;实际 {revisions:?})"
    );
    fx.orch.stop();
}
