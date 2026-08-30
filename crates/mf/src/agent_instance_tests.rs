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
        config_schema_fields: Vec::new(),
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

    let draft: AgentInstanceDraft = state.to_draft();
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

// ---------- 复审阻塞项 5:作用域/运行模式/启用 + Schema 表单 + Secret 管理 ----------

#[test]
fn editor_roundtrips_scope_project_run_mode_and_enabled() {
    let mut state = crate::agent_instance_editor::AgentInstanceEditorState::new(detected_type());
    state.set_name("项目实例");
    state.set_scope(mf_agent::InstanceScope::Project);
    state.set_project_key("proj-key");
    state.set_run_mode(mf_agent::RunMode::OneShot);
    state.set_enabled(false);
    let draft = state.to_draft();
    // 编辑器状态必须真实落到草案(不再硬编码 User/None/true)
    assert_eq!(draft.scope, mf_agent::InstanceScope::Project);
    assert_eq!(draft.project_key.as_deref(), Some("proj-key"));
    assert_eq!(draft.run_mode, mf_agent::RunMode::OneShot);
    assert!(!draft.enabled, "启用开关必须可编辑");
    // user 作用域不得携带 project key
    state.set_scope(mf_agent::InstanceScope::User);
    state.set_project_key("proj-key");
    assert!(
        state
            .validation()
            .iter()
            .any(|e| e.code == "scope-project-key"),
        "User 作用域携带 project_key 必须报错"
    );
}

#[test]
fn config_schema_form_renders_into_draft_config() {
    // 插件声明式 Schema(与 manifest config_schema 文件同构)
    let schema = serde_json::json!({
        "fields": [
            { "id": "permission_mode", "label": "权限模式", "kind": "select",
              "required": true, "options": ["default", "acceptEdits"] },
            { "id": "api_key_ref", "label": "API Key", "kind": "secret", "required": false }
        ]
    });
    let form = crate::declarative_form::DeclarativeForm::from_json(&schema);
    assert_eq!(form.fields().len(), 2);
    let mut state = crate::agent_instance_editor::AgentInstanceEditorState::new(detected_type());
    state.set_config_form(form);
    state.set_name("claude 实例");
    state.set_config_value("permission_mode", "acceptEdits");
    state.set_config_value("api_key_ref", "sec-123");
    // Secret 字段只存引用
    assert_eq!(state.config_form().masked_value("api_key_ref"), "••••");
    let draft = state.to_draft();
    assert_eq!(draft.config["permission_mode"], "acceptEdits");
    assert_eq!(draft.config["api_key_ref"], "sec-123");
    // 必填校验在表单层生效
    let mut empty = crate::agent_instance_editor::AgentInstanceEditorState::new(detected_type());
    empty.set_config_form(crate::declarative_form::DeclarativeForm::from_json(&schema));
    empty.set_name("x");
    assert!(empty
        .validation()
        .iter()
        .any(|e| e.message.contains("权限模式")));
}

#[test]
fn app_ctx_seals_and_deletes_secrets_storing_only_references() {
    // 独立目录库 + 注入主密钥:不触 OS keyring、不碰用户真实目录库
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let id = ctx.seal_secret("ANTHROPIC_KEY", "sk-live-abc").unwrap();
    assert!(!id.is_empty());
    // 列表只有脱敏描述
    let list = ctx.list_secrets().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "ANTHROPIC_KEY");
    assert!(!format!("{:?}", list[0]).contains("sk-live-abc"));
    // 删除幂等语义
    assert!(ctx.delete_secret(&id).unwrap());
    assert!(!ctx.delete_secret(&id).unwrap());
    assert!(ctx.list_secrets().unwrap().is_empty());
}

// ---------- Runtime 按 Revision 冻结的插件包 pin 解析 Adapter ----------

mod pinned_adapter_tests {
    use crate::adapter_launch::resolve_adapter_for_pin;
    use mf_agent::workflow::PluginSourcePin;
    use mf_plugins::PluginHost;

    fn registry() -> std::sync::Arc<PluginHost> {
        PluginHost::load_at_with_catalog(
            std::env::temp_dir().join("mf-pin-adapter-test"),
            mf_agent::CatalogStore::memory().unwrap(),
            &mf_agent::Config::default(),
            &[],
        )
    }

    /// 内置 claude 贡献的当前身份(合成插件 content_hash 为空)。
    fn claude_pin(plugins: &PluginHost, version_override: Option<&str>) -> PluginSourcePin {
        let (_, source, _) = plugins
            .contributions()
            .agent_types()
            .into_iter()
            .find(|(_, s, c)| s.plugin_full_id == "monkeyfence.claude" && c.id == "claude")
            .expect("内置 claude 贡献");
        PluginSourcePin {
            full_id: source.plugin_full_id.clone(),
            version: version_override
                .map(str::to_string)
                .unwrap_or_else(|| source.plugin_version.clone()),
            content_hash: String::new(), // 内置合成插件:无内容寻址哈希
            contribution_id: String::new(),
        }
    }

    #[test]
    fn pinned_version_resolves_adapter_from_matching_contribution() {
        let plugins = registry();
        let pin = claude_pin(&plugins, None);
        let adapter =
            resolve_adapter_for_pin(&plugins, Some(&pin), "monkeyfence.claude.claude").unwrap();
        assert_eq!(adapter.id(), "claude-code");
    }

    #[test]
    fn pinned_version_mismatch_is_rejected() {
        let plugins = registry();
        let pin = claude_pin(&plugins, Some("0.0.0-other"));
        let err = match resolve_adapter_for_pin(&plugins, Some(&pin), "monkeyfence.claude.claude") {
            Err(e) => e,
            Ok(_) => panic!("版本不一致的 pin 必须被拒绝"),
        };
        assert!(err.to_string().contains("不一致"), "{err:#}");
    }

    #[test]
    fn content_addressed_pin_with_bad_hash_is_rejected() {
        let plugins = registry();
        let mut pin = claude_pin(&plugins, None);
        pin.content_hash = "sha256:not-a-real-package".into();
        let err = match resolve_adapter_for_pin(&plugins, Some(&pin), "monkeyfence.claude.claude") {
            Err(e) => e,
            Ok(_) => panic!("内容哈希不存在的 pin 必须被拒绝"),
        };
        assert!(err.to_string().contains("插件包不存在"), "{err:#}");
    }

    #[test]
    fn missing_pin_falls_back_to_current_registry() {
        let plugins = registry();
        let adapter = resolve_adapter_for_pin(&plugins, None, "monkeyfence.claude.claude").unwrap();
        assert_eq!(adapter.id(), "claude-code");
    }
}

// ---------- 完整贡献 ID + 结构化 secret_env(I5/I6)----------

#[test]
fn new_references_use_full_contribution_id() {
    let mut state = AgentInstanceEditorState::new(detected_type());
    state.set_name("审查");
    let draft = state.to_draft();
    assert_eq!(
        draft.agent_type, "monkeyfence.generic-command",
        "新建引用必须使用完整贡献 ID(短 id 仅兼容旧实例)"
    );
    let snapshot = state.to_launch_snapshot("tmp-key");
    assert_eq!(snapshot.agent_type, "monkeyfence.generic-command");
}

#[test]
fn secret_env_map_roundtrips_as_structured_object() {
    let mut state = AgentInstanceEditorState::new(detected_type());
    state.set_name("带密钥");
    state.add_secret_ref("sk-prod");
    state.add_secret_ref("sk-stage");
    state.set_secret_env("ANTHROPIC_API_KEY", "sk-prod");
    state.set_secret_env("STAGE_TOKEN", "sk-stage");
    // 覆盖同一 ENV
    state.set_secret_env("STAGE_TOKEN", "sk-prod");
    assert_eq!(
        state.secret_env_display(),
        vec![
            "ANTHROPIC_API_KEY → sk-prod".to_string(),
            "STAGE_TOKEN → sk-prod".to_string(),
        ]
    );
    let draft = state.to_draft();
    let mapping = draft.config["secret_env"].as_object().unwrap();
    assert_eq!(mapping.len(), 2, "ENV→SecretRef map 结构: {mapping:?}");
    assert_eq!(mapping["ANTHROPIC_API_KEY"], "sk-prod");
    assert_eq!(mapping["STAGE_TOKEN"], "sk-prod");

    // 回填:从快照 config.secret_env 还原结构化行
    let mut reloaded = AgentInstanceEditorState::new(detected_type());
    reloaded.secret_refs = draft.sealed_secret_ids.clone();
    reloaded.secret_env_map = Vec::new();
    let snapshot = state.to_launch_snapshot("k");
    if let Some(mapping) = snapshot
        .config
        .get("secret_env")
        .and_then(|v| v.as_object())
    {
        reloaded.secret_env_map = mapping
            .iter()
            .filter_map(|(env, id)| id.as_str().map(|id| (env.clone(), id.to_string())))
            .collect();
    }
    assert_eq!(reloaded.secret_env_map.len(), 2);
}

#[test]
fn secret_env_referencing_undeclared_secret_is_rejected() {
    let mut state = AgentInstanceEditorState::new(detected_type());
    state.set_name("x");
    state.set_secret_env("MY_TOKEN", "sk-not-declared");
    let errors = state.validation();
    assert!(
        errors.iter().any(|e| e.code == "secret-env"),
        "未声明引用必须被校验拒绝: {errors:?}"
    );
    assert!(!state.can_save());
    // 声明后通过
    state.add_secret_ref("sk-not-declared");
    assert!(state.can_save(), "{:?}", state.validation());
    // 移除引用时连带清理映射行
    state.remove_secret_ref("sk-not-declared");
    assert!(state.secret_env_map.is_empty());
}

// ---------- I5:编辑页按完整贡献 ID 解析类型 ----------

#[test]
fn editor_resolves_type_info_by_full_contribution_id() {
    use crate::agent_instance_editor::resolve_type_info;
    let third_party = AgentTypeInfo {
        id: "super-agent".into(),
        full_contribution_id: "acme.tools.super-agent".into(),
        name: "Super Agent".into(),
        plugin_name: "acme.tools".into(),
        plugin_version: "1.0.0".into(),
        content_hash: "hash".into(),
        detected: true,
        supports_isolated_config: true,
        default_command: "super-agent".into(),
        adapter: "generic-command".into(),
        modes: vec![mf_agent::RunMode::Interactive],
        config_schema_fields: Vec::new(),
    };
    let types = vec![detected_type(), third_party.clone()];
    // 实例快照保存的是完整贡献 ID(to_draft 新引用一律完整形态)
    let resolved = resolve_type_info(&types, "acme.tools.super-agent").unwrap();
    assert_eq!(resolved.full_contribution_id, "acme.tools.super-agent");
    // legacy 内置实例的短 id 仍可解析(显式兼容回退)
    let legacy = resolve_type_info(&types, &detected_type().id).unwrap();
    assert_eq!(legacy.id, detected_type().id);
    // 编辑页导出的新引用是完整贡献 ID
    let mut state =
        crate::agent_instance_editor::AgentInstanceEditorState::new(third_party.clone());
    state.set_name("第三方实例");
    state.set_executable("super-agent");
    let draft = state.to_draft();
    assert_eq!(
        draft.agent_type, "acme.tools.super-agent",
        "编辑页导出的 agent_type 必须是完整贡献 ID"
    );
}
