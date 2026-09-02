//! In-process Core 的生产 RunLifecyclePort adapter。

use mf_agent::Orchestrator;
use mf_kernel::handles::{AgentRunHandle, AgentSessionHandle, CommandId};
use mf_kernel::kernel::{KernelProblem, WorkflowRunCommand};
use mf_kernel::run_lifecycle::{RunActionDelivery, RunLifecyclePort, RunPreparation};
use std::sync::Arc;

pub struct OrchestratorRunLifecyclePort {
    orchestrator: Arc<Orchestrator>,
}

impl OrchestratorRunLifecyclePort {
    pub fn new(orchestrator: Arc<Orchestrator>) -> Self {
        Self { orchestrator }
    }
}

impl RunLifecyclePort for OrchestratorRunLifecyclePort {
    fn supports_question_bound_answers(&self) -> bool {
        self.orchestrator.supports_question_bound_answers()
    }

    fn prepare(
        &self,
        _command_id: &CommandId,
        command: &WorkflowRunCommand,
    ) -> Result<RunPreparation, KernelProblem> {
        match command {
            WorkflowRunCommand::Cancel { .. } => Err(KernelProblem::ServiceUnavailable(
                "cancel_requires_durable_fence".into(),
            )),
            WorkflowRunCommand::RetryStep {
                step,
                mode: mf_agent::RetryMode::ContinueSession,
                ..
            } => {
                let session = self
                    .orchestrator
                    .live_session_for_step_handle(step.as_str())
                    .map_err(port_error)?
                    .ok_or_else(|| {
                        KernelProblem::ValidationFailed(
                            "目标 Step 没有存活的 Agent Session 可继续".into(),
                        )
                    })?;
                Ok(RunPreparation::ContinueSessionAlive {
                    session: AgentSessionHandle::parse(session.public_handle)
                        .map_err(|error| KernelProblem::Internal(error.to_string()))?,
                })
            }
            WorkflowRunCommand::Start { .. } => Err(KernelProblem::ServiceUnavailable(
                "Workflow Start 由独立 Operation port 承载".into(),
            )),
            _ => Ok(RunPreparation::Ready),
        }
    }

    fn stop_cancel_target(
        &self,
        _command_id: &CommandId,
        agent_run: &AgentRunHandle,
    ) -> Result<mf_agent::RunStopOutcome, KernelProblem> {
        self.orchestrator
            .stop_cancel_run(agent_run.as_str())
            .map_err(port_error)
    }

    fn execute_post_commit(&self, delivery: &RunActionDelivery) -> Result<(), KernelProblem> {
        let delivery_key = delivery.delivery_key();
        self.orchestrator
            .execute_durable_run_action_for_delivery(&delivery_key, &delivery.action)
            .map_err(port_error)
    }
}

fn port_error(error: anyhow::Error) -> KernelProblem {
    KernelProblem::ServiceUnavailable(format!("run_lifecycle_action_failed:{error:#}"))
}
