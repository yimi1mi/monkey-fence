//! Workflow 模板与不可变快照(设计 §4.3 / §9.1)。
//!
//! - 模板是可编辑的 DAG 数据(节点引用 Agent Instance,不引用 Profile);
//! - 编辑模板追加新的不可变版本行,既有版本不受影响;
//! - 任务本地模板默认不进入全局列表,用户显式"另存为模板"后提升;
//! - `freeze_workflow` 把模板版本 + 实例当前快照冻结为
//!   `WorkflowSnapshot`,Revision 只保存序列化快照。

use crate::agent_instance::AgentInstanceSnapshot;
use crate::catalog_store::CatalogStore;
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// 工作流节点草案:稳定键 + 依赖 + Agent Instance 引用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowNodeDraft {
    /// 稳定节点键(变量引用 `${nodes.<key>.output...}` 使用它)。
    pub key: String,
    pub title: String,
    pub instructions: String,
    /// 引用 Agent Instance 稳定 ID(不是 Agent Profile)。
    pub agent_instance_id: String,
    /// 上游节点键列表(串行/并行/汇合;禁止循环 —— 编译器校验)。
    pub deps: Vec<String>,
}

/// 模板草案;`task_local` 为真的模板只属于单个任务,不进全局列表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowTemplateDraft {
    pub key: String,
    pub name: String,
    pub task_local: bool,
    pub nodes: Vec<WorkflowNodeDraft>,
}

/// 目录库 `workflow_templates` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowTemplate {
    pub key: String,
    pub name: String,
    pub task_local: bool,
    pub current_version: i64,
}

/// 不可变模板版本行(nodes 整体序列化进 graph_json)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowTemplateVersion {
    /// 版本行 rowid(引用固定版本的句柄)。
    pub version_id: i64,
    pub template_key: String,
    pub version: i64,
    pub nodes: Vec<WorkflowNodeDraft>,
    #[serde(default)]
    pub created_at: String,
}

/// 冻结的节点:节点数据 + 编译时刻的实例配置快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowNodeSnapshot {
    pub key: String,
    pub title: String,
    pub instructions: String,
    pub instance: AgentInstanceSnapshot,
    pub deps: Vec<String>,
}

/// 不可变工作流快照:Revision 保存的就是它。
/// 模板后续编辑、实例后续版本都不影响已冻结内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    pub template_key: String,
    pub template_version: i64,
    pub nodes: Vec<WorkflowNodeSnapshot>,
}

/// 冻结(最小编译):模板版本 + 实例当前配置 → 不可变快照。
/// DAG/变量/并行安全等完整校验属于 Workflow Compiler(纯函数,另一模块)。
pub fn freeze_workflow(
    catalog: &CatalogStore,
    version: &WorkflowTemplateVersion,
) -> Result<WorkflowSnapshot> {
    let mut nodes = Vec::with_capacity(version.nodes.len());
    for draft in &version.nodes {
        let instance = catalog
            .snapshot_agent_instance(&draft.agent_instance_id, None)
            .map_err(|e| anyhow::anyhow!("节点 `{}` 引用的实例不可用: {e:#}", draft.key))?;
        nodes.push(WorkflowNodeSnapshot {
            key: draft.key.clone(),
            title: draft.title.clone(),
            instructions: draft.instructions.clone(),
            instance,
            deps: draft.deps.clone(),
        });
    }
    Ok(WorkflowSnapshot {
        template_key: version.template_key.clone(),
        template_version: version.version,
        nodes,
    })
}
