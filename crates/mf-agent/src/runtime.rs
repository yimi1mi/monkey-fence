//! Agent Runtime 抽象:Orchestrator 通过 `RuntimeHost` 驱动具体执行器,
//! 不感知 PTY / HTTP / 插件 worker 的实现细节(见 ADR 0002)。

use crate::model::AgentState;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Runtime 类型(对应三种 Adapter)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    Pty,
    Http,
    PluginWorker,
}

impl RuntimeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeKind::Pty => "pty",
            RuntimeKind::Http => "http",
            RuntimeKind::PluginWorker => "plugin-worker",
        }
    }
}

/// 解析后的 Agent Profile 执行规格(由插件注册表 + 用户覆盖合成)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfileSpec {
    pub id: String,
    pub display_name: String,
    pub runtime: RuntimeKind,
    /// Pty:命令;Http:provider 名称;PluginWorker:worker 可执行文件。
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// 权限模式追加参数(yolo / manual)。
    #[serde(default)]
    pub permission_args: Vec<String>,
    /// Http:ProviderConfig。
    pub provider: Option<crate::config::ProviderConfig>,
    pub icon: Option<String>,
    pub homepage: Option<String>,
    /// 状态钩子配置(写入本地 Agent 配置的命名空间条目)。
    #[serde(default)]
    pub hook: Option<HookSpec>,
}

/// 状态钩子:Agent 插件上报 working/waiting/blocked/done 的机制。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    /// 钩子写入的目标配置文件(相对用户主目录或绝对路径)。
    pub config_path: String,
    /// MonkeyFence 命名空间内的条目(JSON 文件内的键名,或 TOML 表名)。
    pub namespace: String,
    /// 钩子命令模板({state} 占位符)。
    pub command_template: String,
}

/// Agent Type 描述:插件贡献的 CLI 执行类型的内核投影。
/// 与 `AgentProfileSpec` 的区别:Profile 面向旧调度路径的完整启动规格,
/// Descriptor 只声明类型契约 —— 启动所有权在 Agent Adapter +
/// `AgentInstanceSnapshot` 编译出的 LaunchPlan(设计 §6.1)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTypeDescriptor {
    pub id: String,
    pub name: String,
    /// 适配器契约标识(claude-code / codex / generic-command / http / plugin-worker)。
    pub adapter: String,
    /// 类型默认命令(创建实例时的初始 executable)。
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub detect_commands: Vec<String>,
    /// 支持的运行模式(oneshot / interactive)。
    #[serde(default)]
    pub modes: Vec<String>,
    /// 是否支持进程级隔离配置(不支持时禁止请求 config 注入,
    /// 防止静默改写真实 CLI 全局配置)。
    #[serde(default)]
    pub supports_isolated_config: bool,
}

/// 一次 Agent Run 的启动规格。
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub run_id: i64,
    pub step_id: i64,
    pub task_id: i64,
    pub session_id: i64,
    pub session_key: Option<String>,
    /// 复用已存在(存活)的会话则不再拉起进程。
    pub attach_existing_session: bool,
    pub profile: AgentProfileSpec,
    pub step_title: String,
    /// 发给 Agent 的初始 prompt(包含 mfctl 结算指令)。
    pub prompt: String,
    pub capability_token: String,
    /// mfctl 管道名与环境(mfctl 从 env 读取令牌)。
    pub pipe_name: String,
    pub mfctl_hint: Option<String>,
    pub workdir: PathBuf,
}

/// 离散 CLI 会话启动规格(设计 §4.7 / §10):挂在 Task 下,
/// 但没有 Step / Agent Run / 结算令牌,不参与 Task 成功判定。
/// 宿主以 `(project, session_id)` 路由,session_id 是 ad_hoc_sessions 行号。
#[derive(Debug, Clone)]
pub struct AdHocLaunchSpec {
    pub task_id: i64,
    pub session_id: i64,
    pub title: String,
    pub run_mode: crate::model::RunMode,
    pub profile: AgentProfileSpec,
    pub prompt: Option<String>,
    pub workdir: PathBuf,
}

/// Runtime → Orchestrator 事件。
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// 进程/请求已成功启动。
    Launched,
    /// 启动失败(如 CLI 不存在)。
    SpawnError(String),
    /// Agent 插件钩子上报的状态(结算唯一依据之外的辅助状态)。
    AgentState(AgentState),
    /// 终端标题/OSC/屏幕嗅探得出的 tui-idle(仅提示用,不能结算)。
    TuiIdle(bool),
    /// 终端有新输出(未读标记)。
    Output,
    /// API transcript 消息。
    Transcript { role: String, text: String },
    /// Agent 发起提问(needs-input)。
    Question(String),
    /// 进程退出 / API 轮次结束(未结算 → awaiting-outcome)。
    Exited { code: Option<i32> },
    /// 结构化 Runtime 直接提交结算。
    Settled(crate::model::Settlement),
}

/// Runtime → Orchestrator 事件(带 run_id)。
pub type TaggedRuntimeEvent = (i64, RuntimeEvent);

/// Orchestrator 调用宿主(GPUI 进程内实现)执行 Agent Run。
pub trait RuntimeHost: Send + Sync {
    /// 启动(或复用会话并发送 prompt)。事件通过 `events` 回调(首元素为 run_id)。
    fn launch(&self, spec: LaunchSpec, events: crossbeam_channel::Sender<TaggedRuntimeEvent>);
    /// 启动离散 CLI 会话:无 run 事件流,状态由宿主直接管理;
    /// 不得发明 Step / Agent Run,也不得触碰 Task 状态。
    fn launch_ad_hoc(&self, spec: AdHocLaunchSpec);
    /// 向运行中的 Agent 追加提示。
    /// `project`:项目根路径 —— run/session id 是各项目数据库的行号,
    /// 跨项目会碰撞,宿主必须以 (project, id) 定位真实会话。
    fn send_prompt(&self, project: &str, run_id: i64, session_id: i64, text: &str);
    /// 停止一次运行(可复用会话保留)。
    fn stop_run(&self, project: &str, run_id: i64);
    /// 强制终止整个会话(进程)。
    fn kill_session(&self, project: &str, session_id: i64);
    /// 回答 Agent 的提问(阻塞等待中的 HTTP Runtime)。
    fn answer_question(&self, project: &str, run_id: i64, answer: &str);
}
