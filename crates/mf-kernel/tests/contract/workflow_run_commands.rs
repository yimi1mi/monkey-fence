use crate::command::{CommandType, ServiceIdempotencyKey};
use crate::handles::{
    AgentRunHandle, AgentSessionHandle, ClientId, CommandId, Principal, ProjectStoreHandle,
    StepHandle, WorkflowHandle, WorkflowRunHandle,
};
use crate::kernel::{
    CoreKernel, InProcessCoreKernel, KernelCommand, KernelCommandRequest, KernelOutcome,
    KernelProblem, LegacyKernelClient, VersionedHandle, WorkflowRunCommand, WorkflowRunExpected,
};
use crate::project_registry::ServiceStore;
use crate::projection::{SnapshotData, SnapshotQuery};
use crate::run_lifecycle::{PreparedRunStop, RunActionDelivery, RunLifecyclePort, RunPreparation};
use mf_agent::{
    PipelineDraft, ProjectWorkflowDraft, RetryMode, SessionPolicy, Settlement, StepDraft, Store,
    WorkflowNodeDraft,
};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub struct FakeRunPort {
    prepares: Mutex<Vec<String>>,
    pub deliveries: Mutex<Vec<RunActionDelivery>>,
    delivered: Mutex<HashSet<(i64, u32)>>,
    fail_next_delivery: AtomicBool,
    cancel_unconfirmed: Mutex<HashSet<String>>,
    cancel_stop_calls: Mutex<Vec<String>>,
}

impl FakeRunPort {
    pub fn fail_next(&self) {
        self.fail_next_delivery.store(true, Ordering::SeqCst);
    }
}

impl RunLifecyclePort for FakeRunPort {
    fn supports_question_bound_answers(&self) -> bool {
        true
    }

    fn prepare(
        &self,
        command_id: &CommandId,
        command: &WorkflowRunCommand,
    ) -> Result<RunPreparation, KernelProblem> {
        self.prepares.lock().push(command_id.as_str().to_owned());
        Ok(match command {
            WorkflowRunCommand::Cancel { expected, .. } => RunPreparation::Cancel {
                run_stops: expected
                    .agent_runs
                    .iter()
                    .map(|run| PreparedRunStop {
                        agent_run: run.handle.clone(),
                        outcome: if self.cancel_unconfirmed.lock().contains(run.handle.as_str()) {
                            mf_agent::RunStopOutcome::Unconfirmed
                        } else {
                            mf_agent::RunStopOutcome::Confirmed
                        },
                    })
                    .collect(),
            },
            WorkflowRunCommand::RetryStep {
                mode: RetryMode::ContinueSession,
                expected,
                ..
            } => RunPreparation::ContinueSessionAlive {
                session: expected.agent_sessions[0].handle.clone(),
            },
            _ => RunPreparation::Ready,
        })
    }

    fn execute_post_commit(&self, delivery: &RunActionDelivery) -> Result<(), KernelProblem> {
        if self.fail_next_delivery.swap(false, Ordering::SeqCst) {
            return Err(KernelProblem::ServiceUnavailable(
                "fake_action_failure".into(),
            ));
        }
        if self
            .delivered
            .lock()
            .insert((delivery.outbox_id, delivery.action_index))
        {
            self.deliveries.lock().push(delivery.clone());
        }
        Ok(())
    }

    fn stop_cancel_target(
        &self,
        _command_id: &CommandId,
        agent_run: &AgentRunHandle,
    ) -> Result<mf_agent::RunStopOutcome, KernelProblem> {
        self.cancel_stop_calls
            .lock()
            .push(agent_run.as_str().to_owned());
        Ok(
            if self.cancel_unconfirmed.lock().contains(agent_run.as_str()) {
                mf_agent::RunStopOutcome::Unconfirmed
            } else {
                mf_agent::RunStopOutcome::Confirmed
            },
        )
    }
}

/// 模拟仍只有 legacy `(run_handle, answer)` 注入能力的生产 port。
/// Kernel 必须在写 command intent / Project Store 之前 fail-closed。
struct LegacyAnswerPort;

impl RunLifecyclePort for LegacyAnswerPort {
    fn prepare(
        &self,
        _command_id: &CommandId,
        _command: &WorkflowRunCommand,
    ) -> Result<RunPreparation, KernelProblem> {
        Ok(RunPreparation::Ready)
    }

    fn execute_post_commit(&self, _delivery: &RunActionDelivery) -> Result<(), KernelProblem> {
        Ok(())
    }
}

pub struct RunFixture {
    pub _tmp: tempfile::TempDir,
    pub store: Arc<Store>,
    pub service: Arc<ServiceStore>,
    pub kernel: Arc<InProcessCoreKernel>,
    pub project: ProjectStoreHandle,
    pub workflow: WorkflowHandle,
    pub workflow_run: WorkflowRunHandle,
    pub step: StepHandle,
    pub client: ClientId,
    pub principal: Principal,
    pub epoch: u64,
}

impl RunFixture {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("project-v7.db")).unwrap();
        store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: "wf-run-contract".into(),
                name: "Run contract".into(),
                nodes: vec![WorkflowNodeDraft {
                    key: "work".into(),
                    title: "work".into(),
                    instructions: "do it".into(),
                    agent_instance_id: "instance".into(),
                    deps: vec![],
                }],
                allow_unsafe_parallel: false,
            })
            .unwrap();
        let workflow = WorkflowHandle::parse(
            store
                .load_project_workflow("wf-run-contract")
                .unwrap()
                .unwrap()
                .public_handle,
        )
        .unwrap();
        let task = store.create_task("run", "contract").unwrap();
        store
            .create_draft_revision(
                task.id,
                &PipelineDraft {
                    steps: vec![StepDraft {
                        key: "work".into(),
                        title: "work".into(),
                        instructions: "do it".into(),
                        agent_profile: "test".into(),
                        session_policy: SessionPolicy::Fresh,
                        deps: vec![],
                    }],
                },
            )
            .unwrap();
        store.activate_revision(task.id).unwrap();
        let workflow_run = WorkflowRunHandle::parse(task.public_handle).unwrap();
        let step =
            StepHandle::parse(store.task_steps(task.id).unwrap()[0].public_handle.clone()).unwrap();
        let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
        let kernel = Arc::new(InProcessCoreKernel::new(
            service.clone(),
            ServiceIdempotencyKey::new(vec![0x26; 32]).unwrap(),
        ));
        let project = kernel
            .register_project_store(tmp.path(), store.clone())
            .unwrap();
        let client = ClientId::parse("run-controller").unwrap();
        let principal = Principal::parse("run-user").unwrap();
        let epoch = kernel
            .grant_controller_checked(&client, &principal)
            .unwrap();
        Self {
            _tmp: tmp,
            store,
            service,
            kernel,
            project,
            workflow,
            workflow_run,
            step,
            client,
            principal,
            epoch,
        }
    }

    pub fn request(&self, command: WorkflowRunCommand) -> KernelCommandRequest {
        self.request_with_id(CommandId::new(), command)
    }

    pub fn request_with_id(
        &self,
        command_id: CommandId,
        command: WorkflowRunCommand,
    ) -> KernelCommandRequest {
        KernelCommandRequest::new(
            command_id,
            self.client.clone(),
            self.principal.clone(),
            self.epoch,
            KernelCommand::WorkflowRun(command),
        )
    }

    pub fn register_port(&self, port: Arc<FakeRunPort>) {
        self.kernel
            .register_run_lifecycle_port(&self.project, port)
            .unwrap();
    }

    pub fn dispatch(&self, command: WorkflowRunCommand) -> Result<(), KernelProblem> {
        self.kernel.dispatch(self.request(command)).map(|_| ())
    }

    pub fn current_expected(&self) -> WorkflowRunExpected {
        let run = self
            .store
            .task_view_by_handle(self.workflow_run.as_str())
            .unwrap()
            .unwrap();
        let step = self
            .store
            .step_view_by_handle(self.step.as_str())
            .unwrap()
            .unwrap();
        WorkflowRunExpected {
            workflow_run_revision: run.revision as u64,
            steps: vec![VersionedHandle {
                handle: self.step.clone(),
                revision: step.revision as u64,
            }],
            agent_runs: vec![],
            agent_sessions: vec![],
        }
    }

    pub fn write_counts(&self) -> (i64, i64, i64) {
        let intents = self
            .service
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM command_intent", [], |row| row.get(0))
                    .map_err(Into::into)
            })
            .unwrap();
        let (receipts, outbox) = self
            .store
            .with_conn(|conn| {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM command_receipt", [], |row| row.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM projection_outbox", [], |row| {
                        row.get(0)
                    })?,
                ))
            })
            .unwrap();
        (intents, receipts, outbox)
    }
}

#[test]
fn workflow_run_command_debug_redacts_user_and_sensitive_bodies() {
    let f = RunFixture::new();
    let sentinel = "mft-sensitive-body-must-not-leak";
    let agent_run = AgentRunHandle::parse(uuid::Uuid::now_v7().to_string()).unwrap();
    let commands = vec![
        WorkflowRunCommand::Start {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
            goal: sentinel.into(),
            expected_semantic_revision: 1,
        },
        WorkflowRunCommand::Respond {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            step: f.step.clone(),
            question_id: 1,
            answer: sentinel.into(),
            expected: f.current_expected(),
        },
        WorkflowRunCommand::Settle {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            step: f.step.clone(),
            agent_run,
            settlement: Settlement::complete(sentinel),
            expected: f.current_expected(),
        },
    ];
    for command in commands {
        let debug = format!("{:?}", f.request(command));
        assert!(!debug.contains(sentinel), "request Debug 泄露正文:{debug}");
        assert!(debug.contains("<redacted>"));
    }
}

fn assert_port_missing(result: Result<(), KernelProblem>) {
    assert_eq!(
        result,
        Err(KernelProblem::ServiceUnavailable(
            "run_lifecycle_port_not_registered".into()
        ))
    );
}

#[test]
fn project_store_run_handles_are_distinct_uuidv7_types() {
    let raw = uuid::Uuid::now_v7().to_string();
    assert_eq!(WorkflowRunHandle::parse(&raw).unwrap().as_str(), raw);
    assert_eq!(StepHandle::parse(&raw).unwrap().as_str(), raw);
    assert_eq!(AgentRunHandle::parse(&raw).unwrap().as_str(), raw);
    assert_eq!(AgentSessionHandle::parse(&raw).unwrap().as_str(), raw);
    assert!(WorkflowRunHandle::parse(format!("sess_{raw}")).is_err());
}

#[test]
fn start_authorizes_only_confirmed_semantic_revision() {
    let f = RunFixture::new();
    assert_eq!(
        f.dispatch(WorkflowRunCommand::Start {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
            goal: "contract".into(),
            expected_semantic_revision: 1,
        }),
        Err(KernelProblem::ServiceUnavailable(
            "workflow_start_port_not_registered".into()
        ))
    );
    assert_eq!(
        f.dispatch(WorkflowRunCommand::Start {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
            goal: "contract".into(),
            expected_semantic_revision: 2,
        }),
        Err(KernelProblem::RevisionConflict)
    );
}

#[test]
fn start_goal_is_required_and_participates_in_canonical_payload() {
    let f = RunFixture::new();
    let command = |goal: &str| WorkflowRunCommand::Start {
        project: f.project.clone(),
        workflow: f.workflow.clone(),
        goal: goal.into(),
        expected_semantic_revision: 1,
    };
    let before = f.write_counts();
    assert_eq!(
        f.dispatch(command("  \n  ")),
        Err(KernelProblem::InvalidEnvelope(
            "Workflow Run goal 不能为空".into()
        ))
    );
    assert_eq!(
        f.write_counts(),
        before,
        "空 goal 必须在任何 durable 写前拒绝"
    );

    let first = crate::kernel::workflow_run_payload(&command("修复登录"));
    let second = crate::kernel::workflow_run_payload(&command("修复支付"));
    assert_ne!(
        crate::command::canonical_json(&first).unwrap(),
        crate::command::canonical_json(&second).unwrap(),
        "goal 必须进入 dispatch 使用的 canonical semantic payload"
    );
}

#[test]
fn observer_equivalent_principal_cannot_authorize_run_command() {
    let f = RunFixture::new();
    let request = KernelCommandRequest::new(
        CommandId::new(),
        ClientId::parse("observer-client").unwrap(),
        f.principal.clone(),
        f.epoch,
        KernelCommand::WorkflowRun(WorkflowRunCommand::Cancel {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            expected: f.current_expected(),
        }),
    );
    assert_eq!(
        f.kernel.dispatch(request),
        Err(KernelProblem::ControllerLeaseExpired)
    );
}

#[test]
fn cancel_requires_every_active_concurrency_prerequisite() {
    let f = RunFixture::new();
    let run = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(
        f.dispatch(WorkflowRunCommand::Cancel {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            expected: WorkflowRunExpected::only_run(run.revision as u64),
        }),
        Err(KernelProblem::InvalidEnvelope(
            "expected Step 并发前提不完整".into()
        ))
    );
    assert_port_missing(f.dispatch(WorkflowRunCommand::Cancel {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        expected: f.current_expected(),
    }));
}

#[test]
fn skip_step_is_atomic_replayable_and_delivers_post_commit_actions() {
    let f = RunFixture::new();
    let port = Arc::new(FakeRunPort::default());
    f.register_port(port.clone());
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::Failed)
        .unwrap();
    f.store
        .set_task_status(step.task_id, mf_agent::TaskStatus::NeedsYou)
        .unwrap();
    let expected = f.current_expected();
    let command_id = CommandId::new();
    let command = || WorkflowRunCommand::SkipStep {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        expected: expected.clone(),
    };

    let first = f
        .kernel
        .dispatch(f.request_with_id(command_id.clone(), command()))
        .unwrap();
    assert!(matches!(
        first,
        KernelOutcome::RunApplied {
            replayed: false,
            ..
        }
    ));
    let task_after_first = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step_after_first = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    let snapshot = f
        .kernel
        .snapshot(SnapshotQuery::WorkflowRun {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
        })
        .unwrap();
    let SnapshotData::WorkflowRun(data) = snapshot.data else {
        panic!("expected WorkflowRun snapshot")
    };
    assert_eq!(data.revision.revision, task_after_first.revision as u64);
    assert_eq!(
        data.steps
            .iter()
            .find(|step| step.step == f.step)
            .unwrap()
            .revision
            .revision,
        step_after_first.revision as u64
    );
    let replay = f
        .kernel
        .dispatch(f.request_with_id(command_id, command()))
        .unwrap();
    assert!(matches!(
        replay,
        KernelOutcome::RunApplied { replayed: true, .. }
    ));
    assert_eq!(
        f.store
            .task_view_by_handle(f.workflow_run.as_str())
            .unwrap()
            .unwrap()
            .revision,
        task_after_first.revision,
        "receipt 重放不得再次 bump Workflow Run revision"
    );
    assert_eq!(
        f.store
            .step_view_by_handle(f.step.as_str())
            .unwrap()
            .unwrap()
            .revision,
        step_after_first.revision,
        "receipt 重放不得再次 bump Step revision"
    );
    assert_eq!(
        f.store
            .step_view_by_handle(f.step.as_str())
            .unwrap()
            .unwrap()
            .status,
        mf_agent::StepStatus::Skipped
    );
    assert_eq!(
        f.store
            .task_view_by_handle(f.workflow_run.as_str())
            .unwrap()
            .unwrap()
            .status,
        mf_agent::TaskStatus::Succeeded
    );
    let deliveries = port.deliveries.lock();
    assert!(deliveries
        .iter()
        .any(|delivery| matches!(delivery.action, mf_agent::RunAction::AfterSkip { .. })));
}

#[test]
fn skip_action_lost_ack_replays_without_revision_bump_or_stale_projection() {
    let f = RunFixture::new();
    let port = Arc::new(FakeRunPort::default());
    f.register_port(port.clone());
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::Failed)
        .unwrap();
    let expected = f.current_expected();
    let command_id = CommandId::new();
    let command = || WorkflowRunCommand::SkipStep {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        expected: expected.clone(),
    };
    port.fail_next();
    assert!(matches!(
        f.kernel
            .dispatch(f.request_with_id(command_id.clone(), command())),
        Err(KernelProblem::ServiceUnavailable(_))
    ));
    let committed_task_revision = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap()
        .revision;
    let committed_step_revision = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap()
        .revision;

    let replay = f
        .kernel
        .dispatch(f.request_with_id(command_id, command()))
        .unwrap();
    assert!(matches!(
        replay,
        KernelOutcome::RunApplied { replayed: true, .. }
    ));
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(task.revision, committed_task_revision);
    assert_eq!(step.revision, committed_step_revision);
    let snapshot = f
        .kernel
        .snapshot(SnapshotQuery::WorkflowRun {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
        })
        .unwrap();
    let SnapshotData::WorkflowRun(data) = snapshot.data else {
        panic!("expected WorkflowRun snapshot")
    };
    assert_eq!(data.revision.revision, task.revision as u64);
    assert_eq!(data.status, task.status.as_str());
    assert_eq!(data.unread, task.unread);
    assert_eq!(
        data.steps
            .iter()
            .find(|candidate| candidate.step == f.step)
            .unwrap()
            .revision
            .revision,
        step.revision as u64
    );
}

#[test]
fn skip_step_rejects_observer_stale_expected_and_active_run() {
    let f = RunFixture::new();
    f.register_port(Arc::new(FakeRunPort::default()));
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::Failed)
        .unwrap();
    let expected = f.current_expected();
    let command = || WorkflowRunCommand::SkipStep {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        expected: expected.clone(),
    };
    let before = f.write_counts();
    let observer = KernelCommandRequest::new(
        CommandId::new(),
        ClientId::parse("skip-observer").unwrap(),
        f.principal.clone(),
        f.epoch,
        KernelCommand::WorkflowRun(command()),
    );
    assert_eq!(
        f.kernel.dispatch(observer),
        Err(KernelProblem::ControllerLeaseExpired)
    );
    assert_eq!(f.write_counts(), before);

    f.store
        .set_step_auto_retry(step.id, 1)
        .expect("推进 Step revision");
    assert_eq!(f.dispatch(command()), Err(KernelProblem::RevisionConflict));

    let session = f
        .store
        .create_session(None, "pty", "test", "active")
        .unwrap();
    let active = f
        .store
        .create_run(step.task_id, step.id, step.revision_id, Some(session.id))
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let active_expected = WorkflowRunExpected {
        workflow_run_revision: task.revision as u64,
        steps: vec![VersionedHandle {
            handle: f.step.clone(),
            revision: step.revision as u64,
        }],
        agent_runs: vec![VersionedHandle {
            handle: AgentRunHandle::parse(active.public_handle).unwrap(),
            revision: active.revision as u64,
        }],
        agent_sessions: vec![VersionedHandle {
            handle: AgentSessionHandle::parse(session.public_handle).unwrap(),
            revision: session.revision as u64,
        }],
    };
    let active_result = f.dispatch(WorkflowRunCommand::SkipStep {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        expected: active_expected,
    });
    assert!(matches!(
        active_result,
        Err(KernelProblem::ValidationFailed(_))
    ));
}

#[test]
fn legacy_client_skip_step_builds_expected_from_opaque_snapshot() {
    let f = RunFixture::new();
    f.register_port(Arc::new(FakeRunPort::default()));
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::Failed)
        .unwrap();
    let client = LegacyKernelClient::new(
        f.kernel.clone(),
        f.principal.clone(),
        f.client.clone(),
        f.epoch,
    );
    assert!(matches!(
        client.skip_workflow_step(&f.project, &f.workflow_run, &f.step),
        Ok(KernelOutcome::RunApplied { .. })
    ));
    assert_eq!(
        f.store
            .step_view_by_handle(f.step.as_str())
            .unwrap()
            .unwrap()
            .status,
        mf_agent::StepStatus::Skipped
    );
}

#[test]
fn workflow_run_revision_is_rechecked_inside_authorization_transaction() {
    let f = RunFixture::new();
    let mut expected = f.current_expected();
    expected.workflow_run_revision += 1;
    assert_eq!(
        f.dispatch(WorkflowRunCommand::Cancel {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            expected,
        }),
        Err(KernelProblem::RevisionConflict)
    );
}

#[test]
fn stale_cancel_is_rejected_before_runtime_stop_preparation() {
    let f = RunFixture::new();
    let port = Arc::new(FakeRunPort::default());
    f.register_port(port.clone());
    let mut expected = f.current_expected();
    expected.workflow_run_revision += 1;

    assert_eq!(
        f.dispatch(WorkflowRunCommand::Cancel {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            expected,
        }),
        Err(KernelProblem::RevisionConflict)
    );
    assert!(
        port.prepares.lock().is_empty(),
        "stale Cancel 在 durable fence/最终 CAS 前不得触发 runtime stop"
    );
}

#[test]
fn respond_uses_step_handle_and_requires_exactly_one_open_question() {
    let f = RunFixture::new();
    let command = |question_id| WorkflowRunCommand::Respond {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        question_id,
        answer: "yes".into(),
        expected: f.current_expected(),
    };
    assert!(matches!(
        f.dispatch(command(0)),
        Err(KernelProblem::InvalidEnvelope(message))
            if message.contains("恰有一个 open question")
    ));
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::NeedsInput)
        .unwrap();
    let first_question = f
        .store
        .ask_question(task.id, Some(step.id), None, "continue?")
        .unwrap();
    assert_port_missing(f.dispatch(command(first_question.id)));
    f.store
        .ask_question(task.id, Some(step.id), None, "really?")
        .unwrap();
    assert!(matches!(
        f.dispatch(command(first_question.id)),
        Err(KernelProblem::InvalidEnvelope(message))
            if message.contains("恰有一个 open question")
    ));
}

#[test]
fn all_staged_commands_fail_closed_without_receipt_or_projection_writes() {
    let f = RunFixture::new();
    let before = f.write_counts();
    assert_port_missing(f.dispatch(WorkflowRunCommand::RetryStep {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        mode: RetryMode::FreshSession,
        expected: f.current_expected(),
    }));
    let skipped = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(skipped.id, mf_agent::StepStatus::Failed)
        .unwrap();
    let skip = WorkflowRunCommand::SkipStep {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        expected: f.current_expected(),
    };
    assert_eq!(skip.command_type(), CommandType::WorkflowSkipStep);
    assert_port_missing(f.dispatch(skip));
    assert_eq!(
        WorkflowRunCommand::Start {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
            goal: "contract".into(),
            expected_semantic_revision: 1,
        }
        .command_type(),
        CommandType::WorkflowRun
    );

    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    let session = f
        .store
        .create_session(None, "mock", "test", "session")
        .unwrap();
    let run = f
        .store
        .create_run(task.id, step.id, step.revision_id, Some(session.id))
        .unwrap();
    let agent_run = AgentRunHandle::parse(run.public_handle).unwrap();
    let agent_session = AgentSessionHandle::parse(session.public_handle).unwrap();
    let mut expected = f.current_expected();
    expected.agent_runs.push(VersionedHandle {
        handle: agent_run.clone(),
        revision: run.revision as u64,
    });
    expected.agent_sessions.push(VersionedHandle {
        handle: agent_session,
        revision: session.revision as u64,
    });
    assert_port_missing(f.dispatch(WorkflowRunCommand::Settle {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        agent_run,
        settlement: Settlement::complete("done"),
        expected,
    }));
    assert_eq!(f.write_counts(), before);
}

#[test]
fn registered_port_executes_cancel_after_per_run_stop_preparation() {
    let f = RunFixture::new();
    let port = Arc::new(FakeRunPort::default());
    f.register_port(port.clone());
    let before = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    assert!(matches!(
        f.kernel.dispatch(f.request(WorkflowRunCommand::Cancel {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            expected: f.current_expected(),
        })),
        Ok(KernelOutcome::RunApplied {
            replayed: false,
            ..
        })
    ));
    assert_eq!(
        f.store
            .task_view_by_handle(f.workflow_run.as_str())
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "cancelled"
    );
    assert_ne!(before.status.as_str(), "cancelled");
    assert!(port.deliveries.lock().iter().any(|delivery| matches!(
        delivery.action,
        mf_agent::RunAction::ReleaseTaskResources { .. }
    )));
    assert!(port.prepares.lock().is_empty());
    assert_eq!(port.cancel_stop_calls.lock().len(), 0);
    let pending_private: i64 = f
        .store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM projection_outbox
             WHERE published_at IS NULL OR event_json LIKE '%run_actions%'",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
        })
        .unwrap();
    assert_eq!(pending_private, 0);
}

#[test]
fn cancel_partial_stop_is_one_transaction_and_preserves_unconfirmed_resources() {
    let f = RunFixture::new();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::Running)
        .unwrap();
    let first_session = f
        .store
        .create_session(None, "mock", "test", "first")
        .unwrap();
    let first = f
        .store
        .create_run(task.id, step.id, step.revision_id, Some(first_session.id))
        .unwrap();
    let second_session = f
        .store
        .create_session(None, "mock", "test", "second")
        .unwrap();
    let second = f
        .store
        .create_run(task.id, step.id, step.revision_id, Some(second_session.id))
        .unwrap();
    let first_handle = AgentRunHandle::parse(&first.public_handle).unwrap();
    let second_handle = AgentRunHandle::parse(&second.public_handle).unwrap();
    let mut expected = f.current_expected();
    expected.agent_runs = vec![
        VersionedHandle {
            handle: first_handle.clone(),
            revision: first.revision as u64,
        },
        VersionedHandle {
            handle: second_handle.clone(),
            revision: second.revision as u64,
        },
    ];
    expected.agent_sessions = vec![
        VersionedHandle {
            handle: AgentSessionHandle::parse(first_session.public_handle).unwrap(),
            revision: first_session.revision as u64,
        },
        VersionedHandle {
            handle: AgentSessionHandle::parse(second_session.public_handle).unwrap(),
            revision: second_session.revision as u64,
        },
    ];
    let port = Arc::new(FakeRunPort::default());
    port.cancel_unconfirmed
        .lock()
        .insert(second_handle.as_str().to_owned());
    f.register_port(port.clone());

    assert!(matches!(
        f.kernel.dispatch(f.request(WorkflowRunCommand::Cancel {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            expected,
        })),
        Ok(KernelOutcome::RunApplied {
            replayed: false,
            ..
        })
    ));
    assert_eq!(
        f.store.run_view(first.id).unwrap().unwrap().status,
        mf_agent::RunStatus::Cancelled
    );
    assert_eq!(
        f.store.run_view(second.id).unwrap().unwrap().status,
        mf_agent::RunStatus::Interrupted
    );
    assert_eq!(
        f.store.task_view(task.id).unwrap().unwrap().status,
        mf_agent::TaskStatus::NeedsYou
    );
    assert_eq!(
        f.store.step_view(step.id).unwrap().unwrap().status,
        mf_agent::StepStatus::Running
    );
    let deliveries = port.deliveries.lock();
    assert!(deliveries.iter().any(|delivery| matches!(
        delivery.action,
        mf_agent::RunAction::ReleaseRunResources { run_id } if run_id == first.id
    )));
    assert!(deliveries.iter().any(|delivery| matches!(
        delivery.action,
        mf_agent::RunAction::ReleaseRunSlot { run_id } if run_id == second.id
    )));
    assert!(!deliveries.iter().any(|delivery| matches!(
        delivery.action,
        mf_agent::RunAction::ReleaseRunResources { run_id } if run_id == second.id
    )));
    assert!(!deliveries.iter().any(|delivery| matches!(
        delivery.action,
        mf_agent::RunAction::ReleaseTaskResources { .. }
    )));
}

#[test]
fn fenced_cancel_recovers_after_crash_and_rejects_other_command_without_stop() {
    let f = RunFixture::new();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::Running)
        .unwrap();
    let session = f.store.create_session(None, "mock", "test", "run").unwrap();
    let run = f
        .store
        .create_run(task.id, step.id, step.revision_id, Some(session.id))
        .unwrap();
    let run_handle = AgentRunHandle::parse(&run.public_handle).unwrap();
    let mut expected = f.current_expected();
    expected.agent_runs.push(VersionedHandle {
        handle: run_handle.clone(),
        revision: run.revision as u64,
    });
    expected.agent_sessions.push(VersionedHandle {
        handle: AgentSessionHandle::parse(session.public_handle).unwrap(),
        revision: session.revision as u64,
    });
    let port = Arc::new(FakeRunPort::default());
    f.register_port(port.clone());
    let command_id = CommandId::new();
    // 模拟 reserve fence 已提交后进程退出，尚未 stop。
    f.store
        .with_tx(|tx| {
            Store::reserve_cancel_fence_tx(
                tx,
                command_id.as_str(),
                task.id,
                &[(run.id, run.public_handle.clone(), run.revision)],
            )
            .map(|_| ())
        })
        .unwrap();
    let other = f.kernel.dispatch(f.request(WorkflowRunCommand::Cancel {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        expected: expected.clone(),
    }));
    assert!(matches!(other, Err(KernelProblem::ValidationFailed(_))));
    assert!(port.cancel_stop_calls.lock().is_empty());

    let old_request = f.request_with_id(
        command_id.clone(),
        WorkflowRunCommand::Cancel {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            expected: expected.clone(),
        },
    );
    let takeover_client = ClientId::parse("run-controller-takeover").unwrap();
    let takeover_epoch = f
        .kernel
        .grant_controller_checked(&takeover_client, &f.principal)
        .unwrap();
    assert_eq!(
        f.kernel.dispatch(old_request),
        Err(KernelProblem::ControllerLeaseExpired)
    );
    assert!(port.cancel_stop_calls.lock().is_empty());

    assert!(matches!(
        f.kernel.dispatch(KernelCommandRequest::new(
            command_id,
            takeover_client,
            f.principal.clone(),
            takeover_epoch,
            KernelCommand::WorkflowRun(WorkflowRunCommand::Cancel {
                project: f.project.clone(),
                workflow_run: f.workflow_run.clone(),
                expected,
            }),
        )),
        Ok(KernelOutcome::RunApplied {
            replayed: false,
            ..
        })
    ));
    assert_eq!(
        port.cancel_stop_calls.lock().as_slice(),
        &[run_handle.as_str()]
    );
    assert_eq!(
        f.store.run_view(run.id).unwrap().unwrap().status,
        mf_agent::RunStatus::Cancelled
    );
}

#[test]
fn port_registration_recovers_fence_without_original_request_or_controller() {
    let f = RunFixture::new();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::Running)
        .unwrap();
    let run = f
        .store
        .create_run(task.id, step.id, step.revision_id, None)
        .unwrap();
    let command_id = CommandId::new();
    f.store
        .with_tx(|tx| {
            Store::reserve_cancel_fence_tx(
                tx,
                command_id.as_str(),
                task.id,
                &[(run.id, run.public_handle.clone(), run.revision)],
            )
            .map(|_| ())
        })
        .unwrap();

    // 模拟 Core 进程重启：新 Kernel 没有原 request，也没有 Controller。
    let restarted = Arc::new(InProcessCoreKernel::new(
        f.service.clone(),
        ServiceIdempotencyKey::new(vec![0x26; 32]).unwrap(),
    ));
    let project = restarted
        .register_project_store(f._tmp.path(), f.store.clone())
        .unwrap();
    let port = Arc::new(FakeRunPort::default());
    restarted
        .register_run_lifecycle_port(&project, port.clone())
        .unwrap();

    assert_eq!(
        port.cancel_stop_calls.lock().as_slice(),
        &[run.public_handle]
    );
    assert_eq!(
        f.store.task_view(task.id).unwrap().unwrap().status,
        mf_agent::TaskStatus::Cancelled
    );
    let (state, finalized): (String, Option<String>) = f
        .store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT state,finalized_at FROM run_cancel_fence WHERE command_id=?1",
                [command_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(Into::into)
        })
        .unwrap();
    assert_eq!(state, "finalized");
    assert!(finalized.is_some());
}

#[test]
fn port_registration_finalizes_durable_outcome_without_restop() {
    let f = RunFixture::new();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::Running)
        .unwrap();
    let run = f
        .store
        .create_run(task.id, step.id, step.revision_id, None)
        .unwrap();
    let command_id = CommandId::new();
    f.store
        .with_tx(|tx| {
            Store::reserve_cancel_fence_tx(
                tx,
                command_id.as_str(),
                task.id,
                &[(run.id, run.public_handle.clone(), run.revision)],
            )?;
            assert!(Store::claim_cancel_target_tx(
                tx,
                command_id.as_str(),
                run.id
            )?);
            Store::record_cancel_outcome_tx(
                tx,
                command_id.as_str(),
                run.id,
                mf_agent::RunStopOutcome::Confirmed,
            )
        })
        .unwrap();

    let restarted = Arc::new(InProcessCoreKernel::new(
        f.service.clone(),
        ServiceIdempotencyKey::new(vec![0x26; 32]).unwrap(),
    ));
    let project = restarted
        .register_project_store(f._tmp.path(), f.store.clone())
        .unwrap();
    let port = Arc::new(FakeRunPort::default());
    restarted
        .register_run_lifecycle_port(&project, port.clone())
        .unwrap();
    assert!(
        port.cancel_stop_calls.lock().is_empty(),
        "durable outcome 不得重做 stop"
    );
    assert_eq!(
        f.store.run_view(run.id).unwrap().unwrap().status,
        mf_agent::RunStatus::Cancelled
    );
}

#[test]
fn action_failure_blocks_snapshot_and_same_command_replays_without_prepare() {
    let f = RunFixture::new();
    let step_id = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap()
        .id;
    f.store
        .set_step_status(step_id, mf_agent::StepStatus::Failed)
        .unwrap();
    let port = Arc::new(FakeRunPort::default());
    f.register_port(port.clone());
    port.fail_next();
    let command_id = CommandId::new();
    let request = f.request_with_id(
        command_id,
        WorkflowRunCommand::RetryStep {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            step: f.step.clone(),
            mode: RetryMode::FreshSession,
            expected: f.current_expected(),
        },
    );
    assert_eq!(
        f.kernel.dispatch(request.clone()),
        Err(KernelProblem::ServiceUnavailable(
            "fake_action_failure".into()
        ))
    );
    assert_eq!(port.prepares.lock().len(), 1);
    assert_eq!(
        f.kernel.snapshot(SnapshotQuery::Workflow {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
        }),
        Err(KernelProblem::ResyncRequired)
    );
    assert!(matches!(
        f.kernel.dispatch(request),
        Ok(KernelOutcome::RunApplied { replayed: true, .. })
    ));
    assert_eq!(
        port.prepares.lock().len(),
        1,
        "receipt replay 不得重做 prepare"
    );
    assert_eq!(
        port.deliveries.lock().len(),
        1,
        "retry replay 只重投 DispatchReady，不得重放会话选择"
    );
    assert_eq!(
        f.store.next_attempt_session(step_id).unwrap(),
        Some(mf_agent::NextAttemptSession::fresh()),
        "会话选择由 Store 保留一份，不由 post-commit replay 反复插入"
    );
    assert!(matches!(
        f.kernel.snapshot(SnapshotQuery::Workflow {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
        }),
        Ok(envelope) if matches!(envelope.data, SnapshotData::Workflow(_))
    ));
}

#[test]
fn legacy_adapter_continue_retry_includes_live_session_from_terminal_attempt() {
    let f = RunFixture::new();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    let session = f
        .store
        .create_session(None, "mock", "test", "continued")
        .unwrap();
    f.store
        .update_session(session.id, Some(mf_agent::SessionStatus::Idle), None, None)
        .unwrap();
    let run = f
        .store
        .create_run(task.id, step.id, step.revision_id, Some(session.id))
        .unwrap();
    f.store
        .set_run_status(run.id, mf_agent::RunStatus::Failed)
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::Failed)
        .unwrap();
    f.register_port(Arc::new(FakeRunPort::default()));
    let client = LegacyKernelClient::new(
        f.kernel.clone(),
        f.principal.clone(),
        f.client.clone(),
        f.epoch,
    );
    assert!(matches!(
        client.retry_workflow_step(
            &f.project,
            &f.workflow_run,
            &f.step,
            RetryMode::ContinueSession,
        ),
        Ok(KernelOutcome::RunApplied { .. })
    ));
    assert_eq!(
        f.store.next_attempt_session(step.id).unwrap(),
        Some(mf_agent::NextAttemptSession::continue_session(session.id))
    );
}

#[test]
fn run_events_expose_only_opaque_handles_and_never_rowids_or_tokens() {
    let f = RunFixture::new();
    let step_id = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap()
        .id;
    f.store
        .set_step_status(step_id, mf_agent::StepStatus::Failed)
        .unwrap();
    f.register_port(Arc::new(FakeRunPort::default()));
    f.dispatch(WorkflowRunCommand::RetryStep {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        mode: RetryMode::FreshSession,
        expected: f.current_expected(),
    })
    .unwrap();
    let events = f
        .store
        .with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT event_json FROM projection_outbox ORDER BY outbox_id")?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap();
    assert!(!events.is_empty());
    for event in events {
        let value: serde_json::Value = serde_json::from_str(&event).unwrap();
        let data = value
            .pointer("/projection/delta/data")
            .expect("run event must carry replace data");
        assert_no_internal_keys(data);
        let wire = serde_json::to_string(data).unwrap();
        assert!(!wire.contains("capability_token"));
    }
}

fn assert_no_internal_keys(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                assert!(
                    !matches!(
                        key.as_str(),
                        "id" | "task_id" | "step_id" | "revision_id" | "session_id"
                    ),
                    "run event 泄露内部 rowid key `{key}`"
                );
                assert_no_internal_keys(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                assert_no_internal_keys(value);
            }
        }
        _ => {}
    }
}

#[test]
fn respond_without_question_bound_idempotency_fails_before_writes() {
    let f = RunFixture::new();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::NeedsInput)
        .unwrap();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let question = f
        .store
        .ask_question(task.id, Some(step.id), None, "continue?")
        .unwrap();
    f.kernel
        .register_run_lifecycle_port(&f.project, Arc::new(LegacyAnswerPort))
        .unwrap();
    let before = f.write_counts();
    assert_eq!(
        f.kernel.dispatch(f.request(WorkflowRunCommand::Respond {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            step: f.step.clone(),
            question_id: question.id,
            answer: "stale answer".into(),
            expected: f.current_expected(),
        })),
        Err(KernelProblem::ServiceUnavailable(
            "question_bound_answer_port_not_registered".into()
        ))
    );
    assert_eq!(f.write_counts(), before);
    assert_eq!(
        f.store.open_questions(Some(task.id)).unwrap()[0].id,
        question.id,
        "fail-closed 不得提前消费 question"
    );
}

#[test]
fn runtime_bound_respond_commits_pending_delivery_and_durable_action() {
    let f = RunFixture::new();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::NeedsInput)
        .unwrap();
    let session = f
        .store
        .create_session(None, "mock", "test", "session")
        .unwrap();
    let run = f
        .store
        .create_run(task.id, step.id, step.revision_id, Some(session.id))
        .unwrap();
    let question = f
        .store
        .ask_question(task.id, Some(step.id), Some(run.id), "continue?")
        .unwrap();
    let mut expected = f.current_expected();
    expected.agent_runs.push(VersionedHandle {
        handle: AgentRunHandle::parse(&run.public_handle).unwrap(),
        revision: run.revision as u64,
    });
    expected.agent_sessions.push(VersionedHandle {
        handle: AgentSessionHandle::parse(&session.public_handle).unwrap(),
        revision: session.revision as u64,
    });
    let port = Arc::new(FakeRunPort::default());
    f.register_port(port.clone());

    assert!(matches!(
        f.kernel.dispatch(f.request(WorkflowRunCommand::Respond {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            step: f.step.clone(),
            question_id: question.id,
            answer: "yes".into(),
            expected,
        })),
        Ok(KernelOutcome::RunApplied {
            replayed: false,
            ..
        })
    ));
    assert_eq!(
        f.store.question(question.id).unwrap().unwrap().status,
        "open",
        "post-commit port 只确认接收 action；真实宿主确认前问题保持 open"
    );
    let delivery = f
        .store
        .answer_delivery_of_question(question.id)
        .unwrap()
        .unwrap();
    assert_eq!(delivery.status, "pending");
    assert!(port.deliveries.lock().iter().any(|delivery| matches!(
        delivery.action,
        mf_agent::RunAction::AnswerRuntime { question_id, .. }
            if question_id == question.id
    )));
}

#[test]
fn delayed_q1_respond_never_answers_later_q2_on_same_run() {
    let f = RunFixture::new();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::NeedsInput)
        .unwrap();
    let session = f
        .store
        .create_session(None, "mock", "test", "session")
        .unwrap();
    let run = f
        .store
        .create_run(task.id, step.id, step.revision_id, Some(session.id))
        .unwrap();
    let q1 = f
        .store
        .ask_question(task.id, Some(step.id), Some(run.id), "q1?")
        .unwrap();
    f.register_port(Arc::new(FakeRunPort::default()));

    // 模拟用户看到 q1 后命令延迟；期间 q1 已由其它路径收口，同一 run
    // 又打开 q2。Step/run expected 没变，只有 question identity 能阻止误答。
    let accepted = f
        .store
        .with_tx(|tx| {
            mf_agent::Store::apply_run_mutation_tx(
                tx,
                mf_agent::RunMutation::Respond {
                    question_id: q1.id,
                    answer: "other answer".into(),
                },
            )
        })
        .unwrap();
    let nonce = match accepted.actions.as_slice() {
        [mf_agent::RunAction::AnswerRuntime { nonce, .. }] => nonce.clone(),
        actions => panic!("q1 accept 应产生唯一投递 action: {actions:?}"),
    };
    f.store.confirm_answer_delivery(q1.id, &nonce).unwrap();
    let q2 = f
        .store
        .ask_question(task.id, Some(step.id), Some(run.id), "q2?")
        .unwrap();
    f.store
        .set_step_status(step.id, mf_agent::StepStatus::NeedsInput)
        .unwrap();
    let current_run = f.store.run_view(run.id).unwrap().unwrap();
    let current_session = f.store.session_view(session.id).unwrap().unwrap();
    let open_before_dispatch = f.store.open_questions(Some(task.id)).unwrap();
    assert_eq!(open_before_dispatch.len(), 1, "{open_before_dispatch:?}");
    assert_eq!(
        open_before_dispatch[0].id, q2.id,
        "{open_before_dispatch:?}"
    );
    let mut expected = f.current_expected();
    expected.agent_runs.push(VersionedHandle {
        handle: AgentRunHandle::parse(&run.public_handle).unwrap(),
        revision: current_run.revision as u64,
    });
    expected.agent_sessions.push(VersionedHandle {
        handle: AgentSessionHandle::parse(&session.public_handle).unwrap(),
        revision: current_session.revision as u64,
    });
    let result = f.kernel.dispatch(f.request(WorkflowRunCommand::Respond {
        project: f.project.clone(),
        workflow_run: f.workflow_run.clone(),
        step: f.step.clone(),
        question_id: q1.id,
        answer: "stale q1 answer".into(),
        expected,
    }));
    assert!(
        matches!(
            &result,
            Err(KernelProblem::InvalidEnvelope(message))
                if message.contains("question") && message.contains("已回答")
        ),
        "延迟 q1 必须稳定拒绝: {result:?}"
    );
    let open = f.store.open_questions(Some(task.id)).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, q2.id, "延迟 q1 command 不得消费 q2");
}

#[test]
fn retry_respond_and_settle_use_registered_port() {
    // retry
    let retry = RunFixture::new();
    retry
        .store
        .set_step_status(
            retry
                .store
                .step_view_by_handle(retry.step.as_str())
                .unwrap()
                .unwrap()
                .id,
            mf_agent::StepStatus::Failed,
        )
        .unwrap();
    let retry_port = Arc::new(FakeRunPort::default());
    retry.register_port(retry_port.clone());
    assert!(matches!(
        retry
            .kernel
            .dispatch(retry.request(WorkflowRunCommand::RetryStep {
                project: retry.project.clone(),
                workflow_run: retry.workflow_run.clone(),
                step: retry.step.clone(),
                mode: RetryMode::FreshSession,
                expected: retry.current_expected(),
            })),
        Ok(KernelOutcome::RunApplied {
            replayed: false,
            ..
        })
    ));
    assert_eq!(
        retry
            .store
            .next_attempt_session(
                retry
                    .store
                    .step_view_by_handle(retry.step.as_str())
                    .unwrap()
                    .unwrap()
                    .id
            )
            .unwrap(),
        Some(mf_agent::NextAttemptSession::fresh())
    );
    assert!(retry_port
        .deliveries
        .lock()
        .iter()
        .any(|delivery| matches!(delivery.action, mf_agent::RunAction::DispatchReady { .. })));

    // respond
    let respond = RunFixture::new();
    let step = respond
        .store
        .step_view_by_handle(respond.step.as_str())
        .unwrap()
        .unwrap();
    respond
        .store
        .set_step_status(step.id, mf_agent::StepStatus::NeedsInput)
        .unwrap();
    let task = respond
        .store
        .task_view_by_handle(respond.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let question = respond
        .store
        .ask_question(task.id, Some(step.id), None, "continue?")
        .unwrap();
    let respond_port = Arc::new(FakeRunPort::default());
    respond.register_port(respond_port);
    assert!(matches!(
        respond
            .kernel
            .dispatch(respond.request(WorkflowRunCommand::Respond {
                project: respond.project.clone(),
                workflow_run: respond.workflow_run.clone(),
                step: respond.step.clone(),
                question_id: question.id,
                answer: "yes".into(),
                expected: respond.current_expected(),
            })),
        Ok(KernelOutcome::RunApplied {
            replayed: false,
            ..
        })
    ));
    assert!(respond
        .store
        .open_questions(Some(task.id))
        .unwrap()
        .is_empty());

    // settle
    let settle = RunFixture::new();
    let task = settle
        .store
        .task_view_by_handle(settle.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = settle
        .store
        .step_view_by_handle(settle.step.as_str())
        .unwrap()
        .unwrap();
    settle
        .store
        .set_step_status(step.id, mf_agent::StepStatus::Running)
        .unwrap();
    let session = settle
        .store
        .create_session(None, "mock", "test", "session")
        .unwrap();
    let run = settle
        .store
        .create_run(task.id, step.id, step.revision_id, Some(session.id))
        .unwrap();
    let agent_run = AgentRunHandle::parse(&run.public_handle).unwrap();
    let mut expected = settle.current_expected();
    expected.agent_runs.push(VersionedHandle {
        handle: agent_run.clone(),
        revision: run.revision as u64,
    });
    expected.agent_sessions.push(VersionedHandle {
        handle: AgentSessionHandle::parse(session.public_handle).unwrap(),
        revision: session.revision as u64,
    });
    let settle_port = Arc::new(FakeRunPort::default());
    settle.register_port(settle_port.clone());
    assert!(matches!(
        settle
            .kernel
            .dispatch(settle.request(WorkflowRunCommand::Settle {
                project: settle.project.clone(),
                workflow_run: settle.workflow_run.clone(),
                step: settle.step.clone(),
                agent_run,
                settlement: Settlement::complete("done"),
                expected,
            })),
        Ok(KernelOutcome::RunApplied {
            replayed: false,
            ..
        })
    ));
    assert!(settle_port
        .deliveries
        .lock()
        .iter()
        .any(|delivery| matches!(delivery.action, mf_agent::RunAction::AfterSettlement { .. })));
}

#[test]
fn settlement_targets_same_key_step_in_active_revision() {
    let f = RunFixture::new();
    let task = f
        .store
        .task_view_by_handle(f.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let old_step = f
        .store
        .step_view_by_handle(f.step.as_str())
        .unwrap()
        .unwrap();
    f.store
        .set_step_status(old_step.id, mf_agent::StepStatus::Running)
        .unwrap();
    let session = f
        .store
        .create_session(None, "mock", "test", "session")
        .unwrap();
    let run = f
        .store
        .create_run(task.id, old_step.id, old_step.revision_id, Some(session.id))
        .unwrap();
    f.store
        .save_edited_revision(
            task.id,
            &PipelineDraft {
                steps: vec![StepDraft {
                    key: "work".into(),
                    title: "updated".into(),
                    instructions: "new revision".into(),
                    agent_profile: "test".into(),
                    session_policy: SessionPolicy::Fresh,
                    deps: vec![],
                }],
            },
        )
        .unwrap();
    let active_step = f.store.task_steps(task.id).unwrap()[0].clone();
    assert_ne!(active_step.id, old_step.id);
    let active_handle = StepHandle::parse(&active_step.public_handle).unwrap();
    let current_task = f.store.task_view(task.id).unwrap().unwrap();
    let agent_run = AgentRunHandle::parse(&run.public_handle).unwrap();
    let expected = WorkflowRunExpected {
        workflow_run_revision: current_task.revision as u64,
        steps: vec![VersionedHandle {
            handle: active_handle.clone(),
            revision: active_step.revision as u64,
        }],
        agent_runs: vec![VersionedHandle {
            handle: agent_run.clone(),
            revision: run.revision as u64,
        }],
        agent_sessions: vec![VersionedHandle {
            handle: AgentSessionHandle::parse(session.public_handle).unwrap(),
            revision: session.revision as u64,
        }],
    };
    f.register_port(Arc::new(FakeRunPort::default()));
    assert!(matches!(
        f.kernel.dispatch(f.request(WorkflowRunCommand::Settle {
            project: f.project.clone(),
            workflow_run: f.workflow_run.clone(),
            step: active_handle,
            agent_run,
            settlement: Settlement::complete("done"),
            expected,
        })),
        Ok(KernelOutcome::RunApplied { .. })
    ));
    assert_eq!(
        f.store.task_steps(task.id).unwrap()[0].status,
        mf_agent::StepStatus::Succeeded
    );
}

#[test]
fn lifecycle_port_registration_is_project_scoped() {
    let first = RunFixture::new();
    let first_port = Arc::new(FakeRunPort::default());
    first.register_port(first_port.clone());

    let second_tmp = tempfile::tempdir().unwrap();
    let second_store = Store::open(&second_tmp.path().join("project-v7.db")).unwrap();
    let task = second_store.create_task("second", "isolated").unwrap();
    second_store
        .create_draft_revision(
            task.id,
            &PipelineDraft {
                steps: vec![StepDraft {
                    key: "work".into(),
                    title: "work".into(),
                    instructions: "do it".into(),
                    agent_profile: "test".into(),
                    session_policy: SessionPolicy::Fresh,
                    deps: vec![],
                }],
            },
        )
        .unwrap();
    second_store.activate_revision(task.id).unwrap();
    let step = second_store.task_steps(task.id).unwrap()[0].clone();
    second_store
        .set_step_status(step.id, mf_agent::StepStatus::Failed)
        .unwrap();
    let step = second_store.step_view(step.id).unwrap().unwrap();
    let task = second_store.task_view(task.id).unwrap().unwrap();
    let project = first
        .kernel
        .register_project_store(second_tmp.path(), second_store)
        .unwrap();
    let command = WorkflowRunCommand::RetryStep {
        project: project.clone(),
        workflow_run: WorkflowRunHandle::parse(task.public_handle).unwrap(),
        step: StepHandle::parse(&step.public_handle).unwrap(),
        mode: RetryMode::FreshSession,
        expected: WorkflowRunExpected {
            workflow_run_revision: task.revision as u64,
            steps: vec![VersionedHandle {
                handle: StepHandle::parse(step.public_handle).unwrap(),
                revision: step.revision as u64,
            }],
            agent_runs: vec![],
            agent_sessions: vec![],
        },
    };
    assert_eq!(
        first.kernel.dispatch(KernelCommandRequest::new(
            CommandId::new(),
            first.client.clone(),
            first.principal.clone(),
            first.epoch,
            KernelCommand::WorkflowRun(command),
        )),
        Err(KernelProblem::ServiceUnavailable(
            "run_lifecycle_port_not_registered".into()
        ))
    );
    assert!(first_port.prepares.lock().is_empty());
}
