use crate::command::{
    CommandEnvelope, CommandPayload, CommandProblem, CommandType, EffectOutput, TargetDatabase,
};
use crate::handles::{
    AggregateKind, AggregateRef, ClientId, CommandId, CommandTarget, ExpectedRevision, Principal,
    TargetStoreKind,
};
use crate::lease::{CommandAuthorizer, CommandPermit, LeaseCheck};
use mf_agent::catalog_store::CatalogV2Store;
use mf_agent::store::Store;
use rusqlite::Transaction;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub const AGGREGATE: &str = "wf_018f0000-0000-7000-8000-000000000001";
pub const PROJECT_HANDLE: &str = "proj_018f0000-0000-7000-8000-000000000001";

pub fn project_target(path: &Path) -> TargetDatabase {
    let store = Store::open(path).unwrap();
    store
        .with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS command_test_effect (
                     aggregate_handle TEXT PRIMARY KEY,
                     value TEXT NOT NULL,
                     revision INTEGER NOT NULL
                 );
                 INSERT OR IGNORE INTO command_test_effect
                     (aggregate_handle, value, revision)
                 VALUES ('wf_018f0000-0000-7000-8000-000000000001', 'initial', 1);",
            )?;
            Ok(())
        })
        .unwrap();
    TargetDatabase::project(PROJECT_HANDLE, store).unwrap()
}

pub fn catalog_target(path: &Path) -> TargetDatabase {
    let store = CatalogV2Store::open(path).unwrap();
    store
        .with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS command_test_effect (
                     aggregate_handle TEXT PRIMARY KEY,
                     value TEXT NOT NULL,
                     revision INTEGER NOT NULL
                 );
                 INSERT OR IGNORE INTO command_test_effect
                     (aggregate_handle, value, revision)
                 VALUES ('wf_018f0000-0000-7000-8000-000000000001', 'initial', 1);",
            )?;
            Ok(())
        })
        .unwrap();
    TargetDatabase::catalog(store)
}

pub fn envelope(
    command_id: CommandId,
    store: TargetStoreKind,
    epoch: u64,
    payload: CommandPayload,
) -> CommandEnvelope {
    envelope_at(command_id, store, epoch, AGGREGATE, payload)
}

pub fn envelope_at(
    command_id: CommandId,
    store: TargetStoreKind,
    epoch: u64,
    aggregate_handle: &str,
    payload: CommandPayload,
) -> CommandEnvelope {
    try_envelope_at(command_id, store, epoch, aggregate_handle, payload).unwrap()
}

pub fn try_envelope_at(
    command_id: CommandId,
    store: TargetStoreKind,
    epoch: u64,
    aggregate_handle: &str,
    payload: CommandPayload,
) -> Result<CommandEnvelope, CommandProblem> {
    try_envelope_with_root(command_id, store, epoch, None, aggregate_handle, payload)
}

pub fn envelope_with_root(
    command_id: CommandId,
    store: TargetStoreKind,
    epoch: u64,
    root_epoch: u64,
    payload: CommandPayload,
) -> CommandEnvelope {
    try_envelope_with_root(
        command_id,
        store,
        epoch,
        Some(root_epoch),
        AGGREGATE,
        payload,
    )
    .unwrap()
}

fn try_envelope_with_root(
    command_id: CommandId,
    store: TargetStoreKind,
    epoch: u64,
    root_epoch: Option<u64>,
    aggregate_handle: &str,
    payload: CommandPayload,
) -> Result<CommandEnvelope, CommandProblem> {
    let mut revisions = BTreeMap::new();
    revisions.insert("revision".into(), 1);
    let aggregate = AggregateRef::new(AggregateKind::ProjectWorkflow, aggregate_handle).unwrap();
    CommandEnvelope::new(
        command_id,
        ClientId::parse("client-test").unwrap(),
        Principal::parse("user-test").unwrap(),
        epoch,
        root_epoch,
        CommandTarget {
            store,
            store_handle: match store {
                TargetStoreKind::Project => PROJECT_HANDLE.into(),
                TargetStoreKind::Catalog => "catalog".into(),
            },
            aggregate: aggregate.clone(),
        },
        vec![ExpectedRevision {
            aggregate,
            revisions,
        }],
        CommandType::WorkflowMoveNode,
        payload,
    )
}

pub fn plain_payload(value: &str) -> CommandPayload {
    CommandPayload::Plain(json!({"value": value, "node_handle": "node_test"}))
}

pub struct TestAuthorizer {
    epoch: parking_lot::Mutex<u64>,
    root_epoch: parking_lot::Mutex<Option<u64>>,
    calls: AtomicUsize,
}

impl TestAuthorizer {
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch: parking_lot::Mutex::new(epoch),
            root_epoch: parking_lot::Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }

    pub fn set_epoch(&self, epoch: u64) {
        *self.epoch.lock() = epoch;
    }

    pub fn set_root_epoch(&self, root_epoch: Option<u64>) {
        *self.root_epoch.lock() = root_epoch;
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CommandAuthorizer for TestAuthorizer {
    fn acquire<'a>(
        &'a self,
        _tx: &Transaction<'_>,
        check: &LeaseCheck<'_>,
    ) -> Result<Box<dyn CommandPermit + 'a>, CommandProblem> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let epoch = self.epoch.lock();
        if check.principal.as_str() != "user-test"
            || check.client_id.as_str() != "client-test"
            || check.controller_epoch != *epoch
            || check.command_type != CommandType::WorkflowMoveNode
        {
            return Err(CommandProblem::ControllerLeaseExpired);
        }
        let root_epoch = self.root_epoch.lock();
        if check.root_epoch != *root_epoch {
            return Err(CommandProblem::RootEpochExpired);
        }
        Ok(Box::new(TestPermit {
            _epoch: epoch,
            _root_epoch: root_epoch,
        }))
    }
}

struct TestPermit<'a> {
    _epoch: parking_lot::MutexGuard<'a, u64>,
    _root_epoch: parking_lot::MutexGuard<'a, Option<u64>>,
}

impl CommandPermit for TestPermit<'_> {
    fn validate_expected(
        &self,
        tx: &Transaction<'_>,
        check: &LeaseCheck<'_>,
    ) -> Result<(), CommandProblem> {
        for expected in check.expected {
            let Some(revision) = expected.revisions.get("revision") else {
                return Err(CommandProblem::RevisionConflict);
            };
            let actual: i64 = tx
                .query_row(
                    "SELECT revision FROM command_test_effect WHERE aggregate_handle=?1",
                    [expected.aggregate.handle.as_str()],
                    |row| row.get(0),
                )
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            if actual != *revision as i64 {
                return Err(CommandProblem::RevisionConflict);
            }
        }
        Ok(())
    }
}

pub fn set_value_effect(
    value: &'static str,
) -> impl FnOnce(&Transaction<'_>) -> Result<EffectOutput, CommandProblem> {
    move |tx| {
        let changed = tx
            .execute(
                "UPDATE command_test_effect SET value=?2, revision=revision+1
                 WHERE aggregate_handle=?1 AND revision=1",
                [AGGREGATE, value],
            )
            .map_err(|error| CommandProblem::Internal(error.to_string()))?;
        if changed != 1 {
            return Err(CommandProblem::RevisionConflict);
        }
        Ok(EffectOutput::for_contract(
            json!({"revision": 2}),
            json!({"value": value, "revision": 2}),
        ))
    }
}

pub fn panic_effect() -> impl FnOnce(&Transaction<'_>) -> Result<EffectOutput, CommandProblem> {
    |_| panic!("幂等 replay 不得再次调用 effect")
}

pub fn target_snapshot(target: &TargetDatabase) -> (String, i64, i64, i64, Vec<String>) {
    let read = |conn: &rusqlite::Connection| -> Result<_, CommandProblem> {
        let read = || -> anyhow::Result<_> {
            let (value, revision) = conn.query_row(
                "SELECT value, revision FROM command_test_effect WHERE aggregate_handle=?1",
                [AGGREGATE],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?;
            let receipts = conn.query_row("SELECT COUNT(*) FROM command_receipt", [], |row| {
                row.get::<_, i64>(0)
            })?;
            let outbox = conn.query_row("SELECT COUNT(*) FROM projection_outbox", [], |row| {
                row.get::<_, i64>(0)
            })?;
            let events = {
                let mut stmt =
                    conn.prepare("SELECT event_json FROM projection_outbox ORDER BY outbox_id")?;
                let rows = stmt
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<Vec<String>, _>>()?;
                rows
            };
            Ok((value, revision, receipts, outbox, events))
        };
        read().map_err(|error| CommandProblem::Internal(format!("{error:#}")))
    };
    target.with_conn(read).unwrap()
}

pub fn all_target_text(target: &TargetDatabase) -> String {
    let read = |conn: &rusqlite::Connection| -> Result<String, CommandProblem> {
        let read = || -> anyhow::Result<String> {
            let receipt: String = conn.query_row(
                "SELECT semantic_digest || aggregate_handle || result_revisions || state
             FROM command_receipt LIMIT 1",
                [],
                |row| row.get(0),
            )?;
            let event: String = conn.query_row(
                "SELECT event_json FROM projection_outbox LIMIT 1",
                [],
                |row| row.get(0),
            )?;
            Ok(receipt + &event)
        };
        read().map_err(|error| CommandProblem::Internal(format!("{error:#}")))
    };
    target.with_conn(read).unwrap()
}

pub fn make_target(kind: TargetStoreKind, tmp: &tempfile::TempDir) -> TargetDatabase {
    match kind {
        TargetStoreKind::Project => project_target(&tmp.path().join("workflow-v1.db")),
        TargetStoreKind::Catalog => catalog_target(&tmp.path().join("catalog-v2.db")),
    }
}

pub fn key() -> crate::command::ServiceIdempotencyKey {
    crate::command::ServiceIdempotencyKey::new(vec![0x5a; 32]).unwrap()
}

pub fn output(value: &str) -> EffectOutput {
    EffectOutput::for_contract(
        json!({"revision": 2}),
        json!({"value": value, "revision": 2}),
    )
}

pub fn update_effect(
    value: String,
    calls: Arc<AtomicUsize>,
) -> impl FnOnce(&Transaction<'_>) -> Result<EffectOutput, CommandProblem> {
    move |tx| {
        calls.fetch_add(1, Ordering::SeqCst);
        tx.execute(
            "UPDATE command_test_effect SET value=?2, revision=revision+1
             WHERE aggregate_handle=?1 AND revision=1",
            [AGGREGATE, value.as_str()],
        )
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
        Ok(output(&value))
    }
}
