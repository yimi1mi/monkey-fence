//! 纯 Workflow Compiler(设计 §5.2):把模板版本 + 实例解析编译为
//! 不可变 `WorkflowSnapshot`。
//!
//! 校验(全部错误一次性返回,按节点键稳定排序):
//! - DAG:重复键、未知依赖、自依赖与循环;
//! - 变量:`${nodes.<key>.output...}` 只能引用传递上游;
//! - 实例:存在且启用;Agent Type 必须来自可用插件贡献;
//! - 并行安全:并行节点不得复用同一 Interactive 实例;
//!   目录提供器不能隔离时,并行必须显式开启风险开关。
//!
//! 编译器是纯函数:实例解析经注入闭包,不触目录库。

use crate::agent_instance::AgentInstanceSnapshot;
use crate::workflow::{
    PluginSourcePin, WorkflowNodeDraft, WorkflowNodeSnapshot, WorkflowSnapshot,
    WorkflowTemplateVersion,
};
use std::collections::{HashMap, HashSet};

/// 编译错误;`code` 是稳定机器码,`node` 为空串表示工作流级错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub code: String,
    pub node: String,
    pub message: String,
}

impl CompileError {
    fn new(code: &str, node: &str, message: impl Into<String>) -> CompileError {
        CompileError {
            code: code.into(),
            node: node.into(),
            message: message.into(),
        }
    }
}

/// 编译输入。`resolve_instance` 由调用方注入(目录库实现),
/// 编译器本身不持久化、不 IO。
pub struct CompileInput<'a> {
    pub template: &'a WorkflowTemplateVersion,
    /// 目录提供器是否支持并行隔离(worktree = true;项目目录 = false)。
    pub directory_provider_isolates: bool,
    /// 用户显式接受的"共享目录并行"风险开关(默认关闭)。
    pub allow_unsafe_shared_directory: bool,
    /// 可用插件贡献的 Agent Type → 所属插件包身份(实例的 agent_type 必须在其中;
    /// 编译通过后把插件 pin 冻结进节点快照)。
    pub agent_type_plugins: &'a HashMap<String, PluginSourcePin>,
    /// Agent Instance 解析器:错误表示实例不存在或不可读。
    pub resolve_instance: &'a dyn Fn(&str) -> anyhow::Result<AgentInstanceSnapshot>,
}

pub struct WorkflowCompiler;

impl WorkflowCompiler {
    pub fn new() -> WorkflowCompiler {
        WorkflowCompiler
    }

    /// 编译:校验失败返回全部错误(稳定排序),成功返回冻结快照。
    pub fn compile(&self, input: CompileInput<'_>) -> Result<WorkflowSnapshot, Vec<CompileError>> {
        let mut errors = Vec::new();
        let nodes = &input.template.nodes;

        // 1. 键唯一性与格式
        let mut keys: HashSet<&str> = HashSet::new();
        for node in nodes {
            if node.key.trim().is_empty() {
                errors.push(CompileError::new("invalid-key", "", "节点键不能为空"));
            } else if !keys.insert(node.key.as_str()) {
                errors.push(CompileError::new(
                    "duplicate-key",
                    &node.key,
                    format!("节点键 `{}` 重复", node.key),
                ));
            }
        }

        // 2. 依赖存在性(重复键去重后逐个校验)
        let key_set: HashSet<&str> = nodes.iter().map(|n| n.key.as_str()).collect();
        for node in nodes {
            for dep in &node.deps {
                if !key_set.contains(dep.as_str()) {
                    errors.push(CompileError::new(
                        "unknown-dep",
                        &node.key,
                        format!("依赖 `{dep}` 不存在"),
                    ));
                }
            }
        }

        // 3. 环检测(DFS 三色标记;自依赖也算环)
        let mut ancestors: HashMap<&str, HashSet<&str>> = HashMap::new();
        for node in nodes {
            let mut state: HashMap<&str, u8> = HashMap::new();
            if Self::dfs_cycles(&node.key, nodes, &mut state, &mut ancestors) {
                errors.push(CompileError::new(
                    "cycle",
                    &node.key,
                    format!("节点 `{}` 处于依赖环中", node.key),
                ));
            }
        }

        // 4. 实例解析(存在 + 启用)+ 插件贡献校验
        let mut resolved: HashMap<&str, AgentInstanceSnapshot> = HashMap::new();
        let mut resolved_plugin: HashMap<&str, PluginSourcePin> = HashMap::new();
        for node in nodes {
            match (input.resolve_instance)(&node.agent_instance_id) {
                Err(e) => errors.push(CompileError::new(
                    "unknown-instance",
                    &node.key,
                    format!("实例 `{}` 不可用: {e:#}", node.agent_instance_id),
                )),
                Ok(snapshot) => {
                    if !snapshot.enabled {
                        errors.push(CompileError::new(
                            "instance-disabled",
                            &node.key,
                            format!("实例 `{}` 已禁用", snapshot.name),
                        ));
                    }
                    if let Some(pin) = input.agent_type_plugins.get(&snapshot.agent_type) {
                        resolved_plugin.insert(node.key.as_str(), pin.clone());
                    } else {
                        errors.push(CompileError::new(
                            "plugin-missing",
                            &node.key,
                            format!(
                                "Agent Type `{}` 不存在或所属插件未启用(实例 {})",
                                snapshot.agent_type, snapshot.name
                            ),
                        ));
                    }
                    resolved.insert(node.key.as_str(), snapshot);
                }
            }
        }

        // 5. 变量引用:只能引用传递上游节点的输出
        for node in nodes {
            for referenced in Self::output_references(&node.instructions) {
                if !key_set.contains(referenced.as_str()) {
                    errors.push(CompileError::new(
                        "unknown-output",
                        &node.key,
                        format!("变量引用的节点 `{referenced}` 不存在"),
                    ));
                } else if !ancestors
                    .get(node.key.as_str())
                    .map(|set| set.contains(referenced.as_str()))
                    .unwrap_or(false)
                {
                    errors.push(CompileError::new(
                        "non-upstream-output",
                        &node.key,
                        format!("变量引用的节点 `{referenced}` 不是 `{}` 的上游", node.key),
                    ));
                }
            }
        }

        // 6. 并行安全
        let parallel_pairs = Self::parallel_pairs(nodes);
        if !parallel_pairs.is_empty() {
            if !input.directory_provider_isolates && !input.allow_unsafe_shared_directory {
                errors.push(CompileError::new(
                    "unsafe-parallel",
                    "",
                    "目录提供器不支持并行隔离;需要 worktree 提供器,或显式开启共享目录并行风险开关",
                ));
            }
            for (a, b) in &parallel_pairs {
                let (Some(ia), Some(ib)) = (resolved.get(a.as_str()), resolved.get(b.as_str()))
                else {
                    continue;
                };
                if ia.id == ib.id && ia.run_mode == crate::model::RunMode::Interactive {
                    errors.push(CompileError::new(
                        "parallel-session",
                        b,
                        format!(
                            "并行节点 `{a}` 与 `{b}` 复用同一 Interactive 实例 `{}`",
                            ia.name
                        ),
                    ));
                }
            }
        }

        if !errors.is_empty() {
            errors.sort_by(|x, y| (&x.node, &x.code).cmp(&(&y.node, &y.code)));
            errors.dedup_by(|x, y| x.code == y.code && x.node == y.node && x.message == y.message);
            return Err(errors);
        }

        // 7. 冻结
        let frozen: Vec<WorkflowNodeSnapshot> = nodes
            .iter()
            .map(|node| {
                let instance = resolved
                    .get(node.key.as_str())
                    .cloned()
                    .expect("校验通过后实例必然已解析");
                WorkflowNodeSnapshot {
                    key: node.key.clone(),
                    title: node.title.clone(),
                    instructions: node.instructions.clone(),
                    instance,
                    deps: node.deps.clone(),
                    plugin: resolved_plugin.get(node.key.as_str()).cloned(),
                }
            })
            .collect();
        Ok(WorkflowSnapshot {
            template_key: input.template.template_key.clone(),
            template_version: input.template.version,
            nodes: frozen,
        })
    }

    /// DFS 三色标记环检测;同时填充每个节点的传递祖先集合。
    /// 返回从 `key` 出发是否检测到环。
    fn dfs_cycles<'n>(
        key: &'n str,
        nodes: &'n [WorkflowNodeDraft],
        state: &mut HashMap<&'n str, u8>,
        ancestors: &mut HashMap<&'n str, HashSet<&'n str>>,
    ) -> bool {
        match state.get(key) {
            Some(2) => return false, // 已完成:复用缓存祖先
            Some(1) => return true,  // 在栈上:环
            _ => {}
        }
        state.insert(key, 1);
        let mut ups: HashSet<&'n str> = HashSet::new();
        let mut has_cycle = false;
        if let Some(node) = nodes.iter().find(|n| n.key == key) {
            for dep in &node.deps {
                if nodes.iter().any(|n| n.key == dep.as_str()) {
                    ups.insert(dep.as_str());
                    if Self::dfs_cycles(dep, nodes, state, ancestors) {
                        has_cycle = true;
                    }
                    if let Some(transitive) = ancestors.get(dep.as_str()) {
                        ups.extend(transitive);
                    }
                }
            }
        }
        ancestors.insert(key, ups);
        state.insert(key, 2);
        has_cycle
    }

    /// 提取 instructions 里的 `${nodes.<key>...}` 引用的节点键。
    fn output_references(text: &str) -> Vec<String> {
        let mut refs = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("${nodes.") {
            let after = &rest[at + "${nodes.".len()..];
            let key: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if !key.is_empty() {
                refs.push(key);
            }
            rest = after;
            // 跳过本引用剩余部分,继续找下一个
            if let Some(end) = rest.find('}') {
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
        refs
    }

    /// 无祖先关系的节点对(可并行执行)。
    fn parallel_pairs(nodes: &[WorkflowNodeDraft]) -> Vec<(String, String)> {
        let mut ups: HashMap<&str, HashSet<&str>> = HashMap::new();
        let mut state: HashMap<&str, u8> = HashMap::new();
        for node in nodes {
            let _ = Self::dfs_cycles(&node.key, nodes, &mut state, &mut ups);
        }
        let mut pairs = Vec::new();
        for (i, a) in nodes.iter().enumerate() {
            for b in &nodes[i + 1..] {
                let disjoint = ups
                    .get(a.key.as_str())
                    .map(|set| !set.contains(b.key.as_str()))
                    .unwrap_or(true)
                    && ups
                        .get(b.key.as_str())
                        .map(|set| !set.contains(a.key.as_str()))
                        .unwrap_or(true);
                if disjoint {
                    pairs.push((a.key.clone(), b.key.clone()));
                }
            }
        }
        pairs
    }
}

impl Default for WorkflowCompiler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_and_multiple_references() {
        let text = "a ${nodes.build.output.report_path} b ${nodes.test.output} ${nodes.ghost}";
        let refs = WorkflowCompiler::output_references(text);
        assert_eq!(refs, vec!["build", "test", "ghost"]);
        assert!(WorkflowCompiler::output_references("${nodes.}").is_empty());
    }
}
