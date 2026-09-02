use crate::command::ServiceIdempotencyKey;
use crate::handles::CommandId;
use crate::kernel::{CoreKernel, InProcessCoreKernel};
use crate::operation::OperationHandle;
use crate::project_registry::ServiceStore;
use crate::projection::{SnapshotData, SnapshotQuery, SNAPSHOT_SCHEMA};

#[test]
fn operation_snapshot_reads_durable_service_state_without_memory_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let command = CommandId::new();
    let operation = OperationHandle::parse(format!("op_{}", uuid::Uuid::now_v7())).unwrap();
    let step = uuid::Uuid::now_v7().to_string();
    service
        .with_tx(|tx| {
            tx.execute(
                "INSERT INTO command_intent
                 (command_id, semantic_digest, target_store, aggregate, principal,
                  client_id, controller_epoch, state, created_at)
                 VALUES (?1, 'digest', 'project:proj_contract', 'wf_contract',
                         'user', 'client', 1, 'reserved', '2026-09-02T00:00:00Z')",
                [command.as_str()],
            )?;
            tx.execute(
                "INSERT INTO operation
                 (operation_handle, command_id, kind, state, saga_state, progress_json,
                  created_at, updated_at)
                 VALUES (?1, ?2, 'workflow_run.start', 'accepted', '{}',
                         '{\"forward_total\":1,\"forward_succeeded\":0,\"outcome\":null,\"problem\":null}',
                         '2026-09-02T00:00:00Z', '2026-09-02T00:00:00Z')",
                rusqlite::params![operation.as_str(), command.as_str()],
            )?;
            tx.execute(
                "INSERT INTO operation_step
                 (operation_handle, step_index, role, step_id, target_store, aggregate,
                  semantic_digest, expected_json, state, result_json, created_at, updated_at)
                 VALUES (?1, 0, 'forward', ?2, 'project:proj_contract', 'wf_contract',
                         'step-digest', '[]', 'pending', '{}',
                         '2026-09-02T00:00:00Z', '2026-09-02T00:00:00Z')",
                rusqlite::params![operation.as_str(), step],
            )?;
            Ok(())
        })
        .unwrap();
    let kernel =
        InProcessCoreKernel::new(service, ServiceIdempotencyKey::new(vec![0x26; 32]).unwrap());

    let snapshot = kernel
        .snapshot(SnapshotQuery::Operation {
            operation: operation.clone(),
        })
        .unwrap();
    assert_eq!(snapshot.schema, SNAPSHOT_SCHEMA);
    let SnapshotData::Operation(data) = snapshot.data else {
        panic!("expected Operation snapshot")
    };
    assert_eq!(data.operation, operation);
    assert_eq!(data.kind, "workflow_run.start");
    assert_eq!(data.state, "accepted");
    assert_eq!(data.steps.len(), 1);
    assert_eq!(data.steps[0].state, "pending");
}
