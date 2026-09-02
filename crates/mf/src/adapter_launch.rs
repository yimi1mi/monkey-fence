//! 薄兼容层(Issue #27):launch 编译生产链已迁至
//! `mf_plugins::adapter_launch`(UI-neutral,typed LaunchPlan 冻结出口)。
//!
//! 本文件只保留:
//! - 旧路径 re-export(既有调用方 app_ctx/runtime_host/测试不改可用);
//! - `compile_instance_launch` 兼容签名(返回旧 `LaunchPlan`,
//!   行为与迁移前等价,typed 冻结身份经 `TypedLaunchPlan` 供新代码使用);
//! - GPUI 表单 Schema 装配(`config_schema_fields`,依赖 declarative_form)。

// 迁移期保留旧路径全量符号(bin 内未必都有调用方,以 allow 压制 unused)
#[allow(unused_imports)]
pub use mf_plugins::adapter_launch::{
    contribution_supports_mode, grants_shell, legacy_builtin_adapter_id, resolve_adapter,
    resolve_adapter_for_pin, resolve_agent_type_pin, verify_trusted_paths, workflow_plugin_index,
    LaunchPlanProvenance, TypedLaunchPlan,
};

use anyhow::Result;
use mf_agent::workflow::PluginSourcePin;
use mf_agent::{AgentInstanceSnapshot, CatalogStore, LaunchPlan};
use mf_plugins::PluginRegistry;
use std::path::PathBuf;
use std::sync::Arc;

/// 兼容入口:typed 编译链(mf-plugins)→ 旧 `LaunchPlan`。
/// 校验实例、解封 Secret、冻结可信路径校验都在 mf-plugins 生产链完成;
/// 这里仅丢弃 provenance 以保持既有消费者签名不变。
#[allow(clippy::too_many_arguments)]
pub fn compile_instance_launch(
    plugins: &Arc<PluginRegistry>,
    catalog: &Arc<CatalogStore>,
    instance: &AgentInstanceSnapshot,
    pin: Option<&PluginSourcePin>,
    run_temp: PathBuf,
    workdir: PathBuf,
    prompt: Option<String>,
    run_token: &str,
    external_config: bool,
    secret_master_key: Option<[u8; 32]>,
) -> Result<LaunchPlan> {
    Ok(mf_plugins::adapter_launch::compile_instance_launch(
        plugins,
        catalog,
        instance,
        pin,
        run_temp,
        workdir,
        prompt,
        run_token,
        external_config,
        secret_master_key,
    )?
    .into_plan())
}

/// 内置 CLI 类型的声明式配置字段(合成插件无根目录,代码内声明;
/// 与 manifest config_schema 文件同构)。
fn builtin_config_schema_fields(agent_type: &str) -> Vec<crate::declarative_form::FormField> {
    use crate::declarative_form::FormField;
    let f = |id: &str, label: &str, kind: &str, required: bool, options: Vec<&str>| FormField {
        id: id.into(),
        label: label.into(),
        kind: kind.into(),
        required,
        placeholder: String::new(),
        options: options.into_iter().map(str::to_string).collect(),
    };
    match agent_type {
        // 权限不再作为 config 字段:编辑器以 Orca 式权限行物化 yolo 参数进 argv
        // (见 AgentInstanceEditorState::permission_mode);claude 无其他声明字段。
        "claude" | "claude-code" => Vec::new(),
        // secret_env 不再是自由文本字段:编辑器以结构化 ENV→SecretRef
        // 行管理(见 AgentInstanceEditorState::secret_env_map)
        "codex" => vec![f("model", "模型", "text", false, vec![])],
        _ => Vec::new(),
    }
}

/// 加载 Agent Type 的 config_schema 字段:
/// 安装插件读 manifest 声明的 Schema 文件(相对插件根),
/// 内置合成类型用代码内声明;其余为空(编辑器不渲染表单段)。
pub fn config_schema_fields(
    plugins: &Arc<PluginRegistry>,
    full_contribution_id: &str,
) -> Vec<crate::declarative_form::FormField> {
    if let Some((source, contribution)) = plugins
        .contributions()
        .find_agent_type(full_contribution_id)
    {
        if contribution.config_schema.is_empty() {
            // 内置合成插件:full_id 无内容寻址根
            return builtin_config_schema_fields(&contribution.id);
        }
        // 安装插件:从插件包根读 Schema 文件
        if let Some(root) = plugins.plugin_root_of(&source.plugin_full_id) {
            let path = root.join(&contribution.config_schema);
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(schema) = serde_json::from_str::<serde_json::Value>(&text) {
                    return crate::declarative_form::DeclarativeForm::from_json(&schema)
                        .fields()
                        .to_vec();
                }
            }
        }
        return builtin_config_schema_fields(&contribution.id);
    }
    builtin_config_schema_fields(full_contribution_id)
}
