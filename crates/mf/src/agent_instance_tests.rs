//! Agent Instance 管理 UI 的纯状态测试(UI Task 1):
//! 类型可见但未检测到不可保存、Secret 掩码、校验、项目覆盖、保存。

use crate::agent_instance_editor::{AgentInstanceEditorState, AgentTypeInfo};
use crate::agent_instances_view::{AgentInstancesViewModel, InstanceListInstance};
use crate::declarative_form::{DeclarativeForm, FormField, FormValue};
use mf_agent::agent_instance::AgentInstanceDraft;
use mf_agent::{InstanceScope, RunMode};

// ---------- AgentTypeInfo / Editor ----------

fn detected_type() -> AgentTypeInfo {
    AgentTypeInfo {
        id: "generic-command".into(),
        full_contribution_id: "monkeyfence.generic-command".into(),
        name: "通用命令".into(),
        plugin_name: "MonkeyFence 内置".into(),
        plugin_version: "0.1.0".into(),
        content_hash: String::new(),
        detected: true,
        supports_isolated_config: false,
        default_command: "agent.exe".into(),
        adapter: "generic-command".into(),
        modes: vec![RunMode::OneShot, RunMode::Interactive],
    }
}

fn unavailable_type() -> AgentTypeInfo {
    AgentTypeInfo {
        detected: false,
        ..detected_type()
    }
}

#[test]
fn unavailable_type_is_visible_but_cannot_save_instance() {
    let mut state = AgentInstanceEditorState::new(unavailable_type());
    state.set_name("Review");
    assert!(
        state
            .validation()
            .iter()
            .any(|e| e.code == "cli-not-detected"),
        "{:?}",
        state.validation()
    );
    assert!(!state.can_save());
}

#[test]
fn complete_fields_can_save_and_produce_draft() {
    let mut state = AgentInstanceEditorState::new(detected_type());
    state.set_name("审查");
    state.set_executable("agent.exe");
    state.set_argv("--prompt 目标");
    state.set_env_lines("LANG=C\nROWS=40");
    let errors = state.validation();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(state.can_save());

    let draft: AgentInstanceDraft = state.to_draft(InstanceScope::User, None);
    assert_eq!(draft.name, "审查");
    assert_eq!(draft.executable, "agent.exe");
    assert_eq!(draft.argv, vec!["--prompt".to_string(), "目标".to_string()]);
    assert_eq!(
        draft.env,
        vec![
            ("LANG".to_string(), "C".to_string()),
            ("ROWS".to_string(), "40".to_string())
        ]
    );
}

#[test]
fn validation_flags_missing_core_fields_and_bad_env() {
    let mut state = AgentInstanceEditorState::new(detected_type());
    assert!(state.validation().iter().any(|e| e.code == "name-required"));
    // 类型默认命令会预填;显式清空后必须报错
    state.set_executable("");
    assert!(state
        .validation()
        .iter()
        .any(|e| e.code == "executable-required"));

    state.set_name("x");
    state.set_executable("agent.exe");
    state.set_env_lines("NO_EQUALS");
    assert!(
        state.validation().iter().any(|e| e.code == "env-invalid"),
        "{:?}",
        state.validation()
    );
    assert!(!state.can_save());
}

#[test]
fn secret_values_are_masked_in_editor() {
    let mut state = AgentInstanceEditorState::new(detected_type());
    state.set_name("x");
    state.set_executable("agent.exe");
    state.add_secret_ref("api-key");
    // 掩码显示:列表里只有 id 与掩码,不含明文(明文根本不进编辑器)
    let display = state.secret_display();
    assert!(display.contains("api-key"));
    assert!(display.contains("••••"));
    assert!(!display.contains("sk-"));
}

// ---------- 列表视图模型 ----------

#[test]
fn list_distinguishes_default_clis_from_instances() {
    let mut model = AgentInstancesViewModel::default();
    model.push_type(detected_type());
    model.push_type(AgentTypeInfo {
        id: "claude".into(),
        name: "Claude".into(),
        detected: true,
        supports_isolated_config: true,
        default_command: "claude".into(),
        adapter: "claude-code".into(),
        ..detected_type()
    });
    model.push_instance(InstanceListInstance {
        id: "inst_1".into(),
        name: "Codex Review".into(),
        agent_type: "generic-command".into(),
        type_name: "通用命令".into(),
        enabled: true,
        current_version: 2,
        scope: InstanceScope::User,
        executable: "agent.exe".into(),
        run_mode: RunMode::OneShot,
    });

    let entries = model.entries();
    // 默认 CLI 条目可见(引导入口),实例条目区分展示
    assert!(entries
        .iter()
        .any(|e| e.kind == "default-cli" && e.title == "Claude"));
    assert!(entries
        .iter()
        .any(|e| e.kind == "instance" && e.title == "Codex Review"));
    // 未检测到的类型:可见但标注不可用
    let mut m2 = AgentInstancesViewModel::default();
    m2.push_type(unavailable_type());
    assert!(m2
        .entries()
        .iter()
        .any(|e| e.kind == "default-cli" && !e.available));
}

#[test]
fn list_filters_by_text_across_names() {
    let mut model = AgentInstancesViewModel::default();
    model.push_type(detected_type());
    model.push_instance(InstanceListInstance {
        id: "i".into(),
        name: "夜间审查".into(),
        agent_type: "generic-command".into(),
        type_name: "通用命令".into(),
        enabled: true,
        current_version: 1,
        scope: InstanceScope::User,
        executable: "agent.exe".into(),
        run_mode: RunMode::OneShot,
    });
    assert_eq!(model.filtered("夜间").len(), 1);
    assert_eq!(model.filtered("通用").len(), 2);
    assert_eq!(model.filtered("").len(), 2);
}

// ---------- 声明式表单 ----------

#[test]
fn declarative_form_validates_required_and_holds_values() {
    let mut form = DeclarativeForm::new(vec![
        FormField {
            id: "model".into(),
            label: "模型".into(),
            kind: "text".into(),
            required: true,
            placeholder: "gpt-5".into(),
            options: vec![],
        },
        FormField {
            id: "token".into(),
            label: "API 密钥".into(),
            kind: "secret".into(),
            required: false,
            placeholder: String::new(),
            options: vec![],
        },
    ]);
    assert!(form.validation().iter().any(|e| e.contains("model")));

    form.set_value("model", "gpt-5");
    form.set_value("token", "sk-123");
    assert!(form.validation().is_empty());
    assert_eq!(form.get("model"), Some(&FormValue::Text("gpt-5".into())));
    // Secret 字段展示为掩码
    assert_eq!(form.masked_value("token"), "••••");
    assert_eq!(form.masked_value("model"), "gpt-5");

    // 未知字段安全忽略
    form.set_value("ghost", "x");
    assert_eq!(form.get("ghost"), None);
}
