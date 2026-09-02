use crate::command::ServiceIdempotencyKey;
use crate::kernel::{CoreKernel, InProcessCoreKernel};
use crate::project_registry::ServiceStore;
use crate::projection::{SnapshotData, SnapshotQuery};
use mf_agent::{PipelineDraft, SessionPolicy, StepDraft, StepStatus, Store, TaskStatus};
use std::sync::Arc;

fn workflow_run(store: &Store, title: &str, step_status: StepStatus, task_status: TaskStatus) {
    let task = store.create_task(title, title).unwrap();
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
    store.set_step_status(step.id, step_status).unwrap();
    store.set_task_status(task.id, task_status).unwrap();
}

#[test]
fn workspace_snapshot_counts_needs_you_per_run_and_excludes_blocked_descendants() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("alpha");
    let root_b = tmp.path().join("beta");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    let store_a = Store::open(&root_a.join("project-v7.db")).unwrap();
    let store_b = Store::open(&root_b.join("project-v7.db")).unwrap();
    workflow_run(
        &store_a,
        "needs-you run",
        StepStatus::Failed,
        TaskStatus::NeedsYou,
    );
    workflow_run(
        &store_b,
        "blocked descendant only",
        StepStatus::Blocked,
        TaskStatus::Running,
    );
    store_a
        .create_session(None, "pty", "instance", "agent")
        .unwrap();

    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let kernel = Arc::new(InProcessCoreKernel::new(
        service,
        ServiceIdempotencyKey::new(vec![0x62; 32]).unwrap(),
    ));
    kernel.register_project_store(&root_a, store_a).unwrap();
    kernel.register_project_store(&root_b, store_b).unwrap();
    let snapshot = kernel.snapshot(SnapshotQuery::Workspace).unwrap();
    let wire = serde_json::to_string(&snapshot).unwrap();
    assert!(!wire.contains(root_a.to_string_lossy().as_ref()));
    assert!(!wire.contains(root_b.to_string_lossy().as_ref()));
    assert!(!wire.contains("capability_token"));
    let SnapshotData::Workspace(data) = snapshot.data else {
        panic!("expected workspace snapshot")
    };
    assert_eq!(data.projects.len(), 2);
    assert_eq!(data.active_workflow_runs, 2);
    assert_eq!(data.needs_you_count, 1, "徽标按 Workflow Run 计数");
    let needs = data
        .projects
        .iter()
        .flat_map(|project| &project.workflow_runs)
        .find(|run| run.title == "needs-you run")
        .unwrap();
    assert!(needs.needs_you);
    assert_eq!(needs.reason_count, 1);
    assert!(needs.focus_step.is_some());
    let blocked = data
        .projects
        .iter()
        .flat_map(|project| &project.workflow_runs)
        .find(|run| run.title == "blocked descendant only")
        .unwrap();
    assert!(!blocked.needs_you);
    assert_eq!(blocked.reason_count, 0, "纯 blocked 后代不产生提醒");
}
