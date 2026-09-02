use crate::command::ServiceIdempotencyKey;
use crate::handles::{ClientId, Principal, WorkflowRunHandle};
use crate::kernel::{CoreKernel, InProcessCoreKernel};
use crate::project_registry::ServiceStore;
use crate::projection::{SnapshotData, SnapshotQuery};
use mf_agent::{PipelineDraft, RunStatus, SessionPolicy, StepDraft, StepStatus, Store, TaskStatus};
use std::sync::Arc;

#[test]
fn restart_snapshot_preserves_needs_you_and_exit_is_not_settlement() {
    let tmp = tempfile::tempdir().unwrap();
    let project_path = tmp.path().join("project-v7.db");
    let (workflow_run, run_handle, capability_token) = {
        let store = Store::open(&project_path).unwrap();
        let task = store.create_task("review", "inspect the result").unwrap();
        store
            .create_draft_revision(
                task.id,
                &PipelineDraft {
                    steps: vec![StepDraft {
                        key: "work".into(),
                        title: "work".into(),
                        instructions: "do it".into(),
                        agent_profile: "instance".into(),
                        session_policy: SessionPolicy::Fresh,
                        deps: vec![],
                    }],
                },
            )
            .unwrap();
        store.activate_revision(task.id).unwrap();
        let step = store.task_steps(task.id).unwrap()[0].clone();
        let session = store
            .create_session(None, "mock", "instance", "work")
            .unwrap();
        let run = store
            .create_run(task.id, step.id, step.revision_id, Some(session.id))
            .unwrap();
        // Agent CLI 退出/done 只进入 awaiting-outcome；没有 Settlement，
        // 因而 outcome 必须保持空并在重启后继续 Needs You。
        store
            .set_run_status(run.id, RunStatus::AwaitingOutcome)
            .unwrap();
        store
            .set_step_status(step.id, StepStatus::AwaitingOutcome)
            .unwrap();
        store
            .set_task_status(task.id, TaskStatus::NeedsYou)
            .unwrap();
        store.set_task_unread(task.id, true).unwrap();
        store
            .ask_question(task.id, Some(step.id), Some(run.id), "如何结算？")
            .unwrap();
        (
            WorkflowRunHandle::parse(task.public_handle).unwrap(),
            run.public_handle,
            run.capability_token,
        )
    };

    let reopened = Store::open(&project_path).unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let kernel = Arc::new(InProcessCoreKernel::new(
        service,
        ServiceIdempotencyKey::new(vec![0x61; 32]).unwrap(),
    ));
    let project = kernel.register_project_store(tmp.path(), reopened).unwrap();
    let client = ClientId::parse("snapshot-controller").unwrap();
    let principal = Principal::parse("snapshot-user").unwrap();
    kernel.grant_controller(&client, &principal).unwrap();

    let snapshot = kernel
        .snapshot(SnapshotQuery::WorkflowRun {
            project,
            workflow_run: workflow_run.clone(),
        })
        .unwrap();
    let wire = serde_json::to_string(&snapshot).unwrap();
    assert!(!wire.contains(&capability_token));
    assert!(!wire.contains("capability_token"));
    let SnapshotData::WorkflowRun(data) = snapshot.data else {
        panic!("expected workflow run snapshot")
    };
    assert_eq!(data.workflow_run, workflow_run);
    assert_eq!(data.status, "needs-you");
    assert!(data.needs_you);
    assert!(data.unread);
    assert_eq!(data.steps.len(), 1);
    assert_eq!(data.steps[0].status, "awaiting-outcome");
    assert_eq!(data.agent_runs.len(), 1);
    assert_eq!(data.agent_runs[0].agent_run.as_str(), run_handle);
    assert_eq!(data.agent_runs[0].status, "awaiting-outcome");
    assert_eq!(data.agent_runs[0].outcome, None);
    assert_eq!(data.open_questions.len(), 1);
    assert_eq!(data.open_questions[0].question, "如何结算？");
}
