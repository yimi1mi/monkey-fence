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
pub mod catalog_migration;
pub mod catalog_store;
pub mod config;
pub mod execution_directory;
pub mod handoff;
pub mod migration;
pub mod model;
pub mod observability;
pub mod orchestrator;
pub mod pipeline;
pub mod provider;
pub mod run_mutation;
pub mod runtime;
pub mod schema;
pub mod secrets;
pub mod store;
pub mod workflow;
pub mod workflow_compiler;
pub mod workflow_mutation;
pub mod workflow_validation;

/// 适配器构造 Handoff 时使用的别名:规范类型是 `handoff::Handoff`。
pub use crate::handoff::Handoff as HandoffDraft;
pub use agent_adapter::{
    AgentAdapter, CompletionDetector, CompletionMode, CompletionObservation, ExecutionContract,
    InputInjection, InputMode, LaunchContext, LaunchPlan, ProcessObservation, TempFileSpec,
};
pub use agent_instance::{
    AgentInstance, AgentInstanceDraft, AgentInstanceOverrides, AgentInstanceSnapshot,
    AgentInstanceVersion,
};
pub use catalog_migration::{CatalogImportCounts, CatalogV1ImportReport};
pub use catalog_store::{CatalogStore, CatalogV2Store, PluginPinRecord};
pub use config::{
    Config, EditorConfig, EngineConfig, ProviderConfig, ProviderKind, TerminalConfig,
};
pub use handoff::Handoff;
pub use migration::{error_code, BackupManifest, MigrationError, StoreKind};
pub use model::{
    AdHocSessionView, AgentState, AnswerDeliveryConfirm, AnswerDeliveryRecord, InstanceScope,
    RevisionAxis, RevisionConflict, RevisionStatus, RunMode, RunStatus, SchedulerEvent,
    SessionStatus, SettleError, SettleOutcome, Settlement, StepQuestionView, StepStatus, StepView,
    TaskStatus, TaskView, WorkflowEdgeIdentityRow, WorkflowNodeIdentityRow,
    WorkflowPresentationState,
};
pub use model::{RetryMode, RetryPolicy};
pub use orchestrator::{
    GlobalLimiter, Orchestrator, ProfileCatalog, WorkflowInstanceResolver, WorkflowKernel,
    WorkflowPluginPins,
};
pub use pipeline::{PipelineDraft, ProfileIndex, SessionPolicy, StepDraft};
pub use run_mutation::{
    CancelFenceRecord, CancelFenceTarget, CancelFenceTargetState, NextAttemptSession, RunAction,
    RunMutation, RunMutationOutput, RunMutationResult, RunStopOutcome, RunStopResult,
};
pub use runtime::{
    AdHocLaunchSpec, AgentProfileSpec, AgentTypeDescriptor, HookSpec, LaunchSpec, RuntimeEvent,
    RuntimeHost, RuntimeKind,
};
pub use schema::{
    catalog_db_path, catalog_v2_db_path, project_db_path, CATALOG_SCHEMA_VERSION,
    CATALOG_V2_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION,
};
pub use store::Store;
pub use workflow::{ProjectWorkflowDraft, ProjectWorkflowRecord, WorkflowNodeDraft};
pub use workflow_compiler::{CompileError, CompileInput, WorkflowCompiler};
pub use workflow_mutation::{
    ProjectWorkflowMutation, WorkflowMutationError, WorkflowMutationResult,
};
pub use workflow_validation::{
    validate_workflow, WorkflowValidationCode, WorkflowValidationError, WorkflowValidationErrors,
    WorkflowValidationInput,
};
