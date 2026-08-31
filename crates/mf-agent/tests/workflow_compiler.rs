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
        external_config: false,
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

// ---------- default-cli 保留引用(Task 3) ----------

/// 模拟插件感知 resolver(生产为 mf::app_ctx::PluginInstanceResolver):
/// 普通字符串按实例表解析;`default-cli:<完整贡献 ID>` 合成临时快照。
fn plugin_aware_resolver<'i>(
    instances: &'i [(&'static str, AgentInstanceSnapshot)],
    available_cli: &'static [&'static str],
) -> impl Fn(&str) -> anyhow::Result<AgentInstanceSnapshot> + 'i {
    move |reference| {
        let Some(full_contribution_id) = reference.strip_prefix("default-cli:") else {
            return resolver_for(instances)(reference);
        };
        if !available_cli.contains(&full_contribution_id) {
            anyhow::bail!(
                "默认 CLI `{full_contribution_id}` 不存在或所属插件未启用(引用必须是完整贡献 ID)"
            );
        }
        Ok(AgentInstanceSnapshot {
            id: format!("default-cli:{full_contribution_id}"),
            name: format!("{full_contribution_id} 默认 CLI"),
            agent_type: full_contribution_id.to_string(),
            version: 0,
            enabled: true,
            run_mode: RunMode::OneShot,
            executable: "agent.exe".into(),
            argv: vec![],
            env: vec![],
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({ "completion": "manual" }),
            sealed_secret_ids: vec![],
            external_config: true,
        })
    }
}

fn plugin_index_with(extra_agent_type: &str) -> HashMap<String, PluginSourcePin> {
    let mut map = agent_types().clone();
    map.insert(
        extra_agent_type.to_string(),
        PluginSourcePin {
            full_id: "test.plugin".into(),
            version: "0.1.0".into(),
            content_hash: "hash-test-plugin".into(),
            contribution_id: String::new(),
        },
    );
    map
}

#[test]
fn default_cli_reference_compiles_to_frozen_external_config_snapshot() {
    // 检测到的 default-cli 引用编译为冻结快照:external_config 与完整
    // agent_type(插件 pin 依据)都保留;普通实例引用行为不变
    let instances = default_instances();
    let resolve = plugin_aware_resolver(&instances, &["test.plugin.agent"]);
    let template = template_with(vec![
        node("a", &[], "做 A", "inst-a"),
        node("b", &["a"], "做 B", "default-cli:test.plugin.agent"),
    ]);
    let mut input = input(&template, &resolve);
    let index = plugin_index_with("test.plugin.agent");
    input.agent_type_plugins = &index;
    let snapshot = compiler().compile(input).unwrap();
    let a = snapshot.nodes.iter().find(|n| n.key == "a").unwrap();
    let b = snapshot.nodes.iter().find(|n| n.key == "b").unwrap();
    assert!(!a.instance.external_config, "保存实例仍是隔离配置");
    assert!(
        b.instance.external_config,
        "default-cli 合成快照 external_config 必须为 true"
    );
    assert_eq!(b.instance.agent_type, "test.plugin.agent");
    assert_eq!(
        b.plugin.as_ref().map(|p| p.full_id.as_str()),
        Some("test.plugin"),
        "插件 pin 按快照中的完整 agent_type 冻结"
    );
    // 快照经序列化往返不丢失 external_config(Revision 存的就是它)
    let json = serde_json::to_string(&snapshot).unwrap();
    let round: mf_agent::workflow::WorkflowSnapshot = serde_json::from_str(&json).unwrap();
    assert!(round.nodes[1].instance.external_config);
}

#[test]
fn default_cli_unknown_contribution_id_is_stable_error() {
    // 未检测/未知贡献 ID:同一稳定错误,以 unknown-instance 报在节点上
    let instances = default_instances();
    let resolve = plugin_aware_resolver(&instances, &[]);
    let template = template_with(vec![node(
        "a",
        &[],
        "做 A",
        "default-cli:ghost.plugin.agent",
    )]);
    let mut input = input(&template, &resolve);
    let index = plugin_index_with("ghost.plugin.agent");
    input.agent_type_plugins = &index;
    let errors = compiler().compile(input).unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "unknown-instance");
    assert_eq!(errors[0].node, "a");
    assert!(
        errors[0].message.contains("完整贡献 ID"),
        "错误必须说明引用必须是完整贡献 ID: {}",
        errors[0].message
    );
}

#[test]
fn plain_instance_ids_resolve_through_plugin_aware_resolver_unchanged() {
    // 插件感知 resolver 下普通实例 ID 仍按实例表解析(既有行为不变)
    let instances = default_instances();
    let resolve = plugin_aware_resolver(&instances, &["test.plugin.agent"]);
    let template = template_with(vec![node("a", &[], "做 A", "inst-a")]);
    let snapshot = compiler().compile(input(&template, &resolve)).unwrap();
    assert_eq!(snapshot.nodes[0].instance.id, "inst-a");
    assert!(!snapshot.nodes[0].instance.external_config);
    // 未知普通实例 ID 仍报 unknown-instance
    let missing = template_with(vec![node("a", &[], "做 A", "inst-missing")]);
    let errors = compiler().compile(input(&missing, &resolve)).unwrap_err();
    assert_eq!(errors[0].code, "unknown-instance");
}
