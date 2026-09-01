//! Issue #24: same-epoch resume、gap 与 epoch mismatch。

use crate::handles::StreamEpoch;
use crate::kernel::{CoreKernel, KernelProblem};
use crate::projection::EventCursor;
use crate::projection_support::{tiny_limits, ProjectionFixture};

#[test]
fn resume_inside_journal_window_replays_in_bounded_batches() {
    let mut limits = tiny_limits();
    limits.journal_min_age_secs = 0;
    limits.client_event_queue_max_events = 1;
    let fixture = ProjectionFixture::with_limits(limits);
    let epoch = fixture.kernel.current_event_cursor().stream_epoch;
    fixture.rename("one", 1).unwrap();
    fixture.rename("two", 2).unwrap();
    fixture.rename("three", 3).unwrap();

    let mut subscription = fixture
        .kernel
        .subscribe_events(EventCursor {
            stream_epoch: epoch,
            through_seq: 0,
        })
        .unwrap();
    assert_eq!(subscription.hello().schema, "events.hello.v1");
    assert_eq!(subscription.hello().first_available_seq, 1);
    assert_eq!(subscription.hello().last_seq, 3);
    let hello_wire = serde_json::to_value(subscription.hello()).unwrap();
    assert_eq!(hello_wire["first_available_seq"], "1");
    assert_eq!(hello_wire["last_seq"], "3");
    assert_eq!(subscription.poll().unwrap()[0].seq, 1);
    assert_eq!(subscription.poll().unwrap()[0].seq, 2);
    assert_eq!(subscription.poll().unwrap()[0].seq, 3);
    assert!(subscription.poll().unwrap().is_empty());
}

#[test]
fn future_cursor_epoch_mismatch_and_gap_require_resync() {
    let mut limits = tiny_limits();
    limits.journal_max_events = 2;
    limits.journal_min_age_secs = 0;
    let fixture = ProjectionFixture::with_limits(limits);
    let initial = fixture.kernel.current_event_cursor();
    fixture.rename("one", 1).unwrap();
    fixture.rename("two", 2).unwrap();
    fixture.rename("three", 3).unwrap();
    let current = fixture.kernel.current_event_cursor();
    assert_eq!(fixture.kernel.projection_stats().first_available_seq, 2);

    assert_eq!(
        fixture
            .kernel
            .subscribe_events(EventCursor {
                stream_epoch: current.stream_epoch.clone(),
                through_seq: 0,
            })
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );
    assert!(fixture
        .kernel
        .subscribe_events(EventCursor {
            stream_epoch: current.stream_epoch.clone(),
            through_seq: 1,
        })
        .is_ok());
    assert_eq!(
        fixture
            .kernel
            .subscribe_events(EventCursor {
                stream_epoch: current.stream_epoch.clone(),
                through_seq: current.through_seq + 1,
            })
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );
    assert_eq!(
        fixture
            .kernel
            .subscribe_events(EventCursor {
                stream_epoch: StreamEpoch::new(),
                through_seq: current.through_seq,
            })
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );
    assert_eq!(initial.stream_epoch, current.stream_epoch);
}

#[test]
fn poisoned_project_blocks_other_projects_until_its_old_outbox_is_reconciled() {
    let fixture = ProjectionFixture::new();
    let other = fixture.add_project("wf-other");
    let old = fixture.insert_outbox(fixture.raw_event(
        "workflow.rename",
        1,
        2,
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": {"name": "old-epoch"},
        }),
    ));
    fixture
        .store
        .with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER fail_projection_publish
                 BEFORE UPDATE OF published_at ON projection_outbox
                 BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;",
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        fixture
            .kernel
            .publish_pending_for_test(&fixture.project)
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );

    assert_eq!(
        fixture
            .rename_target(&other.project, &other.workflow, "must-block", 1)
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );
    assert_eq!(
        other
            .store
            .load_project_workflow("wf-other")
            .unwrap()
            .unwrap()
            .name,
        "wf-other",
        "全局 Recovering phase 未收口时不得提交其它 Project"
    );

    fixture
        .store
        .with_conn(|conn| {
            conn.execute_batch("DROP TRIGGER fail_projection_publish;")?;
            Ok(())
        })
        .unwrap();
    fixture
        .rename_target(&other.project, &other.workflow, "now-live", 1)
        .unwrap();
    assert!(fixture
        .published_at(old)
        .unwrap()
        .starts_with("reconciled:"));
    assert_eq!(
        other
            .store
            .load_project_workflow("wf-other")
            .unwrap()
            .unwrap()
            .name,
        "now-live"
    );
}

#[test]
fn unregister_failure_is_reported_and_retryable() {
    let fixture = ProjectionFixture::new();
    fixture.insert_outbox(fixture.raw_event(
        "workflow.rename",
        1,
        2,
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": {"name": "poison"},
        }),
    ));
    fixture
        .store
        .with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER fail_projection_unregister
                 BEFORE UPDATE OF published_at ON projection_outbox
                 BEGIN SELECT RAISE(ABORT, 'unregister recovery fault'); END;",
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        fixture
            .kernel
            .publish_pending_for_test(&fixture.project)
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );
    assert_eq!(
        fixture
            .kernel
            .unregister_project_store(&fixture.project)
            .unwrap_err(),
        KernelProblem::ResyncRequired
    );
    fixture
        .store
        .with_conn(|conn| {
            conn.execute_batch("DROP TRIGGER fail_projection_unregister;")?;
            Ok(())
        })
        .unwrap();
    fixture
        .kernel
        .unregister_project_store(&fixture.project)
        .unwrap();
    assert_eq!(
        fixture
            .kernel
            .snapshot(crate::projection::SnapshotQuery::Workflow {
                project: fixture.project.clone(),
                workflow: fixture.workflow.clone(),
            })
            .unwrap_err(),
        KernelProblem::ResourceNotFound
    );
}

#[test]
fn closing_project_rejects_commands_but_remains_visible_to_shutdown_until_finalize() {
    use crate::shutdown::ShutdownIntent;

    let fixture = ProjectionFixture::new();
    let token = fixture
        .kernel
        .prepare_project_close(&fixture.project)
        .unwrap();
    assert_eq!(
        fixture.rename("must-reject", 1).unwrap_err(),
        KernelProblem::ResourceNotFound
    );
    fixture.insert_outbox(fixture.raw_event(
        "workflow.rename",
        1,
        2,
        serde_json::json!({
            "mode": "typed_delta",
            "delta_type": "workflow.rename",
            "data": {"name": "legacy-teardown-pending"},
        }),
    ));
    let assessment = fixture.kernel.shutdown(ShutdownIntent::Assess);
    assert!(!assessment.safe_to_proceed);
    assert_eq!(assessment.pending_outbox_events, 1);

    fixture.kernel.finalize_project_close(token);
    let assessment = fixture.kernel.shutdown(ShutdownIntent::Assess);
    assert!(assessment.safe_to_proceed);
    assert_eq!(assessment.pending_outbox_events, 0);
}

#[test]
fn closing_registration_cannot_be_replaced_until_finalize() {
    let fixture = ProjectionFixture::new();
    let token = fixture
        .kernel
        .prepare_project_close(&fixture.project)
        .unwrap();
    let error = fixture
        .kernel
        .register_project_store(fixture._tmp.path(), fixture.store.clone())
        .unwrap_err();
    assert_eq!(error.code(), "service_unavailable");

    fixture.kernel.finalize_project_close(token);
    let reopened = fixture
        .kernel
        .register_project_store(fixture._tmp.path(), fixture.store.clone())
        .unwrap();
    assert_eq!(reopened, fixture.project, "同路径复用持久 Project handle");
}
