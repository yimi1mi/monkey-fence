use crate::kernel::{CoreKernel, KernelCommand, ProjectWorkflowCommand as C};
use crate::projection_support::ProjectionFixture;
use mf_agent::WorkflowNodeDraft;

#[test]
fn semantic_command_family_uses_one_axis_and_handles() {
    let f = ProjectionFixture::new();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::AddNode {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        node: WorkflowNodeDraft {
            key: "b".into(),
            title: "B".into(),
            instructions: String::new(),
            agent_instance_id: "inst-b".into(),
            deps: vec![],
        },
        expected_semantic_revision: 1,
    }))
    .unwrap();
    let ids = f.store.workflow_node_identities("wf-projection").unwrap();
    let a = ids
        .iter()
        .find(|i| i.node_key == "node-a")
        .unwrap()
        .node_handle
        .clone();
    let b = ids
        .iter()
        .find(|i| i.node_key == "b")
        .unwrap()
        .node_handle
        .clone();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::UpdateNode {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        node_handle: b.clone(),
        title: "B2".into(),
        instructions: "work".into(),
        agent_instance_id: "inst-2".into(),
        expected_semantic_revision: 2,
    }))
    .unwrap();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::Connect {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        upstream_node_handle: a,
        downstream_node_handle: b.clone(),
        expected_semantic_revision: 3,
    }))
    .unwrap();
    let edge = f.store.workflow_edge_identities("wf-projection").unwrap()[0]
        .edge_handle
        .clone();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::Disconnect {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        edge_handle: edge,
        expected_semantic_revision: 4,
    }))
    .unwrap();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::SetUnsafeParallel {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        allow: true,
        expected_semantic_revision: 5,
    }))
    .unwrap();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::RemoveNode {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        node_handle: b,
        expected_semantic_revision: 6,
    }))
    .unwrap();
    let record = f
        .store
        .load_project_workflow("wf-projection")
        .unwrap()
        .unwrap();
    assert_eq!(record.semantic_revision, 7);
    assert_eq!(record.presentation_revision, 1);
    assert_eq!(record.nodes.len(), 1);
}

#[test]
fn move_and_viewport_only_advance_presentation() {
    let f = ProjectionFixture::new();
    let node = f.store.workflow_node_identities("wf-projection").unwrap()[0]
        .node_handle
        .clone();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::MoveNode {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        node_handle: node,
        x: 10.0,
        y: 20.0,
        expected_presentation_revision: 1,
    }))
    .unwrap();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::SetViewport {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        viewport: serde_json::json!({"x":1,"y":2,"zoom":1.2}),
        expected_presentation_revision: 2,
    }))
    .unwrap();
    let record = f
        .store
        .load_project_workflow("wf-projection")
        .unwrap()
        .unwrap();
    assert_eq!(record.semantic_revision, 1);
    assert_eq!(record.presentation_revision, 3);
    let snapshot = f
        .kernel
        .snapshot(crate::projection::SnapshotQuery::Workflow {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
        })
        .unwrap();
    let crate::projection::SnapshotData::Workflow(data) = snapshot.data else {
        panic!("expected workflow snapshot")
    };
    assert_eq!(data.nodes[0].position, Some((10.0, 20.0)));
    assert_eq!(
        data.viewport,
        Some(serde_json::json!({"x":1,"y":2,"zoom":1.2}))
    );
}

#[test]
fn create_and_delete_cas_collection_and_workflow() {
    let f = ProjectionFixture::new();
    let mut events = f
        .kernel
        .subscribe_events(f.kernel.current_event_cursor())
        .unwrap();
    let collection = f.store.workflow_collection_revision().unwrap() as u64;
    let draft = mf_agent::ProjectWorkflowDraft {
        key: "created".into(),
        name: "Created".into(),
        nodes: vec![WorkflowNodeDraft {
            key: "n".into(),
            title: "N".into(),
            instructions: String::new(),
            agent_instance_id: "inst".into(),
            deps: vec![],
        }],
        allow_unsafe_parallel: false,
    };
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::Create {
        project: f.project.clone(),
        draft,
        expected_collection_revision: collection,
    }))
    .unwrap();
    let created_events = events.poll().unwrap();
    assert_eq!(created_events.len(), 2);
    assert_eq!(created_events[0].event_type, "workflow.replace");
    assert_eq!(
        created_events[1].event_type,
        "project.workflow_collection_changed"
    );
    let record = f.store.load_project_workflow("created").unwrap().unwrap();
    let workflow = crate::handles::WorkflowHandle::parse(record.public_handle).unwrap();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::Delete {
        project: f.project.clone(),
        workflow,
        expected_collection_revision: collection + 1,
        expected_semantic_revision: 1,
        expected_presentation_revision: 1,
    }))
    .unwrap();
    let deleted_events = events.poll().unwrap();
    assert_eq!(deleted_events.len(), 2);
    assert_eq!(deleted_events[0].projection["mode"], "tombstone");
    assert_eq!(
        deleted_events[1].event_type,
        "project.workflow_collection_changed"
    );
    assert!(f.store.load_project_workflow("created").unwrap().is_none());
}

#[test]
fn typed_delta_base_mismatch_rotates_and_requires_resync() {
    let f = ProjectionFixture::new();
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::SetUnsafeParallel {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        allow: true,
        expected_semantic_revision: 1,
    }))
    .unwrap();
    let bad = serde_json::json!({"type":"workflow.set_unsafe_parallel.applied","aggregate":{"kind":"project_workflow","handle":f.workflow.as_str()},"projection":{"base_revision":{"semantic_revision":1,"presentation_revision":1},"aggregate_revision":{"semantic_revision":2,"presentation_revision":1},"delta":{"mode":"typed_delta","delta_type":"workflow.set_unsafe_parallel","data":{"allow":false}}}});
    f.insert_outbox(bad);
    assert_eq!(
        f.kernel.publish_pending_for_test(&f.project).unwrap_err(),
        crate::kernel::KernelProblem::ResyncRequired
    );
}

#[test]
fn move_node_cannot_cross_workflow_scope() {
    let f = ProjectionFixture::new();
    let collection = f.store.workflow_collection_revision().unwrap() as u64;
    f.dispatch_command(KernelCommand::ProjectWorkflow(C::Create {
        project: f.project.clone(),
        draft: mf_agent::ProjectWorkflowDraft {
            key: "wf-other-scope".into(),
            name: "Other".into(),
            nodes: vec![WorkflowNodeDraft {
                key: "other".into(),
                title: "Other".into(),
                instructions: String::new(),
                agent_instance_id: "inst".into(),
                deps: vec![],
            }],
            allow_unsafe_parallel: false,
        },
        expected_collection_revision: collection,
    }))
    .unwrap();
    let foreign = f.store.workflow_node_identities("wf-other-scope").unwrap()[0]
        .node_handle
        .clone();
    let error = f
        .dispatch_command(KernelCommand::ProjectWorkflow(C::MoveNode {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
            node_handle: foreign,
            x: 1.0,
            y: 2.0,
            expected_presentation_revision: 1,
        }))
        .unwrap_err();
    assert_eq!(error.code(), "resource_not_found");
    assert_eq!(
        f.store
            .load_project_workflow("wf-projection")
            .unwrap()
            .unwrap()
            .presentation_revision,
        1
    );
}

#[test]
fn move_node_invalid_coordinate_and_unknown_handle_have_stable_codes() {
    let f = ProjectionFixture::new();
    let invalid = f
        .dispatch_command(KernelCommand::ProjectWorkflow(C::MoveNode {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
            node_handle: "missing".into(),
            x: f64::NAN,
            y: 0.0,
            expected_presentation_revision: 1,
        }))
        .unwrap_err();
    assert_eq!(invalid.code(), "validation_failed");
    let missing = f
        .dispatch_command(KernelCommand::ProjectWorkflow(C::MoveNode {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
            node_handle: "missing".into(),
            x: 1.0,
            y: 2.0,
            expected_presentation_revision: 1,
        }))
        .unwrap_err();
    assert_eq!(missing.code(), "resource_not_found");
}
