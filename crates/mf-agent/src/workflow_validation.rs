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
        }
    }
}

/// 一项可定位的工作流领域校验错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowValidationError {
    code: WorkflowValidationCode,
    node_key: String,
    dependency_key: Option<String>,
}

impl WorkflowValidationError {
    pub fn code(&self) -> WorkflowValidationCode {
        self.code
    }

    pub fn node_key(&self) -> &str {
        &self.node_key
    }

    pub fn dependency_key(&self) -> Option<&str> {
        self.dependency_key.as_deref()
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
        errors.push(WorkflowValidationError {
            code: WorkflowValidationCode::EmptyWorkflow,
            node_key: String::new(),
            dependency_key: None,
        });
    }
    let mut keys = HashSet::with_capacity(input.nodes.len());
    for node in input.nodes {
        if node.key.trim().is_empty() {
            errors.push(WorkflowValidationError {
                code: WorkflowValidationCode::EmptyNodeKey,
                node_key: node.key.clone(),
                dependency_key: None,
            });
        } else if !is_valid_key(&node.key) {
            errors.push(WorkflowValidationError {
                code: WorkflowValidationCode::InvalidNodeKey,
                node_key: node.key.clone(),
                dependency_key: None,
            });
        }
        if node.title.trim().is_empty() {
            errors.push(WorkflowValidationError {
                code: WorkflowValidationCode::EmptyNodeTitle,
                node_key: node.key.clone(),
                dependency_key: None,
            });
        }
        if node.agent_instance_id.trim().is_empty() {
            errors.push(WorkflowValidationError {
                code: WorkflowValidationCode::EmptyAgentInstanceId,
                node_key: node.key.clone(),
                dependency_key: None,
            });
        }
        if !keys.insert(node.key.as_str()) {
            errors.push(WorkflowValidationError {
                code: WorkflowValidationCode::DuplicateNodeKey,
                node_key: node.key.clone(),
                dependency_key: None,
            });
        }
    }
    let known_keys: HashSet<&str> = input.nodes.iter().map(|node| node.key.as_str()).collect();
    for node in input.nodes {
        for dependency in &node.deps {
            if !is_valid_key(dependency) {
                errors.push(WorkflowValidationError {
                    code: WorkflowValidationCode::InvalidDependencyKey,
                    node_key: node.key.clone(),
                    dependency_key: Some(dependency.clone()),
                });
            } else if dependency == &node.key {
                errors.push(WorkflowValidationError {
                    code: WorkflowValidationCode::SelfDependency,
                    node_key: node.key.clone(),
                    dependency_key: Some(dependency.clone()),
                });
            } else if !known_keys.contains(dependency.as_str()) {
                errors.push(WorkflowValidationError {
                    code: WorkflowValidationCode::UnknownDependency,
                    node_key: node.key.clone(),
                    dependency_key: Some(dependency.clone()),
                });
            }
        }
    }
    if let Some(node_key) = first_cycle_anchor(input.nodes, &known_keys) {
        errors.push(WorkflowValidationError {
            code: WorkflowValidationCode::Cycle,
            node_key: node_key.to_string(),
            dependency_key: None,
        });
    }
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
