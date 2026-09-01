//! T1f crash windows：step1 后终结旧 intent；step2 后只补结果不重放。

use crate::command::{
    CommandCoordinator, CommandOutcome, CommandProblem, FaultPoint, IntentState, ReconcileOutcome,
};
use crate::command_support::*;
use crate::handles::{CommandId, TargetStoreKind};
use crate::project_registry::ServiceStore;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn crash_after_intent_without_receipt_revokes_without_running_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let service_path = tmp.path().join("service-v1.db");
    let target_path = tmp.path().join("workflow-v1.db");
    let command_id = CommandId::new();
    {
        let service = ServiceStore::open(&service_path).unwrap();
        let coordinator = CommandCoordinator::new(service, key());
        let target = project_target(&target_path);
        let auth = TestAuthorizer::new(1);
        let envelope = envelope(
            command_id.clone(),
            TargetStoreKind::Project,
            1,
            plain_payload("never-run"),
        );
        assert_eq!(
            coordinator
                .dispatch_with_fault(
                    &envelope,
                    &target,
                    &auth,
                    panic_effect(),
                    Some(FaultPoint::AfterIntentReserve),
                )
                .unwrap_err(),
            CommandProblem::FaultInjected("after_intent_reserve")
        );
        assert_eq!(
            coordinator.intent_state(&command_id).unwrap(),
            Some(IntentState::Reserved)
        );
        assert_eq!(target_snapshot(&target).0, "initial");
        assert_eq!(target_snapshot(&target).2, 0);
    }

    let service = ServiceStore::open(&service_path).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&target_path);
    let wrong_target = crate::command::TargetDatabase::project(
        "proj_018f0000-0000-7000-8000-000000000099",
        mf_agent::store::Store::open(&tmp.path().join("wrong-project.db")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        coordinator
            .reconcile(&command_id, &wrong_target)
            .unwrap_err(),
        CommandProblem::TargetStoreMismatch
    );
    assert_eq!(
        coordinator.intent_state(&command_id).unwrap(),
        Some(IntentState::Reserved),
        "传错 Project Store 不得把 intent 误标 revoked"
    );
    assert_eq!(
        coordinator.reconcile(&command_id, &target).unwrap(),
        ReconcileOutcome::Terminal(IntentState::Revoked)
    );
    assert_eq!(target_snapshot(&target).0, "initial");
    assert_eq!(target_snapshot(&target).2, 0);
    assert_eq!(target_snapshot(&target).3, 0);
}

#[test]
fn crash_after_target_commit_recovers_from_receipt_without_replay() {
    for kind in [TargetStoreKind::Project, TargetStoreKind::Catalog] {
        let tmp = tempfile::tempdir().unwrap();
        let service_path = tmp.path().join("service-v1.db");
        let target_path = match kind {
            TargetStoreKind::Project => tmp.path().join("workflow-v1.db"),
            TargetStoreKind::Catalog => tmp.path().join("catalog-v2.db"),
        };
        let command_id = CommandId::new();
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let service = ServiceStore::open(&service_path).unwrap();
            let coordinator = CommandCoordinator::new(service, key());
            let target = match kind {
                TargetStoreKind::Project => project_target(&target_path),
                TargetStoreKind::Catalog => catalog_target(&target_path),
            };
            let auth = TestAuthorizer::new(2);
            let envelope = envelope(
                command_id.clone(),
                kind,
                2,
                plain_payload("committed-before-crash"),
            );
            assert_eq!(
                coordinator
                    .dispatch_with_fault(
                        &envelope,
                        &target,
                        &auth,
                        update_effect("committed-before-crash".into(), calls.clone()),
                        Some(FaultPoint::AfterTargetCommit),
                    )
                    .unwrap_err(),
                CommandProblem::FaultInjected("after_target_commit")
            );
            assert_eq!(
                coordinator.intent_state(&command_id).unwrap(),
                Some(IntentState::Reserved)
            );
            assert_eq!(target_snapshot(&target).0, "committed-before-crash");
            assert_eq!(target_snapshot(&target).2, 1);
            assert_eq!(target_snapshot(&target).3, 1);
        }

        // 模拟 Core 重启：重开 service + target，仅查 receipt 完成 intent。
        let service = ServiceStore::open(&service_path).unwrap();
        let coordinator = CommandCoordinator::new(service, key());
        let target = match kind {
            TargetStoreKind::Project => project_target(&target_path),
            TargetStoreKind::Catalog => catalog_target(&target_path),
        };
        assert_eq!(
            coordinator.reconcile(&command_id, &target).unwrap(),
            ReconcileOutcome::Applied(CommandOutcome::Applied {
                result_revisions: json!({"revision": 2}),
                replayed: true,
            })
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(target_snapshot(&target).0, "committed-before-crash");
        assert_eq!(target_snapshot(&target).2, 1);
        assert_eq!(target_snapshot(&target).3, 1);

        // 同 id 重试仍先复验当前 lease，但不能再做 effect/CAS。
        let auth = TestAuthorizer::new(2);
        let retry = envelope(
            command_id.clone(),
            kind,
            2,
            plain_payload("committed-before-crash"),
        );
        assert_eq!(
            coordinator
                .dispatch_contract(&retry, &target, &auth, panic_effect())
                .unwrap(),
            CommandOutcome::Applied {
                result_revisions: json!({"revision": 2}),
                replayed: true,
            }
        );
        assert_eq!(auth.calls(), 1);
    }
}

#[test]
fn target_receipt_mismatch_fails_closed_during_reconcile() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = TestAuthorizer::new(3);
    let command_id = CommandId::new();
    let envelope = envelope(
        command_id.clone(),
        TargetStoreKind::Project,
        3,
        plain_payload("tamper"),
    );
    coordinator
        .dispatch_with_fault(
            &envelope,
            &target,
            &auth,
            set_value_effect("tamper"),
            Some(FaultPoint::AfterTargetCommit),
        )
        .unwrap_err();
    target
        .with_conn(|conn| {
            conn.execute(
                "UPDATE command_receipt SET semantic_digest='corrupt' WHERE command_id=?1",
                [command_id.as_str()],
            )
            .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        coordinator.reconcile(&command_id, &target).unwrap_err(),
        CommandProblem::CommandIdReused
    );
    assert_eq!(
        coordinator.intent_state(&command_id).unwrap(),
        Some(IntentState::Reserved)
    );
}

#[test]
fn reconcile_cannot_revoke_an_inflight_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = Arc::new(CommandCoordinator::new(service, key()));
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = Arc::new(TestAuthorizer::new(31));
    let command_id = CommandId::new();
    let envelope = envelope(
        command_id.clone(),
        TargetStoreKind::Project,
        31,
        plain_payload("race-safe"),
    );
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let dispatch = {
        let coordinator = coordinator.clone();
        let target = target.clone();
        let auth = auth.clone();
        std::thread::spawn(move || {
            coordinator.dispatch_contract(&envelope, &target, auth.as_ref(), |tx| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                tx.execute(
                    "UPDATE command_test_effect SET value='race-safe', revision=2
                     WHERE aggregate_handle=?1 AND revision=1",
                    [AGGREGATE],
                )
                .unwrap();
                Ok(output("race-safe"))
            })
        })
    };
    entered_rx.recv().unwrap();

    let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let reconcile = {
        let coordinator = coordinator.clone();
        let target = target.clone();
        let command_id = command_id.clone();
        std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            result_tx
                .send(coordinator.reconcile(&command_id, &target))
                .unwrap();
        })
    };
    attempt_rx.recv().unwrap();
    assert!(
        result_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "service coordinator guard 必须阻止 reconcile 越过在途 L-CMD"
    );
    release_tx.send(()).unwrap();
    dispatch.join().unwrap().unwrap();
    assert!(matches!(
        result_rx.recv().unwrap().unwrap(),
        ReconcileOutcome::Applied(_)
    ));
    reconcile.join().unwrap();
    assert_eq!(
        coordinator.intent_state(&command_id).unwrap(),
        Some(IntentState::Applied)
    );
    assert_eq!(target_snapshot(&target).0, "race-safe");
}

#[test]
fn reconcile_cannot_enter_between_reserve_and_coordinator_transaction() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = Arc::new(CommandCoordinator::new(service, key()));
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = Arc::new(TestAuthorizer::new(32));
    let command_id = CommandId::new();
    let envelope = envelope(
        command_id.clone(),
        TargetStoreKind::Project,
        32,
        plain_payload("reserve-gap-safe"),
    );
    let (reserved_tx, reserved_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let dispatch = {
        let coordinator = coordinator.clone();
        let target = target.clone();
        let auth = auth.clone();
        std::thread::spawn(move || {
            coordinator.dispatch_with_reserve_hook(
                &envelope,
                &target,
                auth.as_ref(),
                set_value_effect("reserve-gap-safe"),
                || {
                    reserved_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            )
        })
    };
    reserved_rx.recv().unwrap();

    let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let reconcile = {
        let coordinator = coordinator.clone();
        let target = target.clone();
        let command_id = command_id.clone();
        std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            result_tx
                .send(coordinator.reconcile(&command_id, &target))
                .unwrap();
        })
    };
    attempt_rx.recv().unwrap();
    assert!(
        result_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "command_gate 必须覆盖 reserve→coordinator transaction 空窗"
    );
    release_tx.send(()).unwrap();
    dispatch.join().unwrap().unwrap();
    assert!(matches!(
        result_rx.recv().unwrap().unwrap(),
        ReconcileOutcome::Applied(_)
    ));
    reconcile.join().unwrap();
    assert_eq!(
        coordinator.intent_state(&command_id).unwrap(),
        Some(IntentState::Applied)
    );
    assert_eq!(target_snapshot(&target).0, "reserve-gap-safe");
}
