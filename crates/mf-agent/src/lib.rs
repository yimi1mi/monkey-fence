//! mf-agent:MonkeyFence 的 Agent 编排层(v2)。
//!
//! 核心模块:
//! - `schema`:全新 v1 存储命名空间(项目库 / 目录库 DDL 与版本)
//! - `store`:项目库(Task/Revision/Step/Session/Run/Event/Question)
//! - `catalog_store`:目录库(Agent Instance、模板、Secret、插件包地基)
//! - `pipeline`:Pipeline Draft、DAG 校验、会话策略
//! - `orchestrator`:调度器(自动派发/并发限制/串行化/显式结算/崩溃恢复)
//! - `runtime`:RuntimeHost 抽象(PTY / HTTP / PluginWorker)
//! - `provider`:OpenAI 兼容 / Anthropic / mock 提供方
//! - `tools`:worker 工具沙箱

pub mod agent_adapter;
pub mod agent_instance;
pub mod catalog_store;
pub mod config;
pub mod model;
pub mod orchestrator;
pub mod pipeline;
pub mod provider;
pub mod runtime;
pub mod schema;
pub mod secrets;
pub mod store;

pub use agent_adapter::{
    AgentAdapter, CompletionDetector, CompletionMode, CompletionObservation, ExecutionContract,
    HandoffDraft, InputInjection, InputMode, LaunchContext, LaunchPlan, ProcessObservation,
    TempFileSpec,
};
pub use agent_instance::{
    AgentInstance, AgentInstanceDraft, AgentInstanceOverrides, AgentInstanceSnapshot,
    AgentInstanceVersion,
};
pub use catalog_store::{CatalogStore, PluginPinRecord};
pub use config::{
    Config, EditorConfig, EngineConfig, ProviderConfig, ProviderKind, TerminalConfig,
};
pub use model::{
    AgentState, InstanceScope, RevisionStatus, RunMode, RunStatus, SchedulerEvent, SessionStatus,
    SettleError, SettleOutcome, Settlement, StepQuestionView, StepStatus, StepView, TaskStatus,
    TaskView,
};
pub use orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
pub use pipeline::{PipelineDraft, ProfileIndex, SessionPolicy, StepDraft};
pub use runtime::{
    AgentProfileSpec, AgentTypeDescriptor, HookSpec, LaunchSpec, RuntimeEvent, RuntimeHost,
    RuntimeKind,
};
pub use schema::{
    catalog_db_path, project_db_path, CATALOG_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION,
};
pub use store::Store;
