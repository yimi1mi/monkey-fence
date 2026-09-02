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
        yolo_args: None,
        modes: vec![RunMode::OneShot, RunMode::Interactive],
    }
}

/// 内置 Claude 投影:yolo 参数表命中,编辑器渲染权限行。
fn claude_type() -> AgentTypeInfo {
    AgentTypeInfo {
        id: "claude".into(),
        full_contribution_id: "monkeyfence.claude".into(),
        name: "Claude".into(),
        plugin_name: "MonkeyFence 内置".into(),
        plugin_version: "0.1.0".into(),
        content_hash: String::new(),
        config_schema_fields: Vec::new(),
        detected: true,
        supports_isolated_config: true,
        default_command: "claude".into(),
        adapter: "claude-code".into(),
        yolo_args: mf_plugins::builtin::yolo_args_of("claude"),
        modes: vec![RunMode::Interactive, RunMode::OneShot],
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
    // 未检测到的类型:entries 完整投影仍标注不可用,
    // 但页面列表(filtered)不出现——选了也起不来
    let mut m2 = AgentInstancesViewModel::default();
    m2.push_type(unavailable_type());
    assert!(m2
        .entries()
        .iter()
        .any(|e| e.kind == "default-cli" && !e.available));
    assert!(!m2
        .filtered("")
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
    // 插件声明式 Schema(与 manifest config_schema 文件同构);
    // 权限不再是 config 字段(Orca 式权限行走 argv,见下方权限测试)
    let schema = serde_json::json!({
        "fields": [
            { "id": "log_level", "label": "日志级别", "kind": "select",
              "required": true, "options": ["debug", "info"] },
            { "id": "api_key_ref", "label": "API Key", "kind": "secret", "required": false }
        ]
    });
    let form = crate::declarative_form::DeclarativeForm::from_json(&schema);
    assert_eq!(form.fields().len(), 2);
    let mut state = crate::agent_instance_editor::AgentInstanceEditorState::new(detected_type());
    state.set_config_form(form);
    state.set_name("claude 实例");
    state.set_config_value("log_level", "info");
    state.set_config_value("api_key_ref", "sec-123");
    // Secret 字段只存引用
    assert_eq!(state.config_form().masked_value("api_key_ref"), "••••");
    let draft = state.to_draft();
    assert_eq!(draft.config["log_level"], "info");
    assert_eq!(draft.config["api_key_ref"], "sec-123");
    // 必填校验在表单层生效
    let mut empty = crate::agent_instance_editor::AgentInstanceEditorState::new(detected_type());
    empty.set_config_form(crate::declarative_form::DeclarativeForm::from_json(&schema));
    empty.set_name("x");
    assert!(empty
        .validation()
        .iter()
        .any(|e| e.message.contains("日志级别")));
}

// ---------- Orca 式权限(模式由参数推导,切换物化/清空 yolo 参数) ----------

#[test]
fn permission_mode_is_derived_from_argv() {
    use crate::agent_instance_editor::{
        apply_permission_mode, resolve_permission_mode, PermissionMode,
    };
    let yolo = Some("--dangerously-skip-permissions");
    // 空 = Manual;恰好等于 yolo 参数 = Yolo(空白归一);其余 = Custom
    assert_eq!(resolve_permission_mode("", yolo), PermissionMode::Manual);
    assert_eq!(
        resolve_permission_mode("  --dangerously-skip-permissions  ", yolo),
        PermissionMode::Yolo
    );
    assert_eq!(
        resolve_permission_mode("--model sonnet", yolo),
        PermissionMode::Custom
    );
    // 类型不支持权限切换(无 yolo 参数)恒为 Manual
    assert_eq!(
        resolve_permission_mode("--anything", None),
        PermissionMode::Manual
    );
    // 应用:空/恰好 yolo 时改写;自定义参数永不触碰
    assert_eq!(
        apply_permission_mode(PermissionMode::Yolo, "", yolo),
        "--dangerously-skip-permissions"
    );
    assert_eq!(
        apply_permission_mode(
            PermissionMode::Manual,
            "--dangerously-skip-permissions",
            yolo
        ),
        ""
    );
    assert_eq!(
        apply_permission_mode(PermissionMode::Manual, "--model sonnet", yolo),
        "--model sonnet",
        "自定义参数不受权限切换影响(Orca: overridden args untouched)"
    );
}

#[test]
fn editor_permission_toggle_materializes_yolo_args_into_draft() {
    let mut state = crate::agent_instance_editor::AgentInstanceEditorState::new(claude_type());
    state.set_name("claude-yolo");
    state.set_executable("claude");
    // 新建默认 Manual:参数为空,不附加权限参数
    assert_eq!(
        state.permission_mode(),
        crate::agent_instance_editor::PermissionMode::Manual
    );
    // 切到 Yolo:参数物化为 yolo 串,随草案落库(启动链路直接用 argv)
    state.toggle_permission();
    assert_eq!(
        state.permission_mode(),
        crate::agent_instance_editor::PermissionMode::Yolo
    );
    let yolo = mf_plugins::builtin::yolo_args_of("claude").unwrap();
    assert_eq!(state.argv_text, yolo);
    let draft = state.to_draft();
    assert_eq!(draft.argv, vec![yolo]);
    // 再切回 Manual:恰好等于 yolo 参数 → 清空
    state.toggle_permission();
    assert_eq!(
        state.permission_mode(),
        crate::agent_instance_editor::PermissionMode::Manual
    );
    assert_eq!(state.argv_text, "");
}

#[test]
fn editor_permission_toggle_never_touches_custom_argv() {
    let mut state = crate::agent_instance_editor::AgentInstanceEditorState::new(claude_type());
    state.set_name("claude-custom");
    state.set_executable("claude");
    state.set_argv("--model sonnet --permission-mode plan");
    // 自定义参数:推导为 Custom,切换不改写(显示层折叠为 Yolo)
    assert_eq!(
        state.permission_mode(),
        crate::agent_instance_editor::PermissionMode::Custom
    );
    state.toggle_permission();
    assert_eq!(state.argv_text, "--model sonnet --permission-mode plan");
    assert_eq!(
        state.permission_mode(),
        crate::agent_instance_editor::PermissionMode::Custom
    );
}

#[test]
fn builtin_yolo_args_table_covers_cli_agents() {
    // 权限参数表与内置 CLI 对齐:已知 CLI 给出 yolo 串,未知/不支持为 None
    assert_eq!(
        mf_plugins::builtin::yolo_args_of("claude").as_deref(),
        Some("--dangerously-skip-permissions")
    );
    assert_eq!(
        mf_plugins::builtin::yolo_args_of("codex").as_deref(),
        Some("--dangerously-bypass-approvals-and-sandbox")
    );
    assert_eq!(
        mf_plugins::builtin::yolo_args_of("gemini").as_deref(),
        Some("--yolo")
    );
    assert_eq!(mf_plugins::builtin::yolo_args_of("opencode"), None);
    assert_eq!(mf_plugins::builtin::yolo_args_of("not-a-cli"), None);
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

// ---------- Issue #27:短别名影子化防护(工作流编译 → 冻结 pin 全链) ----------

mod short_alias_shadow_tests {
    use crate::adapter_launch::workflow_plugin_index;
    use crate::adapter_launch::{resolve_adapter_for_pin, resolve_agent_type_pin};
    use mf_agent::workflow::{WorkflowNodeDraft, WorkflowSnapshot, WorkflowTemplateVersion};
    use mf_agent::workflow_compiler::CompileInput;
    use mf_agent::{AgentInstanceSnapshot, CompileError, RunMode, WorkflowCompiler};
    use mf_plugins::install::InstallSource;
    use mf_plugins::PluginHost;

    /// 带内置合成插件的宿主(临时插件根 + 内存目录库,不碰真实安装目录)。
    fn host_with_builtins() -> (std::sync::Arc<PluginHost>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        (
            PluginHost::load_at_with_catalog(
                tmp.path().to_path_buf(),
                mf_agent::CatalogStore::memory().unwrap(),
                &mf_agent::Config::default(),
                &[],
            ),
            tmp,
        )
    }

    /// 安装第三方插件(完整贡献 ID = `{publisher}.{id}.{agent_id}`);
    /// 只安装不启用 —— enable 会持久化锁文件,后续 install 会按锁文件
    /// 重载内存状态,多包场景须装完再统一 enable。
    fn install(host: &PluginHost, publisher: &str, id: &str, agent_id: &str) -> String {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("monkeyfence-plugin.toml"),
            format!(
                r#"[manifest]
version = 3
publisher = "{publisher}"
id = "{id}"
name = "{publisher}.{id} shadow fixture"
version_str = "0.1.0"
description = "short-alias shadow fixture"

[[agent_types]]
id = "{agent_id}"
name = "{agent_id} Agent"
adapter = "generic-command"
command = "{agent_id}"
modes = ["interactive", "oneshot"]
"#
            ),
        )
        .unwrap();
        let resolved = host
            .install_package(
                src.path(),
                InstallSource::Local {
                    path: src.path().display().to_string(),
                },
            )
            .expect("安装 fixture 不应失败");
        resolved.full_id
    }

    /// 装完多个包后统一启用(规避 enable→install 的锁重载时序)。
    fn enable_all(host: &PluginHost, full_ids: &[&str]) {
        for full_id in full_ids {
            host.enable(full_id, true).expect("启用 fixture 不应失败");
        }
    }

    fn instance(agent_type: &str) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: "inst-alias".into(),
            name: "别名实例".into(),
            agent_type: agent_type.into(),
            version: 1,
            enabled: true,
            run_mode: RunMode::OneShot,
            executable: agent_type.into(),
            argv: vec![],
            env: vec![],
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({}),
            sealed_secret_ids: vec![],
            external_config: false,
        }
    }

    /// 单节点工作流经真实编译器编译(生产索引 workflow_plugin_index 注入)。
    fn compile_one(
        host: &std::sync::Arc<PluginHost>,
        agent_type: &str,
    ) -> Result<WorkflowSnapshot, Vec<CompileError>> {
        let template = WorkflowTemplateVersion {
            version_id: 1,
            template_key: "alias-shadow-test".into(),
            version: 1,
            nodes: vec![WorkflowNodeDraft {
                key: "n1".into(),
                title: "节点".into(),
                instructions: String::new(),
                agent_instance_id: "inst-alias".into(),
                deps: vec![],
            }],
            created_at: String::new(),
        };
        let inst = instance(agent_type);
        let index = workflow_plugin_index(host);
        WorkflowCompiler::new().compile(CompileInput {
            template: &template,
            directory_provider_isolates: true,
            allow_unsafe_shared_directory: false,
            agent_type_plugins: &index,
            resolve_instance: &|_| Ok(inst.clone()),
            directory_provider: None,
        })
    }

    /// 旧实例快照的 legacy 短 id `codex` 在 aaa.codex.codex 存在时,
    /// 编译冻结的 pin 必须指向内置 monkeyfence.codex(不被字典序更靠前的
    /// 第三方影子化),派发期按该 pin 解析到内置适配器。
    #[test]
    fn builtin_codex_short_alias_pins_builtin_package_under_shadow_attempt() {
        let (host, _tmp) = host_with_builtins();
        let aaa = install(&host, "aaa", "codex", "codex");
        enable_all(&host, &[&aaa]);

        let snapshot = compile_one(&host, "codex").expect("内置短别名必须可编译");
        let pin = snapshot.nodes[0].plugin.as_ref().expect("pin 必须冻结");
        assert_eq!(pin.full_id, "monkeyfence.codex");
        assert_eq!(pin.contribution_id, "monkeyfence.codex.codex");

        // 第三方完整贡献 ID 始终精确可用,且指向自己的包
        let index = workflow_plugin_index(&host);
        let third = index
            .get("aaa.codex.codex")
            .expect("第三方完整贡献 ID 必须可用");
        assert_eq!(third.full_id, "aaa.codex");

        // 派发期按冻结 pin 解析:内置 codex 适配器
        let adapter = resolve_adapter_for_pin(&host, Some(pin), "codex").unwrap();
        assert_eq!(adapter.id(), "codex");
    }

    /// 两个第三方同短别名(x/y 各贡献 foo)时,foo 在编译器处稳定拒绝
    /// (plugin-missing,错误确定且重复一致),完整贡献 ID 正常编译冻结;
    /// 薄层单点解析给出歧义错误并要求完整贡献 ID。
    #[test]
    fn compiler_stably_rejects_ambiguous_short_alias_and_accepts_full_ids() {
        let (host, _tmp) = host_with_builtins();
        let x = install(&host, "x", "tools", "foo"); // x.tools.foo
        let y = install(&host, "y", "tools", "foo"); // y.tools.foo
        enable_all(&host, &[&x, &y]);

        // 短别名 foo:索引不含它 → 编译器稳定拒绝(两次错误完全一致)
        let e1 = compile_one(&host, "foo").unwrap_err();
        let e2 = compile_one(&host, "foo").unwrap_err();
        assert_eq!(e1, e2, "歧义短别名的编译拒绝必须稳定");
        assert!(
            e1.iter().any(|e| e.code == "plugin-missing"),
            "应按 plugin-missing 稳定拒绝: {e1:?}"
        );

        // 完整贡献 ID:编译冻结,pin 冻结包身份 + 完整贡献身份
        for full in ["x.tools.foo", "y.tools.foo"] {
            let snapshot = compile_one(&host, full).expect("完整贡献 ID 必须可编译");
            let pin = snapshot.nodes[0].plugin.as_ref().expect("pin 必须冻结");
            assert_eq!(pin.contribution_id, full);
        }

        // 薄层 re-export 的单点解析:歧义错误列出候选并要求完整贡献 ID
        let err = resolve_agent_type_pin(&host, "foo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("歧义") && err.contains("完整贡献 ID"), "{err}");
        assert!(
            err.contains("x.tools.foo") && err.contains("y.tools.foo"),
            "{err}"
        );
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
        yolo_args: None,
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

// ---------- LaunchPlan 迁移等价(Issue #27:GPUI 薄层 ↔ mf-plugins 生产链) ----------

mod launch_migration_equivalence_tests {
    use crate::adapter_launch::compile_instance_launch as gpui_compile;
    use mf_agent::{AgentInstanceSnapshot, RunMode};
    use mf_plugins::adapter_launch::compile_instance_launch as plugin_compile;
    use mf_plugins::PluginHost;

    fn host() -> (std::sync::Arc<PluginHost>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        (PluginHost::empty_at(tmp.path().to_path_buf()), tmp)
    }

    fn instance() -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: "inst_eq".into(),
            name: "等价实例".into(),
            agent_type: "generic-command".into(),
            version: 3,
            enabled: true,
            run_mode: RunMode::OneShot,
            executable: "demo-cli".into(),
            argv: vec!["--fast".into()],
            env: vec![("MF_EQ".into(), "1".into())],
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({"input": "argv", "completion": "process-exit"}),
            sealed_secret_ids: vec![],
            external_config: false,
        }
    }

    /// 同一输入分别走旧路径(crate::adapter_launch 薄兼容)与新生产链
    /// (mf_plugins::adapter_launch typed 编译):编译产物逐字段一致,
    /// 迁移不改变任何启动语义;typed 冻结身份是新增强,不进入旧出口。
    #[test]
    fn gpui_thin_adapter_matches_mf_plugins_chain() {
        let (host, _tmp) = host();
        let catalog = mf_agent::CatalogStore::memory().unwrap();
        let run_temp = std::env::temp_dir().join("mf-launch-eq-run");
        let workdir = std::env::temp_dir().join("mf-launch-eq-work");
        let (inst, pin): (
            AgentInstanceSnapshot,
            Option<mf_agent::workflow::PluginSourcePin>,
        ) = (instance(), None);

        let legacy = gpui_compile(
            &host,
            &catalog,
            &inst,
            pin.as_ref(),
            run_temp.clone(),
            workdir.clone(),
            Some("do it".into()),
            "tok-eq",
            false,
            None,
        )
        .unwrap();
        let typed = plugin_compile(
            &host,
            &catalog,
            &inst,
            pin.as_ref(),
            run_temp.clone(),
            workdir.clone(),
            Some("do it".into()),
            "tok-eq",
            false,
            None,
        )
        .unwrap();
        let frozen = typed.plan();

        assert_eq!(legacy.run_temp, frozen.run_temp);
        assert_eq!(legacy.executable, frozen.executable);
        assert_eq!(legacy.argv, frozen.argv);
        assert_eq!(legacy.env, frozen.env);
        assert_eq!(legacy.cwd, frozen.cwd);
        assert_eq!(legacy.uses_shell, frozen.uses_shell);
        assert_eq!(legacy.temp_files, frozen.temp_files);

        // typed 冻结身份只存在于新出口,旧 LaunchPlan 不携带
        assert_eq!(typed.provenance().agent_instance_id, "inst_eq");
        assert_eq!(typed.provenance().agent_instance_revision, 3);
        assert_eq!(typed.provenance().adapter_id, "generic-command");
    }
}
