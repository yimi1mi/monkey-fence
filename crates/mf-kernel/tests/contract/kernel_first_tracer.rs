//! T2a(Issue #23)CoreKernel facade 首条 workflow tracer 契约。
//!
//! 一条 `workflow.rename` 从 legacy adapter 经 `dispatch → Project Store →
//! snapshot/event` 完整贯通,并冻结边界:
//! - rename 只推进 presentation revision(semantic/collection 不动);
//! - 同 command id + digest 重试幂等且 effect 不重放,异 digest 拒绝;
//! - snapshot 与 event 携带同一最终 presentation revision;
//! - UI adapter 路径无 Store 直写(dispatch 失败即无任何写入);
//! - stale lease/CAS 冲突 fail-closed;事件只在 commit/receipt/outbox
//!   之后可见,崩溃窗口由 reconcile 修复。

use crate::command::{FaultPoint, ReconcileOutcome, ServiceIdempotencyKey};
use crate::handles::{
    ClientId, CommandId, Principal, ProjectStoreHandle, SessionHandle, StreamEpoch, WorkflowHandle,
};
use crate::kernel::{
    CoreKernel, InProcessCoreKernel, InProcessKernelRuntime, KernelCommand, KernelCommandRequest,
    KernelOutcome, KernelProblem, LegacyKernelClient, TerminalAttach,
};
use crate::project_registry::ServiceStore;
use crate::projection::{EventCursor, RevisionVector, SnapshotData, SnapshotQuery};
use crate::shutdown::ShutdownIntent;
use mf_agent::workflow::WorkflowNodeDraft;
use mf_agent::{ProjectWorkflowDraft, ProjectWorkflowRecord, Store};
use std::sync::Arc;

struct TracerFixture {
    _tmp: tempfile::TempDir,
    store: Arc<Store>,
    kernel: Arc<InProcessCoreKernel>,
    project: ProjectStoreHandle,
    workflow: WorkflowHandle,
    client_id: ClientId,
    principal: Principal,
    epoch: u64,
}

impl TracerFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
        store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: "wf-1".into(),
                name: "原名".into(),
                nodes: vec![WorkflowNodeDraft {
                    key: "a".into(),
                    title: "A".into(),
                    instructions: String::new(),
                    agent_instance_id: "inst".into(),
                    deps: vec![],
                }],
                allow_unsafe_parallel: false,
            })
            .unwrap();
        let record = store.load_project_workflow("wf-1").unwrap().unwrap();
        let workflow = WorkflowHandle::parse(&record.public_handle).unwrap();
        let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
        let kernel = Arc::new(InProcessCoreKernel::new(
            service,
            ServiceIdempotencyKey::new(vec![0x5a; 32]).unwrap(),
        ));
        let project = kernel
            .register_project_store(tmp.path(), store.clone())
            .unwrap();
        let client_id = ClientId::parse("client-tracer").unwrap();
        let principal = Principal::parse("user-tracer").unwrap();
        let epoch = kernel.grant_controller(&client_id, &principal).unwrap();
        Self {
            _tmp: tmp,
            store,
            kernel,
            project,
            workflow,
            client_id,
            principal,
            epoch,
        }
    }

    fn rename_request(
        &self,
        command_id: CommandId,
        workflow: &WorkflowHandle,
        name: &str,
        expected_presentation_revision: u64,
    ) -> KernelCommandRequest {
        KernelCommandRequest::new(
            command_id,
            self.client_id.clone(),
            self.principal.clone(),
            self.epoch,
            KernelCommand::workflow_rename(
                self.project.clone(),
                workflow.clone(),
                name,
                expected_presentation_revision,
            ),
        )
    }

    fn dispatch_rename(
        &self,
        workflow: &WorkflowHandle,
        name: &str,
        expected_presentation_revision: u64,
    ) -> Result<KernelOutcome, KernelProblem> {
        let request = self.rename_request(
            CommandId::new(),
            workflow,
            name,
            expected_presentation_revision,
        );
        self.kernel.dispatch(request)
    }

    fn record(&self) -> ProjectWorkflowRecord {
        self.store.load_project_workflow("wf-1").unwrap().unwrap()
    }

    fn collection_revision(&self) -> i64 {
        self.store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT workflow_collection_revision FROM project_meta WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .map_err(anyhow::Error::from)
            })
            .unwrap()
    }

    fn count(&self, sql: &str) -> i64 {
        self.store
            .with_conn(|conn| {
                conn.query_row(sql, [], |row| row.get(0))
                    .map_err(anyhow::Error::from)
            })
            .unwrap()
    }

    fn receipts(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM command_receipt")
    }

    fn outbox_events(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM projection_outbox")
    }

    fn pending_outbox(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM projection_outbox WHERE published_at IS NULL")
    }

    fn revisions(&self) -> RevisionVector {
        let record = self.record();
        RevisionVector {
            semantic_revision: record.semantic_revision as u64,
            presentation_revision: record.presentation_revision as u64,
        }
    }
}

#[test]
fn rename_only_advances_presentation_revision() {
    let fixture = TracerFixture::new();
    let collection_before = fixture.collection_revision();
    assert_eq!(
        fixture.revisions(),
        RevisionVector {
            semantic_revision: 1,
            presentation_revision: 1
        }
    );

    let outcome = fixture
        .dispatch_rename(&fixture.workflow, "新名字", 1)
        .unwrap();

    assert_eq!(
        outcome,
        KernelOutcome::Applied {
            revisions: RevisionVector {
                semantic_revision: 1,
                presentation_revision: 2
            },
            replayed: false,
        }
    );
    let record = fixture.record();
    assert_eq!(record.name, "新名字");
    assert_eq!(
        record.semantic_revision, 1,
        "rename 归入 presentation 轴,不得推进 semantic revision"
    );
    assert_eq!(record.presentation_revision, 2);
    assert_eq!(
        fixture.collection_revision(),
        collection_before,
        "rename 不得推进 workflow collection revision"
    );
    assert_eq!(fixture.receipts(), 1, "业务写与 receipt/outbox 同事务");
    assert_eq!(fixture.outbox_events(), 1);
}

#[test]
fn same_command_id_and_digest_retries_without_replaying_effect() {
    let fixture = TracerFixture::new();
    let command_id = CommandId::new();
    let first = fixture
        .kernel
        .dispatch(fixture.rename_request(command_id.clone(), &fixture.workflow, "第一次", 1))
        .unwrap();
    let second = fixture
        .kernel
        .dispatch(fixture.rename_request(command_id, &fixture.workflow, "第一次", 1))
        .unwrap();

    assert!(matches!(
        first,
        KernelOutcome::Applied {
            replayed: false,
            ..
        }
    ));
    let KernelOutcome::Applied {
        revisions,
        replayed,
    } = second;
    assert!(replayed, "同 id 同 digest 重试必须命中 receipt 幂等重放");
    assert_eq!(
        revisions,
        RevisionVector {
            semantic_revision: 1,
            presentation_revision: 2
        }
    );
    assert_eq!(
        fixture.record().presentation_revision,
        2,
        "重试不得再次推进 revision"
    );
    assert_eq!(fixture.receipts(), 1);
    assert_eq!(fixture.outbox_events(), 1, "重试不得重复发布事件");
}

#[test]
fn same_command_id_with_different_digest_is_rejected() {
    let fixture = TracerFixture::new();
    let command_id = CommandId::new();
    fixture
        .kernel
        .dispatch(fixture.rename_request(command_id.clone(), &fixture.workflow, "第一次", 1))
        .unwrap();

    let error = fixture
        .kernel
        .dispatch(fixture.rename_request(command_id, &fixture.workflow, "换内容", 1))
        .unwrap_err();

    assert_eq!(error.code(), "command_id_reused");
    let record = fixture.record();
    assert_eq!(record.name, "第一次", "异 digest 命令不得改写已应用结果");
    assert_eq!(record.presentation_revision, 2);
    assert_eq!(fixture.receipts(), 1);
}

#[test]
fn snapshot_and_event_carry_same_final_revision() {
    let fixture = TracerFixture::new();
    let mut subscription = fixture
        .kernel
        .subscribe_events(fixture.kernel.current_event_cursor())
        .unwrap();
    assert!(subscription.poll().unwrap().is_empty());

    let command_id = CommandId::new();
    fixture
        .kernel
        .dispatch(fixture.rename_request(command_id.clone(), &fixture.workflow, "展示名", 1))
        .unwrap();

    let snapshot = fixture
        .kernel
        .snapshot(SnapshotQuery::Workflow {
            project: fixture.project.clone(),
            workflow: fixture.workflow.clone(),
        })
        .unwrap();
    let SnapshotData::Workflow(data) = snapshot.data;
    assert_eq!(data.name, "展示名");
    assert_eq!(
        data.revisions,
        RevisionVector {
            semantic_revision: 1,
            presentation_revision: 2
        }
    );

    let events = subscription.poll().unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.event_type, "workflow.rename");
    assert_eq!(event.aggregate.handle, fixture.workflow.as_str());
    assert_eq!(
        event.base_revision,
        RevisionVector {
            semantic_revision: 1,
            presentation_revision: 1
        }
    );
    assert_eq!(
        event.aggregate_revision, data.revisions,
        "event 与 snapshot 必须携带同一最终 revision"
    );
    assert_eq!(
        event.caused_by_command_id.as_deref(),
        Some(command_id.as_str())
    );
    assert_eq!(
        event.projection["delta_type"], "workflow.rename",
        "typed delta 白名单"
    );
    assert_eq!(
        event.seq, snapshot.cursor.through_seq,
        "snapshot cursor 必须覆盖已发布事件"
    );
    assert!(subscription.poll().unwrap().is_empty(), "事件不得重复投递");
}

#[test]
fn ui_adapter_path_cannot_write_store_directly() {
    let fixture = TracerFixture::new();
    let client = LegacyKernelClient::new(
        fixture.kernel.clone(),
        fixture.principal.clone(),
        fixture.client_id.clone(),
        fixture.epoch,
    );

    // 正常路径:唯一写路径是 dispatch,因此权威链(receipt + outbox)完整。
    client
        .rename_workflow(&fixture.project, "wf-1", "改名")
        .unwrap();
    assert_eq!(fixture.record().name, "改名");
    assert_eq!(fixture.receipts(), 1);
    assert_eq!(fixture.outbox_events(), 1);

    // takeover:新 Controller 使旧 epoch 失效。adapter 此时 rename 失败,
    // 且 Store 无任何变化 —— 若存在直写旁路,这里仍会改名成功。
    let intruder = ClientId::parse("client-web").unwrap();
    fixture
        .kernel
        .grant_controller(&intruder, &fixture.principal)
        .unwrap();
    let error = client
        .rename_workflow(&fixture.project, "wf-1", "越权改名")
        .unwrap_err();
    assert_eq!(error.code(), "controller_lease_expired");
    let record = fixture.record();
    assert_eq!(record.name, "改名");
    assert_eq!(record.presentation_revision, 2);
    assert_eq!(fixture.receipts(), 1, "被拒命令不得留下 effect/receipt");
    assert_eq!(fixture.outbox_events(), 1);
}

#[test]
fn cas_conflict_fails_closed_without_partial_write() {
    let fixture = TracerFixture::new();
    let error = fixture
        .dispatch_rename(&fixture.workflow, "冲突名", 99)
        .unwrap_err();
    assert_eq!(error.code(), "revision_conflict");
    let record = fixture.record();
    assert_eq!(record.name, "原名");
    assert_eq!(record.presentation_revision, 1);
    assert_eq!(fixture.receipts(), 0);
    assert_eq!(fixture.outbox_events(), 0);
}

#[test]
fn events_publish_only_after_commit_and_reconcile_repairs_fault_window() {
    let fixture = TracerFixture::new();
    let mut subscription = fixture
        .kernel
        .subscribe_events(fixture.kernel.current_event_cursor())
        .unwrap();
    let command_id = CommandId::new();

    // 故障注入:目标事务已 commit(effect + receipt + outbox 落库),但
    // service finalize 与 publication 之前"崩溃"。
    let error = fixture
        .kernel
        .dispatch_rename_with_fault(
            fixture.rename_request(command_id.clone(), &fixture.workflow, "崩溃窗口", 1),
            Some(FaultPoint::AfterTargetCommit),
        )
        .unwrap_err();
    assert_eq!(error.code(), "internal_error", "故障注入以内部错误表面化");
    assert_eq!(fixture.record().name, "崩溃窗口", "目标事务确已提交");
    assert_eq!(fixture.receipts(), 1);
    assert_eq!(fixture.pending_outbox(), 1);
    assert!(
        subscription.poll().unwrap().is_empty(),
        "publication 前事件绝不可见"
    );

    // 恢复:reconcile 以 target receipt 为权威,不重放业务写,只补发事件。
    let outcome = fixture
        .kernel
        .reconcile_command(&fixture.project, &command_id)
        .unwrap();
    assert!(matches!(
        outcome,
        ReconcileOutcome::Applied(crate::command::CommandOutcome::Applied { replayed: true, .. })
    ));
    let events = subscription.poll().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(event_revision(&events[0]), fixture.revisions());
    assert_eq!(fixture.pending_outbox(), 0);

    // 崩溃窗口后同 id 重试:命中 receipt,幂等且不产生第二条事件。
    let retry = fixture
        .kernel
        .dispatch(fixture.rename_request(command_id, &fixture.workflow, "崩溃窗口", 1))
        .unwrap();
    assert!(matches!(
        retry,
        KernelOutcome::Applied { replayed: true, .. }
    ));
    assert_eq!(fixture.record().presentation_revision, 2);
    assert_eq!(fixture.outbox_events(), 1);
}

#[test]
fn rename_to_same_name_is_idempotent_noop_without_event() {
    let fixture = TracerFixture::new();
    let outcome = fixture
        .dispatch_rename(&fixture.workflow, "原名", 1)
        .unwrap();
    assert_eq!(
        outcome,
        KernelOutcome::Applied {
            revisions: RevisionVector {
                semantic_revision: 1,
                presentation_revision: 1
            },
            replayed: false,
        }
    );
    assert_eq!(fixture.record().presentation_revision, 1);
    assert_eq!(fixture.receipts(), 1, "no-op 命令仍留下幂等 receipt");
    assert_eq!(fixture.outbox_events(), 0, "无投影变化不得产生事件");
}

#[test]
fn shutdown_assessment_is_read_only_and_reports_blockers() {
    let fixture = TracerFixture::new();
    let idle = fixture.kernel.shutdown(ShutdownIntent::Assess);
    assert!(idle.safe_to_proceed);
    assert_eq!(idle.active_workflow_runs, 0);
    assert_eq!(idle.pending_outbox_events, 0);
    assert_eq!(idle.unfinished_intents, 0);

    // 故障注入:intent 已 reserve、目标事务未执行 → 未终结 intent。
    let error = fixture
        .kernel
        .dispatch_rename_with_fault(
            fixture.rename_request(CommandId::new(), &fixture.workflow, "保留", 1),
            Some(FaultPoint::AfterIntentReserve),
        )
        .unwrap_err();
    assert_eq!(error.code(), "internal_error");

    let assessed = fixture.kernel.shutdown(ShutdownIntent::Assess);
    assert!(!assessed.safe_to_proceed);
    assert_eq!(assessed.unfinished_intents, 1);
    assert!(!assessed.blockers.is_empty());
    // 只读评估:重复调用结果一致,且不改变任何状态。
    assert_eq!(fixture.kernel.shutdown(ShutdownIntent::Assess), assessed);
    assert_eq!(fixture.pending_outbox(), 0);
    assert_eq!(fixture.receipts(), 0);
}

#[test]
fn closed_surface_fails_closed_before_migration() {
    let fixture = TracerFixture::new();

    assert!(SessionHandle::parse("sess_x").is_err());

    // attach_terminal:T3 mf-terminal 接管前显式不可用。
    let error = fixture
        .kernel
        .attach_terminal(
            SessionHandle::parse(format!("sess_{}", uuid::Uuid::now_v7())).unwrap(),
            TerminalAttach { after_seq: 0 },
        )
        .unwrap_err();
    assert_eq!(error.code(), "service_unavailable");

    // 未知工作流 handle:统一 resource_not_found,不泄露存在性差异。
    let ghost = WorkflowHandle::parse(uuid::Uuid::now_v7().to_string()).unwrap();
    let snapshot_error = fixture
        .kernel
        .snapshot(SnapshotQuery::Workflow {
            project: fixture.project.clone(),
            workflow: ghost.clone(),
        })
        .unwrap_err();
    assert_eq!(snapshot_error.code(), "resource_not_found");
    let dispatch_error = fixture.dispatch_rename(&ghost, "幽灵", 1).unwrap_err();
    assert_eq!(dispatch_error.code(), "resource_not_found");

    // 未登记的 Project Store 同样 resource_not_found。
    let unregistered = ProjectStoreHandle::generate();
    let error = fixture
        .kernel
        .snapshot(SnapshotQuery::Workflow {
            project: unregistered,
            workflow: fixture.workflow.clone(),
        })
        .unwrap_err();
    assert_eq!(error.code(), "resource_not_found");

    // 陈旧 stream epoch → resync_required。
    let stale = EventCursor {
        stream_epoch: StreamEpoch::new(),
        through_seq: 0,
    };
    assert_eq!(
        fixture.kernel.subscribe_events(stale).unwrap_err().code(),
        "resync_required"
    );

    // 空名称 envelope 拒绝。
    assert_eq!(
        fixture
            .dispatch_rename(&fixture.workflow, "   ", 1)
            .unwrap_err()
            .code(),
        "invalid_envelope"
    );
}

#[test]
fn tracer_journal_hard_cap_rotates_before_loading_unbounded_backlog() {
    let fixture = TracerFixture::new();
    let cursor = fixture.kernel.current_event_cursor();
    let mut subscription = fixture.kernel.subscribe_events(cursor.clone()).unwrap();
    let event = serde_json::json!({
        "type": "workflow.rename.applied",
        "aggregate": {
            "kind": "project_workflow",
            "handle": fixture.workflow.as_str(),
        },
        "caused_by_command_id": CommandId::new().as_str(),
        "projection": {
            "base_revision": {"semantic_revision": 1, "presentation_revision": 1},
            "aggregate_revision": {"semantic_revision": 1, "presentation_revision": 2},
            "delta": {"mode": "typed_delta", "delta_type": "workflow.rename", "data": {"name": "x"}},
        },
    })
    .to_string();
    fixture
        .store
        .with_tx(|tx| {
            let mut stmt = tx.prepare(
                "INSERT INTO projection_outbox(event_json, published_at) VALUES (?1, NULL)",
            )?;
            for _ in 0..20_001 {
                stmt.execute([&event])?;
            }
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
        subscription.poll().unwrap_err(),
        KernelProblem::ResyncRequired
    );
    let (pending, reconciled): (i64, i64) = fixture
        .store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT
                    SUM(CASE WHEN published_at IS NULL THEN 1 ELSE 0 END),
                    SUM(CASE WHEN published_at LIKE 'reconciled:%' THEN 1 ELSE 0 END)
                 FROM projection_outbox",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(anyhow::Error::from)
        })
        .unwrap();
    assert_eq!(pending, 0);
    assert_eq!(reconciled, 20_001);
}

#[test]
fn tracer_journal_preflight_counts_utf8_bytes_not_characters() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
    store
        .save_project_workflow(&ProjectWorkflowDraft {
            key: "wf-1".into(),
            name: "原名".into(),
            nodes: vec![WorkflowNodeDraft {
                key: "a".into(),
                title: "A".into(),
                instructions: String::new(),
                agent_instance_id: "inst".into(),
                deps: vec![],
            }],
            allow_unsafe_parallel: false,
        })
        .unwrap();
    let workflow = store.load_project_workflow("wf-1").unwrap().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let kernel = Arc::new(InProcessCoreKernel::new_with_journal_limits(
        service,
        ServiceIdempotencyKey::new(vec![0x62; 32]).unwrap(),
        100,
        1_024,
    ));
    let project = kernel
        .register_project_store(tmp.path(), store.clone())
        .unwrap();
    let payload = "😀".repeat(300); // 300 chars,1,200 UTF-8 bytes + envelope
    let event = serde_json::json!({
        "type": "workflow.rename.applied",
        "aggregate": {"kind": "project_workflow", "handle": workflow.public_handle},
        "caused_by_command_id": CommandId::new().as_str(),
        "projection": {
            "base_revision": {"semantic_revision": 1, "presentation_revision": 1},
            "aggregate_revision": {"semantic_revision": 1, "presentation_revision": 2},
            "delta": {"mode": "typed_delta", "delta_type": "workflow.rename", "data": {"name": payload}},
        },
    })
    .to_string();
    assert!(event.chars().count() < 1_024);
    assert!(event.len() > 1_024);
    store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO projection_outbox(event_json, published_at) VALUES (?1, NULL)",
                [&event],
            )
            .map_err(anyhow::Error::from)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        kernel.publish_pending_for_test(&project).unwrap_err(),
        KernelProblem::ResyncRequired
    );
    let mark: String = store
        .with_conn(|conn| {
            conn.query_row("SELECT published_at FROM projection_outbox", [], |row| {
                row.get(0)
            })
            .map_err(anyhow::Error::from)
        })
        .unwrap();
    assert!(mark.starts_with("reconciled:"));
}

#[test]
fn runtime_open_reconciles_old_intent_and_outbox_before_serving() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&mf_agent::project_db_path(tmp.path())).unwrap();
    store
        .save_project_workflow(&ProjectWorkflowDraft {
            key: "wf-1".into(),
            name: "原名".into(),
            nodes: vec![WorkflowNodeDraft {
                key: "a".into(),
                title: "A".into(),
                instructions: String::new(),
                agent_instance_id: "inst".into(),
                deps: vec![],
            }],
            allow_unsafe_parallel: false,
        })
        .unwrap();
    let record = store.load_project_workflow("wf-1").unwrap().unwrap();
    let workflow = WorkflowHandle::parse(record.public_handle).unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let old = Arc::new(InProcessCoreKernel::new(
        service.clone(),
        ServiceIdempotencyKey::new(vec![0x61; 32]).unwrap(),
    ));
    let project = old
        .register_project_store(tmp.path(), store.clone())
        .unwrap();
    let client_id = ClientId::parse("old-client").unwrap();
    let principal = Principal::parse("user").unwrap();
    let epoch = old.grant_controller(&client_id, &principal).unwrap();
    let command_id = CommandId::new();
    old.dispatch_rename_with_fault(
        KernelCommandRequest::new(
            command_id.clone(),
            client_id,
            principal.clone(),
            epoch,
            KernelCommand::workflow_rename(project, workflow, "已提交", 1),
        ),
        Some(FaultPoint::AfterTargetCommit),
    )
    .unwrap_err();
    drop(old);
    drop(store);

    let (runtime, _client) = InProcessKernelRuntime::for_test(
        service.clone(),
        ServiceIdempotencyKey::new(vec![0x61; 32]).unwrap(),
        ClientId::parse("new-client").unwrap(),
        principal,
    )
    .unwrap();
    let opened = runtime.open_project(tmp.path()).unwrap();
    let reopened = opened
        .legacy_store()
        .load_project_workflow("wf-1")
        .unwrap()
        .unwrap();
    assert_eq!(reopened.name, "已提交");
    assert_eq!(reopened.presentation_revision, 2, "业务效果不得重放");
    let intent_state: String = service
        .with_conn(|conn| {
            conn.query_row(
                "SELECT state FROM command_intent WHERE command_id=?1",
                [command_id.as_str()],
                |row| row.get(0),
            )
            .map_err(anyhow::Error::from)
        })
        .unwrap();
    assert_eq!(intent_state, "applied");
    let mark: String = opened
        .legacy_store()
        .with_conn(|conn| {
            conn.query_row("SELECT published_at FROM projection_outbox", [], |row| {
                row.get(0)
            })
            .map_err(anyhow::Error::from)
        })
        .unwrap();
    assert!(mark.starts_with("reconciled:"));
}

#[test]
fn wire_revisions_and_seq_serialize_as_decimal_strings() {
    let large = 9_007_199_254_740_993u64;
    let revision = serde_json::to_value(RevisionVector {
        semantic_revision: large,
        presentation_revision: large + 1,
    })
    .unwrap();
    assert_eq!(revision["semantic_revision"], large.to_string());
    assert_eq!(revision["presentation_revision"], (large + 1).to_string());
    let cursor = serde_json::to_value(EventCursor {
        stream_epoch: StreamEpoch::new(),
        through_seq: large,
    })
    .unwrap();
    assert_eq!(cursor["through_seq"], large.to_string());

    let fixture = TracerFixture::new();
    let cursor = fixture.kernel.current_event_cursor();
    let mut subscription = fixture.kernel.subscribe_events(cursor).unwrap();
    fixture
        .dispatch_rename(&fixture.workflow, "wire", 1)
        .unwrap();
    let mut event = subscription.poll().unwrap().remove(0);
    event.seq = large;
    let event = serde_json::to_value(event).unwrap();
    assert_eq!(event["seq"], large.to_string());
}

fn event_revision(event: &crate::projection::EventEnvelope) -> RevisionVector {
    event.aggregate_revision
}
