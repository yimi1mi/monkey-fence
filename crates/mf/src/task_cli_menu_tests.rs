//! Task `+` CLI 菜单与工作流分配测试(UI Task 3;设计 §10 / §11.3):
//! 菜单条目(终端/默认 CLI/实例/临时实例)、过滤、
//! 任务创建选择(模板或任务本地)、ad-hoc 启动不影响任务状态。

use crate::task_cli_menu::{
    build_task_cli_menu, filter_menu, launch_menu_entry, MenuEntry, MenuKind, WorkflowAssignment,
};

fn menu_types() -> Vec<crate::agent_instance_editor::AgentTypeInfo> {
    use mf_agent::RunMode;
    vec![
        crate::agent_instance_editor::AgentTypeInfo {
            full_contribution_id: format!("monkeyfence.{}", "t"),
            plugin_version: "0.1.0".into(),
            content_hash: String::new(),
            config_schema_fields: Vec::new(),
            id: "codex".into(),
            name: "Codex".into(),
            plugin_name: "monkeyfence.codex".into(),
            detected: true,
            supports_isolated_config: true,
            default_command: "codex".into(),
            adapter: "codex".into(),
            yolo_args: None,
            modes: vec![RunMode::Interactive, RunMode::OneShot],
        },
        crate::agent_instance_editor::AgentTypeInfo {
            full_contribution_id: format!("monkeyfence.{}", "t"),
            plugin_version: "0.1.0".into(),
            content_hash: String::new(),
            config_schema_fields: Vec::new(),
            id: "gemini".into(),
            name: "Gemini CLI".into(),
            plugin_name: "monkeyfence.gemini".into(),
            detected: false,
            supports_isolated_config: false,
            default_command: "gemini".into(),
            adapter: "generic-command".into(),
            yolo_args: None,
            modes: vec![RunMode::Interactive],
        },
    ]
}

fn menu_instances() -> Vec<crate::agent_instances_view::InstanceListInstance> {
    use mf_agent::{InstanceScope, RunMode};
    vec![crate::agent_instances_view::InstanceListInstance {
        id: "inst_1".into(),
        name: "Codex Review".into(),
        agent_type: "codex".into(),
        type_name: "Codex".into(),
        enabled: true,
        current_version: 1,
        scope: InstanceScope::User,
        executable: "codex".into(),
        run_mode: RunMode::OneShot,
    }]
}

#[test]
fn plus_menu_lists_terminal_detected_types_and_instances() {
    let menu: Vec<MenuEntry> = build_task_cli_menu(&menu_types(), &menu_instances());
    // 终端入口
    assert!(menu
        .iter()
        .any(|i| i.kind == MenuKind::Terminal && i.label.contains("终端")));
    // 检测到的默认 CLI(Gemini 未检测到 → 不出现)
    assert!(menu
        .iter()
        .any(|i| i.label == "Codex" && i.kind == MenuKind::DefaultCli));
    assert!(
        !menu.iter().any(|i| i.label == "Gemini CLI"),
        "未检测到的 CLI 不进 + 菜单"
    );
    // Agent Instance
    assert!(menu
        .iter()
        .any(|i| i.label == "Codex Review" && i.kind == MenuKind::AgentInstance));
    // 临时实例入口
    assert!(menu.iter().any(|i| i.kind == MenuKind::TemporaryInstance));
}

#[test]
fn menu_filter_matches_label_and_kind() {
    let menu = build_task_cli_menu(&menu_types(), &menu_instances());
    let hits = crate::task_cli_menu::filter_menu(&menu, "codex");
    assert!(hits
        .iter()
        .all(|i| i.label.to_lowercase().contains("codex")));
    assert!(hits.len() >= 2);
    assert!(crate::task_cli_menu::filter_menu(&menu, "").len() == menu.len());
}

#[test]
fn launch_entry_carries_launch_mode_and_reference() {
    let entry = launch_menu_entry(&menu_types()[0], None);
    assert_eq!(entry.kind, MenuKind::DefaultCli);
    // 启动引用是完整贡献 ID(短 id 只留给显式 legacy 内置回退)
    assert_eq!(entry.agent_ref.as_deref(), Some("monkeyfence.t"));
    // 默认 CLI 交互启动,沿用外部已有配置,不写入
    assert!(entry.note.contains("沿用"));

    let inst = launch_menu_entry(&menu_types()[0], Some("inst_1".into()));
    assert_eq!(inst.kind, MenuKind::AgentInstance);
    assert_eq!(inst.agent_ref.as_deref(), Some("inst_1"));
    assert!(inst.note.contains("隔离"));
}

#[test]
fn assignment_offers_templates_and_task_local() {
    let templates = vec![
        ("全局模板A".to_string(), false),
        ("task-7 草稿".to_string(), true),
    ];
    let choices = WorkflowAssignment::choices(&templates);
    // 模板选项 + 任务本地新建选项
    assert!(choices.iter().any(|c| c.contains("全局模板A")));
    assert!(choices.iter().any(|c| c.contains("任务本地")));
    // 任务本地草稿也在选择列表(标记为草稿)
    assert!(choices.iter().any(|c| c.contains("task-7")));
}

#[test]
fn ad_hoc_launch_never_changes_task_status_label() {
    // 菜单契约说明:离散会话不参与任务成功判定
    let menu = build_task_cli_menu(&menu_types(), &menu_instances());
    let term = menu.iter().find(|i| i.kind == MenuKind::Terminal).unwrap();
    assert!(term.note.contains("不改变任务状态"), "{}", term.note);
}

// ---------- I5:第三方贡献全链路使用完整贡献 ID ----------

#[test]
fn default_cli_menu_entries_carry_full_contribution_id() {
    // DefaultCli 的启动引用必须是完整贡献 ID(publisher.plugin.agent_type):
    // 短 id 只对显式 legacy 内置回退路径有意义 —— 第三方类型用短 id
    // 会在 resolve_adapter(按完整贡献 ID 查找)处解析失败
    let mut types = menu_types();
    types.push(crate::agent_instance_editor::AgentTypeInfo {
        full_contribution_id: "acme.tools.super-agent".into(),
        plugin_version: "1.0.0".into(),
        content_hash: "hash-1".into(),
        config_schema_fields: Vec::new(),
        id: "super-agent".into(),
        name: "Super Agent".into(),
        plugin_name: "acme.tools".into(),
        detected: true,
        supports_isolated_config: true,
        default_command: "super-agent".into(),
        adapter: "generic-command".into(),
        yolo_args: None,
        modes: vec![mf_agent::RunMode::Interactive],
    });
    let menu = build_task_cli_menu(&types, &menu_instances());
    let entry = menu
        .iter()
        .find(|e| e.kind == MenuKind::DefaultCli && e.label == "Super Agent")
        .expect("检测到的第三方 CLI 应出现在菜单");
    assert_eq!(
        entry.agent_ref.as_deref(),
        Some("acme.tools.super-agent"),
        "DefaultCli 启动引用必须是完整贡献 ID(短 id 解析不了第三方贡献)"
    );
    // 内置类型同样携带完整贡献 ID(下游按完整 ID 查找,短 id 仅作
    // legacy 回退兼容)
    let codex = menu
        .iter()
        .find(|e| e.kind == MenuKind::DefaultCli && e.label == "Codex")
        .unwrap();
    assert!(
        codex.agent_ref.as_deref().unwrap_or("").contains('.'),
        "内置类型也应以完整贡献 ID 引用:{:?}",
        codex.agent_ref
    );
}

#[test]
fn launch_menu_entry_default_cli_uses_full_contribution_id() {
    let mut types = menu_types();
    types.push(crate::agent_instance_editor::AgentTypeInfo {
        full_contribution_id: "acme.tools.super-agent".into(),
        plugin_version: "1.0.0".into(),
        content_hash: "hash-1".into(),
        config_schema_fields: Vec::new(),
        id: "super-agent".into(),
        name: "Super Agent".into(),
        plugin_name: "acme.tools".into(),
        detected: true,
        supports_isolated_config: true,
        default_command: "super-agent".into(),
        adapter: "generic-command".into(),
        yolo_args: None,
        modes: vec![mf_agent::RunMode::Interactive],
    });
    let info = types.last().unwrap();
    let entry = launch_menu_entry(info, None);
    assert_eq!(
        entry.agent_ref.as_deref(),
        Some("acme.tools.super-agent"),
        "默认 CLI 启动条目必须用完整贡献 ID"
    );
}
