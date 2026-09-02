//! Agent Runtime 抽象:Orchestrator 通过 `RuntimeHost` 驱动具体执行器,
//! 不感知 PTY / HTTP / 插件 worker 的实现细节(见 ADR 0002)。

use crate::model::AgentState;
use anyhow::Result;
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
#[derive(Clone, Serialize, Deserialize)]
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

impl std::fmt::Debug for AgentProfileSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env_keys = self.env.iter().map(|(key, _)| key).collect::<Vec<_>>();
        f.debug_struct("AgentProfileSpec")
            .field("id", &self.id)
            .field("display_name", &self.display_name)
            .field("runtime", &self.runtime)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env_keys", &env_keys)
            .field("permission_args", &self.permission_args)
            .field("provider", &self.provider.as_ref().map(|_| "<configured>"))
            .field("icon", &self.icon)
            .field("homepage", &self.homepage)
            .field("hook", &self.hook)
            .finish()
    }
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
#[derive(Clone)]
pub struct LaunchSpec {
    /// 项目根只描述项目归属；运行时注册表不得用路径寻址。
    pub project_root: PathBuf,
    pub run_id: i64,
    pub run_handle: String,
    pub step_id: i64,
    pub task_id: i64,
    pub session_id: i64,
    pub session_handle: String,
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

impl std::fmt::Debug for LaunchSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchSpec")
            .field("project_root", &self.project_root)
            .field("run_id", &self.run_id)
            .field("run_handle", &self.run_handle)
            .field("step_id", &self.step_id)
            .field("task_id", &self.task_id)
            .field("session_id", &self.session_id)
            .field("session_handle", &self.session_handle)
            .field("session_key", &self.session_key)
            .field("attach_existing_session", &self.attach_existing_session)
            .field("profile", &self.profile)
            .field("step_title", &self.step_title)
            .field("prompt", &"<redacted>")
            .field("capability_token", &"<redacted>")
            .field("pipe_name", &self.pipe_name)
            .field(
                "mfctl_hint",
                &self.mfctl_hint.as_ref().map(|_| "<configured>"),
            )
            .field("workdir", &self.workdir)
            .finish()
    }
}

/// 离散 CLI 会话启动规格(设计 §4.7 / §10):挂在 Task 下,
/// 但没有 Step / Agent Run / 结算令牌,不参与 Task 成功判定。
/// 宿主以 `(project, session_id)` 路由,session_id 是 ad_hoc_sessions 行号。
#[derive(Clone)]
pub struct AdHocLaunchSpec {
    pub task_id: i64,
    pub session_id: i64,
    pub title: String,
    pub run_mode: crate::model::RunMode,
    /// Adapter 已编译完成的启动计划;Runtime Host 不解释 Agent 专属配置。
    pub plan: crate::agent_adapter::LaunchPlan,
    /// App/Orchestrator 提供的可信物化根,不得由 Adapter 改写。
    pub run_temp: PathBuf,
    /// 项目路由键;真正的进程 cwd 取自 plan.cwd。
    pub workdir: PathBuf,
    /// 展示会话行号(agent_sessions):进程注册、卡片与终端交互用;
    /// 与 session_id(ad_hoc_sessions 行)分属两个表,互不挤占命名空间。
    pub display_session_id: i64,
    /// `agent_sessions.public_handle`，是展示终端的唯一运行时路由身份。
    pub display_session_handle: String,
    /// 退出事件通道(tag 为 session_id):进程结束时宿主上报
    /// `RuntimeEvent::AdHocExited`,由 Orchestrator 做完成分类。
    pub events: crossbeam_channel::Sender<TaggedRuntimeEvent>,
}

impl std::fmt::Debug for AdHocLaunchSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdHocLaunchSpec")
            .field("task_id", &self.task_id)
            .field("session_id", &self.session_id)
            .field("title", &self.title)
            .field("run_mode", &self.run_mode)
            .field("plan", &"<redacted-launch-plan>")
            .field("run_temp", &self.run_temp)
            .field("workdir", &self.workdir)
            .field("display_session_id", &self.display_session_id)
            .field("display_session_handle", &self.display_session_handle)
            .field("events", &"<channel>")
            .finish()
    }
}

/// 工作流 Step 派发规格(设计 §6.1):Orchestrator 从冻结 Revision
/// 取出 Agent Instance 快照,交由宿主侧真实 Agent Adapter 编译 LaunchPlan
/// 后启动。宿主不得改写 `run_temp`(可信物化根由调度器提供)。
#[derive(Clone)]
pub struct WorkflowLaunchSpec {
    /// 项目根只描述归属；worktree 租约路径只作进程 cwd。
    pub project_root: PathBuf,
    pub run_id: i64,
    pub run_handle: String,
    pub step_id: i64,
    pub task_id: i64,
    pub session_id: i64,
    pub session_handle: String,
    pub session_key: Option<String>,
    /// 复用已存在(存活)的会话则不再拉起进程。
    pub attach_existing_session: bool,
    /// 工作流节点键(快照内稳定标识)。
    pub node_key: String,
    pub step_title: String,
    /// 冻结的 Agent Instance 快照(Revision 时刻的配置)。
    pub instance: crate::agent_instance::AgentInstanceSnapshot,
    /// 贡献该节点 Agent Type 的插件包 pin(宿主校验后编译)。
    pub plugin: Option<crate::workflow::PluginSourcePin>,
    /// 发给 Agent 的提示(已做变量替换、含 goal/上游 Handoff 与结算纪律)。
    pub prompt: String,
    pub capability_token: String,
    pub pipe_name: String,
    pub mfctl_hint: Option<String>,
    /// Execution Lease 提供的工作目录(进程 cwd)。
    pub workdir: PathBuf,
    /// 可信 run-temp(临时文件物化根)。
    pub run_temp: PathBuf,
}

impl std::fmt::Debug for WorkflowLaunchSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowLaunchSpec")
            .field("project_root", &self.project_root)
            .field("run_id", &self.run_id)
            .field("run_handle", &self.run_handle)
            .field("step_id", &self.step_id)
            .field("task_id", &self.task_id)
            .field("session_id", &self.session_id)
            .field("session_handle", &self.session_handle)
            .field("session_key", &self.session_key)
            .field("attach_existing_session", &self.attach_existing_session)
            .field("node_key", &self.node_key)
            .field("step_title", &self.step_title)
            .field("instance_id", &self.instance.id)
            .field("agent_type", &self.instance.agent_type)
            .field("plugin", &self.plugin)
            .field("prompt", &"<redacted>")
            .field("capability_token", &"<redacted>")
            .field("pipe_name", &self.pipe_name)
            .field(
                "mfctl_hint",
                &self.mfctl_hint.as_ref().map(|_| "<configured>"),
            )
            .field("workdir", &self.workdir)
            .field("run_temp", &self.run_temp)
            .finish()
    }
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
    /// 离散 CLI 会话退出(tag 是 ad_hoc 行号):由 Orchestrator 按
    /// 完成契约与退出码分类,不会发明 Step / Agent Run。
    AdHocExited {
        session_id: i64,
        exit_code: Option<i32>,
        /// stdout-marker 契约:标记是否出现。
        marker_seen: bool,
        /// result-file 契约:结果文件是否出现。
        result_file_present: bool,
    },
    /// 结构化 Runtime 直接提交结算。
    Settled(crate::model::Settlement),
}

/// Runtime → Orchestrator 事件(带 run_id;离散会话事件带 session_id)。
pub type TaggedRuntimeEvent = (i64, RuntimeEvent);

/// Orchestrator 调用宿主(GPUI 进程内实现)执行 Agent Run。
pub trait RuntimeHost: Send + Sync {
    /// 启动(或复用会话并发送 prompt)。事件通过 `events` 回调(首元素为 run_id)。
    fn launch(&self, spec: LaunchSpec, events: crossbeam_channel::Sender<TaggedRuntimeEvent>);
    /// 启动工作流 Step:宿主用真实 Agent Adapter 把冻结实例编译为
    /// LaunchPlan 并启动。同步返回的错误表示编译/启动失败
    /// (调度方按失败结算,不留 Running)。
    fn launch_workflow(
        &self,
        spec: WorkflowLaunchSpec,
        events: crossbeam_channel::Sender<TaggedRuntimeEvent>,
    ) -> Result<()>;
    /// 启动离散 CLI 会话:无 run 事件流,状态由宿主直接管理;
    /// 不得发明 Step / Agent Run,也不得触碰 Task 状态。
    fn launch_ad_hoc(&self, spec: AdHocLaunchSpec) -> Result<()>;
    /// 向运行中的 Agent 追加提示。
    /// 运行时只接受 Store 持久化的 opaque handle；数据库行号不得参与寻址。
    fn send_prompt(&self, run_handle: &str, session_handle: &str, text: &str) -> Result<()>;
    /// 停止一次运行:真终止 run 绑定的会话进程,并**等待真实终止确认**
    /// (child 已被 wait/reap、生命周期已收口)后才返回 Ok。
    /// Err = 停止未在时限内确认(进程可能仍在运行):调用方不得标记
    /// Cancelled / 释放执行租约,应转入 Interrupted 等人工处理。
    /// 无绑定会话(已退出/未知 run)时 Ok(无进程可停)。
    fn stop_run(&self, run_handle: &str) -> Result<()>;
    /// 强制终止整个会话(进程)。
    fn kill_session(&self, session_handle: &str);
    /// 强制终止离散 CLI 会话(补偿路径:启动后 DB 写失败等场景,
    /// 必须杀掉进程,不留孤儿)。`session_id` 是展示会话行
    /// (agent_sessions)的编号 —— 进程注册在展示会话键下;
    /// ad_hoc_sessions 行号只作为事件 tag,不用于进程路由。
    fn kill_ad_hoc(&self, display_session_handle: &str);
    /// 回答 Agent 的提问(阻塞等待中的 HTTP Runtime)。
    fn answer_question(&self, run_handle: &str, answer: &str);
    /// Orchestrator 持久化 question 行之后立即回填:把 run 当前等待中的
    /// ask_human 待答槽绑定到具体 question,使 question-bound 投递可以
    /// 验证"等待的正是这一题"。这是尽力而为的关联通知;真正的
    /// fail-closed 边界在 [`RuntimeHost::answer_question_bound`]。
    /// 默认 no-op:不支持 question-bound 回答的宿主无需实现。
    fn bind_open_question(&self, run_handle: &str, question_id: i64) {
        let _ = (run_handle, question_id);
    }
    /// 是否能把回答绑定到具体的持久 question，并以该 question 为幂等键。
    /// 默认拒绝：只支持 legacy `(run, answer)` 的宿主无法排除旧 action 在
    /// 重启后命中同一 run 的下一题。
    fn supports_question_bound_answers(&self) -> bool {
        false
    }
    /// question-bound 回答。实现必须验证当前等待的正是 `question_id`，且同
    /// id 重放不产生第二次输入；无法证明时返回错误，不得回退到
    /// [`RuntimeHost::answer_question`]。
    fn answer_question_bound(
        &self,
        question_id: i64,
        run_handle: &str,
        answer: &str,
    ) -> Result<()> {
        let _ = (question_id, run_handle, answer);
        anyhow::bail!("question-bound answer unsupported")
    }
    /// 宿主是否能确认会话仍存活(重启恢复用)。
    /// 默认 false(无法确认 = 未知状态,绝不推断为失败)。
    fn is_session_alive(&self, session_handle: &str) -> bool {
        let _ = session_handle;
        false
    }
}

#[cfg(test)]
mod sensitive_debug_tests {
    use super::*;

    #[test]
    fn launch_and_profile_debug_never_expose_token_prompt_or_env_values() {
        let sentinel = "mft-never-print-this-secret";
        let profile = AgentProfileSpec {
            id: "profile".into(),
            display_name: "Profile".into(),
            runtime: RuntimeKind::Pty,
            command: "agent".into(),
            args: Vec::new(),
            env: vec![("API_KEY".into(), sentinel.into())],
            permission_args: Vec::new(),
            provider: None,
            icon: None,
            homepage: None,
            hook: None,
        };
        let profile_debug = format!("{profile:?}");
        assert!(!profile_debug.contains(sentinel), "{profile_debug}");
        assert!(profile_debug.contains("API_KEY"), "{profile_debug}");

        let spec = LaunchSpec {
            project_root: PathBuf::from("project"),
            run_id: 1,
            run_handle: "run_handle".into(),
            step_id: 2,
            task_id: 3,
            session_id: 4,
            session_handle: "session_handle".into(),
            session_key: None,
            attach_existing_session: false,
            profile,
            step_title: "step".into(),
            prompt: format!("prompt includes {sentinel}"),
            capability_token: sentinel.into(),
            pipe_name: "pipe".into(),
            mfctl_hint: Some(format!("hint {sentinel}")),
            workdir: PathBuf::from("work"),
        };
        let debug = format!("{spec:?}");
        assert!(!debug.contains(sentinel), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }
}
