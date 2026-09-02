//! Project Workflow → durable Workflow Start plan 的生产 adapter。
//!
//! 这里是 legacy Orchestrator/Plugin Host 与 UI-neutral CoreKernel 之间唯一
//! 编译点：prepare 只读当前 Project Workflow 与用户级 Agent Instance，
//! 把实例版本、Agent Type plugin source pin、目录 provider pin 全部冻结进
//! `PreparedWorkflowStartPlan`。Secret 只以 `sealed_secret_ids` 留在快照中，
//! 明文解封仍只发生在真正启动 Adapter 的临界区。

use mf_agent::execution_directory::ExecutionDirectoryProvider;
use mf_agent::orchestrator::{Orchestrator, WorkflowInstanceResolver};
use mf_agent::workflow::{PluginSourcePin, WorkflowTemplateVersion};
use mf_agent::workflow_compiler::{CompileInput, WorkflowCompiler};
use mf_kernel::handles::{CommandId, WorkflowHandle};
use mf_kernel::kernel::KernelProblem;
use mf_kernel::workflow_start::{PreparedWorkflowStartPlan, WorkflowStartPort};
use std::collections::HashMap;
use std::sync::Arc;

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
        // 复验，防止损坏/旧迁移行被包装成“冻结计划”。
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

fn port_error(error: anyhow::Error) -> KernelProblem {
    KernelProblem::ServiceUnavailable(format!("workflow_start_prepare_failed:{error:#}"))
}
