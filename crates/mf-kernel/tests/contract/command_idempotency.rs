//! T1f 单目标 command：canonical digest、lease-first replay 与目标原子链。

use crate::command::{
    CommandCoordinator, CommandOutcome, CommandPayload, CommandProblem, IntentState,
};
use crate::command_support::*;
use crate::handles::{CommandId, TargetStoreKind};
use crate::project_registry::ServiceStore;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use zeroize::Zeroizing;

#[test]
fn command_id_deserialize_is_uuid_v7_and_canonical() {
    let canonical = CommandId::new();
    let uuid = uuid::Uuid::parse_str(canonical.as_str()).unwrap();
    let uppercase = canonical.as_str().to_ascii_uppercase();
    let simple = uuid.simple().to_string();
    assert_eq!(CommandId::parse(uppercase).unwrap(), canonical);
    assert_eq!(CommandId::parse(simple).unwrap(), canonical);
    let decoded: CommandId =
        serde_json::from_str(&format!("\"{}\"", canonical.as_str().to_ascii_uppercase())).unwrap();
    assert_eq!(decoded, canonical);
    assert!(serde_json::from_str::<CommandId>("\"not-a-command-id\"").is_err());
    assert!(serde_json::from_str::<CommandId>("\"550e8400-e29b-41d4-a716-446655440000\"").is_err());
}

#[test]
fn same_id_and_digest_replays_once_but_revalidates_current_lease() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = TestAuthorizer::new(7);
    let command_id = CommandId::new();
    let first = envelope(
        command_id.clone(),
        TargetStoreKind::Project,
        7,
        plain_payload("ready"),
    );
    let applied = coordinator
        .dispatch_contract(&first, &target, &auth, set_value_effect("ready"))
        .unwrap();
    assert_eq!(
        applied,
        CommandOutcome::Applied {
            result_revisions: json!({"revision": 2}),
            replayed: false
        }
    );

    // JSON object 插入顺序不同但语义相同，canonical digest 必须一致。
    let mut payload = serde_json::Map::new();
    payload.insert("node_handle".into(), json!("node_test"));
    payload.insert("value".into(), json!("ready"));
    let retry = envelope(
        command_id.clone(),
        TargetStoreKind::Project,
        7,
        CommandPayload::Plain(payload.into()),
    );
    let replay = coordinator
        .dispatch_contract(&retry, &target, &auth, panic_effect())
        .unwrap();
    assert_eq!(
        replay,
        CommandOutcome::Applied {
            result_revisions: json!({"revision": 2}),
            replayed: true
        }
    );
    assert_eq!(auth.calls(), 2, "receipt replay 前仍必须复验当前 lease");
    assert_eq!(target_snapshot(&target).0, "ready");
    assert_eq!(target_snapshot(&target).2, 1);
    assert_eq!(target_snapshot(&target).3, 1);

    auth.set_epoch(8);
    let stale = coordinator.dispatch_contract(&retry, &target, &auth, panic_effect());
    assert_eq!(stale.unwrap_err(), CommandProblem::ControllerLeaseExpired);
    assert_eq!(auth.calls(), 3);
    assert_eq!(
        coordinator.intent_state(&command_id).unwrap(),
        Some(IntentState::Applied),
        "旧 client 失败不得倒退已线性化 intent"
    );
}

#[test]
fn same_id_with_different_digest_or_target_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = TestAuthorizer::new(1);
    let command_id = CommandId::new();
    coordinator
        .dispatch_contract(
            &envelope(
                command_id.clone(),
                TargetStoreKind::Project,
                1,
                plain_payload("one"),
            ),
            &target,
            &auth,
            set_value_effect("one"),
        )
        .unwrap();

    let changed_payload = envelope(
        command_id.clone(),
        TargetStoreKind::Project,
        99, // lease epoch 排除在 digest 外；payload 才造成 reuse 冲突
        plain_payload("two"),
    );
    assert_eq!(
        coordinator
            .dispatch_contract(&changed_payload, &target, &auth, panic_effect())
            .unwrap_err(),
        CommandProblem::ControllerLeaseExpired,
        "异 digest 也必须先复验当前 lease"
    );
    let changed_payload = envelope(
        command_id.clone(),
        TargetStoreKind::Project,
        1,
        plain_payload("two"),
    );
    assert_eq!(
        coordinator
            .dispatch_contract(&changed_payload, &target, &auth, panic_effect())
            .unwrap_err(),
        CommandProblem::CommandIdReused
    );
    let changed_target = envelope_at(
        command_id,
        TargetStoreKind::Project,
        1,
        "wf_018f0000-0000-7000-8000-000000000099",
        plain_payload("one"),
    );
    assert_eq!(
        coordinator
            .dispatch_contract(&changed_target, &target, &auth, panic_effect())
            .unwrap_err(),
        CommandProblem::CommandIdReused
    );
}

#[test]
fn same_id_and_digest_replays_original_terminal_problem() {
    // RevisionConflict 持久为 failed + exact problem code。
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    target
        .with_conn(|conn| {
            conn.execute("UPDATE command_test_effect SET revision=2", [])
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            Ok(())
        })
        .unwrap();
    let auth = TestAuthorizer::new(2);
    let command_id = CommandId::new();
    let revision_envelope = envelope(
        command_id.clone(),
        TargetStoreKind::Project,
        2,
        plain_payload("conflict"),
    );
    assert_eq!(
        coordinator
            .dispatch_contract(&revision_envelope, &target, &auth, panic_effect())
            .unwrap_err(),
        CommandProblem::RevisionConflict
    );
    assert_eq!(
        coordinator
            .dispatch_contract(&revision_envelope, &target, &auth, panic_effect())
            .unwrap_err(),
        CommandProblem::RevisionConflict
    );

    // Controller lease 后续恢复有效，也必须返回首次 terminal problem。
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = TestAuthorizer::new(4);
    let command_id = CommandId::new();
    let lease_envelope = envelope(
        command_id,
        TargetStoreKind::Project,
        3,
        plain_payload("lease"),
    );
    assert_eq!(
        coordinator
            .dispatch_contract(&lease_envelope, &target, &auth, panic_effect())
            .unwrap_err(),
        CommandProblem::ControllerLeaseExpired
    );
    auth.set_epoch(3);
    assert_eq!(
        coordinator
            .dispatch_contract(&lease_envelope, &target, &auth, panic_effect())
            .unwrap_err(),
        CommandProblem::ControllerLeaseExpired
    );

    // Root epoch 同理，不被 generic revoked 覆盖。
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = TestAuthorizer::new(5);
    auth.set_root_epoch(Some(9));
    let root_envelope = envelope_with_root(
        CommandId::new(),
        TargetStoreKind::Project,
        5,
        8,
        plain_payload("root"),
    );
    assert_eq!(
        coordinator
            .dispatch_contract(&root_envelope, &target, &auth, panic_effect())
            .unwrap_err(),
        CommandProblem::RootEpochExpired
    );
    auth.set_root_epoch(Some(8));
    assert_eq!(
        coordinator
            .dispatch_contract(&root_envelope, &target, &auth, panic_effect())
            .unwrap_err(),
        CommandProblem::RootEpochExpired
    );
}

#[test]
fn project_and_catalog_effect_receipt_outbox_commit_atomically() {
    for kind in [TargetStoreKind::Project, TargetStoreKind::Catalog] {
        let tmp = tempfile::tempdir().unwrap();
        let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
        let coordinator = CommandCoordinator::new(service, key());
        let target = make_target(kind, &tmp);
        let auth = TestAuthorizer::new(3);
        let command_id = CommandId::new();
        coordinator
            .dispatch_contract(
                &envelope(command_id, kind, 3, plain_payload("atomic")),
                &target,
                &auth,
                set_value_effect("atomic"),
            )
            .unwrap();
        let (value, revision, receipts, outbox, events) = target_snapshot(&target);
        assert_eq!(
            (value.as_str(), revision, receipts, outbox),
            ("atomic", 2, 1, 1)
        );
        assert_eq!(events.len(), 1);
        assert!(!events[0].contains("stream_epoch"));
        assert!(!events[0].contains("\"seq\""));
        assert!(events[0].contains("caused_by_command_id"));
    }
}

#[test]
fn target_failure_rolls_back_effect_receipt_and_outbox() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = TestAuthorizer::new(4);
    let envelope = envelope(
        CommandId::new(),
        TargetStoreKind::Project,
        4,
        plain_payload("rollback"),
    );
    let error = coordinator
        .dispatch_contract(&envelope, &target, &auth, |tx| {
            tx.execute(
                "UPDATE command_test_effect SET value='must-rollback', revision=2",
                [],
            )
            .unwrap();
            Err(CommandProblem::Internal("fault:effect".into()))
        })
        .unwrap_err();
    assert!(matches!(error, CommandProblem::Internal(_)));
    assert_eq!(target_snapshot(&target).0, "initial");
    assert_eq!(target_snapshot(&target).1, 1);
    assert_eq!(target_snapshot(&target).2, 0);
    assert_eq!(target_snapshot(&target).3, 0);
}

#[test]
fn write_only_secret_uses_hmac_and_never_reaches_durable_output() {
    let tmp = tempfile::tempdir().unwrap();
    let service_path = tmp.path().join("service-v1.db");
    let service = ServiceStore::open(&service_path).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = catalog_target(&tmp.path().join("catalog-v2.db"));
    let auth = TestAuthorizer::new(5);
    let command_id = CommandId::new();
    let secret = "ultra-secret-api-key";
    let secret_envelope = envelope(
        command_id.clone(),
        TargetStoreKind::Catalog,
        5,
        CommandPayload::WriteOnlySecret {
            public_fields: json!({"profile_handle": "provider_test", "endpoint": "https://example.invalid"}),
            secret: Zeroizing::new(secret.to_string()),
        },
    );
    let debug = format!("{secret_envelope:?}");
    assert!(!debug.contains(secret));
    coordinator
        .dispatch_contract(
            &secret_envelope,
            &target,
            &auth,
            set_value_effect("secret-written"),
        )
        .unwrap();
    assert!(!all_target_text(&target).contains(secret));
    for path in [
        service_path.clone(),
        tmp.path().join("service-v1.db-wal"),
        tmp.path().join("service-v1.db-shm"),
    ] {
        if path.exists() {
            assert!(!std::fs::read(path)
                .unwrap()
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()));
        }
    }
    for path in [
        tmp.path().join("catalog-v2.db"),
        tmp.path().join("catalog-v2.db-wal"),
    ] {
        if path.exists() {
            assert!(!std::fs::read(path)
                .unwrap()
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()));
        }
    }

    // client/lease epoch 排除于 semantic digest；同 key+Secret 跨重启稳定。
    let same_semantics = envelope(
        command_id,
        TargetStoreKind::Catalog,
        999,
        CommandPayload::WriteOnlySecret {
            public_fields: json!({"endpoint": "https://example.invalid", "profile_handle": "provider_test"}),
            secret: Zeroizing::new(secret.to_string()),
        },
    );
    assert_eq!(
        secret_envelope.semantic_digest(&key()).unwrap(),
        same_semantics.semantic_digest(&key()).unwrap()
    );
    let different_secret = envelope(
        CommandId::new(),
        TargetStoreKind::Catalog,
        5,
        CommandPayload::WriteOnlySecret {
            public_fields: json!({"profile_handle": "provider_test", "endpoint": "https://example.invalid"}),
            secret: Zeroizing::new("different-secret".to_string()),
        },
    );
    assert_ne!(
        secret_envelope.semantic_digest(&key()).unwrap(),
        different_secret.semantic_digest(&key()).unwrap()
    );
    let other_key = crate::command::ServiceIdempotencyKey::new(vec![0xa5; 32]).unwrap();
    assert_ne!(
        secret_envelope.semantic_digest(&key()).unwrap(),
        secret_envelope.semantic_digest(&other_key).unwrap()
    );
}

#[test]
fn write_only_secret_public_fields_reject_credentials_and_reusable_refs() {
    for public_fields in [
        json!({"profile_handle": "provider", "secret_ref": "reusable"}),
        json!({"profile_handle": "provider", "api_key": "plaintext"}),
        json!({"profile_handle": "provider", "api-key": "plaintext"}),
        json!({"profile_handle": "provider", "api.key": "plaintext"}),
        json!({"profile_handle": "provider", "credential-ref": "reusable"}),
        json!({"profile_handle": "provider", "nested": {"access_token": "plaintext"}}),
    ] {
        let result = try_envelope_at(
            CommandId::new(),
            TargetStoreKind::Catalog,
            1,
            AGGREGATE,
            CommandPayload::WriteOnlySecret {
                public_fields,
                secret: Zeroizing::new("actual-secret".into()),
            },
        );
        assert!(matches!(result, Err(CommandProblem::InvalidEnvelope(_))));
    }
    assert!(try_envelope_at(
        CommandId::new(),
        TargetStoreKind::Project,
        1,
        AGGREGATE,
        CommandPayload::Plain(json!({"max_tokens": 4096, "token_count": 12})),
    )
    .is_ok());
    for payload in [
        json!({"node_handle": "node", "api_key": "plaintext"}),
        json!({"node_handle": "node", "token_ref": "reusable"}),
        json!({"node_handle": "node", "mf_run_token": "mft_leak"}),
    ] {
        let result = try_envelope_at(
            CommandId::new(),
            TargetStoreKind::Project,
            1,
            AGGREGATE,
            CommandPayload::Plain(payload),
        );
        assert!(matches!(result, Err(CommandProblem::InvalidEnvelope(_))));
    }
}

#[test]
fn secret_ref_or_plaintext_in_effect_output_is_rejected_and_rolled_back() {
    for projection in [
        json!({"value": "ultra-secret-api-key"}),
        json!({"secret_ref": "reusable-ref"}),
        json!({"access_token": "plaintext"}),
        json!({"password": "plaintext"}),
        json!({"api-key": "plaintext"}),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
        let coordinator = CommandCoordinator::new(service, key());
        let target = catalog_target(&tmp.path().join("catalog-v2.db"));
        let auth = TestAuthorizer::new(6);
        let envelope = envelope(
            CommandId::new(),
            TargetStoreKind::Catalog,
            6,
            CommandPayload::WriteOnlySecret {
                public_fields: json!({"profile_handle": "provider_test"}),
                secret: Zeroizing::new("ultra-secret-api-key".to_string()),
            },
        );
        let projection = projection.clone();
        assert!(matches!(
            coordinator.dispatch_contract(&envelope, &target, &auth, |tx| {
                tx.execute(
                    "UPDATE command_test_effect SET value='must-rollback', revision=2",
                    [],
                )
                .unwrap();
                Ok(crate::command::EffectOutput::for_contract(
                    json!({"revision": 2}),
                    projection,
                ))
            }),
            Err(CommandProblem::InvalidEnvelope(_))
        ));
        assert_eq!(target_snapshot(&target).0, "initial");
        assert_eq!(target_snapshot(&target).2, 0);
    }
}

#[test]
fn plain_command_sensitive_output_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = TestAuthorizer::new(8);
    let envelope = envelope(
        CommandId::new(),
        TargetStoreKind::Project,
        8,
        plain_payload("safe-input"),
    );
    assert!(matches!(
        coordinator.dispatch_contract(&envelope, &target, &auth, |_tx| {
            Ok(crate::command::EffectOutput::for_contract(
                json!({"revision": 2}),
                json!({"credential": "must-not-persist"}),
            ))
        }),
        Err(CommandProblem::InvalidEnvelope(_))
    ));
    assert_eq!(target_snapshot(&target).2, 0);
    assert_eq!(target_snapshot(&target).3, 0);
}

#[test]
fn concurrent_same_command_runs_effect_once() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = Arc::new(CommandCoordinator::new(service, key()));
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = Arc::new(TestAuthorizer::new(12));
    let command_id = CommandId::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let coordinator = coordinator.clone();
            let target = target.clone();
            let auth = auth.clone();
            let command_id = command_id.clone();
            let calls = calls.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let envelope = envelope(
                    command_id,
                    TargetStoreKind::Project,
                    12,
                    plain_payload("concurrent"),
                );
                barrier.wait();
                coordinator.dispatch_contract(
                    &envelope,
                    &target,
                    auth.as_ref(),
                    update_effect("concurrent".into(), calls),
                )
            })
        })
        .collect();
    let outcomes: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap().unwrap())
        .collect();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcomes.len(), 2);
    assert_eq!(target_snapshot(&target).2, 1);
}

#[test]
fn applied_intent_with_missing_receipt_fails_closed_without_reapplying() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = CommandCoordinator::new(service, key());
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = TestAuthorizer::new(13);
    let command_id = CommandId::new();
    let envelope = envelope(
        command_id.clone(),
        TargetStoreKind::Project,
        13,
        plain_payload("once"),
    );
    coordinator
        .dispatch_contract(&envelope, &target, &auth, set_value_effect("once"))
        .unwrap();
    target
        .with_conn(|conn| {
            conn.execute(
                "DELETE FROM command_receipt WHERE command_id=?1",
                [command_id.as_str()],
            )
            .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            Ok(())
        })
        .unwrap();
    let error = coordinator
        .dispatch_contract(&envelope, &target, &auth, panic_effect())
        .unwrap_err();
    assert!(matches!(error, CommandProblem::Internal(_)));
    assert_eq!(target_snapshot(&target).0, "once");
    assert_eq!(target_snapshot(&target).1, 2);
    assert_eq!(
        coordinator.intent_state(&command_id).unwrap(),
        Some(IntentState::Applied)
    );
}

#[test]
fn lease_permit_blocks_epoch_rotation_until_target_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let coordinator = Arc::new(CommandCoordinator::new(service, key()));
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let auth = Arc::new(TestAuthorizer::new(21));
    let envelope = envelope(
        CommandId::new(),
        TargetStoreKind::Project,
        21,
        plain_payload("barrier"),
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
                    "UPDATE command_test_effect SET value='barrier', revision=2
                     WHERE aggregate_handle=?1 AND revision=1",
                    [AGGREGATE],
                )
                .unwrap();
                Ok(output("barrier"))
            })
        })
    };
    entered_rx.recv().unwrap();
    let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
    let (rotated_tx, rotated_rx) = std::sync::mpsc::channel();
    let rotate = {
        let auth = auth.clone();
        std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            auth.set_epoch(22);
            rotated_tx.send(()).unwrap();
        })
    };
    attempt_rx.recv().unwrap();
    assert!(
        rotated_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "permit 必须把 epoch lock 持有到 L-CMD commit"
    );
    release_tx.send(()).unwrap();
    dispatch.join().unwrap().unwrap();
    rotated_rx.recv().unwrap();
    rotate.join().unwrap();
    assert_eq!(target_snapshot(&target).0, "barrier");
}
