//! Agent Adapter 启动编译共享层(Issue #27:自 GPUI crate 迁入,UI-neutral)。
//!
//! Agent Type → 适配器解析、Secret 解封、`AgentInstanceSnapshot → LaunchPlan`
//! 编译,以及 typed 冻结出口 `TypedLaunchPlan`。离散 CLI 会话与工作流
//! Step 派发共用同一生产链,不各写一份;GPUI 侧只保留薄 re-export。
//!
//! 冻结与安全不变量:
//! - `TypedLaunchPlan` 编译后不可变:executable/argv/env/cwd 与
//!   materialization root(run_temp)只来自一次 `compile_instance_launch`,
//!   外部仅能经 `plan()`/`provenance()` 只读访问,拿不到可变引用;
//! - Agent Type 短别名禁止 first-match/字典序选取(Issue #27):
//!   完整贡献 ID 始终精确可用;内置 `monkeyfence.*` 短别名不可被第三方
//!   影子化;第三方同短别名冲突显式歧义(稳定拒绝并要求完整贡献 ID);
//!   pin 冻结完整贡献身份 + 包版本/内容哈希,短别名不是冻结身份;
//! - `verify_trusted_paths`:Adapter 产出的计划绝不允许改写 Core 提供的
//!   可信 run_temp/workdir,临时/提示/结果文件只允许落在 run-temp 之下;
//!   违规稳定拒绝(纯函数比较,错误信息确定);
//! - Secret 明文只以 `Arc<SecretLease>` 存在于 `secret_env`;
//!   `LaunchPlan`/`TypedLaunchPlan` 均不实现 Serialize,Debug 输出一律脱敏。

use crate::contribution_registry::{AgentTypeContribution, ContributionSource};
use crate::PluginRegistry;
use anyhow::Result;
use mf_agent::secrets::SecretStore;
use mf_agent::workflow::PluginSourcePin;
use mf_agent::{AgentAdapter, AgentInstanceSnapshot, CatalogStore, LaunchContext, LaunchPlan};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// 内置插件 full_id 集合(短别名归属判定、空哈希 pin 校验用)。
/// 以宿主 `builtin` 标记为准,不以 `monkeyfence.` 前缀字符串猜测身份。
fn builtin_plugin_ids(plugins: &Arc<PluginRegistry>) -> HashSet<String> {
    plugins
        .summaries()
        .into_iter()
        .filter(|summary| summary.builtin)
        .map(|summary| summary.full_id)
        .collect()
}

/// 完整贡献 ID 的短别名形态(最后一个 `.` 段;与历史索引一致,
/// 贡献 id 含 `.` 时别名是其末段)。
fn short_alias_of(full_contribution_id: &str) -> Option<&str> {
    full_contribution_id
        .rsplit_once('.')
        .map(|(_, short)| short)
}

/// 短别名归属判定(Issue #27:禁止 first-match/字典序选取):
/// - 候选含内置贡献时内置优先:唯一内置候选直接归属;多个内置候选
///   (如 API Provider 与 CLI 同名)时仅合成正典形态
///   `monkeyfence.{alias}.{alias}` 可归属,否则同样显式歧义 ——
///   第三方任何命名都不能影子化内置短别名;
/// - 仅第三方候选:唯一可用(兼容既有唯一短别名),多个显式歧义;
/// - 无候选:None(按不存在处理)。
enum ShortAliasOwner {
    Unique(String),
    Ambiguous(Vec<String>),
}

fn short_alias_owner(
    alias: &str,
    entries: &[(String, ContributionSource, AgentTypeContribution)],
    builtin: &HashSet<String>,
) -> Option<ShortAliasOwner> {
    let mut all: Vec<&str> = Vec::new();
    let mut builtin_candidates: Vec<&str> = Vec::new();
    for (full_id, source, _) in entries {
        if short_alias_of(full_id) != Some(alias) {
            continue;
        }
        all.push(full_id.as_str());
        if builtin.contains(&source.plugin_full_id) {
            builtin_candidates.push(full_id.as_str());
        }
    }
    let owned = |ids: Vec<&str>| ids.into_iter().map(str::to_string).collect::<Vec<_>>();
    match builtin_candidates.as_slice() {
        [] => match all.as_slice() {
            [] => None,
            [only] => Some(ShortAliasOwner::Unique((*only).to_string())),
            _ => Some(ShortAliasOwner::Ambiguous(owned(all))),
        },
        [only] => Some(ShortAliasOwner::Unique((*only).to_string())),
        _ => {
            // 多个内置候选:只有与 legacy 内置类型一一对应的合成正典形态
            // (`monkeyfence.codex.codex` 等)稳定归属,其余一律显式歧义
            let canonical = format!("monkeyfence.{alias}.{alias}");
            builtin_candidates
                .iter()
                .find(|full_id| **full_id == canonical)
                .map(|full_id| ShortAliasOwner::Unique((*full_id).to_string()))
                .or_else(|| Some(ShortAliasOwner::Ambiguous(owned(all))))
        }
    }
}

/// 贡献来源 → Revision 冻结语义的插件包 pin。`contribution_id` 冻结
/// 完整贡献身份(派发期精确校验);短别名永远不作为冻结身份。
fn source_pin_of(full_contribution_id: &str, source: &ContributionSource) -> PluginSourcePin {
    PluginSourcePin {
        full_id: source.plugin_full_id.clone(),
        version: source.plugin_version.clone(),
        content_hash: source.content_hash.clone(),
        contribution_id: full_contribution_id.to_string(),
    }
}

/// pin 冻结了贡献身份(contribution_id 非空)时精确校验解析结果;
/// 旧快照 contribution_id 为空 → 兼容放行(已冻结 Revision 行为不变)。
fn ensure_contribution_identity(
    pin: &PluginSourcePin,
    matched_full_contribution_id: &str,
) -> Result<()> {
    anyhow::ensure!(
        pin.contribution_id.is_empty() || pin.contribution_id == matched_full_contribution_id,
        "pin 的贡献身份与解析结果不一致(pin 冻结 `{}`,实际 `{}`);请使用完整贡献 ID 重新编译",
        pin.contribution_id,
        matched_full_contribution_id
    );
    Ok(())
}

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
/// 短别名歧义时给出确定性的候选列表与完整贡献 ID 指引(不 first-match)。
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
        None => legacy_builtin_adapter_id(agent_type)
            .ok_or_else(|| ambiguous_or_missing_agent_type(plugins, &contributions, agent_type))?,
    };
    let adapter = crate::builtin::adapter_for(adapter_id)
        .ok_or_else(|| anyhow::anyhow!("尚不支持 Agent Adapter: {adapter_id}"))?;
    Ok((resolved, std::sync::Arc::from(adapter)))
}

/// 短别名/未知 Agent Type 的稳定错误:歧义时列出全部候选并要求
/// 完整贡献 ID;错误文本只含插件标识,不含敏感值。
fn ambiguous_or_missing_agent_type(
    plugins: &Arc<PluginRegistry>,
    contributions: &crate::contribution_registry::ContributionRegistry,
    agent_type: &str,
) -> anyhow::Error {
    let entries = contributions.agent_types();
    match short_alias_owner(agent_type, &entries, &builtin_plugin_ids(plugins)) {
        Some(ShortAliasOwner::Ambiguous(candidates)) => anyhow::anyhow!(
            "Agent Type 短别名 `{agent_type}` 歧义(候选:{}),禁止按字典序选取;请使用完整贡献 ID",
            candidates.join("、")
        ),
        _ => anyhow::anyhow!("Agent Type `{agent_type}` 不存在、已禁用或所属插件未启用"),
    }
}

/// 按 Revision 冻结的插件包 pin 解析 Adapter(Runtime 从 pinned package
/// manifest/worker 声明取适配器,不随插件更新漂移):
/// - 内容寻址包:`resolve` 校验包身份/版本/内容哈希后从该包 manifest 的
///   agent_types 定位 Adapter(worker 型贡献同样由 manifest 声明,首版接入
///   适配器契约);pin 冻结了贡献身份(contribution_id 非空)时精确校验;
/// - 内置合成插件(空内容哈希):只允许 builtin 插件(与 pin 生命周期
///   `ensure_builtin_plugin` 一致,第三方包不得借空哈希绕过内容寻址),
///   当前注册表贡献必须与 pin 的包版本一致;
/// - 无 pin(旧快照/离散会话):回退当前注册表 + legacy 映射。
pub fn resolve_adapter_for_pin(
    plugins: &Arc<PluginRegistry>,
    pin: Option<&PluginSourcePin>,
    agent_type: &str,
) -> Result<Arc<dyn AgentAdapter>> {
    let Some(pin) = pin else {
        let (_, adapter) = resolve_adapter(plugins, agent_type)?;
        return Ok(adapter);
    };
    if !pin.content_hash.is_empty() {
        let resolved = plugins.resolve(&pin.full_id, &pin.version, &pin.content_hash)?;
        let contribution = resolved
            .manifest
            .agent_types
            .iter()
            .find(|a| a.id == agent_type || format!("{}.{}", pin.full_id, a.id) == agent_type);
        let contribution = contribution.ok_or_else(|| {
            anyhow::anyhow!(
                "pin 的插件包 {}@{} 不贡献 Agent Type `{agent_type}`",
                pin.full_id,
                pin.version
            )
        })?;
        ensure_contribution_identity(pin, &format!("{}.{}", pin.full_id, contribution.id))?;
        let adapter_id = contribution.adapter.as_str();
        return crate::builtin::adapter_for(adapter_id)
            .map(std::sync::Arc::from)
            .ok_or_else(|| anyhow::anyhow!("尚不支持 Agent Adapter: {adapter_id}"));
    }
    // 空内容哈希的 pin 只允许内置合成插件:第三方包必须内容寻址,
    // 否则包身份/哈希校验可被整体绕过(短别名不是冻结身份)
    anyhow::ensure!(
        builtin_plugin_ids(plugins).contains(&pin.full_id),
        "Agent Type `{agent_type}` 不存在或不属于 pin 的内置合成插件 {}@{}(空内容哈希的 pin 只允许内置插件)",
        pin.full_id,
        pin.version
    );
    // 内置合成插件:在 pin 的插件包贡献里定位 Agent Type(接受完整贡献 ID
    // 与历史短 id 两种引用形态),当前注册表版本必须与 pin 一致
    let entries = plugins.contributions().agent_types();
    let matched = entries.into_iter().find(|(full_id, source, contribution)| {
        source.plugin_full_id == pin.full_id
            && (full_id == agent_type
                || contribution.id == agent_type
                || format!("{}.{}", pin.full_id, contribution.id) == agent_type)
    });
    let (full_contribution_id, source, contribution) = matched.ok_or_else(|| {
        anyhow::anyhow!(
            "Agent Type `{agent_type}` 不存在或不属于 pin 的插件包 {}@{}",
            pin.full_id,
            pin.version
        )
    })?;
    anyhow::ensure!(
        source.plugin_version == pin.version,
        "Agent Type `{agent_type}` 的插件版本与 pin 不一致(pin {}@{},当前 {}@{})",
        pin.full_id,
        pin.version,
        source.plugin_full_id,
        source.plugin_version
    );
    ensure_contribution_identity(pin, &full_contribution_id)?;
    crate::builtin::adapter_for(contribution.adapter.as_str())
        .map(std::sync::Arc::from)
        .ok_or_else(|| anyhow::anyhow!("尚不支持 Agent Adapter: {}", contribution.adapter))
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

/// 编译时刻冻结的身份(Revision 冻结语义,不随插件/实例后续编辑漂移)。
/// 只承载非敏感标识;凭据永远不进本结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlanProvenance {
    /// Agent Instance 稳定 ID。
    pub agent_instance_id: String,
    /// 编译依据的实例版本行(Revision)。
    pub agent_instance_revision: i64,
    /// Agent Type(与实例快照一致:短 id 或完整贡献 ID)。
    pub agent_type: String,
    /// 实际执行编译的 Adapter 契约 ID。
    pub adapter_id: String,
    /// Revision 冻结的插件包 pin(离散会话为 None,按当前注册表解析)。
    pub plugin_pin: Option<PluginSourcePin>,
    /// Provider 身份(实例绑定的 Provider Profile 引用;未绑定为 None)。
    /// 只是标识引用,Secret 仍按 sealed ref 走独立解封链。
    pub provider_identity: Option<String>,
}

/// 实例 config 中预留的 Provider Profile 引用键(非敏感标识)。
const PROVIDER_PROFILE_CONFIG_KEY: &str = "provider_profile";

fn provider_identity_of(instance: &AgentInstanceSnapshot) -> Option<String> {
    instance
        .config
        .get(PROVIDER_PROFILE_CONFIG_KEY)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 冻结的 typed 启动计划:一次编译产出的 LaunchPlan + 编译身份。
/// 字段私有:外部只能经 `plan()`/`provenance()` 只读访问,
/// 兼容旧消费者的出口是 `into_plan()`(provenance 不随之携带)。
pub struct TypedLaunchPlan {
    plan: LaunchPlan,
    provenance: LaunchPlanProvenance,
}

impl TypedLaunchPlan {
    /// 冻结的启动计划(只读)。
    pub fn plan(&self) -> &LaunchPlan {
        &self.plan
    }

    /// 编译身份(只读)。
    pub fn provenance(&self) -> &LaunchPlanProvenance {
        &self.provenance
    }

    /// 兼容出口:迁移期旧消费者(GPUI 薄层等)仍消费 `mf_agent::LaunchPlan`。
    pub fn into_plan(self) -> LaunchPlan {
        self.plan
    }

    /// 脱敏明文值列表(委托 LaunchPlan;仅启动期喂给输出过滤,不进日志)。
    pub fn redaction_values(&self) -> Vec<&str> {
        self.plan.redaction_values()
    }
}

// Debug 手写脱敏:只输出身份与计划形状。argv/env 的值可能含提示文本或
// 实例私有配置,只输出 env 键名与计数;secret_env 只显示 ENV 键名。
impl fmt::Debug for TypedLaunchPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env_keys: Vec<&str> = self.plan.env.iter().map(|(k, _)| k.as_str()).collect();
        let secret_keys: Vec<&str> = self
            .plan
            .secret_env
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        f.debug_struct("TypedLaunchPlan")
            .field("provenance", &self.provenance)
            .field("run_temp", &self.plan.run_temp)
            .field("cwd", &self.plan.cwd)
            .field("executable", &self.plan.executable)
            .field("argv_len", &self.plan.argv.len())
            .field("env_keys", &env_keys)
            .field("secret_env_keys(redacted)", &secret_keys)
            .finish()
    }
}

/// 校验 Adapter 产出的 LaunchPlan 没有改写 Core 提供的可信路径:
/// - `run_temp`(materialization root)与 `cwd` 必须逐字节等于
///   LaunchContext 携带的可信值(路径由 Core/Execution Lease 提供,
///   adapter/插件传入任何不同路径都视为越权);
/// - 待物化临时文件必须为相对路径(由 Runtime Host 在 run-temp 下物化),
///   组件规则与 Host 物化一致(`..`、前导 `./`、根/盘符组件、空路径均拒),
///   提示/结果文件必须位于 run-temp 之下;
/// - `starts_with` 只做组件前缀匹配,中间的 `..` 仍可逃逸,需一并拒绝。
///
/// 纯函数:同样的 plan/ctx 永远得到同样结论与错误文本(稳定拒绝)。
pub fn verify_trusted_paths(plan: &LaunchPlan, ctx: &LaunchContext) -> Result<()> {
    if plan.run_temp != ctx.run_temp {
        anyhow::bail!(
            "LaunchPlan 不得改写 Core 提供的可信 run-temp(计划 {},可信 {})",
            plan.run_temp.display(),
            ctx.run_temp.display()
        );
    }
    match &plan.cwd {
        Some(cwd) if cwd == &ctx.workdir => {}
        other => anyhow::bail!(
            "LaunchPlan 不得改写 Core 提供的可信工作目录(计划 {:?},可信 {})",
            other.as_ref().map(|p| p.display().to_string()),
            ctx.workdir.display()
        ),
    }
    for spec in &plan.temp_files {
        ensure_relative(spec.path.as_path(), "临时文件")?;
    }
    if let mf_agent::InputInjection::PromptFile(path) = &plan.input {
        ensure_under_temp(path, &ctx.run_temp, "提示文件")?;
    }
    if let mf_agent::CompletionDetector::ResultFile(path) = &plan.completion {
        ensure_under_temp(path, &ctx.run_temp, "结果文件")?;
    }
    Ok(())
}

/// 相对路径校验:物化型文件必须由 Runtime Host 在 run-temp 下落盘。
/// 组件规则与 Runtime Host `materialize_temp_files` 逐条一致(编译期
/// 稳定拒绝,不把逃逸留到 spawn 期才暴露):
/// 空路径、绝对路径,以及 `Prefix`(盘符相对如 `C:file`,Windows 上
/// `join` 会整体替换基路径)、`RootDir`(如 `\file`,无盘符根)、
/// `CurDir`(前导 `./`)、`ParentDir`(`..`)组件一律拒绝。
fn ensure_relative(path: &Path, label: &str) -> Result<()> {
    use Component::{CurDir, ParentDir, Prefix, RootDir};
    if path.as_os_str().is_empty() {
        anyhow::bail!("{label}路径不能为空(在 run-temp 下物化)");
    }
    if path.is_absolute() {
        anyhow::bail!(
            "{label}路径必须是相对路径(在 run-temp 下物化): {}",
            path.display()
        );
    }
    if path
        .components()
        .any(|c| matches!(c, CurDir | ParentDir | RootDir | Prefix(_)))
    {
        anyhow::bail!(
            "{label}路径不允许 `..`/`./`/根/盘符组件(路径逃逸): {}",
            path.display()
        );
    }
    Ok(())
}

/// 逃逸校验:启动期引用的文件必须位于可信 run-temp 之下。
/// `starts_with` 只做组件前缀匹配,中间的 `..` 仍可逃逸,需一并拒绝。
fn ensure_under_temp(path: &Path, run_temp: &Path, label: &str) -> Result<()> {
    if !path.starts_with(run_temp) || path.components().any(|c| c == Component::ParentDir) {
        anyhow::bail!("{label}路径必须位于可信 run-temp 之下: {}", path.display());
    }
    Ok(())
}

/// 校验实例 + 解封 Secret + 编译 LaunchPlan(生产链唯一入口)。
/// `run_token` 是本次运行的凭据(离散会话/工作流 Step 各自命名);
/// `pin` 是 Revision 冻结的插件包身份(工作流 Step 按 pin 解析 Adapter;
/// 离散会话为 None,走当前注册表);
/// `external_config = true` 表示 Default CLI 只读外部配置意图。
///
/// Adapter 编译产物先经 `verify_trusted_paths` 冻结校验,再附着
/// `LaunchPlanProvenance` 以 typed 只读结构返回。
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
) -> Result<TypedLaunchPlan> {
    let adapter = resolve_adapter_for_pin(plugins, pin, &instance.agent_type)?;
    let validation_errors = adapter.validate(instance);
    if !validation_errors.is_empty() {
        anyhow::bail!("Agent Instance 配置无效: {}", validation_errors.join("; "));
    }
    let mut launch_ctx = LaunchContext::new(run_temp, workdir);
    launch_ctx.prompt = prompt;
    launch_ctx.external_config = external_config;
    // Shell 能力门控按当前注册表的启用状态(pinned 包已停用时保守拒绝)
    let contribution_source = plugins
        .contributions()
        .find_agent_type(&instance.agent_type)
        .map(|(src, _)| src);
    launch_ctx.grants_shell = grants_shell(plugins, contribution_source.as_ref());
    if !instance.sealed_secret_ids.is_empty() {
        let secret_store = match secret_master_key {
            Some(key) => crate::builtin_secret_store::BuiltinSecretStore::with_master_key(
                catalog.clone(),
                key,
            )?,
            None => crate::builtin_secret_store::BuiltinSecretStore::open(catalog.clone())?,
        };
        // run token 授权:本次 run 只能解封其实例声明的 Secret;
        // RAII 守卫保证解封(或出错/panic)后立即撤销,
        // 不留长期有效凭据(I11:配对由类型系统保证)
        let declared: Vec<&str> = instance
            .sealed_secret_ids
            .iter()
            .map(String::as_str)
            .collect();
        let _grant = crate::builtin_secret_store::RunSecretGrant::authorize(run_token, &declared);
        for secret_id in &instance.sealed_secret_ids {
            let lease = secret_store.unseal_for_run(run_token, secret_id)?;
            launch_ctx
                .secrets
                .insert(secret_id.clone(), Arc::new(lease));
        }
    }
    let plan = adapter.compile_launch(instance, &launch_ctx)?;
    // 冻结校验:adapter 不得改写 Core 提供的可信路径,违规稳定拒绝
    verify_trusted_paths(&plan, &launch_ctx)?;
    let provenance = LaunchPlanProvenance {
        agent_instance_id: instance.id.clone(),
        agent_instance_revision: instance.version,
        agent_type: instance.agent_type.clone(),
        adapter_id: adapter.id().to_string(),
        plugin_pin: pin.cloned(),
        provider_identity: provider_identity_of(instance),
    };
    Ok(TypedLaunchPlan { plan, provenance })
}

/// 把 Agent Type 引用解析为插件包 pin(工作流编译输入的单点解析器):
/// - 完整贡献 ID(`publisher.plugin.agent_type`)始终精确命中;
/// - 短别名按显式归属规则(同 `workflow_plugin_index`):内置优先、
///   唯一第三方兼容;歧义别名稳定拒绝,列出全部候选并要求完整贡献 ID;
/// - 错误文本只含插件标识/版本,不含任何敏感值。
/// 该函数只看当前注册表,不包含 legacy 内置回退(那是 Adapter 解析层
/// `resolve_adapter` 的职责)。
pub fn resolve_agent_type_pin(
    plugins: &Arc<PluginRegistry>,
    agent_type: &str,
) -> Result<PluginSourcePin> {
    let entries = plugins.contributions().agent_types();
    if let Some((full_contribution_id, source, _)) =
        entries.iter().find(|(full_id, _, _)| full_id == agent_type)
    {
        return Ok(source_pin_of(full_contribution_id, source));
    }
    match short_alias_owner(agent_type, &entries, &builtin_plugin_ids(plugins)) {
        Some(ShortAliasOwner::Unique(full_contribution_id)) => {
            let (full_contribution_id, source, _) = entries
                .iter()
                .find(|(full_id, _, _)| *full_id == full_contribution_id)
                .expect("归属结果必然来自候选集");
            Ok(source_pin_of(full_contribution_id, source))
        }
        Some(ShortAliasOwner::Ambiguous(candidates)) => anyhow::bail!(
            "Agent Type 短别名 `{agent_type}` 歧义(候选:{}),禁止按字典序选取;请使用完整贡献 ID",
            candidates.join("、")
        ),
        None => anyhow::bail!(
            "Agent Type `{agent_type}` 不存在、已禁用或所属插件未启用(引用必须是完整贡献 ID,或无歧义的短别名)"
        ),
    }
}

/// 工作流编译输入:可用 Agent Type → 插件包 pin(Issue #27 短别名安全):
/// - 完整贡献 ID 始终精确可用,永不被别名规则改写;
/// - 短别名(实例快照引用的历史形态,如 `codex`)按显式归属规则注册,
///   禁止按注册/字典序 first-match 悄悄选一个:
///   * 内置 `monkeyfence.*` 贡献的稳定短别名不可被第三方影子化;
///   * 仅第三方候选且唯一 → 保持兼容可用;
///   * 候选冲突且无确定性归属 → 短别名显式歧义、不进索引,
///     查找落空使调用方(Workflow Compiler)稳定拒绝并要求完整贡献 ID
///     (单点解析与确定性错误文本见 `resolve_agent_type_pin`);
/// - pin 冻结完整贡献身份(contribution_id)+ 包版本/内容哈希,
///   派发期 `resolve_adapter_for_pin` 逐项校验,短别名不是冻结身份。
pub fn workflow_plugin_index(plugins: &Arc<PluginRegistry>) -> HashMap<String, PluginSourcePin> {
    let entries = plugins.contributions().agent_types();
    let builtin = builtin_plugin_ids(plugins);
    let mut index = HashMap::new();
    for (full_contribution_id, source, _) in &entries {
        index.insert(
            full_contribution_id.clone(),
            source_pin_of(full_contribution_id, source),
        );
    }
    // 短别名去重后逐个判定归属(与候选顺序无关)
    let mut aliases: Vec<&str> = entries
        .iter()
        .filter_map(|(full_contribution_id, _, _)| short_alias_of(full_contribution_id))
        .collect();
    aliases.sort_unstable();
    aliases.dedup();
    for alias in aliases {
        if let Some(ShortAliasOwner::Unique(full_contribution_id)) =
            short_alias_owner(alias, &entries, &builtin)
        {
            if let Some(pin) = index.get(&full_contribution_id) {
                index.insert(alias.to_string(), pin.clone());
            }
        }
    }
    index
}
