//! Issue #24: projection envelope、revision chain 与字节容量契约。

use crate::kernel::KernelProblem;
use crate::projection_support::{tiny_limits, ProjectionFixture};

#[test]
fn unknown_critical_delta_json_patch_and_revision_discontinuity_rotate() {
    for delta in [
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.future_unknown",
            "data": {},
        }),
        serde_json::json!({"mode": "json_patch", "data": []}),
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": {"name": "jump"},
        }),
    ] {
        let fixture = ProjectionFixture::new();
        let aggregate_revision = if delta["data"]["name"] == "jump" {
            3
        } else {
            2
        };
        let event_type = if delta["delta_type"].is_string() {
            delta["delta_type"].as_str().unwrap().to_string()
        } else {
            "workflow.rename".to_string()
        };
        let id =
            fixture.insert_outbox(fixture.raw_event(&event_type, 1, aggregate_revision, delta));
        assert_eq!(
            fixture
                .kernel
                .publish_pending_for_test(&fixture.project)
                .unwrap_err(),
            KernelProblem::ResyncRequired
        );
        assert!(fixture.published_at(id).unwrap().starts_with("reconciled:"));
        assert_eq!(fixture.kernel.projection_stats().events, 0);
        assert_eq!(fixture.kernel.projection_stats().protocol_rotations, 1);
    }
}

#[test]
fn workflow_rename_delta_requires_strict_name_dto() {
    for data in [
        serde_json::Value::Null,
        serde_json::json!(42),
        serde_json::json!({}),
        serde_json::json!({"name": 42}),
        serde_json::json!({"name": ""}),
        serde_json::json!({"name": "valid", "unexpected": true}),
    ] {
        let fixture = ProjectionFixture::new();
        let id = fixture.insert_outbox(fixture.raw_event(
            "workflow.rename",
            1,
            2,
            serde_json::json!({
                "mode": "typed_delta",
                "delta_type": "workflow.rename",
                "data": data,
            }),
        ));
        assert_eq!(
            fixture
                .kernel
                .publish_pending_for_test(&fixture.project)
                .unwrap_err(),
            KernelProblem::ResyncRequired
        );
        assert!(fixture.published_at(id).unwrap().starts_with("reconciled:"));
    }
}

#[test]
fn aggregate_head_rejects_duplicate_base_revision() {
    let fixture = ProjectionFixture::new();
    fixture.insert_outbox(fixture.raw_event(
        "workflow.rename",
        1,
        2,
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": {"name": "first"},
        }),
    ));
    fixture
        .kernel
        .publish_pending_for_test(&fixture.project)
        .unwrap();
    let duplicate = fixture.insert_outbox(fixture.raw_event(
        "workflow.rename",
        1,
        2,
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": {"name": "duplicate"},
        }),
    ));
    assert_eq!(
        fixture
            .kernel
            .publish_pending_for_test(&fixture.project)
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );
    assert!(fixture
        .published_at(duplicate)
        .unwrap()
        .starts_with("reconciled:"));
}

#[test]
fn replace_and_tombstone_are_closed_modes_and_tombstone_prevents_handle_reuse() {
    let fixture = ProjectionFixture::new();
    fixture.insert_outbox(fixture.raw_event(
        "workflow.replace",
        1,
        2,
        serde_json::json!({"mode": "replace", "data": {"name": "full"}}),
    ));
    fixture
        .kernel
        .publish_pending_for_test(&fixture.project)
        .unwrap();
    fixture.insert_outbox(fixture.raw_event(
        "workflow.delete",
        2,
        2,
        serde_json::json!({"mode": "tombstone"}),
    ));
    fixture
        .kernel
        .publish_pending_for_test(&fixture.project)
        .unwrap();

    let reuse = fixture.insert_outbox(fixture.raw_event(
        "workflow.rename",
        2,
        3,
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": {"name": "must-not-reuse"},
        }),
    ));
    assert_eq!(
        fixture
            .kernel
            .publish_pending_for_test(&fixture.project)
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );
    assert!(fixture
        .published_at(reuse)
        .unwrap()
        .starts_with("reconciled:"));
}

#[test]
fn event_limit_is_measured_in_utf8_wire_bytes() {
    let mut limits = tiny_limits();
    limits.journal_event_max_bytes = 1_024;
    let fixture = ProjectionFixture::with_limits(limits);
    let payload = "😀".repeat(300);
    let id = fixture.insert_outbox(fixture.raw_event(
        "workflow.rename",
        1,
        2,
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": {"name": payload},
        }),
    ));
    assert_eq!(
        fixture
            .kernel
            .publish_pending_for_test(&fixture.project)
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );
    assert!(fixture.published_at(id).unwrap().starts_with("reconciled:"));
}

#[test]
fn append_and_eight_client_fanout_meet_a9_release_budget() {
    let limits = crate::limits::JournalLimits {
        journal_max_events: 5_000,
        client_event_queue_max_events: 5_000,
        ..Default::default()
    };
    let fixture = ProjectionFixture::with_limits(limits);
    let cursor = fixture.kernel.current_event_cursor();
    let mut clients: Vec<_> = (0..8)
        .map(|_| {
            use crate::kernel::CoreKernel as _;
            fixture.kernel.subscribe_events(cursor.clone()).unwrap()
        })
        .collect();
    let mut samples = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        let started = std::time::Instant::now();
        fixture.kernel.append_projection_probe().unwrap();
        samples.push(started.elapsed());
        for client in &mut clients {
            assert_eq!(client.poll().unwrap().len(), 1);
        }
    }
    samples.sort_unstable();
    let p99 = samples[samples.len() * 99 / 100];
    let release_append_budget = std::time::Duration::from_millis(5);
    let debug_budget = std::time::Duration::from_millis(50);
    assert!(
        p99 <= if cfg!(debug_assertions) {
            debug_budget
        } else {
            release_append_budget
        },
        "journal append + 8-client fan-out p99 {p99:?} 超预算"
    );
    // fan-out additive 的 10ms budget 比上述 5ms 总时延预算更宽；同一
    // 样本已覆盖 8 client clone/enqueue，故无需脆弱的两次 wall-clock 相减。
    if !cfg!(debug_assertions) {
        assert!(p99 <= std::time::Duration::from_millis(10));
    }
}

#[test]
fn ordinary_aggregate_uses_scalar_revision_wire_shape() {
    use crate::kernel::CoreKernel as _;

    let fixture = ProjectionFixture::new();
    let mut subscription = fixture
        .kernel
        .subscribe_events(fixture.kernel.current_event_cursor())
        .unwrap();
    fixture.insert_outbox(serde_json::json!({
        "type": "run.replace.applied",
        "aggregate": {"kind": "workflow_run", "handle": "run-handle-1"},
        "caused_by_command_id": null,
        "projection": {
            "base_revision": {"revision": 7},
            "aggregate_revision": {"revision": 8},
            "delta": {"mode": "replace", "data": {"state": "running"}},
        },
    }));
    fixture
        .kernel
        .publish_pending_for_test(&fixture.project)
        .unwrap();
    let event = subscription.poll().unwrap().pop().unwrap();
    let wire = serde_json::to_value(event).unwrap();
    assert_eq!(wire["base_revision"]["revision"], "7");
    assert_eq!(wire["aggregate_revision"]["revision"], "8");
}

#[test]
fn min_age_uses_injected_monotonic_clock() {
    let mut limits = tiny_limits();
    limits.journal_max_events = 2;
    limits.journal_min_age_secs = 10;

    let young_clock = crate::journal::ManualClock::new();
    let young =
        crate::journal::EventJournal::for_test_limits_and_clock(limits, young_clock.clone());
    young.append_probe().unwrap();
    young.append_probe().unwrap();
    assert_eq!(
        young.append_probe().unwrap_err(),
        KernelProblem::ResyncRequired,
        "目标 min-age 窗口内无法逐出时必须 rotate"
    );

    let old_clock = crate::journal::ManualClock::new();
    let old = crate::journal::EventJournal::for_test_limits_and_clock(limits, old_clock.clone());
    let epoch = old.cursor().stream_epoch;
    old.append_probe().unwrap();
    old.append_probe().unwrap();
    old_clock.advance(std::time::Duration::from_secs(11));
    old.append_probe().unwrap();
    assert_eq!(old.cursor().stream_epoch, epoch);
    assert_eq!(old.stats().events, 2);
    assert_eq!(old.stats().evicted, 1);
}
