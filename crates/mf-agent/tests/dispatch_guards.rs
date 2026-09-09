mod common;
#[path = "common/run_lifecycle.rs"]
mod lifecycle;
use common::*;
use mf_agent::{RetryMode, RunMutation, SchedulerEvent, Settlement, Store};
use std::time::Duration;

#[test]
fn input_storage_failure_never_launches_or_consumes_attempt() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    fx.orch.store.with_conn(|c|{c.execute_batch("CREATE TRIGGER fail_input BEFORE INSERT ON node_inputs BEGIN SELECT RAISE(ABORT,'input-storage-unavailable'); END;")?;Ok(())}).unwrap();
    let version = fx.template(
        "input-failure",
        vec![node("a", &[], "work", &fx.instance_id)],
    );
    let task = fx.orch.create_task("storage", "storage").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch.events_rx.try_iter().any(|event|matches!(event,SchedulerEvent::Error(error) if error.contains("input-storage-unavailable")))
    }));
    fx.orch.stop();
    assert!(fx.host.workflow.lock().is_empty());
    assert_eq!(fx.orch.store.task_steps(task.id).unwrap()[0].attempts, 0);
}

#[test]
fn manual_retry_does_not_resume_a_paused_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "paused-retry",
        vec![node("a", &[], "work", &fx.instance_id)],
    );
    let task = fx.orch.create_task("retry", "retry").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "a"),
            Settlement::Fail {
                reason: "retry".into(),
            },
        )
        .unwrap();
    fx.orch.pause_task(task.id).unwrap();
    let step = fx.orch.store.task_steps(task.id).unwrap()[0].clone();
    lifecycle::retry_step(&fx.orch, step.id, RetryMode::FreshSession).unwrap();
    assert!(fx.orch.store.task_view(task.id).unwrap().unwrap().paused);
    assert!(!wait_until(Duration::from_millis(700), || fx
        .host
        .workflow
        .lock()
        .len()
        > 1));
    fx.orch.resume_task(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 2));
    fx.orch.stop();
}

#[test]
fn automatic_retry_preserves_pause_when_another_branch_needs_attention() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "auto-paused",
        vec![
            node("a", &[], "a", &fx.instance_id),
            node("b", &[], "b", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("auto pause", "auto pause").unwrap();
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
            Settlement::Fail {
                reason: "a requires attention".into(),
            },
        )
        .unwrap();
    fx.orch.pause_task(task.id).unwrap();
    fx.orch
        .store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE steps SET auto_retry=1 WHERE task_id=?1 AND step_key='b'",
                [task.id],
            )?;
            Ok(())
        })
        .unwrap();
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "b"),
            Settlement::Fail {
                reason: "retry b".into(),
            },
        )
        .unwrap();
    assert!(fx.orch.store.task_view(task.id).unwrap().unwrap().paused);
    assert!(!wait_until(Duration::from_millis(700), || fx
        .host
        .workflow
        .lock()
        .len()
        > 2));
    fx.orch.stop();
}

#[test]
fn successor_of_independent_branch_starts_after_sibling_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "branches",
        vec![
            node("a", &[], "fail", &fx.instance_id),
            node("b", &[], "independent", &fx.instance_id),
            node("c", &["b"], "continue", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("branches", "branches").unwrap();
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
            Settlement::Fail {
                reason: "a failed".into(),
            },
        )
        .unwrap();
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "b"),
            Settlement::Complete {
                summary: "b done".into(),
                output: Default::default(),
            },
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .iter()
        .any(|(spec, _)| spec.node_key == "c")));
    fx.orch.stop();
}

#[test]
fn old_ready_step_cannot_start_after_active_revision_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let mut b = node("b", &["a"], "old", &fx.instance_id);
    b.require_input_review = true;
    let version = fx.template(
        "revision-race",
        vec![node("a", &[], "a", &fx.instance_id), b],
    );
    let task = fx.orch.create_task("revision", "revision").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    let session = fx.host.workflow.lock()[0].0.session_id;
    fx.orch.pause_task(task.id).unwrap();
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "a"),
            Settlement::Complete {
                summary: "a".into(),
                output: Default::default(),
            },
        )
        .unwrap();
    fx.orch.stop();
    let old = fx
        .orch
        .store
        .task_steps(task.id)
        .unwrap()
        .into_iter()
        .find(|step| step.step_key == "b")
        .unwrap();
    let mut snapshot = fx
        .orch
        .store
        .revision_snapshot(old.revision_id)
        .unwrap()
        .unwrap();
    snapshot
        .nodes
        .iter_mut()
        .find(|node| node.key == "b")
        .unwrap()
        .instructions = "new".into();
    fx.orch
        .store
        .with_tx(|tx| Store::create_patched_revision_tx(tx, task.id, &snapshot, "test-revision"))
        .unwrap();
    fx.orch
        .store
        .with_tx(|tx| Store::apply_run_mutation_tx(tx, RunMutation::Resume { task_id: task.id }))
        .unwrap();
    assert!(fx
        .orch
        .store
        .dispatch_run_consuming(task.id, old.id, old.revision_id, session, None)
        .is_err());
    assert_eq!(
        fx.orch.store.step_view(old.id).unwrap().unwrap().attempts,
        0
    );
}
