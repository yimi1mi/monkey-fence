//! Issue #24: hard-cap rotate 与慢 client 隔离。

use crate::kernel::{CoreKernel, KernelProblem};
use crate::projection::{SnapshotData, SnapshotQuery};
use crate::projection_support::{tiny_limits, ProjectionFixture};

#[test]
fn event_flood_rotates_epoch_and_all_old_clients_resync_without_exceeding_cap() {
    let mut limits = tiny_limits();
    limits.journal_max_events = 2;
    limits.journal_min_age_secs = 1_800;
    let fixture = ProjectionFixture::with_limits(limits);
    let old_epoch = fixture.kernel.current_event_cursor().stream_epoch;
    let mut client = fixture
        .kernel
        .subscribe_events(fixture.kernel.current_event_cursor())
        .unwrap();
    fixture.rename("one", 1).unwrap();
    fixture.rename("two", 2).unwrap();
    assert_eq!(fixture.kernel.projection_stats().events, 2);

    assert_eq!(
        fixture.rename("three", 3).unwrap_err(),
        KernelProblem::ResyncRequired
    );
    let stats = fixture.kernel.projection_stats();
    assert!(stats.events <= 2);
    assert_eq!(stats.events, 0);
    assert_eq!(stats.rotations, 1);
    assert_eq!(stats.capacity_rotations, 1);
    assert_eq!(client.poll().unwrap_err(), KernelProblem::ResyncRequired);

    let snapshot = fixture
        .kernel
        .snapshot(SnapshotQuery::Workflow {
            project: fixture.project.clone(),
            workflow: fixture.workflow.clone(),
        })
        .unwrap();
    let SnapshotData::Workflow(data) = snapshot.data else {
        panic!("expected workflow snapshot")
    };
    assert_eq!(data.name, "three", "业务 commit 不由 journal 回滚");
    assert_ne!(snapshot.cursor.stream_epoch, old_epoch);
    assert_eq!(snapshot.cursor.through_seq, 0);
}

#[test]
fn slow_client_is_evicted_without_blocking_fast_client() {
    let mut limits = tiny_limits();
    limits.journal_max_events = 16;
    limits.journal_min_age_secs = 0;
    limits.client_event_queue_max_events = 2;
    let fixture = ProjectionFixture::with_limits(limits);
    let cursor = fixture.kernel.current_event_cursor();
    let mut fast = fixture.kernel.subscribe_events(cursor.clone()).unwrap();
    let mut slow = fixture.kernel.subscribe_events(cursor).unwrap();

    for expected in 1..=6 {
        fixture
            .rename(&format!("event-{expected}"), expected)
            .unwrap();
        let events = fast.poll().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, expected);
    }
    assert_eq!(slow.poll().unwrap_err(), KernelProblem::ResyncRequired);
    assert_eq!(fixture.kernel.projection_stats().clients, 1);
}

#[test]
fn dropping_subscription_releases_its_queue() {
    let fixture = ProjectionFixture::new();
    let subscription = fixture
        .kernel
        .subscribe_events(fixture.kernel.current_event_cursor())
        .unwrap();
    assert_eq!(fixture.kernel.projection_stats().clients, 1);
    drop(subscription);
    assert_eq!(fixture.kernel.projection_stats().clients, 0);
}

#[test]
fn slow_client_flood_does_not_serialize_scheduler_or_pty_progress() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    let mut limits = tiny_limits();
    limits.journal_max_events = 64;
    limits.client_event_queue_max_events = 2;
    let fixture = ProjectionFixture::with_limits(limits);
    let cursor = fixture.kernel.current_event_cursor();
    let mut fast = fixture.kernel.subscribe_events(cursor.clone()).unwrap();
    let mut slow = fixture.kernel.subscribe_events(cursor).unwrap();
    let start = Arc::new(Barrier::new(3));
    let scheduler_progress = Arc::new(AtomicUsize::new(0));
    let pty_progress = Arc::new(AtomicUsize::new(0));
    let scheduler = {
        let start = start.clone();
        let progress = scheduler_progress.clone();
        std::thread::spawn(move || {
            start.wait();
            for _ in 0..10_000 {
                progress.fetch_add(1, Ordering::Relaxed);
                std::thread::yield_now();
            }
        })
    };
    let pty = {
        let start = start.clone();
        let progress = pty_progress.clone();
        std::thread::spawn(move || {
            start.wait();
            for _ in 0..10_000 {
                progress.fetch_add(1, Ordering::Relaxed);
                std::thread::yield_now();
            }
        })
    };
    start.wait();
    for expected in 1..=20 {
        fixture
            .rename(&format!("flood-{expected}"), expected)
            .unwrap();
        assert_eq!(fast.poll().unwrap().len(), 1);
    }
    assert_eq!(slow.poll().unwrap_err(), KernelProblem::ResyncRequired);
    scheduler.join().unwrap();
    pty.join().unwrap();
    assert_eq!(scheduler_progress.load(Ordering::Relaxed), 10_000);
    assert_eq!(pty_progress.load(Ordering::Relaxed), 10_000);
}

#[test]
fn active_subscription_hard_cap_bounds_total_fanout() {
    let fixture = ProjectionFixture::new();
    let cursor = fixture.kernel.current_event_cursor();
    let subscriptions: Vec<_> = (0..256)
        .map(|_| fixture.kernel.subscribe_events(cursor.clone()).unwrap())
        .collect();
    let error = fixture.kernel.subscribe_events(cursor).unwrap_err();
    assert_eq!(error.code(), "service_unavailable");
    drop(subscriptions);
    assert_eq!(fixture.kernel.projection_stats().clients, 0);
}
