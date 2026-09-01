//! Issue #24: L-PUBLISH 与 Snapshot cursor 的线性化契约。

use crate::command::FaultPoint;
use crate::kernel::{CoreKernel, KernelProblem};
use crate::projection::{SnapshotData, SnapshotQuery};
use crate::projection_support::ProjectionFixture;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[test]
fn concurrent_commit_and_snapshot_never_expose_new_store_with_old_cursor() {
    let fixture = Arc::new(ProjectionFixture::new());
    let done = Arc::new(AtomicBool::new(false));
    let writer_fixture = fixture.clone();
    let writer_done = done.clone();
    let writer = std::thread::spawn(move || {
        for expected in 1..=40 {
            writer_fixture
                .rename(&format!("name-{expected}"), expected)
                .unwrap();
        }
        writer_done.store(true, Ordering::Release);
    });

    while !done.load(Ordering::Acquire) {
        let snapshot = fixture
            .kernel
            .snapshot(SnapshotQuery::Workflow {
                project: fixture.project.clone(),
                workflow: fixture.workflow.clone(),
            })
            .unwrap();
        let SnapshotData::Workflow(data) = snapshot.data;
        assert_eq!(
            snapshot.cursor.through_seq,
            data.revisions.presentation_revision - 1,
            "Store revision 与 cursor 必须来自同一 publication barrier"
        );
    }
    writer.join().unwrap();
    let snapshot = fixture
        .kernel
        .snapshot(SnapshotQuery::Workflow {
            project: fixture.project.clone(),
            workflow: fixture.workflow.clone(),
        })
        .unwrap();
    let SnapshotData::Workflow(data) = snapshot.data;
    assert_eq!(data.revisions.presentation_revision, 41);
    assert_eq!(snapshot.cursor.through_seq, 40);
}

#[test]
fn target_commit_then_failure_rotates_epoch_before_snapshot_can_observe_commit() {
    let fixture = ProjectionFixture::new();
    let old_cursor = fixture.kernel.current_event_cursor();
    let mut old_subscription = fixture.kernel.subscribe_events(old_cursor.clone()).unwrap();

    let error = fixture
        .rename_with_fault("committed-before-crash", 1, FaultPoint::AfterTargetCommit)
        .unwrap_err();
    assert_eq!(error.code(), "internal_error");
    assert_eq!(
        old_subscription.poll().unwrap_err(),
        KernelProblem::ResyncRequired
    );

    let snapshot = fixture
        .kernel
        .snapshot(SnapshotQuery::Workflow {
            project: fixture.project.clone(),
            workflow: fixture.workflow.clone(),
        })
        .unwrap();
    let SnapshotData::Workflow(data) = snapshot.data;
    assert_eq!(data.name, "committed-before-crash");
    assert_ne!(snapshot.cursor.stream_epoch, old_cursor.stream_epoch);
    assert_eq!(snapshot.cursor.through_seq, 0);
}

#[test]
fn two_projects_share_one_gap_free_global_sequence() {
    let fixture = Arc::new(ProjectionFixture::new());
    let other = Arc::new(fixture.add_project("wf-global-b"));
    let mut subscription = fixture
        .kernel
        .subscribe_events(fixture.kernel.current_event_cursor())
        .unwrap();
    let a_fixture = fixture.clone();
    let a = std::thread::spawn(move || {
        for expected in 1..=20 {
            a_fixture
                .rename(&format!("a-{expected}"), expected)
                .unwrap();
        }
    });
    let b_fixture = fixture.clone();
    let b_project = other.clone();
    let b = std::thread::spawn(move || {
        for expected in 1..=20 {
            b_fixture
                .rename_target(
                    &b_project.project,
                    &b_project.workflow,
                    &format!("b-{expected}"),
                    expected,
                )
                .unwrap();
        }
    });
    a.join().unwrap();
    b.join().unwrap();
    let events = subscription.poll().unwrap();
    assert_eq!(events.len(), 40);
    assert!(
        events
            .iter()
            .enumerate()
            .all(|(index, event)| event.seq == index as u64 + 1),
        "跨 Project stream 必须使用唯一连续全局 seq"
    );
}

#[test]
fn registering_project_publishes_online_targets_before_reconciling_new_target() {
    let fixture = ProjectionFixture::new();
    let mut subscription = fixture
        .kernel
        .subscribe_events(fixture.kernel.current_event_cursor())
        .unwrap();
    fixture.insert_outbox(fixture.raw_event(
        "workflow.rename",
        1,
        2,
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": {"name": "online-pending"},
        }),
    ));

    let _other = fixture.add_project("wf-register-b");
    let events = subscription.poll().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].projection["data"]["name"], "online-pending");
}

#[test]
fn unregister_linearization_fences_request_that_prefetched_registry_snapshot() {
    let fixture = Arc::new(ProjectionFixture::new());
    let gate = Arc::new(std::sync::Barrier::new(2));
    let dispatch_fixture = fixture.clone();
    let dispatch_gate = gate.clone();
    let request = fixture.rename_request(&fixture.project, &fixture.workflow, "too-late", 1);
    let dispatch = std::thread::spawn(move || {
        dispatch_fixture
            .kernel
            .dispatch_rename_with_barrier_hook(request, || {
                dispatch_gate.wait();
                dispatch_gate.wait();
            })
    });
    gate.wait(); // request 已克隆 registered targets，但尚未进入 L-PUBLISH
    fixture
        .kernel
        .unregister_project_store(&fixture.project)
        .unwrap();
    gate.wait();
    assert_eq!(
        dispatch.join().unwrap().unwrap_err(),
        KernelProblem::ResourceNotFound
    );
    assert_eq!(
        fixture
            .store
            .load_project_workflow("wf-projection")
            .unwrap()
            .unwrap()
            .name,
        "初始"
    );
}
