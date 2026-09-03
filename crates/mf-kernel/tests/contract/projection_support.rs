use crate::command::{FaultPoint, ServiceIdempotencyKey};
use crate::handles::{ClientId, CommandId, Principal, ProjectStoreHandle, WorkflowHandle};
use crate::kernel::{
    CoreKernel, InProcessCoreKernel, KernelCommand, KernelCommandRequest, KernelOutcome,
    KernelProblem,
};
use crate::limits::JournalLimits;
use crate::project_registry::ServiceStore;
use mf_agent::workflow::WorkflowNodeDraft;
use mf_agent::{ProjectWorkflowDraft, Store};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct ProjectionFixture {
    pub(crate) _tmp: tempfile::TempDir,
    pub(crate) store: Arc<Store>,
    pub(crate) kernel: Arc<InProcessCoreKernel>,
    pub(crate) project: ProjectStoreHandle,
    pub(crate) workflow: WorkflowHandle,
    client: ClientId,
    principal: Principal,
    epoch: u64,
}

pub(crate) struct AdditionalProject {
    pub(crate) _tmp: tempfile::TempDir,
    pub(crate) store: Arc<Store>,
    pub(crate) project: ProjectStoreHandle,
    pub(crate) workflow: WorkflowHandle,
}

impl ProjectionFixture {
    pub(crate) fn new() -> Self {
        Self::with_limits(JournalLimits::default())
    }

    pub(crate) fn with_limits(limits: JournalLimits) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("project-v7.db")).unwrap();
        store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: "wf-projection".into(),
                name: "初始".into(),
                nodes: vec![WorkflowNodeDraft {
                    key: "node-a".into(),
                    title: "A".into(),
                    instructions: String::new(),
                    agent_instance_id: "instance-a".into(),
                    deps: vec![],
                }],
                allow_unsafe_parallel: false,
            })
            .unwrap();
        let record = store
            .load_project_workflow("wf-projection")
            .unwrap()
            .unwrap();
        let workflow = WorkflowHandle::parse(record.public_handle).unwrap();
        let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
        let kernel = Arc::new(InProcessCoreKernel::new_with_projection_limits(
            service,
            ServiceIdempotencyKey::new(vec![0x24; 32]).unwrap(),
            limits,
        ));
        let project = kernel
            .register_project_store(tmp.path(), store.clone())
            .unwrap();
        let client = ClientId::parse("client-projection-contract").unwrap();
        let principal = Principal::parse("user-projection-contract").unwrap();
        let epoch = kernel.grant_controller_checked(&client, &principal).unwrap();
        Self {
            _tmp: tmp,
            store,
            kernel,
            project,
            workflow,
            client,
            principal,
            epoch,
        }
    }

    pub(crate) fn rename(
        &self,
        name: &str,
        expected_presentation_revision: u64,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.rename_target(
            &self.project,
            &self.workflow,
            name,
            expected_presentation_revision,
        )
    }

    pub(crate) fn rename_target(
        &self,
        project: &ProjectStoreHandle,
        workflow: &WorkflowHandle,
        name: &str,
        expected_presentation_revision: u64,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.kernel.dispatch(self.rename_request(
            project,
            workflow,
            name,
            expected_presentation_revision,
        ))
    }

    pub(crate) fn rename_request(
        &self,
        project: &ProjectStoreHandle,
        workflow: &WorkflowHandle,
        name: &str,
        expected_presentation_revision: u64,
    ) -> KernelCommandRequest {
        KernelCommandRequest::new(
            CommandId::new(),
            self.client.clone(),
            self.principal.clone(),
            self.epoch,
            KernelCommand::workflow_rename(
                project.clone(),
                workflow.clone(),
                name,
                expected_presentation_revision,
            ),
        )
    }

    pub(crate) fn dispatch_command(
        &self,
        command: KernelCommand,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.kernel.dispatch(KernelCommandRequest::new(
            CommandId::new(),
            self.client.clone(),
            self.principal.clone(),
            self.epoch,
            command,
        ))
    }

    pub(crate) fn add_project(&self, key: &str) -> AdditionalProject {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("project-v7.db")).unwrap();
        store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: key.into(),
                name: key.into(),
                nodes: vec![WorkflowNodeDraft {
                    key: "node-a".into(),
                    title: "A".into(),
                    instructions: String::new(),
                    agent_instance_id: "instance-a".into(),
                    deps: vec![],
                }],
                allow_unsafe_parallel: false,
            })
            .unwrap();
        let record = store.load_project_workflow(key).unwrap().unwrap();
        let workflow = WorkflowHandle::parse(record.public_handle).unwrap();
        let project = self
            .kernel
            .register_project_store(tmp.path(), store.clone())
            .unwrap();
        AdditionalProject {
            _tmp: tmp,
            store,
            project,
            workflow,
        }
    }

    pub(crate) fn rename_with_fault(
        &self,
        name: &str,
        expected_presentation_revision: u64,
        fault: FaultPoint,
    ) -> Result<KernelOutcome, KernelProblem> {
        self.kernel.dispatch_rename_with_fault(
            KernelCommandRequest::new(
                CommandId::new(),
                self.client.clone(),
                self.principal.clone(),
                self.epoch,
                KernelCommand::workflow_rename(
                    self.project.clone(),
                    self.workflow.clone(),
                    name,
                    expected_presentation_revision,
                ),
            ),
            Some(fault),
        )
    }

    pub(crate) fn insert_outbox(&self, event: Value) -> i64 {
        let event = serde_json::to_string(&event).unwrap();
        self.store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO projection_outbox(event_json, published_at) VALUES (?1, NULL)",
                    [event],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .unwrap()
    }

    pub(crate) fn published_at(&self, outbox_id: i64) -> Option<String> {
        self.store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT published_at FROM projection_outbox WHERE outbox_id=?1",
                    [outbox_id],
                    |row| row.get(0),
                )
                .map_err(anyhow::Error::from)
            })
            .unwrap()
    }

    pub(crate) fn raw_event(
        &self,
        event_type: &str,
        base_presentation: u64,
        aggregate_presentation: u64,
        delta: Value,
    ) -> Value {
        serde_json::json!({
            "type": format!("{event_type}.applied"),
            "aggregate": {
                "kind": "project_workflow",
                "handle": self.workflow.as_str(),
            },
            "caused_by_command_id": CommandId::new().as_str(),
            "projection_critical": true,
            "projection": {
                "base_revision": {
                    "semantic_revision": 1,
                    "presentation_revision": base_presentation,
                },
                "aggregate_revision": {
                    "semantic_revision": 1,
                    "presentation_revision": aggregate_presentation,
                },
                "delta": delta,
            }
        })
    }
}

pub(crate) fn tiny_limits() -> JournalLimits {
    JournalLimits {
        journal_max_events: 4,
        journal_max_bytes: 64 * 1024,
        journal_min_age_secs: 1_800,
        journal_event_max_bytes: 16 * 1024,
        client_event_queue_max_events: 2,
        client_event_queue_max_bytes: 32 * 1024,
        ..JournalLimits::default()
    }
}
