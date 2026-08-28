//! mf-agent:MonkeyFence 的 Agent 编排层(v2)。
//!
//! 核心模块:
//! - `store`:数据库迁移 + v2 schema(Task/Revision/Step/Session/Run/Event/Question)
//! - `pipeline`:Pipeline Draft、DAG 校验、会话策略
//! - `orchestrator`:调度器(自动派发/并发限制/串行化/显式结算/崩溃恢复)
//! - `runtime`:RuntimeHost 抽象(PTY / HTTP / PluginWorker)
//! - `provider`:OpenAI 兼容 / Anthropic / mock 提供方
//! - `tools`:worker 工具沙箱

pub mod config;
pub mod model;
pub mod orchestrator;
pub mod pipeline;
pub mod provider;
pub mod runtime;
pub mod store;

pub use config::{
    Config, EditorConfig, EngineConfig, ProviderConfig, ProviderKind, TerminalConfig,
};
pub use model::{
    AgentState, RevisionStatus, RunStatus, SchedulerEvent, SessionStatus, SettleError,
    SettleOutcome, Settlement, StepQuestionView, StepStatus, StepView, TaskStatus, TaskView,
};
pub use orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
pub use pipeline::{PipelineDraft, ProfileIndex, SessionPolicy, StepDraft};
pub use runtime::{AgentProfileSpec, HookSpec, LaunchSpec, RuntimeEvent, RuntimeHost, RuntimeKind};
pub use store::Store;
