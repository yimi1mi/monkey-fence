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
        ..Default::default()
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
        ..Default::default()
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

// ── T1:输入映射与输出约束 ────────────────────────────────────────────

use mf_agent::workflow::InputBinding;

fn binding(name: &str, source: &str, path: &str, required: bool) -> InputBinding {
    InputBinding {
        name: name.into(),
        source_node_key: source.into(),
        field_path: path.into(),
        required,
        default_value: None,
    }
}

#[test]
fn input_binding_to_transitive_ancestor_is_accepted() {
    // a→b→c:c 绑定 a(跨多跳的传递祖先)合法
    let mut c = node("c", &["b"]);
    c.input_bindings = vec![binding("report", "a", "output.report_path", true)];
    let nodes = vec![node("a", &[]), node("b", &["a"]), c];

    assert!(validate_workflow(WorkflowValidationInput::new(&nodes)).is_ok());
}

#[test]
fn input_binding_to_non_ancestor_or_unknown_node_is_rejected() {
    let mut bound = node("consumer", &[]);
    bound.input_bindings = vec![
        binding("x", "sibling", "summary", true), // 不是上游(并行兄弟)
        binding("y", "ghost", "summary", true),   // 未知节点
    ];
    let nodes = vec![node("sibling", &[]), bound];

    let actual: std::collections::BTreeSet<_> = codes(&nodes).into_iter().collect();
    assert_eq!(
        actual,
        [
            WorkflowValidationCode::BindingNotAncestor,
            WorkflowValidationCode::BindingUnknownNode,
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn input_binding_shape_is_validated() {
    let mut bad = node("consumer", &["up"]);
    bad.input_bindings = vec![InputBinding {
        name: "非法 名".into(),
        source_node_key: "up".into(),
        field_path: "  ".into(),
        required: true,
        default_value: Some("默认".into()), // 必填 + 默认值
    }];
    let nodes = vec![node("up", &[]), bad];

    assert_eq!(codes(&nodes).len(), 3, "名称非法/路径空/必填默认值各报一项");
    assert_eq!(
        codes(&nodes)[0],
        WorkflowValidationCode::InvalidInputBinding
    );
}

#[test]
fn output_schema_must_be_object_with_supported_keywords() {
    let ok = serde_json::json!({
        "type": "object",
        "properties": {
            "report_path": {"type": "string", "description": "报告路径"},
            "issues": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["report_path"],
    });
    let mut good = node("good", &[]);
    good.output_schema = Some(ok);
    assert!(validate_workflow(WorkflowValidationInput::new(&[good])).is_ok());

    let mut missing_type = node("no-type", &[]);
    missing_type.output_schema = Some(serde_json::json!({"properties": {}}));
    assert_eq!(
        codes(&[missing_type]),
        vec![WorkflowValidationCode::InvalidOutputSchema]
    );

    let mut unsupported = node("fancy", &[]);
    unsupported.output_schema = Some(serde_json::json!({
        "type": "object", "patternProperties": {"^x": {"type": "string"}}
    }));
    assert_eq!(
        codes(&[unsupported]),
        vec![WorkflowValidationCode::InvalidOutputSchema]
    );
}
