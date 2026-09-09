//! 节点输入编译(T2):模板解析、必填校验、来源追踪、业务 prompt 与
//! 结算协议段拼装的**唯一实现**。模板预览与运行派发共用同一函数,
//! 派发消费的是按 attempt 持久化的冻结记录,不在发送时重新组装。

use crate::handoff::Handoff;
use crate::model::TaskView;
use crate::workflow::{ContextPolicy, InputBinding, WorkflowNodeSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 只读模板试算。上游范围由编辑图提供，示例值不会进入 Store 或 Runtime。
#[derive(Debug, Clone, Deserialize)]
pub struct NodeInputPreviewRequest {
    #[serde(default)]
    pub goal: String,
    pub node: crate::workflow::WorkflowNodeDraft,
    #[serde(default)]
    pub allowed_upstream_keys: Vec<String>,
    #[serde(default)]
    pub upstream: HashMap<String, serde_json::Value>,
}

pub fn preview_node_input(
    request: NodeInputPreviewRequest,
) -> Result<CompiledNodeInput, Vec<String>> {
    use crate::workflow::{PluginSourcePin, WorkflowNodeDraft, WorkflowTemplateVersion};
    use crate::workflow_compiler::{CompileInput, WorkflowCompiler};
    let allowed: std::collections::HashSet<_> =
        request.allowed_upstream_keys.iter().cloned().collect();
    if request.upstream.keys().any(|key| !allowed.contains(key)) {
        return Err(vec!["示例数据包含不属于该节点上游的节点".into()]);
    }
    if allowed.contains(&request.node.key) {
        return Err(vec!["节点不能引用自身作为上游".into()]);
    }
    let mut keys: Vec<_> = allowed.into_iter().collect();
    keys.sort();
    let key = request.node.key.clone();
    let mut nodes: Vec<_> = keys
        .iter()
        .map(|key| WorkflowNodeDraft {
            key: key.clone(),
            title: key.clone(),
            agent_instance_id: "preview".into(),
            ..Default::default()
        })
        .collect();
    let mut draft = request.node;
    // 试算只使用已由图展开的祖先可见范围，不建立新的执行图。
    draft.deps = keys;
    nodes.push(draft);
    crate::workflow_validation::validate_workflow(
        crate::workflow_validation::WorkflowValidationInput::new(&nodes),
    )
    .map_err(|errors| errors.iter().map(ToString::to_string).collect::<Vec<_>>())?;
    let plugins = HashMap::from([(
        "preview".into(),
        PluginSourcePin {
            full_id: "preview".into(),
            version: "0".into(),
            content_hash: String::new(),
            contribution_id: String::new(),
        },
    )]);
    let template = WorkflowTemplateVersion {
        version_id: 0,
        template_key: "preview".into(),
        version: 0,
        nodes,
        created_at: String::new(),
    };
    let resolver = |reference: &str| {
        Ok(crate::AgentInstanceSnapshot {
            id: reference.into(),
            name: reference.into(),
            agent_type: "preview".into(),
            version: 0,
            enabled: true,
            run_mode: crate::RunMode::OneShot,
            executable: String::new(),
            argv: Vec::new(),
            env: Vec::new(),
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({}),
            sealed_secret_ids: Vec::new(),
            external_config: false,
        })
    };
    let snapshot = WorkflowCompiler::new()
        .compile(CompileInput {
            template: &template,
            directory_provider_isolates: true,
            allow_unsafe_shared_directory: false,
            agent_type_plugins: &plugins,
            resolve_instance: &resolver,
            directory_provider: None,
        })
        .map_err(|errors| {
            errors
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>()
        })?;
    let node = snapshot
        .nodes
        .iter()
        .find(|node| node.key == key)
        .expect("validated preview node");
    let mut upstream = HashMap::new();
    for (key, fields) in request.upstream {
        let Some(fields) = fields.as_object() else {
            return Err(vec![format!("上游 {key} 的示例必须是 JSON 对象")]);
        };
        let mut value = serde_json::to_value(Handoff::default()).expect("handoff serialization");
        let object = value.as_object_mut().expect("handoff object");
        object.insert("status".into(), serde_json::json!("complete"));
        object.extend(fields.clone());
        let handoff = serde_json::from_value(value)
            .map_err(|error| vec![format!("上游 {key} 示例字段非法:{error}")])?;
        upstream.insert(
            key,
            UpstreamHandoff {
                handoff,
                agent_run_id: None,
                agent_run_handle: None,
            },
        );
    }
    let task = TaskView {
        id: 0,
        public_handle: "preview".into(),
        revision: 0,
        title: "模板试算".into(),
        goal: request.goal,
        status: crate::TaskStatus::Ready,
        paused: false,
        unread: false,
        active_revision: None,
        revision_count: 0,
        created_at: String::new(),
        updated_at: String::new(),
    };
    Ok(compile_node_input(&task, node, &upstream))
}

/// 结算协议段版本:协议文案变更必须递增,冻结记录据此判断新旧。
pub const PROTOCOL_SEGMENT_VERSION: &str = "v1";

fn default_protocol_version() -> String {
    // 首版记录没有此字段；以后升级协议时也必须保留它们的 v1 归属。
    "v1".into()
}

/// 上游交接 + 来源身份(哪次 Agent Run 提交的 Handoff)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpstreamHandoff {
    pub handoff: Handoff,
    /// 来源 Agent Run 的库内 id(投影/审计用;旧数据可能缺失)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_run_id: Option<i64>,
    /// 来源 Agent Run 的公开句柄。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_run_handle: Option<String>,
}

/// 一条输入映射的解析结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingResolution {
    pub name: String,
    pub source_node_key: String,
    pub field_path: String,
    pub required: bool,
    /// 命中时的解析值(字符串形态,与模板替换一致)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// optional 绑定显式设置的默认值(命中时为 None)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    /// 缺失原因(必填缺失 = 阻止启动;optional 缺失 = 使用默认值/空)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_reason: Option<String>,
    /// 值的来源(上游存在时)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<BindingSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingSource {
    pub node_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_run_handle: Option<String>,
}

/// 一次节点输入的编译结果(冻结进 `node_inputs` 的载荷)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledNodeInput {
    pub node_key: String,
    /// 指令模板原文(未替换)。
    pub template: String,
    /// 输入映射解析结果(含来源与缺失说明)。
    pub bindings: Vec<BindingResolution>,
    /// 替换 `${inputs.*}` 与 `${nodes.*}` 后的工作说明。
    pub resolved_instructions: String,
    /// 上游原始输出的摘要快照(legacy 策略注入 prompt 的祖先摘要)。
    #[serde(default)]
    pub upstream_summaries: Vec<UpstreamSummary>,
    /// 最终业务 prompt(不含结算协议段;用户可编辑的部分)。
    pub business_prompt: String,
    /// 结算协议段(只读)。
    pub protocol_segment: String,
    #[serde(default = "default_protocol_version")]
    pub protocol_segment_version: String,
    /// 生效的上下文策略(冻结时的解释)。
    pub context_policy: ContextPolicy,
    /// 缺失的必填绑定名(非空 = 不得启动 Agent)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_required: Vec<String>,
    // ── 渲染上下文(R3:统一重渲染所需;覆盖后按同一路径重建 prompt)──
    /// 节点标题(渲染段落用)。
    #[serde(default)]
    pub node_title: String,
    /// 任务标题/目标(渲染段落用)。
    #[serde(default)]
    pub task_title: String,
    #[serde(default)]
    pub task_goal: String,
    /// 验收说明原文。
    #[serde(default)]
    pub acceptance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// ${nodes.*} 已替换、${inputs.*} 未替换的中间模板(R3 重渲染入口;
    /// 旧记录缺省为空,重渲染回退到 resolved_instructions)。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nodes_resolved_template: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpstreamSummary {
    pub node_key: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_run_handle: Option<String>,
}

/// 严格解析 Handoff 字段:路径不存在返回 None(区别于空值)。
pub fn resolve_binding_value(handoff: &Handoff, field_path: &str) -> Option<String> {
    let path = field_path.trim();
    if path.is_empty() {
        return None;
    }
    match path {
        "summary" => return Some(handoff.summary.clone()),
        "status" => return Some(handoff.status.clone()),
        "changed_files" => return Some(handoff.changed_files.join("\n")),
        "artifacts" => return Some(handoff.artifacts.join("\n")),
        "blockers" => return Some(handoff.blockers.join("\n")),
        "recommendations" => return Some(handoff.recommendations.join("\n")),
        _ => {}
    }
    let is_output = path == "output" || path.starts_with("output.");
    if !is_output {
        return None;
    }
    let json_path = path["output".len()..].trim_start_matches('.');
    let mut value = handoff.output.clone();
    if json_path.is_empty() {
        return serde_json::to_string_pretty(&value).ok();
    }
    for segment in json_path.split('.') {
        let next = match &value {
            serde_json::Value::Object(map) => map.get(segment).cloned(),
            _ => None,
        };
        value = next?;
    }
    match value {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Null => None,
        other => serde_json::to_string_pretty(&other).ok(),
    }
}

/// 编译一次节点输入(模板预览与派发共用)。
///
/// - `context_policy = explicit_only`:prompt 只含显式选择的内容
///   (输入映射解析值 + 模板内显式引用),不自动注入祖先摘要;
/// - `legacy_ancestors`(旧语义):自动附上全部祖先摘要,再替换显式引用;
/// - 必填绑定缺失进入 `missing_required`(调用方阻止启动)。
pub fn compile_node_input(
    task: &TaskView,
    node: &WorkflowNodeSnapshot,
    upstream: &HashMap<String, UpstreamHandoff>,
) -> CompiledNodeInput {
    let policy = ContextPolicy::resolve(node.context_policy);
    let mut bindings = Vec::with_capacity(node.input_bindings.len());
    let mut missing_required = Vec::new();
    for binding in &node.input_bindings {
        let mut resolution = BindingResolution {
            name: binding.name.clone(),
            source_node_key: binding.source_node_key.clone(),
            field_path: binding.field_path.clone(),
            required: binding.required,
            value: None,
            default_value: if binding.required {
                None
            } else {
                binding.default_value.clone()
            },
            missing_reason: None,
            source: None,
        };
        match upstream.get(&binding.source_node_key) {
            None => {
                resolution.missing_reason =
                    Some(format!("上游节点 `{}` 尚无交接", binding.source_node_key));
                if binding.required {
                    missing_required.push(binding.name.clone());
                }
            }
            Some(source) => {
                resolution.source = Some(BindingSource {
                    node_key: binding.source_node_key.clone(),
                    agent_run_handle: source.agent_run_handle.clone(),
                });
                match resolve_binding_value(&source.handoff, &binding.field_path) {
                    Some(value) => resolution.value = Some(value),
                    None => {
                        resolution.missing_reason = Some(format!(
                            "上游 `{}` 的交接没有字段 `{}`",
                            binding.source_node_key, binding.field_path
                        ));
                        if binding.required {
                            missing_required.push(binding.name.clone());
                        }
                    }
                }
            }
        }
        bindings.push(resolution);
    }

    // 模板替换(R3 统一渲染):先 ${nodes.*}(上游引用),再 ${inputs.*}
    // (本节点映射)。中间形态 nodes_resolved_template 随记录保存——覆盖
    // 绑定值后从它重放进同一条渲染路径,不做无约束字符串替换。
    let nodes_resolved_template = substitute_node_references(&node.instructions, upstream);
    let resolved_instructions = substitute_inputs(&nodes_resolved_template, &bindings);
    let mut upstream_summaries = Vec::new();
    if policy == ContextPolicy::LegacyAncestors && !upstream.is_empty() {
        let mut keys: Vec<&String> = upstream.keys().collect();
        keys.sort();
        for key in &keys {
            let source = &upstream[*key];
            upstream_summaries.push(UpstreamSummary {
                node_key: (*key).clone(),
                summary: source.handoff.summary.clone(),
                agent_run_handle: source.agent_run_handle.clone(),
            });
        }
    }
    let business_prompt = render_business_prompt(RenderParts {
        node_title: &node.title,
        task_title: &task.title,
        task_goal: &task.goal,
        policy,
        upstream_summaries: &upstream_summaries,
        bindings: &bindings,
        acceptance: &node.acceptance_criteria,
        output_schema: node.output_schema.as_ref(),
        resolved_instructions: &resolved_instructions,
    });

    CompiledNodeInput {
        node_key: node.key.clone(),
        template: node.instructions.clone(),
        bindings,
        resolved_instructions,
        upstream_summaries,
        business_prompt,
        protocol_segment: protocol_segment().to_string(),
        protocol_segment_version: PROTOCOL_SEGMENT_VERSION.into(),
        context_policy: policy,
        missing_required,
        node_title: node.title.clone(),
        task_title: task.title.clone(),
        task_goal: task.goal.clone(),
        acceptance: node.acceptance_criteria.clone(),
        output_schema: node.output_schema.clone(),
        nodes_resolved_template,
    }
}

/// 统一渲染的输入部件(编译与覆盖重渲染共用)。
pub struct RenderParts<'a> {
    pub node_title: &'a str,
    pub task_title: &'a str,
    pub task_goal: &'a str,
    pub policy: ContextPolicy,
    pub upstream_summaries: &'a [UpstreamSummary],
    pub bindings: &'a [BindingResolution],
    pub acceptance: &'a str,
    pub output_schema: Option<&'a serde_json::Value>,
    pub resolved_instructions: &'a str,
}

/// 从部件重建业务 prompt(唯一渲染路径;编译与 [`apply_overrides`] 共用)。
pub fn render_business_prompt(parts: RenderParts<'_>) -> String {
    let RenderParts {
        node_title,
        task_title,
        task_goal,
        policy,
        upstream_summaries,
        bindings,
        acceptance,
        output_schema,
        resolved_instructions,
    } = parts;
    let mut sections = vec![format!(
        "你在 MonkeyFence 中执行工作流节点「{node_title}」(任务: {task_title})。"
    )];
    if !task_goal.trim().is_empty() {
        sections.push(format!("任务目标:\n{task_goal}"));
    }
    if policy == ContextPolicy::LegacyAncestors && !upstream_summaries.is_empty() {
        let lines: Vec<String> = std::iter::once("上游交接:".to_string())
            .chain(upstream_summaries.iter().map(|upstream| {
                format!(
                    "- {}: {}",
                    upstream.node_key,
                    if upstream.summary.trim().is_empty() {
                        "(无摘要)"
                    } else {
                        &upstream.summary
                    }
                )
            }))
            .collect();
        sections.push(lines.join("\n"));
    }
    // 输入映射区块:explicit_only 下这是上游数据进入 prompt 的唯一通道;
    // legacy 下作为显式选择的补充呈现。
    let resolved_pairs: Vec<&BindingResolution> = bindings
        .iter()
        .filter(|binding| binding.value.is_some() || binding.default_value.is_some())
        .collect();
    if !resolved_pairs.is_empty() {
        let lines: Vec<String> = std::iter::once("输入映射:".to_string())
            .chain(resolved_pairs.iter().map(|binding| {
                let value = binding
                    .value
                    .as_deref()
                    .or(binding.default_value.as_deref())
                    .unwrap_or("");
                format!("- {}: {}", binding.name, value)
            }))
            .collect();
        sections.push(lines.join("\n"));
    }
    if !acceptance.trim().is_empty() {
        sections.push(format!("验收说明:\n{acceptance}"));
    }
    if let Some(schema) = output_schema {
        sections.push(format!(
            "输出要求（Handoff.output）:\n{}\n成功结算时使用 mfctl step complete --summary \"总结\" --output-json '<符合上述要求的 JSON 对象>' 提交。",
            serde_json::to_string_pretty(schema).unwrap_or_default()
        ));
    }
    sections.push(format!(
        "工作说明:\n{}",
        if resolved_instructions.trim().is_empty() {
            "(无补充说明)".to_string()
        } else {
            resolved_instructions.to_string()
        }
    ));
    sections.join("\n\n")
}

/// 结算协议段(只读;版本见 [`PROTOCOL_SEGMENT_VERSION`])。
pub fn protocol_segment() -> &'static str {
    "完成后必须显式结算(MonkeyFence 已通过 MF_RUN_TOKEN 环境变量注入本步骤令牌,不要打印或复制令牌):\n- 成功:mfctl step complete --summary \"一句话总结\"\n- 失败:mfctl step fail --reason \"失败原因\"\n\n规则:\n- 不要提交、推送或搁置任何版本控制变更。\n- 需要用户决策时,直接在终端中说明并等待。\n- 令牌仅对本步骤有效;重复提交相同结算是幂等的,提交冲突结算会被拒绝。"
}

/// 冻结记录 → 发送给 CLI 的完整 prompt(业务 + 协议)。
pub fn full_prompt(compiled: &CompiledNodeInput) -> String {
    format!(
        "{}\n\n{}",
        compiled.business_prompt, compiled.protocol_segment
    )
}

/// 替换 `${inputs.<name>}`:命中解析值/默认值;缺失必填以占位说明呈现
/// (调用方仍会因 missing_required 阻止启动)。
pub fn substitute_inputs(text: &str, bindings: &[BindingResolution]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("${inputs.") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "${inputs.".len()..];
        let Some(end_rel) = after.find('}') else {
            out.push_str("${inputs.");
            out.push_str(after);
            return out;
        };
        let reference = &after[..end_rel];
        let name: String = reference
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if name.is_empty() {
            out.push_str(&rest[at..at + "${inputs.".len() + end_rel + 1]);
            rest = &after[end_rel + 1..];
            continue;
        }
        match bindings
            .iter()
            .find(|binding| binding.name == name)
            .and_then(|binding| {
                binding
                    .value
                    .as_deref()
                    .or(binding.default_value.as_deref())
            }) {
            Some(value) => out.push_str(value),
            None => out.push_str(&format!("(输入映射 `{name}` 缺失)")),
        }
        rest = &after[end_rel + 1..];
    }
    out.push_str(rest);
    out
}

/// 替换 `${nodes.<key>.<路径>}` 变量:引用上游节点最近一次 Handoff。
/// 支持的路径:``(整个 Handoff 的 JSON)、`.summary`、`.status`、
/// `.changed_files`、`.artifacts`、`.blockers`、`.recommendations`、
/// `.verification`、`.output`(自定义 JSON)与其下的嵌套键。
/// 上游无输出(跳过/未结算)替换为占位说明,不保留原始变量。
pub fn substitute_node_references(
    text: &str,
    upstream: &HashMap<String, UpstreamHandoff>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("${nodes.") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "${nodes.".len()..];
        // 取到最近的 `}` 作为引用结束(引用语法内不嵌套花括号)
        let Some(end_rel) = after.find('}') else {
            out.push_str("${nodes.");
            out.push_str(after);
            return out;
        };
        let reference = &after[..end_rel];
        let key: String = reference
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if key.is_empty() {
            out.push_str(&rest[at..at + "${nodes.".len() + end_rel + 1]);
            rest = &after[end_rel + 1..];
            continue;
        }
        let path = reference[key.len()..].trim_start_matches('.');
        match upstream.get(&key) {
            Some(source) => out.push_str(&resolve_handoff_path(&source.handoff, path)),
            None => out.push_str(&format!("(上游节点 `{key}` 暂无交接输出)")),
        }
        rest = &after[end_rel + 1..];
    }
    out.push_str(rest);
    out
}

/// 解析 Handoff 输出路径(`output.` 之后的部分)。
fn resolve_handoff_path(handoff: &Handoff, path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        return serde_json::to_string_pretty(handoff).unwrap_or_default();
    }
    match path {
        "summary" => return handoff.summary.clone(),
        "status" => return handoff.status.clone(),
        "changed_files" => return handoff.changed_files.join("\n"),
        "artifacts" => return handoff.artifacts.join("\n"),
        "blockers" => return handoff.blockers.join("\n"),
        "recommendations" => return handoff.recommendations.join("\n"),
        _ => {}
    }
    // `output` 或 `output.<嵌套键...>`:走自定义 JSON
    let is_output = path == "output" || path.starts_with("output.");
    let json_path = if is_output {
        path["output".len()..].trim_start_matches('.')
    } else {
        path
    };
    let mut value = if is_output {
        handoff.output.clone()
    } else {
        serde_json::to_value(handoff).unwrap_or(serde_json::Value::Null)
    };
    if json_path.is_empty() {
        return serde_json::to_string_pretty(&value).unwrap_or_default();
    }
    for segment in json_path.split('.') {
        let next = match &value {
            serde_json::Value::Object(map) => map.get(segment).cloned(),
            serde_json::Value::Array(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|i| items.get(i).cloned()),
            _ => None,
        };
        value = next.unwrap_or(serde_json::Value::Null);
    }
    serde_json::to_string_pretty(&value).unwrap_or_default()
}

/// T3 人工检查的用户覆盖:绑定补值 + 业务 prompt 覆盖。
/// 只在 review_state=awaiting_review 期间可写;确认后随记录冻结。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InputOverrides {
    /// 绑定名 → 覆盖值(必填缺失的补值入口;不改变来源记录)。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub binding_values: std::collections::BTreeMap<String, String>,
    /// 覆盖后的完整业务 prompt(None = 使用自动生成值)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub business_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<String>,
}

/// 应用覆盖:绑定值替换(来源标注为用户覆盖)、prompt 覆盖。
/// 自动生成值保留在记录里,界面可对比差异;上游原始 Handoff 不变。
pub fn apply_overrides(
    mut compiled: CompiledNodeInput,
    overrides: &InputOverrides,
) -> CompiledNodeInput {
    let binding_changed = !overrides.binding_values.is_empty();
    for binding in &mut compiled.bindings {
        if let Some(value) = overrides.binding_values.get(&binding.name) {
            binding.value = Some(value.clone());
            binding.missing_reason = None;
            binding.source = None; // 覆盖值不来自上游
        }
    }
    compiled
        .missing_required
        .retain(|name| !overrides.binding_values.contains_key(name));
    if binding_changed {
        // R3 统一渲染:绑定值变化后从中间模板重放进同一渲染路径,
        // 重建 resolved_instructions 与 business_prompt(补值同样生效,
        // 不残留"(输入映射 x 缺失)"占位);旧记录缺中间形态时以已替换
        // 说明为底再替换一次 inputs。
        let base = if compiled.nodes_resolved_template.is_empty() {
            compiled.resolved_instructions.clone()
        } else {
            compiled.nodes_resolved_template.clone()
        };
        compiled.resolved_instructions = substitute_inputs(&base, &compiled.bindings);
        let rendered = render_business_prompt(RenderParts {
            node_title: &compiled.node_title,
            task_title: &compiled.task_title,
            task_goal: &compiled.task_goal,
            policy: compiled.context_policy,
            upstream_summaries: &compiled.upstream_summaries,
            bindings: &compiled.bindings,
            acceptance: &compiled.acceptance,
            output_schema: compiled.output_schema.as_ref(),
            resolved_instructions: &compiled.resolved_instructions,
        });
        compiled.business_prompt = rendered;
    }
    // 显式 business_prompt 覆盖具有最高优先级,整体替换渲染结果
    if let Some(prompt) = &overrides.business_prompt {
        compiled.business_prompt = prompt.clone();
    }
    compiled
}

// ── 输出约束校验(成功 Settlement 的事务前置检查) ────────────────────

/// 按首版支持的 JSON Schema 子集(object/properties/required/items/type)
/// 校验 Handoff.output。返回全部违规(可操作错误)。
pub fn validate_output_against_schema(
    schema: &serde_json::Value,
    output: &serde_json::Value,
) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    validate_value(schema, output, "$", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_value(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(expected) = object.get("type").and_then(|ty| ty.as_str()) {
        let ok = match expected {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "boolean" => value.is_boolean(),
            _ => true,
        };
        if !ok {
            errors.push(format!(
                "{path}:类型应为 {expected},实际是 {}",
                json_type_of(value)
            ));
            return;
        }
    }
    if let (Some(properties), Some(map)) = (object.get("properties"), value.as_object()) {
        if let Some(properties) = properties.as_object() {
            for (name, child_schema) in properties {
                if let Some(child) = map.get(name) {
                    validate_value(child_schema, child, &format!("{path}.{name}"), errors);
                }
            }
        }
    }
    if let (Some(required), Some(map)) = (object.get("required"), value.as_object()) {
        if let Some(names) = required.as_array() {
            for name in names {
                if let Some(name) = name.as_str() {
                    if !map.contains_key(name) {
                        errors.push(format!("{path}:缺少必填字段 `{name}`"));
                    }
                }
            }
        }
    }
    if let (Some(items_schema), Some(items)) = (object.get("items"), value.as_array()) {
        for (index, item) in items.iter().enumerate() {
            validate_value(items_schema, item, &format!("{path}[{index}]"), errors);
        }
    }
}

fn json_type_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// 供输入记录摘要使用的短描述(运行总快照不携带长 prompt)。
pub fn input_summary(compiled: &CompiledNodeInput) -> String {
    if !compiled.missing_required.is_empty() {
        format!("输入缺失必填项:{}", compiled.missing_required.join("、"))
    } else {
        format!(
            "映射 {} 项已解析",
            compiled
                .bindings
                .iter()
                .filter(|b| b.value.is_some())
                .count()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff_with_output(output: serde_json::Value) -> Handoff {
        Handoff {
            status: "complete".into(),
            summary: "上游摘要".into(),
            changed_files: vec![],
            artifacts: vec![],
            verification: None,
            blockers: vec![],
            recommendations: vec![],
            output,
            raw_log_ref: None,
        }
    }

    fn task() -> TaskView {
        TaskView {
            id: 1,
            public_handle: "task_x".into(),
            revision: 1,
            title: "任务标题".into(),
            goal: "目标".into(),
            status: crate::model::TaskStatus::Running,
            paused: false,
            unread: false,
            active_revision: Some(1),
            revision_count: 1,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn snapshot(
        key: &str,
        instructions: &str,
        bindings: Vec<InputBinding>,
    ) -> WorkflowNodeSnapshot {
        WorkflowNodeSnapshot {
            key: key.into(),
            title: "节点".into(),
            instructions: instructions.into(),
            instance: crate::agent_instance::AgentInstanceSnapshot {
                id: "inst".into(),
                name: "inst".into(),
                agent_type: "generic-command".into(),
                version: 1,
                enabled: true,
                run_mode: crate::model::RunMode::OneShot,
                executable: "cmd".into(),
                argv: vec![],
                env: vec![],
                config: serde_json::json!({}),
                execution_contract: serde_json::json!({}),
                sealed_secret_ids: vec![],
                external_config: false,
            },
            deps: vec![],
            plugin: None,
            acceptance_criteria: String::new(),
            output_schema: None,
            input_bindings: bindings,
            context_policy: None,
            require_input_review: false,
        }
    }

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
    fn required_binding_missing_blocks_and_reports() {
        let node = snapshot(
            "b",
            "读取 ${inputs.report}",
            vec![binding("report", "a", "output.report_path", true)],
        );
        let mut upstream = HashMap::new();
        upstream.insert(
            "a".to_string(),
            UpstreamHandoff {
                handoff: handoff_with_output(serde_json::json!({"other": 1})),
                agent_run_id: Some(7),
                agent_run_handle: Some("run_abc".into()),
            },
        );
        let compiled = compile_node_input(&task(), &node, &upstream);
        assert_eq!(compiled.missing_required, vec!["report".to_string()]);
        assert!(compiled
            .business_prompt
            .contains("(输入映射 `report` 缺失)"));

        // 命中字段:值进入映射区块与模板替换
        upstream.insert(
            "a".to_string(),
            UpstreamHandoff {
                handoff: handoff_with_output(serde_json::json!({"report_path": "reports/r1.md"})),
                agent_run_id: Some(7),
                agent_run_handle: Some("run_abc".into()),
            },
        );
        let compiled = compile_node_input(&task(), &node, &upstream);
        assert!(compiled.missing_required.is_empty());
        assert!(compiled.business_prompt.contains("reports/r1.md"));
        assert!(compiled.business_prompt.contains("输入映射:"));
    }

    #[test]
    fn explicit_only_policy_omits_ancestor_summaries() {
        let mut node = snapshot("b", "读取 ${nodes.a.summary}", vec![]);
        node.context_policy = Some(ContextPolicy::ExplicitOnly);
        let mut upstream = HashMap::new();
        upstream.insert(
            "a".to_string(),
            UpstreamHandoff {
                handoff: handoff_with_output(serde_json::json!({})),
                agent_run_id: None,
                agent_run_handle: None,
            },
        );
        let compiled = compile_node_input(&task(), &node, &upstream);
        assert!(
            compiled.business_prompt.contains("上游摘要"),
            "显式引用仍然替换"
        );
        assert!(
            !compiled.business_prompt.contains("上游交接:"),
            "explicit_only 不自动注入祖先摘要区块"
        );

        node.context_policy = Some(ContextPolicy::LegacyAncestors);
        let compiled = compile_node_input(&task(), &node, &upstream);
        assert!(
            compiled.business_prompt.contains("上游交接:"),
            "legacy 策略保留祖先摘要"
        );
        assert_eq!(compiled.upstream_summaries.len(), 1);
    }

    #[test]
    fn optional_binding_uses_explicit_default_only() {
        let mut optional = binding("note", "a", "output.missing_field", false);
        optional.default_value = Some("默认说明".into());
        let node = snapshot("b", "注:${inputs.note}", vec![optional]);
        let mut upstream = HashMap::new();
        upstream.insert(
            "a".to_string(),
            UpstreamHandoff {
                handoff: handoff_with_output(serde_json::json!({})),
                agent_run_id: None,
                agent_run_handle: None,
            },
        );
        let compiled = compile_node_input(&task(), &node, &upstream);
        assert!(compiled.missing_required.is_empty(), "optional 缺失不阻止");
        assert!(compiled.business_prompt.contains("注:默认说明"));
    }

    #[test]
    fn output_schema_validation_reports_actionable_errors() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "verdict": {"type": "string"},
                "issues": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["verdict"]
        });
        assert!(validate_output_against_schema(
            &schema,
            &serde_json::json!({"verdict": "pass", "issues": ["a"]})
        )
        .is_ok());
        let errors = validate_output_against_schema(&schema, &serde_json::json!({"issues": [1]}))
            .unwrap_err();
        assert_eq!(errors.len(), 2, "缺 verdict + issues[0] 类型");
        assert!(errors.iter().any(|e| e.contains("verdict")));
    }
}
