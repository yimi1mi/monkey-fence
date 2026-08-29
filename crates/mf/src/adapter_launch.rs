//! Agent Adapter 启动编译共享层:Agent Type → 适配器解析、Secret 解封、
//! `AgentInstanceSnapshot → LaunchPlan` 编译。离散 CLI 会话(AppCtx)与
//! 工作流 Step 派发(RuntimeHostImpl)共用同一生产链,不各写一份。

use anyhow::Result;
use mf_agent::secrets::SecretStore;
use mf_agent::workflow::PluginSourcePin;
use mf_agent::{AgentAdapter, AgentInstanceSnapshot, CatalogStore, LaunchContext, LaunchPlan};
use mf_plugins::contribution_registry::{AgentTypeContribution, ContributionSource};
use mf_plugins::PluginRegistry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// 旧内置 Agent Type(无插件贡献时)的适配器回退映射。
pub fn legacy_builtin_adapter_id(agent_type: &str) -> Option<&'static str> {
    match agent_type {
        "claude" | "claude-code" => Some("claude-code"),
        "codex" => Some("codex"),
        "generic-command" | "opencode" | "cursor" | "gemini" | "copilot" | "qwen" | "iflow"
        | "aider" | "amp" | "kimi" => Some("generic-command"),
        _ => None,
    }
}

/// 解析 Agent Type:优先插件贡献(完整贡献 ID),旧内置类型回退。
/// 返回 (贡献来源与声明, 适配器) —— 贡献为 None 表示走了 legacy 回退。
pub fn resolve_adapter(
    plugins: &Arc<PluginRegistry>,
    agent_type: &str,
) -> Result<(
    Option<(ContributionSource, AgentTypeContribution)>,
    Arc<dyn AgentAdapter>,
)> {
    let contributions = plugins.contributions();
    let resolved = contributions.find_agent_type(agent_type);
    let adapter_id = match &resolved {
        Some((_, contribution)) => contribution.adapter.as_str(),
        None => legacy_builtin_adapter_id(agent_type).ok_or_else(|| {
            anyhow::anyhow!("Agent Type `{agent_type}` 不存在、已禁用或所属插件未启用")
        })?,
    };
    let adapter = mf_plugins::builtin::adapter_for(adapter_id)
        .ok_or_else(|| anyhow::anyhow!("尚不支持 Agent Adapter: {adapter_id}"))?;
    Ok((resolved, std::sync::Arc::from(adapter)))
}

/// 声明的运行模式是否包含请求模式。
pub fn contribution_supports_mode(modes: &[String], mode: mf_agent::RunMode) -> bool {
    modes.iter().any(|declared| declared == mode.as_str())
}

/// 插件是否授予 Shell 能力(仅当来源插件存在且启用)。
pub fn grants_shell(plugins: &Arc<PluginRegistry>, source: Option<&ContributionSource>) -> bool {
    let Some(source) = source else {
        return false;
    };
    plugins
        .summaries()
        .into_iter()
        .find(|summary| summary.full_id == source.plugin_full_id)
        .is_some_and(|summary| summary.enabled && summary.capabilities.shell)
}

/// 校验实例 + 解封 Secret + 编译 LaunchPlan(生产链唯一入口)。
/// `run_token` 是本次运行的凭据(离散会话/工作流 Step 各自命名);
/// `external_config = true` 表示 Default CLI 只读外部配置意图。
#[allow(clippy::too_many_arguments)]
pub fn compile_instance_launch(
    plugins: &Arc<PluginRegistry>,
    catalog: &Arc<CatalogStore>,
    instance: &AgentInstanceSnapshot,
    run_temp: PathBuf,
    workdir: PathBuf,
    prompt: Option<String>,
    run_token: &str,
    external_config: bool,
) -> Result<LaunchPlan> {
    let (resolved, adapter) = resolve_adapter(plugins, &instance.agent_type)?;
    let validation_errors = adapter.validate(instance);
    if !validation_errors.is_empty() {
        anyhow::bail!("Agent Instance 配置无效: {}", validation_errors.join("; "));
    }
    let mut launch_ctx = LaunchContext::new(run_temp, workdir);
    launch_ctx.prompt = prompt;
    launch_ctx.external_config = external_config;
    launch_ctx.grants_shell = grants_shell(plugins, resolved.as_ref().map(|(src, _)| src));
    if !instance.sealed_secret_ids.is_empty() {
        let secret_store =
            mf_plugins::builtin_secret_store::BuiltinSecretStore::open(catalog.clone())?;
        for secret_id in &instance.sealed_secret_ids {
            let lease = secret_store.unseal_for_run(run_token, secret_id)?;
            launch_ctx
                .secrets
                .insert(secret_id.clone(), Arc::new(lease));
        }
    }
    adapter.compile_launch(instance, &launch_ctx)
}

/// 工作流编译输入:可用 Agent Type → 插件包 pin。
/// 同时注册短 id(实例快照引用的形态,如 `codex`)与完整贡献 ID
/// (`monkeyfence.codex`);pin 数据来自贡献来源,与 Revision 冻结一致。
pub fn workflow_plugin_index(plugins: &Arc<PluginRegistry>) -> HashMap<String, PluginSourcePin> {
    let mut index = HashMap::new();
    for (full_contribution_id, source, _) in plugins.contributions().agent_types() {
        let pin = PluginSourcePin {
            full_id: source.plugin_full_id.clone(),
            version: source.plugin_version.clone(),
            content_hash: source.content_hash.clone(),
        };
        if let Some(short) = full_contribution_id.rsplit_once('.') {
            index
                .entry(short.1.to_string())
                .or_insert_with(|| pin.clone());
        }
        index.insert(full_contribution_id, pin);
    }
    index
}
