//! Project Workflow 的 UI-neutral DAG 领域校验契约。
//!
//! Kernel 与 legacy editor 都只依赖 `mf-agent` 的同一个公共校验缝隙，
//! 不依赖 GPUI、Store 或编译期实例解析。

use mf_agent::{
    validate_workflow, WorkflowNodeDraft, WorkflowValidationCode, WorkflowValidationInput,
};

fn node(key: &str, deps: &[&str]) -> WorkflowNodeDraft {
    WorkflowNodeDraft {
        key: key.into(),
        title: format!("节点 {key}"),
        instructions: String::new(),
        agent_instance_id: "inst-a".into(),
        deps: deps.iter().map(|dep| (*dep).to_string()).collect(),
    }
}

fn codes(nodes: &[WorkflowNodeDraft]) -> Vec<WorkflowValidationCode> {
    validate_workflow(WorkflowValidationInput::new(nodes))
        .expect_err("测试图应被拒绝")
        .iter()
        .map(|error| error.code())
        .collect()
}

#[test]
fn duplicate_node_key_is_rejected_by_public_validator() {
    let nodes = vec![node("build", &[]), node("build", &[])];

    assert_eq!(
        codes(&nodes),
        vec![WorkflowValidationCode::DuplicateNodeKey]
    );
}

#[test]
fn dependency_must_reference_a_known_node() {
    let nodes = vec![node("publish", &["ghost"])];

    assert_eq!(
        codes(&nodes),
        vec![WorkflowValidationCode::UnknownDependency]
    );
    let errors = validate_workflow(WorkflowValidationInput::new(&nodes)).unwrap_err();
    let error = errors.iter().next().unwrap();
    assert_eq!(error.node_key(), "publish");
    assert_eq!(error.dependency_key(), Some("ghost"));
}

#[test]
fn self_dependency_has_a_distinct_error() {
    let nodes = vec![node("build", &["build"])];

    assert_eq!(codes(&nodes), vec![WorkflowValidationCode::SelfDependency]);
}

#[test]
fn dependency_cycle_is_rejected_while_a_valid_dag_passes() {
    let cyclic = vec![
        node("build", &["review"]),
        node("test", &["build"]),
        node("review", &["test"]),
    ];
    assert_eq!(codes(&cyclic), vec![WorkflowValidationCode::Cycle]);

    let valid = vec![
        node("build", &[]),
        node("test", &["build"]),
        node("review", &["test"]),
    ];
    assert!(validate_workflow(WorkflowValidationInput::new(&valid)).is_ok());
}

#[test]
fn workflow_requires_at_least_one_node() {
    assert_eq!(codes(&[]), vec![WorkflowValidationCode::EmptyWorkflow]);
}

#[test]
fn node_required_fields_are_not_blank() {
    let nodes = vec![WorkflowNodeDraft {
        key: " \t".into(),
        title: "  ".into(),
        instructions: String::new(),
        agent_instance_id: "\n".into(),
        deps: vec![],
    }];

    let actual: std::collections::BTreeSet<_> = codes(&nodes).into_iter().collect();
    assert_eq!(
        actual,
        [
            WorkflowValidationCode::EmptyNodeKey,
            WorkflowValidationCode::EmptyNodeTitle,
            WorkflowValidationCode::EmptyAgentInstanceId,
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn node_and_dependency_keys_use_the_stable_key_alphabet() {
    let mut invalid_node = node("bad key", &[]);
    invalid_node.title = "非法键".into();
    let invalid_dependency = node("publish", &["bad dep"]);

    let actual: std::collections::BTreeSet<_> = codes(&[invalid_node, invalid_dependency])
        .into_iter()
        .collect();
    assert_eq!(
        actual,
        [
            WorkflowValidationCode::InvalidNodeKey,
            WorkflowValidationCode::InvalidDependencyKey,
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn validation_errors_are_stable_independent_of_node_input_order() {
    let first = vec![node("zeta", &["missing-z"]), node("alpha", &["missing-a"])];
    let second = vec![node("alpha", &["missing-a"]), node("zeta", &["missing-z"])];
    let signature = |nodes: &[WorkflowNodeDraft]| {
        validate_workflow(WorkflowValidationInput::new(nodes))
            .unwrap_err()
            .iter()
            .map(|error| {
                (
                    error.node_key().to_string(),
                    error.code(),
                    error.dependency_key().map(str::to_string),
                )
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(signature(&first), signature(&second));
    assert_eq!(signature(&first)[0].0, "alpha");
}
