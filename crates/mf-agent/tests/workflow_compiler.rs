//! 纯 Workflow Compiler(Orchestration Task 2):DAG/变量/实例/插件/并行安全
//! 全量校验,错误按稳定节点键排序;成功时冻结为不可变快照。
//!
//! 实例解析经注入闭包:编译器保持纯函数,不触目录库。

use std::collections::HashMap;

use mf_agent::agent_instance::AgentInstanceSnapshot;
use mf_agent::workflow::{PluginSourcePin, WorkflowNodeDraft, WorkflowTemplateVersion};
use mf_agent::workflow_compiler::{CompileInput, WorkflowCompiler};
use mf_agent::RunMode;

fn compiler() -> WorkflowCompiler {
    WorkflowCompiler::new()
}

fn snapshot_of(
    id: &str,
    enabled: bool,
    run_mode: RunMode,
    agent_type: &str,
) -> AgentInstanceSnapshot {
    AgentInstanceSnapshot {
        id: id.into(),
        name: id.into(),
        agent_type: agent_type.into(),
        version: 1,
        enabled,
        run_mode,
        executable: "agent.exe".into(),
        argv: vec![],
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({ "completion": "process-exit" }),
        sealed_secret_ids: vec![],
    }
}

/// 默认实例表:oneshot/generic-command,全部启用。
fn default_instances() -> Vec<(&'static str, AgentInstanceSnapshot)> {
    vec![(
        "inst-a",
        snapshot_of("inst-a", true, RunMode::OneShot, "generic-command"),
    )]
}

fn node(key: &str, deps: &[&str], instructions: &str, instance: &str) -> WorkflowNodeDraft {
    WorkflowNodeDraft {
        key: key.into(),
        title: key.into(),
        instructions: instructions.into(),
        agent_instance_id: instance.into(),
        deps: deps.iter().map(|s| s.to_string()).collect(),
    }
}

fn template_with(nodes: Vec<WorkflowNodeDraft>) -> WorkflowTemplateVersion {
    WorkflowTemplateVersion {
        version_id: 1,
        template_key: "t".into(),
        version: 1,
        nodes,
        created_at: String::new(),
    }
}

fn resolver_for<'i>(
    instances: &'i [(&'static str, AgentInstanceSnapshot)],
) -> impl Fn(&str) -> anyhow::Result<AgentInstanceSnapshot> + 'i {
    move |id| {
        instances
            .iter()
            .find(|(key, _)| *key == id)
            .map(|(_, snapshot)| snapshot.clone())
            .ok_or_else(|| anyhow::anyhow!("Agent Instance `{id}` 不存在"))
    }
}

static AGENT_TYPES: std::sync::OnceLock<HashMap<String, PluginSourcePin>> =
    std::sync::OnceLock::new();

fn agent_types() -> &'static HashMap<String, PluginSourcePin> {
    AGENT_TYPES.get_or_init(|| {
        let mut map = HashMap::new();
        for agent_type in ["generic-command", "claude-code"] {
            map.insert(
                agent_type.to_string(),
                PluginSourcePin {
                    full_id: "builtin.core".into(),
                    version: "1.0.0".into(),
                    content_hash: format!("hash-{agent_type}"),
                    contribution_id: String::new(),
                },
            );
        }
        map
    })
}

fn input<'a>(
    template: &'a WorkflowTemplateVersion,
    resolve: &'a dyn Fn(&str) -> anyhow::Result<AgentInstanceSnapshot>,
) -> CompileInput<'a> {
    CompileInput {
        template,
        directory_provider: None,
        directory_provider_isolates: true,
        allow_unsafe_shared_directory: false,
        agent_type_plugins: agent_types(),
        resolve_instance: resolve,
    }
}

fn compile_default(
    template: &WorkflowTemplateVersion,
) -> Result<mf_agent::workflow::WorkflowSnapshot, Vec<mf_agent::workflow_compiler::CompileError>> {
    let instances = default_instances();
    let resolve = resolver_for(&instances);
    compiler().compile(input(template, &resolve))
}

// ---------- 成功路径 ----------

#[test]
fn valid_template_compiles_to_frozen_snapshot() {
    let template = template_with(vec![
        node("build", &[], "构建", "inst-a"),
        node(
            "package",
            &["build"],
            "读取 ${nodes.build.output.report_path} 打包",
            "inst-a",
        ),
    ]);
    let snapshot = compile_default(&template).unwrap();
    assert_eq!(snapshot.template_key, "t");
    assert_eq!(snapshot.nodes.len(), 2);
    assert_eq!(snapshot.nodes[0].key, "build");
    assert_eq!(snapshot.nodes[1].deps, vec!["build".to_string()]);
    assert_eq!(snapshot.nodes[1].instance.executable, "agent.exe");
}

// ---------- DAG 校验 ----------

#[test]
fn rejects_cycle_and_unknown_output_reference_together() {
    // build → package → build 成环;package 引用了不存在的 ghost 输出
    let template = template_with(vec![
        node("build", &["package"], "构建", "inst-a"),
        node(
            "package",
            &["build"],
            "读取 ${nodes.ghost.output.x}",
            "inst-a",
        ),
    ]);
    let errors = compile_default(&template).unwrap_err();
    assert!(
        errors.iter().any(|e| e.code == "cycle"),
        "应包含 cycle: {errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.code == "unknown-output"),
        "应包含 unknown-output: {errors:?}"
    );
}

#[test]
fn self_dependency_is_cycle() {
    let template = template_with(vec![node("solo", &["solo"], "自依赖", "inst-a")]);
    let errors = compile_default(&template).unwrap_err();
    assert!(errors.iter().any(|e| e.code == "cycle"));
}

#[test]
fn rejects_unknown_dependency_and_duplicate_key() {
    let template = template_with(vec![
        node("a", &["ghost"], "引用幽灵依赖", "inst-a"),
        node("a", &[], "重复键", "inst-a"),
    ]);
    let errors = compile_default(&template).unwrap_err();
    assert!(errors.iter().any(|e| e.code == "unknown-dep"));
    assert!(errors.iter().any(|e| e.code == "duplicate-key"));
}

// ---------- 变量校验 ----------

#[test]
fn variable_must_reference_upstream_node() {
    // package 引用了存在但非上游的 audit 节点输出
    let template = template_with(vec![
        node("build", &[], "构建", "inst-a"),
        node("audit", &[], "审计", "inst-a"),
        node(
            "package",
            &["build"],
            "读取 ${nodes.audit.output.x}",
            "inst-a",
        ),
    ]);
    let errors = compile_default(&template).unwrap_err();
    assert!(
        errors.iter().any(|e| e.code == "non-upstream-output"),
        "应包含 non-upstream-output: {errors:?}"
    );
}

#[test]
fn transitive_upstream_reference_is_allowed() {
    let template = template_with(vec![
        node("build", &[], "构建", "inst-a"),
        node("test", &["build"], "测试", "inst-a"),
        node(
            "package",
            &["test"],
            "读 ${nodes.build.output.a} 与 ${nodes.test.output.b}",
            "inst-a",
        ),
    ]);
    assert!(compile_default(&template).is_ok());
}

// ---------- 实例与插件校验 ----------

#[test]
fn rejects_unknown_and_disabled_instance() {
    let template = template_with(vec![
        node("a", &[], "缺失实例", "missing"),
        node("b", &[], "禁用实例", "inst-off"),
    ]);
    let instances = vec![(
        "inst-off",
        snapshot_of("inst-off", false, RunMode::OneShot, "generic-command"),
    )];
    let resolve = resolver_for(&instances);
    let errors = compiler().compile(input(&template, &resolve)).unwrap_err();
    assert!(errors.iter().any(|e| e.code == "unknown-instance"));
    assert!(errors.iter().any(|e| e.code == "instance-disabled"));
}

#[test]
fn rejects_unknown_agent_type_plugin() {
    let instances = vec![(
        "inst-x",
        snapshot_of("inst-x", true, RunMode::OneShot, "ghost-plugin.agent"),
    )];
    let template = template_with(vec![node("a", &[], "x", "inst-x")]);
    let resolve = resolver_for(&instances);
    let errors = compiler().compile(input(&template, &resolve)).unwrap_err();
    assert!(errors.iter().any(|e| e.code == "plugin-missing"));
}

// ---------- 并行安全 ----------

#[test]
fn parallel_interactive_session_reuse_is_rejected() {
    // 两个无祖先关系的并行节点复用同一个 Interactive 实例
    let instances = vec![(
        "inst-i",
        snapshot_of("inst-i", true, RunMode::Interactive, "claude-code"),
    )];
    let template = template_with(vec![
        node("left", &[], "左", "inst-i"),
        node("right", &[], "右", "inst-i"),
        node("join", &["left", "right"], "汇合", "inst-i"),
    ]);
    let resolve = resolver_for(&instances);
    let errors = compiler().compile(input(&template, &resolve)).unwrap_err();
    assert!(
        errors.iter().any(|e| e.code == "parallel-session"),
        "应包含 parallel-session: {errors:?}"
    );
}

#[test]
fn unsafe_parallel_requires_flag_when_directory_cannot_isolate() {
    let template = template_with(vec![
        node("left", &[], "左", "inst-a"),
        node("right", &[], "右", "inst-a"),
        node("join", &["left", "right"], "汇合", "inst-a"),
    ]);
    let instances = default_instances();

    // 目录不隔离且未开风险开关 → 拒绝
    let resolve = resolver_for(&instances);
    let mut bad = input(&template, &resolve);
    bad.directory_provider_isolates = false;
    let errors = compiler().compile(bad).unwrap_err();
    assert!(
        errors.iter().any(|e| e.code == "unsafe-parallel"),
        "应包含 unsafe-parallel: {errors:?}"
    );

    // 用户显式开启共享目录并行风险开关 → 允许(自行承担冲突)
    let mut risky = input(&template, &resolve);
    risky.directory_provider_isolates = false;
    risky.allow_unsafe_shared_directory = true;
    assert!(compiler().compile(risky).is_ok());

    // 串行工作流不受目录隔离能力影响
    let serial = template_with(vec![
        node("a", &[], "一步", "inst-a"),
        node("b", &["a"], "两步", "inst-a"),
    ]);
    let mut serial_input = input(&serial, &resolve);
    serial_input.directory_provider_isolates = false;
    assert!(compiler().compile(serial_input).is_ok());
}

// ---------- 错误顺序 ----------

#[test]
fn errors_are_deterministic_in_node_key_order() {
    let template = template_with(vec![
        node("zeta", &["ghost-z"], "z", "missing-z"),
        node("alpha", &["ghost-a"], "a", "missing-a"),
    ]);
    let errors = compile_default(&template).unwrap_err();
    let nodes: Vec<&str> = errors.iter().map(|e| e.node.as_str()).collect();
    let mut sorted = nodes.clone();
    sorted.sort();
    assert_eq!(nodes, sorted, "错误应按节点键稳定排序: {errors:?}");
    assert!(errors.iter().all(|e| !e.message.is_empty()));
}
