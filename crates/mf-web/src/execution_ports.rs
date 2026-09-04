//! Web 工作台的执行面装配(#75):把被删 GPUI 栈中的生产 adapter
//! (crates/mf/src/workflow_start_port.rs,删除于 65427a6)恢复到 web
//! 入口——Project Workflow → durable Start plan 的编译 port 与
//! Orchestrator lifecycle port。bin 对每个挂载项目调用
//! [`assemble_project_execution`]。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use mf_agent::execution_directory::ExecutionDirectoryProvider;
use mf_agent::orchestrator::{Orchestrator, ProfileCatalog, WorkflowInstanceResolver};
use mf_agent::workflow::{PluginSourcePin, WorkflowTemplateVersion};
use mf_agent::workflow_compiler::{CompileInput, WorkflowCompiler};
use mf_agent::AgentInstanceSnapshot;
use mf_kernel::handles::{CommandId, ProjectStoreHandle, WorkflowHandle};
use mf_kernel::kernel::KernelProblem;
use mf_kernel::run_lifecycle::{RunActionDelivery, RunLifecyclePort, RunPreparation};
use mf_kernel::workflow_start::{PreparedWorkflowStartPlan, WorkflowStartPort};
use mf_terminal::session_runtime::{RuntimeHostImpl, SessionRegistry};

/// Project Workflow → durable Start plan 的生产编译 adapter(自旧
/// workflow_start_port.rs 恢复):实例版本/插件 pin/目录 provider pin
/// 全部冻结进 plan;Secret 只以 sealed id 留在快照。
pub struct OrchestratorWorkflowStartPort {
    orchestrator: Arc<Orchestrator>,
    agent_type_plugins: HashMap<String, PluginSourcePin>,
    instance_resolver: Arc<dyn WorkflowInstanceResolver>,
    directory_provider_isolates: bool,
    directory_provider_pin: Option<PluginSourcePin>,
}

impl OrchestratorWorkflowStartPort {
    pub fn new(
        orchestrator: Arc<Orchestrator>,
        agent_type_plugins: HashMap<String, PluginSourcePin>,
        instance_resolver: Arc<dyn WorkflowInstanceResolver>,
        directory_provider: &Arc<dyn ExecutionDirectoryProvider>,
        directory_provider_pin: Option<PluginSourcePin>,
    ) -> Self {
        Self {
            orchestrator,
            agent_type_plugins,
            instance_resolver,
            directory_provider_isolates: directory_provider.isolates(),
            directory_provider_pin,
        }
    }

    fn prepare_plan(
        &self,
        workflow: &WorkflowHandle,
        goal: &str,
    ) -> Result<PreparedWorkflowStartPlan, KernelProblem> {
        let record = self
            .orchestrator
            .store
            .with_conn(|conn| {
                mf_agent::Store::project_workflow_by_handle_tx(conn, workflow.as_str())
            })
            .map_err(port_error)?
            .ok_or(KernelProblem::ResourceNotFound)?;
        if record.public_handle != workflow.as_str() {
            return Err(KernelProblem::ResourceNotFound);
        }

        // content_digest 是 Project Workflow semantic CAS 的持久身份。编译前
        // 复验,防止损坏/旧迁移行被包装成"冻结计划"。
        let computed_digest = mf_agent::workflow::workflow_content_digest(
            &record.nodes,
            record.allow_unsafe_parallel,
        );
        if computed_digest != record.content_digest {
            return Err(KernelProblem::ValidationFailed(
                "Project Workflow content digest 不匹配".into(),
            ));
        }
        let template = WorkflowTemplateVersion {
            version_id: 0,
            template_key: format!("project-workflow/{}", record.key),
            version: record.semantic_revision,
            nodes: record.nodes,
            created_at: record.created_at,
        };
        let pipeline = WorkflowCompiler::new()
            .compile(CompileInput {
                template: &template,
                directory_provider_isolates: self.directory_provider_isolates,
                allow_unsafe_shared_directory: record.allow_unsafe_parallel,
                agent_type_plugins: &self.agent_type_plugins,
                resolve_instance: &|reference| self.instance_resolver.resolve(reference),
                directory_provider: self.directory_provider_pin.clone(),
            })
            .map_err(|errors| {
                KernelProblem::ValidationFailed(
                    errors
                        .into_iter()
                        .map(|error| format!("[{}] {}: {}", error.code, error.node, error.message))
                        .collect::<Vec<_>>()
                        .join("; "),
                )
            })?;
        PreparedWorkflowStartPlan::new(
            workflow.clone(),
            goal,
            pipeline,
            computed_digest,
            record.allow_unsafe_parallel,
        )
    }
}

impl WorkflowStartPort for OrchestratorWorkflowStartPort {
    fn prepare(
        &self,
        _command_id: &CommandId,
        workflow: &WorkflowHandle,
        goal: &str,
    ) -> Result<PreparedWorkflowStartPlan, KernelProblem> {
        self.prepare_plan(workflow, goal)
    }
}

/// Orchestrator lifecycle port(与 pipe 契约的 PipeOrchestratorPort 同
/// 语义:委托 Orchestrator 的 durable action 执行)。
pub struct OrchestratorRunLifecyclePort {
    pub orchestrator: Arc<Orchestrator>,
}

impl RunLifecyclePort for OrchestratorRunLifecyclePort {
    fn supports_question_bound_answers(&self) -> bool {
        self.orchestrator.supports_question_bound_answers()
    }

    fn prepare(
        &self,
        _command_id: &CommandId,
        command: &mf_kernel::kernel::WorkflowRunCommand,
    ) -> Result<RunPreparation, KernelProblem> {
        match command {
            mf_kernel::kernel::WorkflowRunCommand::Cancel { expected, .. } => {
                let handles = expected
                    .agent_runs
                    .iter()
                    .map(|run| run.handle.as_str().to_owned())
                    .collect::<Vec<_>>();
                let run_stops =
                    self.orchestrator
                        .prepare_cancel_runs(&handles)
                        .map_err(|error| KernelProblem::ServiceUnavailable(format!("{error:#}")))?
                        .into_iter()
                        .map(|(handle, outcome)| {
                            Ok(mf_kernel::run_lifecycle::PreparedRunStop {
                                agent_run: mf_kernel::handles::AgentRunHandle::parse(handle)
                                    .map_err(|error| {
                                        KernelProblem::Internal(format!(
                                            "Agent Run handle 损坏:{error}"
                                        ))
                                    })?,
                                outcome,
                            })
                        })
                        .collect::<Result<Vec<_>, KernelProblem>>()?;
                Ok(RunPreparation::Cancel { run_stops })
            }
            _ => Ok(RunPreparation::Ready),
        }
    }

    fn execute_post_commit(&self, delivery: &RunActionDelivery) -> Result<(), KernelProblem> {
        self.orchestrator
            .execute_durable_run_action(&delivery.action)
            .map_err(|error| {
                KernelProblem::ServiceUnavailable(format!("run_lifecycle_action_failed:{error:#}"))
            })
    }
}

/// 实例解析(目录库)尚未接入 web 装配:fail-closed——启动包含
/// Agent 节点的工作流将得到明确错误,而非旁路。
struct UnresolvedInstanceCatalog;

impl WorkflowInstanceResolver for UnresolvedInstanceCatalog {
    fn resolve(&self, reference: &str) -> anyhow::Result<AgentInstanceSnapshot> {
        anyhow::bail!(
            "Agent Instance `{reference}` 无法解析:web 执行面尚未接入实例目录(catalog 命令族未接管)"
        )
    }
}

/// 验收模式实例解析:任意引用合成最小 CLI 实例(平台 shell echo)——
/// 让启动/步骤/needs-you/结算链在无真实 agent 目录的验收环境可演示。
/// 生产装配绝不使用。
struct AcceptanceMockCatalog;

impl WorkflowInstanceResolver for AcceptanceMockCatalog {
    fn resolve(&self, reference: &str) -> anyhow::Result<AgentInstanceSnapshot> {
        let (executable, argv): (&str, Vec<String>) = if cfg!(windows) {
            (
                "cmd",
                vec!["/c".into(), "echo".into(), "[acceptance-agent] ok".into()],
            )
        } else {
            (
                "sh",
                vec!["-c".into(), "echo '[acceptance-agent] ok'".into()],
            )
        };
        Ok(AgentInstanceSnapshot {
            id: reference.to_string(),
            name: format!("acceptance:{reference}"),
            agent_type: "generic-command".into(),
            version: 1,
            enabled: true,
            run_mode: mf_agent::model::RunMode::OneShot,
            executable: executable.into(),
            argv,
            env: Vec::new(),
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({}),
            sealed_secret_ids: Vec::new(),
            external_config: false,
        })
    }
}

/// 为已挂载项目装配执行面:Orchestrator(RuntimeHost=session registry)
/// → run lifecycle port → workflow start port(注册顺序为内核契约)。
/// Store 幂等重开(与 kernel 投影连接并存,同 pipe 场景)。
pub fn assemble_project_execution(
    runtime: &Arc<mf_kernel::kernel::InProcessKernelRuntime>,
    registry: &Arc<SessionRegistry>,
    project: &ProjectStoreHandle,
    root: &Path,
) -> Result<(), String> {
    assemble_project_execution_with(runtime, registry, project, root, false)
}

/// `acceptance = true` 时实例解析使用验收 mock([`AcceptanceMockCatalog`])。
pub fn assemble_project_execution_with(
    runtime: &Arc<mf_kernel::kernel::InProcessKernelRuntime>,
    registry: &Arc<SessionRegistry>,
    project: &ProjectStoreHandle,
    root: &Path,
    acceptance: bool,
) -> Result<(), String> {
    let store = mf_agent::Store::open(&mf_agent::project_db_path(root))
        .map_err(|error| format!("打开项目库失败:{error:#}"))?;
    let host = RuntimeHostImpl::new(registry.clone());
    let directory: Arc<dyn ExecutionDirectoryProvider> =
        Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default());
    let orchestrator = Orchestrator::start(
        store,
        root.to_path_buf(),
        mf_agent::Config::default(),
        host,
        Arc::new(parking_lot::RwLock::new(ProfileCatalog::default())),
        mf_agent::GlobalLimiter::new(4),
        "\\\\.\\pipe\\mf-workbench-execution".into(),
        directory.clone(),
    )
    .map_err(|error| format!("调度器启动失败:{error:#}"))?;
    runtime
        .register_run_lifecycle_port(
            project,
            Arc::new(OrchestratorRunLifecyclePort {
                orchestrator: orchestrator.clone(),
            }),
        )
        .map_err(|error| format!("lifecycle port 注册失败:{error}"))?;
    let mut agent_type_plugins = HashMap::new();
    agent_type_plugins.insert(
        "generic-command".to_string(),
        PluginSourcePin {
            full_id: "builtin.core".into(),
            version: "1.2.3".into(),
            content_hash: "hash-generic".into(),
            contribution_id: String::new(),
        },
    );
    let start_port = OrchestratorWorkflowStartPort::new(
        orchestrator,
        agent_type_plugins,
        if acceptance {
            Arc::new(AcceptanceMockCatalog)
        } else {
            Arc::new(UnresolvedInstanceCatalog)
        },
        &directory,
        None,
    );
    runtime
        .register_workflow_start_port(project, Arc::new(start_port))
        .map_err(|error| format!("start port 注册失败:{error}"))?;
    Ok(())
}

fn port_error(error: anyhow::Error) -> KernelProblem {
    KernelProblem::ServiceUnavailable(format!("workflow_start_prepare_failed:{error:#}"))
}
