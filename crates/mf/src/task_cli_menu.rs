//! Task `+` CLI 菜单与工作流分配模型(UI 计划 Task 3;设计 §10 / §11.3)。
//!
//! 纯模型:菜单条目构建/过滤、启动语义标注、任务创建时的
//! 工作流选择(已有模板或任务本地工作流)。GPUI 渲染由
//! task_sidebar/task_composer 接线。

use crate::agent_instance_editor::AgentTypeInfo;
use crate::agent_instances_view::InstanceListInstance;

/// 菜单条目类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// 新建普通终端。
    Terminal,
    /// 检测到的默认 Agent CLI(沿用外部已有配置,不写入)。
    DefaultCli,
    /// 用户 Agent Instance(冻结隔离配置)。
    AgentInstance,
    /// 仅本次任务使用的临时实例。
    TemporaryInstance,
}

/// 菜单条目。
#[derive(Debug, Clone, PartialEq)]
pub struct MenuEntry {
    pub kind: MenuKind,
    pub label: String,
    /// 启动引用:DefaultCli = agent type id;AgentInstance = 实例 id。
    pub agent_ref: Option<String>,
    pub note: String,
}

/// 构建 `+` 菜单:终端 → 检测到的默认 CLI → 实例 → 临时实例。
pub fn build_task_cli_menu(
    types: &[AgentTypeInfo],
    instances: &[InstanceListInstance],
) -> Vec<MenuEntry> {
    let mut out = vec![MenuEntry {
        kind: MenuKind::Terminal,
        label: "新建终端".into(),
        agent_ref: None,
        note: "普通 shell;不改变任务状态".into(),
    }];
    for t in types.iter().filter(|t| t.detected) {
        out.push(MenuEntry {
            kind: MenuKind::DefaultCli,
            label: t.name.clone(),
            // 启动引用用完整贡献 ID:第三方类型只有完整 ID 能被
            // resolve_adapter(按完整贡献 ID 查找)解析;短 id 仅是
            // 显式 legacy 内置回退路径
            agent_ref: Some(t.full_contribution_id.clone()),
            note: format!("默认 CLI:沿用外部已有配置,不执行任何写入;不改变任务状态"),
        });
    }
    for i in instances.iter().filter(|i| i.enabled) {
        out.push(MenuEntry {
            kind: MenuKind::AgentInstance,
            label: i.name.clone(),
            agent_ref: Some(i.id.clone()),
            note: format!("实例配置:隔离启动(冻结配置);不改变任务状态"),
        });
    }
    out.push(MenuEntry {
        kind: MenuKind::TemporaryInstance,
        label: "临时实例…".into(),
        agent_ref: None,
        note: "仅本次任务使用;不进入全局实例列表".into(),
    });
    out
}

/// 过滤(label 大小写不敏感)。
pub fn filter_menu(menu: &[MenuEntry], text: &str) -> Vec<MenuEntry> {
    let needle = text.trim().to_lowercase();
    menu.iter()
        .filter(|e| needle.is_empty() || e.label.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

/// 启动条目语义(agent_ref:实例 id 时为实例启动)。
pub fn launch_menu_entry(info: &AgentTypeInfo, instance_id: Option<String>) -> MenuEntry {
    match instance_id {
        None => MenuEntry {
            kind: MenuKind::DefaultCli,
            label: info.name.clone(),
            agent_ref: Some(info.full_contribution_id.clone()),
            note: "默认 CLI:沿用外部已有配置,不执行任何写入".into(),
        },
        Some(id) => MenuEntry {
            kind: MenuKind::AgentInstance,
            label: info.name.clone(),
            agent_ref: Some(id),
            note: "实例配置:隔离启动(冻结配置)".into(),
        },
    }
}

/// 任务创建时的工作流选择(设计 §11.3 / §9.1)。
pub struct WorkflowAssignment;

impl WorkflowAssignment {
    /// (模板名, 是否任务本地) → 可选列表文案。
    /// 任务本地模板默认私有;全局模板可分配;
    /// 始终提供「任务本地工作流(新建)」。
    pub fn choices(templates: &[(String, bool)]) -> Vec<String> {
        let mut out: Vec<String> = templates
            .iter()
            .filter(|(_, local)| !local)
            .map(|(name, _)| format!("模板:{name}"))
            .collect();
        for (name, _) in templates.iter().filter(|(_, local)| *local) {
            out.push(format!("任务本地草稿:{name}(默认私有)"));
        }
        out.push("任务本地工作流(新建,默认私有)".into());
        out
    }
}
