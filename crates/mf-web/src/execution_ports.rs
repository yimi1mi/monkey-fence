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

/// #92:project → Orchestrator 编排面注册(装配时写入;web 端点寻址)。
pub fn ad_hoc_orchestrators(
) -> &'static parking_lot::Mutex<std::collections::HashMap<String, Arc<Orchestrator>>> {
    static REGISTRY: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<String, Arc<Orchestrator>>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

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
    /// T4 图补丁编译复用 start port 的实例解析/插件 pin/目录 pin。
    pub start: Option<Arc<OrchestratorWorkflowStartPort>>,
}

impl OrchestratorRunLifecyclePort {
    /// T4 图补丁编译:暂停/基线/attempts 复验 + WorkflowCompiler 冻结。
    fn prepare_graph_patch(
        &self,
        workflow_run: &mf_kernel::handles::WorkflowRunHandle,
        nodes: &[mf_agent::WorkflowNodeDraft],
    ) -> Result<RunPreparation, KernelProblem> {
        let Some(start) = self.start.as_ref() else {
            return Err(KernelProblem::ServiceUnavailable(
                "graph_patch_port_not_registered".into(),
            ));
        };
        let store = &self.orchestrator.store;
        let task = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT id, paused FROM agent_tasks WHERE public_handle=?1",
                    [workflow_run.as_str()],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0)),
                )
                .map_err(|error| anyhow::anyhow!("{error}")))
            })
            .map_err(|e: anyhow::Error| KernelProblem::Internal(format!("{e:#}")))?
            .map_err(|e: anyhow::Error| {
                if e.to_string().contains("no rows") {
                    KernelProblem::ResourceNotFound
                } else {
                    KernelProblem::Internal(format!("{e:#}"))
                }
            })?;
        if !task.1 {
            return Err(KernelProblem::ValidationFailed(
                "必须先暂停派发再应用图补丁".into(),
            ));
        }
        let old = store
            .active_revision(task.0)
            .map_err(|e| KernelProblem::Internal(format!("{e:#}")))?
            .and_then(|rev| store.revision_snapshot(rev.id).ok().flatten())
            .ok_or_else(|| KernelProblem::ValidationFailed("运行没有可补丁的活动快照".into()))?;
        // 已启动(attempts>0)节点必须保持冻结定义
        let old_steps = store
            .revision_steps(
                store
                    .active_revision(task.0)
                    .map_err(|e| KernelProblem::Internal(format!("{e:#}")))?
                    .map(|r| r.id)
                    .unwrap_or(-1),
            )
            .unwrap_or_default();
        let mut started_keys = std::collections::HashSet::new();
        for step in &old_steps {
            if step.attempts > 0 {
                started_keys.insert(step.step_key.clone());
            }
        }
        // R2:全部已启动(attempts>0)节点必须保留在新图中(prepare 前置;
        // 事务内还会基于提交时的活动图再复验一次)
        for key in &started_keys {
            if !nodes.iter().any(|node| &node.key == key) {
                return Err(KernelProblem::ValidationFailed(format!(
                    "节点 `{key}` 已启动(attempts>0),不能从图中删除"
                )));
            }
        }
        for node in nodes {
            if let Some(frozen) = old.nodes.iter().find(|n| n.key == node.key) {
                if started_keys.contains(&node.key)
                    && (frozen.title != node.title
                        || frozen.instructions != node.instructions
                        || frozen.instance.id != node.agent_instance_id
                        || frozen.deps != node.deps
                        || frozen.acceptance_criteria != node.acceptance_criteria
                        || frozen.output_schema != node.output_schema
                        || frozen.input_bindings != node.input_bindings
                        || frozen.context_policy != node.context_policy
                        || frozen.require_input_review != node.require_input_review)
                {
                    return Err(KernelProblem::ValidationFailed(format!(
                        "节点 `{}` 已启动(attempts>0),必须保留冻结定义",
                        node.key
                    )));
                }
            }
        }
        // 与 start 同一套编译缝隙:实例解析/插件 pin/目录 pin
        let allow_unsafe = old
            .template_key
            .strip_prefix("project-workflow/")
            .and_then(|key| store.load_project_workflow(key).ok().flatten())
            .map(|record| record.allow_unsafe_parallel)
            .unwrap_or(false);
        let template = WorkflowTemplateVersion {
            version_id: 0,
            template_key: old.template_key.clone(),
            version: old.template_version + 1,
            nodes: nodes.to_vec(),
            created_at: String::new(),
        };
        let pipeline = mf_agent::workflow_compiler::WorkflowCompiler::new()
            .compile(mf_agent::workflow_compiler::CompileInput {
                template: &template,
                directory_provider_isolates: start.directory_provider_isolates,
                allow_unsafe_shared_directory: allow_unsafe,
                agent_type_plugins: &start.agent_type_plugins,
                resolve_instance: &|reference| start.instance_resolver.resolve(reference),
                directory_provider: start.directory_provider_pin.clone(),
            })
            .map_err(|errors| {
                KernelProblem::ValidationFailed(
                    errors
                        .iter()
                        .map(|e| e.message.clone())
                        .collect::<Vec<_>>()
                        .join(";"),
                )
            })?;
        // 已启动节点沿用原冻结定义(实例/插件/目录约束不漂移)
        let mut pipeline = pipeline;
        for node in pipeline.nodes.iter_mut() {
            if started_keys.contains(&node.key) {
                if let Some(frozen) = old.nodes.iter().find(|n| n.key == node.key) {
                    *node = frozen.clone();
                }
            }
        }
        let digest = {
            // 摘要按「已启动节点保持冻结、其余按补丁」计算
            let digest_nodes: Vec<mf_agent::WorkflowNodeDraft> = pipeline
                .nodes
                .iter()
                .map(|frozen| mf_agent::WorkflowNodeDraft {
                    key: frozen.key.clone(),
                    title: frozen.title.clone(),
                    instructions: frozen.instructions.clone(),
                    agent_instance_id: frozen.instance.id.clone(),
                    deps: frozen.deps.clone(),
                    acceptance_criteria: frozen.acceptance_criteria.clone(),
                    output_schema: frozen.output_schema.clone(),
                    input_bindings: frozen.input_bindings.clone(),
                    context_policy: frozen.context_policy,
                    require_input_review: frozen.require_input_review,
                })
                .collect();
            mf_agent::workflow::workflow_content_digest(&digest_nodes, allow_unsafe)
        };
        Ok(RunPreparation::GraphPatch {
            pipeline_json: serde_json::to_string(&pipeline)
                .map_err(|e| KernelProblem::Internal(format!("{e}")))?,
            digest,
        })
    }
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
            mf_kernel::kernel::WorkflowRunCommand::ApplyGraphPatch {
                workflow_run,
                nodes,
                ..
            } => self.prepare_graph_patch(workflow_run, nodes),
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

/// 生产实例目录(#87):真实 catalog 只读解析(READ_ONLY;写面经
/// launcher/CLI,web 不写)。引用 = Agent Instance 稳定 ID。
struct CatalogInstanceResolver {
    catalog: Arc<mf_agent::CatalogStore>,
}

impl WorkflowInstanceResolver for CatalogInstanceResolver {
    fn resolve(&self, reference: &str) -> anyhow::Result<AgentInstanceSnapshot> {
        self.catalog
            .snapshot_agent_instance(reference, None)
            .map_err(|error| anyhow::anyhow!("Agent Instance `{reference}` 解析失败:{error:#}"))
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
            // agent_type 必须是插件注册表真实贡献的类型(编译器要求
            // agent_type_plugins 命中,派发按 pin 解析 adapter)。
            // opencode 是内置 CLI 合成贡献,adapter 为 generic-command,
            // 直接消费本快照的 executable/argv。
            agent_type: "opencode".into(),
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
    assemble_project_execution_with(runtime, registry, project, root, false, None)
}

/// `acceptance = true` 时实例解析使用验收 mock([`AcceptanceMockCatalog`])。
pub fn assemble_project_execution_with(
    runtime: &Arc<mf_kernel::kernel::InProcessKernelRuntime>,
    registry: &Arc<SessionRegistry>,
    project: &ProjectStoreHandle,
    root: &Path,
    acceptance: bool,
    pipe_name: Option<&str>,
) -> Result<(), String> {
    let host = RuntimeHostImpl::new(registry.clone());
    assemble_with_host(
        runtime, registry, host, project, root, acceptance, pipe_name,
    )
}

/// #92 ad-hoc 编排面:host 由调用方注入(bin 构造带 launcher 的完整
/// 宿主);orchestrator 写入共享 registry 供 web 端点寻址。
pub fn assemble_with_host(
    runtime: &Arc<mf_kernel::kernel::InProcessKernelRuntime>,
    registry: &Arc<SessionRegistry>,
    host: Arc<RuntimeHostImpl>,
    project: &ProjectStoreHandle,
    root: &Path,
    acceptance: bool,
    pipe_name: Option<&str>,
) -> Result<(), String> {
    let _ = registry;
    let store = mf_agent::Store::open(&mf_agent::project_db_path(root))
        .map_err(|error| format!("打开项目库失败:{error:#}"))?;
    let directory: Arc<dyn ExecutionDirectoryProvider> =
        Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default());
    // 冻结 pin 表从真实插件注册表派生(须在 host 移交调度器前读取):
    // 内置合成插件空内容哈希,第三方包内容寻址(与 resolve_adapter_for_pin
    // 的两条解析路径一致);agent_type 短 id 与完整贡献 ID 均可引用。
    // 此前伪造的 builtin.core@hash-generic 不存在对应内容寻址包,派发必失败。
    let mut agent_type_plugins = HashMap::new();
    if let Some(launcher) = host.workflow_launcher() {
        for (full_contribution_id, source, contribution) in
            launcher.plugins.contributions().agent_types()
        {
            let pin = mf_agent::workflow::PluginSourcePin {
                full_id: source.plugin_full_id.clone(),
                version: source.plugin_version.clone(),
                content_hash: source.content_hash.clone(),
                contribution_id: full_contribution_id.clone(),
            };
            agent_type_plugins
                .entry(contribution.id.clone())
                .or_insert_with(|| pin.clone());
            agent_type_plugins
                .entry(full_contribution_id)
                .or_insert(pin);
        }
    }
    let orchestrator = Orchestrator::start(
        store,
        root.to_path_buf(),
        mf_agent::Config::default(),
        host,
        Arc::new(parking_lot::RwLock::new(ProfileCatalog::default())),
        mf_agent::GlobalLimiter::new(4),
        pipe_name.unwrap_or("mf-workbench-no-pipe").to_string(),
        directory.clone(),
    )
    .map_err(|error| format!("调度器启动失败:{error:#}"))?;
    ad_hoc_orchestrators()
        .lock()
        .insert(project.as_str().to_string(), orchestrator.clone());
    let instance_resolver: Arc<dyn WorkflowInstanceResolver> = if acceptance {
        Arc::new(AcceptanceMockCatalog)
    } else {
        match mf_agent::CatalogStore::open_read_only(&mf_agent::catalog_db_path()) {
            Ok(catalog) => Arc::new(CatalogInstanceResolver { catalog }),
            Err(error) => {
                // 目录不可用不阻断装配:启动含 Agent 节点的 run 时
                // 才 fail-closed(与 Unresolved 同语义,错误更明确)
                log::warn!("catalog 只读打开失败,实例解析 fail-closed:{error:#}");
                Arc::new(UnresolvedInstanceCatalog)
            }
        }
    };
    let start_port = Arc::new(OrchestratorWorkflowStartPort::new(
        orchestrator.clone(),
        agent_type_plugins,
        instance_resolver,
        &directory,
        None,
    ));
    // 注册顺序是内核契约(orchestrator → lifecycle → start):lifecycle
    // 先注册,但持有 start port 的 Arc 供 T4 图补丁编译复用。
    runtime
        .register_run_lifecycle_port(
            project,
            Arc::new(OrchestratorRunLifecyclePort {
                orchestrator: orchestrator.clone(),
                start: Some(start_port.clone()),
            }),
        )
        .map_err(|error| format!("lifecycle port 注册失败:{error}"))?;
    runtime
        .register_workflow_start_port(project, start_port.clone())
        .map_err(|error| format!("start port 注册失败:{error}"))?;

    Ok(())
}

fn port_error(error: anyhow::Error) -> KernelProblem {
    KernelProblem::ServiceUnavailable(format!("workflow_start_prepare_failed:{error:#}"))
}
