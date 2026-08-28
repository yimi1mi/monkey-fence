//! Agent Adapter 契约(设计 §6.1):插件把 Agent Instance 快照编译为
//! `LaunchPlan`,内核 Runtime Host 只消费 LaunchPlan,不解释 Agent 专属配置。
//!
//! 安全不变量:
//! - LaunchPlan 默认 `executable + argv` 直启,不经 Shell;
//!   `uses_shell` 只有在插件拥有 `capabilities.shell` 授权时才允许为真。
//! - Secret 明文只存在于共享的 `SecretLease` 中,LaunchPlan 只持有租约引用;
//!   Debug 输出一律脱敏,不会复制成普通 String。
//! - 临时文件只写入本次运行的 run-temp 目录,不触碰用户全局配置。

use crate::agent_instance::AgentInstanceSnapshot;
use crate::secrets::SecretLease;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// 临时文件规格:启动前由 Runtime Host 在可信 run-temp 下物化。
/// `path` 必须是相对路径,不得自行携带根目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempFileSpec {
    pub path: PathBuf,
    pub contents: Vec<u8>,
}

/// 输入注入:任务提示进入进程的方式(内容已解析完毕)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputInjection {
    /// 追加为尾随参数(内置 CLI 均支持位置参数提示)。
    Argv(String),
    /// 写入 stdin。
    Stdin(Vec<u8>),
    /// 写入临时文件,值为其路径(内容同时出现在 `temp_files`)。
    PromptFile(PathBuf),
}

/// 完成检测策略(设计 §9.5)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionDetector {
    /// 一次性模式:以进程退出为准。
    ProcessExit,
    /// stdout 出现标记字符串。
    StdoutMarker(String),
    /// 结果文件出现(路径在 run-temp 下)。
    ResultFile(PathBuf),
    /// 人工确认(交互式)。
    Manual,
}

/// 启动计划:Runtime Host 执行的全部输入。
/// `secret_env` 只持有启动期 Secret 租约的共享引用;
/// Debug 输出不含任何 Secret 值,最后一个引用释放时明文被 zeroize。
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    /// 本次运行可物化临时文件的唯一根目录。
    pub run_temp: PathBuf,
    pub executable: PathBuf,
    /// 参数数组(不含 executable 本身;边界由数组元素保证,不经 Shell 拼接)。
    pub argv: Vec<String>,
    /// 非敏感环境变量。
    pub env: Vec<(String, String)>,
    /// 敏感环境变量(值为 zeroizing Secret 租约的共享引用)。
    pub secret_env: Vec<(String, Arc<SecretLease>)>,
    pub cwd: Option<PathBuf>,
    pub temp_files: Vec<TempFileSpec>,
    pub input: InputInjection,
    pub completion: CompletionDetector,
    /// Shell 模式(必须已通过插件 shell 权限门控)。
    pub uses_shell: bool,
}

impl LaunchPlan {
    /// 脱敏明文值列表(仅启动期使用;不进日志)。
    pub fn redaction_values(&self) -> Vec<&str> {
        self.secret_env
            .iter()
            .filter_map(|(_, lease)| std::str::from_utf8(lease.as_slice()).ok())
            .collect()
    }
}

/// 执行契约(Agent Instance 版本行 `execution_contract` JSON 的结构)。
/// 字段名为 snake_case;模式枚举值为 kebab-case(argv / stdin / prompt-file、
/// process-exit / stdout-marker / result-file / manual)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecutionContract {
    /// 输入注入模式。
    #[serde(default)]
    pub input: InputMode,
    /// 完成检测模式。
    #[serde(default)]
    pub completion: CompletionMode,
    /// `stdout-marker` 模式的标记字符串。
    #[serde(default)]
    pub stdout_marker: String,
    /// `result-file` 模式的文件名(相对 run-temp)。
    #[serde(default)]
    pub result_file: String,
    /// Shell 模式(需要插件 `capabilities.shell` 授权)。
    #[serde(default)]
    pub use_shell: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputMode {
    #[default]
    Argv,
    Stdin,
    PromptFile,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompletionMode {
    #[default]
    ProcessExit,
    StdoutMarker,
    ResultFile,
    Manual,
}

impl ExecutionContract {
    /// 从快照的 execution_contract JSON 解析;未知模式返回错误。
    pub fn parse(snapshot: &AgentInstanceSnapshot) -> Result<ExecutionContract> {
        serde_json::from_value(snapshot.execution_contract.clone())
            .map_err(|e| anyhow::anyhow!("执行契约非法: {e}"))
    }
}

/// 编译上下文:Runtime Host 在启动前准备好的环境。
/// `secrets` 是已解封的 zeroizing 租约(secret-id → lease),只在启动期存在。
#[derive(Debug, Clone)]
pub struct LaunchContext {
    /// 本次 Agent Run 专属临时目录(配置/结果文件都写在这里)。
    pub run_temp: PathBuf,
    /// 执行工作目录(Execution Lease 提供的路径)。
    pub workdir: PathBuf,
    /// 发给 Agent 的提示(可选;离散会话可无)。
    pub prompt: Option<String>,
    /// 插件是否被授权 Shell 能力(capabilities.shell)。
    pub grants_shell: bool,
    /// 已解封 Secret(secret-id → zeroizing 租约)。
    pub secrets: HashMap<String, Arc<SecretLease>>,
}

impl LaunchContext {
    pub fn new(run_temp: PathBuf, workdir: PathBuf) -> LaunchContext {
        LaunchContext {
            run_temp,
            workdir,
            prompt: None,
            grants_shell: false,
            secrets: HashMap::new(),
        }
    }
}

/// 进程观察快照(Runtime Host 喂给适配器做完成判定与结果提取)。
#[derive(Debug, Clone, Default)]
pub struct ProcessObservation {
    pub exited: bool,
    pub exit_code: Option<i32>,
    /// stdout 尾部(有界;不是完整转录)。
    pub stdout_tail: String,
    /// 结果文件内容(completion = result-file 时)。
    pub result_file: Option<Vec<u8>>,
}

/// 完成判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionObservation {
    /// 仍在运行。
    Running,
    /// 按契约判定完成。
    Completed,
    /// 判定失败(契约自身错误,如标记文件损坏)。
    Failed(String),
}

/// Handoff 草案:固定字段 + 自定义 `output`(设计 §4.5)。
/// 原始终端输出只通过 `raw_log_ref` 引用,不复制内容。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HandoffDraft {
    pub status: String,
    pub summary: String,
    pub changed_files: Vec<String>,
    pub artifacts: Vec<String>,
    pub verification: Option<serde_json::Value>,
    pub blockers: Vec<String>,
    pub recommendations: Vec<String>,
    pub output: serde_json::Value,
    pub raw_log_ref: Option<String>,
}

/// Agent Adapter:每个 Agent Type 的执行行为(设计 §6.1)。
pub trait AgentAdapter: Send + Sync {
    /// 适配器契约标识(claude-code / codex / generic-command ...)。
    fn id(&self) -> &'static str;
    /// 校验实例快照(结构化契约检查);返回错误列表,空为通过。
    fn validate(&self, snapshot: &AgentInstanceSnapshot) -> Vec<String>;
    /// 把快照 + 上下文编译为启动计划。
    fn compile_launch(
        &self,
        snapshot: &AgentInstanceSnapshot,
        ctx: &LaunchContext,
    ) -> Result<LaunchPlan>;
    /// 按契约观察一次性/交互式完成状态。
    fn observe(
        &self,
        snapshot: &AgentInstanceSnapshot,
        obs: &ProcessObservation,
    ) -> CompletionObservation;
    /// 从观察结果提取 Handoff。
    fn extract_handoff(&self, obs: &ProcessObservation) -> Result<HandoffDraft>;
}

/// 敏感环境变量集合(ENV 名 → zeroizing 租约引用)。
pub type SecretEnv = Vec<(String, Arc<SecretLease>)>;

/// 通用逻辑:从快照 config 的 `secret_env` 映射(ENV 名 → secret-id)
/// 解析出敏感环境变量。所有内置适配器共用此约定。
/// 不复制明文;缺 Secret 或引用未声明的 Secret 时报错阻止启动。
pub fn resolve_secret_env(
    snapshot: &AgentInstanceSnapshot,
    ctx: &LaunchContext,
) -> Result<SecretEnv> {
    let mut secret_env = Vec::new();
    let Some(mapping) = snapshot
        .config
        .get("secret_env")
        .and_then(|v| v.as_object())
    else {
        return Ok(secret_env);
    };
    for (env_name, secret_id) in mapping {
        let Some(id) = secret_id.as_str() else {
            anyhow::bail!("secret_env.{env_name} 必须是 secret id 字符串");
        };
        if !snapshot
            .sealed_secret_ids
            .iter()
            .any(|declared| declared == id)
        {
            anyhow::bail!("Secret `{id}` 未在 Agent Instance 快照中声明,阻止启动");
        }
        let Some(value) = ctx.secrets.get(id) else {
            anyhow::bail!("Secret `{id}` 未解封,阻止启动(env {env_name})");
        };
        secret_env.push((env_name.clone(), Arc::clone(value)));
    }
    Ok(secret_env)
}
