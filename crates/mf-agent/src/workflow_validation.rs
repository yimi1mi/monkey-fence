//! Project Workflow 的 UI-neutral DAG 领域校验。
//!
//! 本模块是 Kernel 与交互客户端共用的纯函数缝隙：不触碰 Store、
//! Agent Instance 目录或任何 UI 框架。

use std::collections::{HashMap, HashSet};

use crate::workflow::WorkflowNodeDraft;

/// 校验器的封闭输入 DTO。
#[derive(Debug, Clone, Copy)]
pub struct WorkflowValidationInput<'a> {
    nodes: &'a [WorkflowNodeDraft],
}

impl<'a> WorkflowValidationInput<'a> {
    pub fn new(nodes: &'a [WorkflowNodeDraft]) -> Self {
        Self { nodes }
    }
}

/// 稳定、封闭的领域错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkflowValidationCode {
    EmptyWorkflow,
    EmptyNodeKey,
    InvalidNodeKey,
    EmptyNodeTitle,
    EmptyAgentInstanceId,
    InvalidDependencyKey,
    DuplicateNodeKey,
    SelfDependency,
    UnknownDependency,
    Cycle,
    InvalidInputBinding,
    BindingUnknownNode,
    BindingNotAncestor,
    InvalidOutputSchema,
}

impl WorkflowValidationCode {
    /// 供日志、projection detail 与跨前端诊断使用的稳定机器码。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmptyWorkflow => "empty_workflow",
            Self::EmptyNodeKey => "empty_node_key",
            Self::InvalidNodeKey => "invalid_node_key",
            Self::EmptyNodeTitle => "empty_node_title",
            Self::EmptyAgentInstanceId => "empty_agent_instance_id",
            Self::InvalidDependencyKey => "invalid_dependency_key",
            Self::DuplicateNodeKey => "duplicate_node_key",
            Self::SelfDependency => "self_dependency",
            Self::UnknownDependency => "unknown_dependency",
            Self::Cycle => "workflow_cycle",
            Self::InvalidInputBinding => "invalid_input_binding",
            Self::BindingUnknownNode => "binding_unknown_node",
            Self::BindingNotAncestor => "binding_not_ancestor",
            Self::InvalidOutputSchema => "invalid_output_schema",
        }
    }
}

/// 一项可定位的工作流领域校验错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowValidationError {
    code: WorkflowValidationCode,
    node_key: String,
    dependency_key: Option<String>,
    /// 机器可读补充定位(如输出约束里不支持的关键字、绑定里非法的
    /// 字段);不参与稳定排序键。
    detail: String,
}

impl WorkflowValidationError {
    fn new(
        code: WorkflowValidationCode,
        node_key: impl Into<String>,
        dependency_key: Option<String>,
    ) -> Self {
        Self {
            code,
            node_key: node_key.into(),
            dependency_key,
            detail: String::new(),
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn code(&self) -> WorkflowValidationCode {
        self.code
    }

    pub fn node_key(&self) -> &str {
        &self.node_key
    }

    pub fn dependency_key(&self) -> Option<&str> {
        self.dependency_key.as_deref()
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for WorkflowValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            WorkflowValidationCode::EmptyWorkflow => write!(formatter, "工作流至少需要一个节点"),
            WorkflowValidationCode::EmptyNodeKey => write!(formatter, "节点键不能为空"),
            WorkflowValidationCode::InvalidNodeKey => write!(
                formatter,
                "节点键 `{}` 非法（仅允许字母、数字、-、_）",
                self.node_key
            ),
            WorkflowValidationCode::EmptyNodeTitle => {
                write!(formatter, "节点 `{}` 的标题不能为空", self.node_key)
            }
            WorkflowValidationCode::EmptyAgentInstanceId => write!(
                formatter,
                "节点 `{}` 必须指派 Agent Instance",
                self.node_key
            ),
            WorkflowValidationCode::InvalidDependencyKey => write!(
                formatter,
                "节点 `{}` 的依赖键 `{}` 非法",
                self.node_key,
                self.dependency_key.as_deref().unwrap_or_default()
            ),
            WorkflowValidationCode::DuplicateNodeKey => {
                write!(formatter, "节点键 `{}` 重复", self.node_key)
            }
            WorkflowValidationCode::SelfDependency => {
                write!(formatter, "节点 `{}` 不能依赖自身", self.node_key)
            }
            WorkflowValidationCode::UnknownDependency => write!(
                formatter,
                "节点 `{}` 依赖未知节点 `{}`",
                self.node_key,
                self.dependency_key.as_deref().unwrap_or_default()
            ),
            WorkflowValidationCode::Cycle => {
                write!(
                    formatter,
                    "工作流存在依赖环（涉及节点 `{}`）",
                    self.node_key
                )
            }
            WorkflowValidationCode::InvalidInputBinding => write!(
                formatter,
                "节点 `{}` 的输入映射无效：{}",
                self.node_key,
                if self.detail.is_empty() {
                    self.dependency_key.as_deref().unwrap_or_default()
                } else {
                    &self.detail
                }
            ),
            WorkflowValidationCode::BindingUnknownNode => write!(
                formatter,
                "节点 `{}` 的输入映射指向未知节点 `{}`",
                self.node_key,
                self.dependency_key.as_deref().unwrap_or_default()
            ),
            WorkflowValidationCode::BindingNotAncestor => write!(
                formatter,
                "节点 `{}` 的输入映射来源 `{}` 不是它的（传递）上游",
                self.node_key,
                self.dependency_key.as_deref().unwrap_or_default()
            ),
            WorkflowValidationCode::InvalidOutputSchema => write!(
                formatter,
                "节点 `{}` 的输出约束无效：{}",
                self.node_key,
                if self.detail.is_empty() {
                    "必须是 object 形态的 JSON Schema"
                } else {
                    &self.detail
                }
            ),
        }
    }
}

/// 一次校验返回的全部错误；调用方不能构造不受支持的错误形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowValidationErrors {
    errors: Vec<WorkflowValidationError>,
}

impl WorkflowValidationErrors {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &WorkflowValidationError> {
        self.errors.iter()
    }

    pub fn len(&self) -> usize {
        self.errors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn into_vec(self) -> Vec<WorkflowValidationError> {
        self.errors
    }
}

impl std::fmt::Display for WorkflowValidationErrors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index != 0 {
                formatter.write_str("；")?;
            }
            error.fmt(formatter)?;
        }
        Ok(())
    }
}

impl std::error::Error for WorkflowValidationErrors {}

/// 校验完整 Project Workflow DAG，失败时一次返回全部稳定排序的错误。
pub fn validate_workflow(
    input: WorkflowValidationInput<'_>,
) -> Result<(), WorkflowValidationErrors> {
    let mut errors = Vec::new();
    if input.nodes.is_empty() {
        errors.push(WorkflowValidationError::new(
            WorkflowValidationCode::EmptyWorkflow,
            "",
            None,
        ));
    }
    let mut keys = HashSet::with_capacity(input.nodes.len());
    for node in input.nodes {
        if node.key.trim().is_empty() {
            errors.push(WorkflowValidationError::new(
                WorkflowValidationCode::EmptyNodeKey,
                node.key.clone(),
                None,
            ));
        } else if !is_valid_key(&node.key) {
            errors.push(WorkflowValidationError::new(
                WorkflowValidationCode::InvalidNodeKey,
                node.key.clone(),
                None,
            ));
        }
        if node.title.trim().is_empty() {
            errors.push(WorkflowValidationError::new(
                WorkflowValidationCode::EmptyNodeTitle,
                node.key.clone(),
                None,
            ));
        }
        if node.agent_instance_id.trim().is_empty() {
            errors.push(WorkflowValidationError::new(
                WorkflowValidationCode::EmptyAgentInstanceId,
                node.key.clone(),
                None,
            ));
        }
        if !keys.insert(node.key.as_str()) {
            errors.push(WorkflowValidationError::new(
                WorkflowValidationCode::DuplicateNodeKey,
                node.key.clone(),
                None,
            ));
        }
    }
    let known_keys: HashSet<&str> = input.nodes.iter().map(|node| node.key.as_str()).collect();
    for node in input.nodes {
        for dependency in &node.deps {
            if !is_valid_key(dependency) {
                errors.push(WorkflowValidationError::new(
                    WorkflowValidationCode::InvalidDependencyKey,
                    node.key.clone(),
                    Some(dependency.clone()),
                ));
            } else if dependency == &node.key {
                errors.push(WorkflowValidationError::new(
                    WorkflowValidationCode::SelfDependency,
                    node.key.clone(),
                    Some(dependency.clone()),
                ));
            } else if !known_keys.contains(dependency.as_str()) {
                errors.push(WorkflowValidationError::new(
                    WorkflowValidationCode::UnknownDependency,
                    node.key.clone(),
                    Some(dependency.clone()),
                ));
            }
        }
    }
    if let Some(node_key) = first_cycle_anchor(input.nodes, &known_keys) {
        errors.push(WorkflowValidationError::new(
            WorkflowValidationCode::Cycle,
            node_key.to_string(),
            None,
        ));
    }
    validate_input_bindings(input.nodes, &known_keys, &mut errors);
    validate_output_schemas(input.nodes, &mut errors);
    errors.sort_by(|left, right| {
        (&left.node_key, left.code, &left.dependency_key).cmp(&(
            &right.node_key,
            right.code,
            &right.dependency_key,
        ))
    });
    errors.dedup();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(WorkflowValidationErrors { errors })
    }
}

/// 输入映射校验:本地名合法且节点内唯一、来源必须是传递祖先、
/// 字段路径非空、默认值只允许出现在 optional 绑定上。
fn validate_input_bindings(
    nodes: &[WorkflowNodeDraft],
    known_keys: &HashSet<&str>,
    errors: &mut Vec<WorkflowValidationError>,
) {
    let deps_of: HashMap<&str, &[String]> = nodes
        .iter()
        .map(|node| (node.key.as_str(), node.deps.as_slice()))
        .collect();
    for node in nodes {
        let mut names = HashSet::new();
        for binding in &node.input_bindings {
            if !is_valid_key(&binding.name) {
                errors.push(
                    WorkflowValidationError::new(
                        WorkflowValidationCode::InvalidInputBinding,
                        node.key.clone(),
                        Some(binding.name.clone()),
                    )
                    .with_detail(format!(
                        "映射名 `{}` 非法（仅允许字母、数字、-、_）",
                        binding.name
                    )),
                );
            } else if !names.insert(binding.name.as_str()) {
                errors.push(
                    WorkflowValidationError::new(
                        WorkflowValidationCode::InvalidInputBinding,
                        node.key.clone(),
                        Some(binding.name.clone()),
                    )
                    .with_detail(format!("映射名 `{}` 在节点内重复", binding.name)),
                );
            }
            if binding.field_path.trim().is_empty() {
                errors.push(
                    WorkflowValidationError::new(
                        WorkflowValidationCode::InvalidInputBinding,
                        node.key.clone(),
                        Some(binding.name.clone()),
                    )
                    .with_detail("字段路径不能为空（如 summary 或 output.report_path）"),
                );
            }
            if binding.required && binding.default_value.is_some() {
                errors.push(
                    WorkflowValidationError::new(
                        WorkflowValidationCode::InvalidInputBinding,
                        node.key.clone(),
                        Some(binding.name.clone()),
                    )
                    .with_detail("必填映射不允许默认值（默认值仅用于 optional 字段）"),
                );
            }
            if !known_keys.contains(binding.source_node_key.as_str()) {
                errors.push(WorkflowValidationError::new(
                    WorkflowValidationCode::BindingUnknownNode,
                    node.key.clone(),
                    Some(binding.source_node_key.clone()),
                ));
            } else if !transitively_reaches(&deps_of, &node.key, &binding.source_node_key) {
                errors.push(WorkflowValidationError::new(
                    WorkflowValidationCode::BindingNotAncestor,
                    node.key.clone(),
                    Some(binding.source_node_key.clone()),
                ));
            }
        }
    }
}

/// `from` 沿 deps 是否能到达 `target`(传递祖先判定;环由 cycle 校验兜底)。
fn transitively_reaches<'a>(
    deps_of: &HashMap<&'a str, &'a [String]>,
    from: &str,
    target: &str,
) -> bool {
    let mut seen = HashSet::new();
    let mut queue: Vec<&str> = deps_of
        .get(from)
        .map(|deps| deps.iter().map(String::as_str).collect())
        .unwrap_or_default();
    while let Some(current) = queue.pop() {
        if !seen.insert(current) {
            continue;
        }
        if current == target {
            return true;
        }
        if let Some(next) = deps_of.get(current) {
            queue.extend(next.iter().map(String::as_str));
        }
    }
    false
}

/// 输出约束校验:根必须是 `{"type":"object"}` 形态;支持的关键字仅
/// type/properties/required/items/description,遇到不支持的关键字报错
/// (不声称支持完整 JSON Schema 标准)。
fn validate_output_schemas(nodes: &[WorkflowNodeDraft], errors: &mut Vec<WorkflowValidationError>) {
    const SUPPORTED: [&str; 5] = ["type", "properties", "required", "items", "description"];
    const TYPES: [&str; 6] = ["object", "array", "string", "number", "integer", "boolean"];
    fn check_schema(
        node_key: &str,
        binding_name: &str,
        schema: &serde_json::Value,
        errors: &mut Vec<WorkflowValidationError>,
    ) {
        let Some(object) = schema.as_object() else {
            errors.push(
                WorkflowValidationError::new(
                    WorkflowValidationCode::InvalidOutputSchema,
                    node_key.to_string(),
                    Some(binding_name.to_string()),
                )
                .with_detail("schema 片段必须是 JSON 对象"),
            );
            return;
        };
        for (keyword, value) in object {
            match keyword.as_str() {
                "type" => {
                    let Ok(ty) = serde_json::from_value::<String>(value.clone()) else {
                        errors.push(
                            WorkflowValidationError::new(
                                WorkflowValidationCode::InvalidOutputSchema,
                                node_key.to_string(),
                                Some(binding_name.to_string()),
                            )
                            .with_detail("type 必须是字符串"),
                        );
                        continue;
                    };
                    if !TYPES.contains(&ty.as_str()) {
                        errors.push(
                            WorkflowValidationError::new(
                                WorkflowValidationCode::InvalidOutputSchema,
                                node_key.to_string(),
                                Some(binding_name.to_string()),
                            )
                            .with_detail(format!("不支持的 type `{ty}`")),
                        );
                    }
                }
                "description" => {
                    if !value.is_string() {
                        errors.push(
                            WorkflowValidationError::new(
                                WorkflowValidationCode::InvalidOutputSchema,
                                node_key.to_string(),
                                Some(binding_name.to_string()),
                            )
                            .with_detail("description 必须是字符串"),
                        );
                    }
                }
                "required" => {
                    let Ok(items) = serde_json::from_value::<Vec<String>>(value.clone()) else {
                        errors.push(
                            WorkflowValidationError::new(
                                WorkflowValidationCode::InvalidOutputSchema,
                                node_key.to_string(),
                                Some(binding_name.to_string()),
                            )
                            .with_detail("required 必须是字符串数组"),
                        );
                        continue;
                    };
                    if items.iter().any(String::is_empty) {
                        errors.push(
                            WorkflowValidationError::new(
                                WorkflowValidationCode::InvalidOutputSchema,
                                node_key.to_string(),
                                Some(binding_name.to_string()),
                            )
                            .with_detail("required 不能包含空字符串"),
                        );
                    }
                }
                "properties" => {
                    let Ok(properties) = serde_json::from_value::<
                        serde_json::Map<String, serde_json::Value>,
                    >(value.clone()) else {
                        errors.push(
                            WorkflowValidationError::new(
                                WorkflowValidationCode::InvalidOutputSchema,
                                node_key.to_string(),
                                Some(binding_name.to_string()),
                            )
                            .with_detail("properties 必须是对象"),
                        );
                        continue;
                    };
                    for (name, child) in properties {
                        check_schema(node_key, &name, &child, errors);
                    }
                }
                "items" => check_schema(node_key, binding_name, value, errors),
                unsupported => {
                    errors.push(
                        WorkflowValidationError::new(
                            WorkflowValidationCode::InvalidOutputSchema,
                            node_key.to_string(),
                            Some(binding_name.to_string()),
                        )
                        .with_detail(format!(
                            "不支持的关键字 `{unsupported}`（首版仅支持 {}）",
                            SUPPORTED.join("/")
                        )),
                    );
                }
            }
        }
    }
    for node in nodes {
        let Some(schema) = &node.output_schema else {
            continue;
        };
        let Some(root_type) = schema.get("type").and_then(|ty| ty.as_str()) else {
            errors.push(
                WorkflowValidationError::new(
                    WorkflowValidationCode::InvalidOutputSchema,
                    node.key.clone(),
                    None,
                )
                .with_detail("根 schema 必须声明 type=object（Handoff.output 是对象）"),
            );
            continue;
        };
        if root_type != "object" {
            errors.push(
                WorkflowValidationError::new(
                    WorkflowValidationCode::InvalidOutputSchema,
                    node.key.clone(),
                    None,
                )
                .with_detail(format!(
                    "根 schema 的 type 必须是 object（当前 `{root_type}`）"
                )),
            );
            continue;
        }
        check_schema(&node.key.clone(), "", schema, errors);
    }
}

fn is_valid_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// 确定性 DFS：未知依赖与自依赖已由更精确的错误覆盖，不重复报 cycle。
fn first_cycle_anchor<'a>(
    nodes: &'a [WorkflowNodeDraft],
    known_keys: &HashSet<&'a str>,
) -> Option<&'a str> {
    fn visit<'a>(
        key: &'a str,
        graph: &HashMap<&'a str, Vec<&'a str>>,
        color: &mut HashMap<&'a str, u8>,
    ) -> Option<&'a str> {
        color.insert(key, 1);
        if let Some(dependencies) = graph.get(key) {
            for &dependency in dependencies {
                match color.get(dependency).copied().unwrap_or(0) {
                    0 => {
                        if let Some(anchor) = visit(dependency, graph, color) {
                            return Some(anchor);
                        }
                    }
                    1 => return Some(dependency),
                    _ => {}
                }
            }
        }
        color.insert(key, 2);
        None
    }

    let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in nodes {
        let dependencies = graph.entry(node.key.as_str()).or_default();
        dependencies.extend(
            node.deps
                .iter()
                .map(String::as_str)
                .filter(|dependency| *dependency != node.key && known_keys.contains(dependency)),
        );
        dependencies.sort_unstable();
        dependencies.dedup();
    }
    let mut roots: Vec<&str> = known_keys.iter().copied().collect();
    roots.sort_unstable();
    let mut color = HashMap::with_capacity(roots.len());
    for root in roots {
        if color.get(root).copied().unwrap_or(0) == 0 {
            if let Some(anchor) = visit(root, &graph, &mut color) {
                return Some(anchor);
            }
        }
    }
    None
}
