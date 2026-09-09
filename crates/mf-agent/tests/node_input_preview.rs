use mf_agent::node_input::{preview_node_input, NodeInputPreviewRequest};
use mf_agent::workflow::{ContextPolicy, InputBinding, WorkflowNodeDraft};
use serde_json::json;
use std::collections::HashMap;

fn request() -> NodeInputPreviewRequest {
    NodeInputPreviewRequest {
        goal: "检查报告".into(),
        node: WorkflowNodeDraft {
            key: "review".into(),
            title: "审查".into(),
            agent_instance_id: "saved-agent".into(),
            instructions: "读取 ${inputs.report}，摘要 ${nodes.build.summary}".into(),
            context_policy: Some(ContextPolicy::ExplicitOnly),
            input_bindings: vec![InputBinding {
                name: "report".into(),
                source_node_key: "build".into(),
                field_path: "output.files.report".into(),
                required: true,
                default_value: None,
            }],
            output_schema: Some(
                json!({"type":"object","properties":{"verdict":{"type":"string"}},"required":["verdict"]}),
            ),
            ..Default::default()
        },
        allowed_upstream_keys: vec!["build".into()],
        upstream: HashMap::from([(
            "build".into(),
            json!({"summary":"构建通过","output":{"files":{"report":"report.md"}}}),
        )]),
    }
}

#[test]
fn preview_uses_nested_values_and_real_prompt_contract() {
    let compiled = preview_node_input(request()).unwrap();
    assert_eq!(
        compiled.resolved_instructions,
        "读取 report.md，摘要 构建通过"
    );
    assert!(compiled.missing_required.is_empty());
    assert!(compiled.business_prompt.contains("verdict"));
    assert!(compiled.business_prompt.contains("--output-json"));
    assert!(!compiled.business_prompt.contains("上游交接:"));
    assert!(compiled.bindings[0]
        .source
        .as_ref()
        .unwrap()
        .agent_run_handle
        .is_none());
    let mut encoded = serde_json::to_value(&compiled).unwrap();
    encoded["protocol_segment_version"] = json!("historical-protocol");
    let restored: mf_agent::node_input::CompiledNodeInput =
        serde_json::from_value(encoded).unwrap();
    assert_eq!(restored.protocol_segment_version, "historical-protocol");
}

#[test]
fn missing_example_is_diagnostic_and_unknown_reference_is_rejected() {
    let mut missing = request();
    missing.upstream.clear();
    assert_eq!(
        preview_node_input(missing).unwrap().missing_required,
        vec!["report"]
    );
    let mut bad = request();
    bad.node.instructions = "${nodes.unrelated.summary}".into();
    assert!(preview_node_input(bad).is_err());
    let mut unknown = request();
    unknown.upstream.insert("unrelated".into(), json!({}));
    assert!(preview_node_input(unknown).is_err());
}

#[test]
fn preview_checks_output_contract_and_supports_all_handoff_fields() {
    let mut bad = request();
    bad.node.output_schema = Some(json!({"type":"object","unsupported":true}));
    assert!(preview_node_input(bad).is_err());
    let mut fields = request();
    fields.node.instructions =
        "产物 ${nodes.build.artifacts} 验证 ${nodes.build.verification.command}".into();
    fields.upstream.get_mut("build").unwrap()["artifacts"] = json!(["bundle.zip"]);
    fields.upstream.get_mut("build").unwrap()["verification"] = json!({"command":"cargo test"});
    let compiled = preview_node_input(fields).unwrap();
    assert!(compiled.resolved_instructions.contains("bundle.zip"));
    assert!(compiled.resolved_instructions.contains("cargo test"));
}
