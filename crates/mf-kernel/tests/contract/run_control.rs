//! RunControl capability-token Settlement 契约(Issue #26,canonical §6.3)。
//!
//! 冻结的行为:
//! - `MF_RUN_TOKEN` 一次性令牌路由:多项目扫描必须恰好命中一个
//!   Project/Agent Run,零命中与多命中都拒绝,绝不「第一个命中」;
//! - 结算走 `WorkflowRunCommand::Settle`(L-CMD 事务 +
//!   `Store::apply_run_mutation_tx` + durable RunAction outbox),
//!   不存在绕过 Kernel 的第二套 Settlement;
//! - 相同结算幂等、冲突结算拒绝、空/无效/跨歧义令牌稳定失败;
//! - post-commit action 失败保留 receipt,重放补投(至少一次);
//! - exit ≠ Settlement:awaiting-outcome 不是结算,需显式 settle;
//! - 令牌明文不进入错误文案或 command receipt。

use crate::command::ServiceIdempotencyKey;
use crate::handles::{ClientId, CommandId, Principal, ProjectStoreHandle};
use crate::kernel::{InProcessCoreKernel, KernelProblem, LegacyKernelClient};
use crate::project_registry::{
    RunCapabilityKey, RunCapabilityResolution, RunCapabilityState, ServiceStore,
};
use crate::run_control::{
    RunControlCommand, RunControlOutcome, TokenSettleOutcome, TokenSettleProblem,
};
use crate::run_lifecycle::{RunActionDelivery, RunLifecyclePort, RunPreparation};
use mf_agent::model::{RunStatus, RunView, Settlement};
use mf_agent::{
    PipelineDraft, ProjectWorkflowDraft, SessionPolicy, StepDraft, Store, WorkflowNodeDraft,
};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 可注入失败、记录 durable action 投递的 run lifecycle port。
struct RecordingRunPort {
    fail_next_prepare: AtomicBool,
    fail_next_delivery: AtomicBool,
    delivered: Mutex<HashSet<(i64, u32)>>,
    deliveries: Mutex<Vec<RunActionDelivery>>,
}

impl RecordingRunPort {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            fail_next_prepare: AtomicBool::new(false),
            fail_next_delivery: AtomicBool::new(false),
            delivered: Mutex::new(HashSet::new()),
            deliveries: Mutex::new(Vec::new()),
        })
    }

    fn after_settlement_count(&self) -> usize {
        self.deliveries
            .lock()
            .iter()
            .filter(|delivery| {
                matches!(
                    delivery.action,
                    mf_agent::run_mutation::RunAction::AfterSettlement { .. }
                )
            })
            .count()
    }
}

impl RunLifecyclePort for RecordingRunPort {
    fn prepare(
        &self,
        _command_id: &CommandId,
        command: &crate::kernel::WorkflowRunCommand,
    ) -> Result<RunPreparation, KernelProblem> {
        if self.fail_next_prepare.swap(false, Ordering::SeqCst) {
            return Err(KernelProblem::ServiceUnavailable(
                "fake_prepare_failure".into(),
            ));
        }
        let _ = command;
        Ok(RunPreparation::Ready)
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
}

/// 单个已登记 Project:真实 Store + 持有 capability token 的 run。
struct ProjectFixture {
    _tmp: tempfile::TempDir,
    store: Arc<Store>,
    project: ProjectStoreHandle,
    port: Arc<RecordingRunPort>,
    run: RunView,
}

struct RunControlFixture {
    _service_tmp: tempfile::TempDir,
    service: Arc<ServiceStore>,
    kernel: Arc<InProcessCoreKernel>,
    client: LegacyKernelClient,
    projects: Vec<ProjectFixture>,
}

impl RunControlFixture {
    /// 单项目;`extra_projects` 追加额外项目(多项目路由契约)。
    fn new(extra_projects: usize) -> Self {
        let service_tmp = tempfile::tempdir().unwrap();
        let service = ServiceStore::open(&service_tmp.path().join("service-v1.db")).unwrap();
        let kernel = Arc::new(InProcessCoreKernel::new(
            service.clone(),
            ServiceIdempotencyKey::new(vec![0x2c; 32]).unwrap(),
        ));
        let client_id = ClientId::parse("run-control").unwrap();
        let principal = Principal::parse("mfctl-agent").unwrap();
        let epoch = kernel
            .grant_controller_checked(&client_id, &principal)
            .unwrap();
        let client = LegacyKernelClient::new(kernel.clone(), principal, client_id, epoch);
        let mut projects = Vec::new();
        for index in 0..=extra_projects {
            projects.push(Self::register_project(&kernel, index));
        }
        Self {
            _service_tmp: service_tmp,
            service,
            kernel,
            client,
            projects,
        }
    }

    fn register_project(kernel: &Arc<InProcessCoreKernel>, index: usize) -> ProjectFixture {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("project-v7.db")).unwrap();
        store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: format!("wf-run-control-{index}"),
                name: format!("Run control {index}"),
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
        let step = store.task_steps(task.id).unwrap().remove(0);
        let run = store
            .create_run(task.id, step.id, step.revision_id, None)
            .unwrap();
        let project = kernel
            .register_project_store(tmp.path(), store.clone())
            .unwrap();
        let port = RecordingRunPort::new();
        kernel
            .register_run_lifecycle_port(&project, port.clone())
            .unwrap();
        ProjectFixture {
            _tmp: tmp,
            store,
            project,
            port,
            run,
        }
    }

    fn settle(
        &self,
        token: &str,
        settlement: Settlement,
    ) -> Result<TokenSettleOutcome, TokenSettleProblem> {
        self.client
            .settle_agent_run_by_token(token, settlement, None)
    }

    fn execute(
        &self,
        token: &str,
        command: RunControlCommand,
        command_id: Option<CommandId>,
    ) -> Result<RunControlOutcome, TokenSettleProblem> {
        self.client
            .execute_agent_run_by_token(token, command, command_id)
    }

    /// service intent / project receipt 计数(验证失败路径未写命令)。
    fn command_counts(&self, store: &Store) -> (i64, i64) {
        let intents = self
            .service
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM command_intent", [], |row| row.get(0))
                    .map_err(Into::into)
            })
            .unwrap();
        let receipts = store
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM command_receipt", [], |row| row.get(0))
                    .map_err(Into::into)
            })
            .unwrap();
        (intents, receipts)
    }

    /// receipt 表中全部持久化文本(断言 token 不泄漏进 command receipt)。
    fn receipt_text(&self, store: &Store) -> String {
        store
            .with_conn(|conn| -> anyhow::Result<String> {
                let mut stmt = conn.prepare(
                    "SELECT command_id, semantic_digest, aggregate_handle, result_revisions, state
                     FROM command_receipt",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(format!(
                            "{}|{}|{}|{}|{}",
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows.join("\n"))
            })
            .unwrap()
    }
}

#[test]
fn report_done_is_one_transaction_needs_you_and_not_settlement() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    let outcome = f
        .execute(
            &token,
            RunControlCommand::ReportState(mf_agent::AgentState::Done),
            None,
        )
        .unwrap();
    assert!(matches!(
        outcome,
        RunControlOutcome::StateReported {
            state: mf_agent::AgentState::Done,
            ..
        }
    ));
    let run = project.store.run_view(project.run.id).unwrap().unwrap();
    assert_eq!(run.agent_state, mf_agent::AgentState::Done);
    assert_eq!(run.status, RunStatus::AwaitingOutcome);
    assert!(run.outcome.is_none(), "agent.state done 绝不是 Settlement");
    assert_eq!(
        project
            .store
            .step_view(run.step_id)
            .unwrap()
            .unwrap()
            .status,
        mf_agent::StepStatus::AwaitingOutcome
    );
    assert_eq!(
        project
            .store
            .task_view(run.task_id)
            .unwrap()
            .unwrap()
            .status,
        mf_agent::TaskStatus::NeedsYou
    );
}

#[test]
fn report_state_command_id_replays_original_receipt_and_conflicts_on_payload_change() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    let command_id = CommandId::new();
    let first = f
        .execute(
            &token,
            RunControlCommand::ReportState(mf_agent::AgentState::Working),
            Some(command_id.clone()),
        )
        .unwrap();
    let replay = f
        .execute(
            &token,
            RunControlCommand::ReportState(mf_agent::AgentState::Working),
            Some(command_id.clone()),
        )
        .unwrap();
    assert_eq!(first, replay);
    assert!(matches!(
        f.execute(
            &token,
            RunControlCommand::ReportState(mf_agent::AgentState::BlockedState),
            Some(command_id),
        ),
        Err(TokenSettleProblem::Kernel(KernelProblem::CommandIdReused))
    ));
}

#[test]
fn pipeline_propose_creates_normalized_draft_without_activation() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    let task = project
        .store
        .task_view(project.run.task_id)
        .unwrap()
        .unwrap();
    let active_before = task.active_revision;
    project
        .store
        .set_task_status(task.id, mf_agent::TaskStatus::NeedsYou)
        .unwrap();
    let outcome = f
        .execute(
            &token,
            RunControlCommand::ProposePipeline(PipelineDraft {
                steps: vec![StepDraft {
                    key: " next ".into(),
                    title: " next title ".into(),
                    instructions: "plan".into(),
                    agent_profile: " test ".into(),
                    session_policy: SessionPolicy::Fresh,
                    deps: vec![],
                }],
            }),
            None,
        )
        .unwrap();
    assert!(matches!(
        outcome,
        RunControlOutcome::PipelineProposed { .. }
    ));
    let task = project
        .store
        .task_view(project.run.task_id)
        .unwrap()
        .unwrap();
    assert_eq!(task.status, mf_agent::TaskStatus::Draft);
    assert_eq!(
        task.active_revision, active_before,
        "提案不得代替用户确认激活"
    );
    let (draft_id, _) = project
        .store
        .latest_draft_revision(task.id)
        .unwrap()
        .unwrap();
    let statuses = project.store.revision_statuses(task.id).unwrap();
    assert!(statuses
        .iter()
        .any(|(id, status)| *id == draft_id && status == "draft"));
    let steps = project.store.revision_steps(draft_id).unwrap();
    assert_eq!(steps[0].step_key, "next");
    assert_eq!(steps[0].title, "next title");
    assert_eq!(steps[0].agent_profile, "test");
}

#[test]
fn complete_settles_via_kernel_and_delivers_after_settlement() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();

    let outcome = f.settle(&token, Settlement::complete("完成-契约")).unwrap();
    assert_eq!(
        outcome,
        TokenSettleOutcome::Applied {
            agent_run: crate::handles::AgentRunHandle::parse(project.run.public_handle.clone())
                .unwrap()
        }
    );
    let settled = project.store.run_view(project.run.id).unwrap().unwrap();
    assert_eq!(settled.outcome.as_deref(), Some("complete"));
    assert_eq!(settled.outcome_payload.as_deref(), Some("完成-契约"));
    // Handoff 与 Store seam 同事务落库。
    let handoffs = project
        .store
        .list_handoff_rows(project.run.task_id)
        .unwrap();
    assert_eq!(handoffs.len(), 1);
    // 结算后 AfterSettlement durable action 至少投递一次。
    assert!(project.port.after_settlement_count() >= 1);
    // token 不进入 command receipt 任何持久化文本。
    assert!(!f.receipt_text(&project.store).contains(&token));
}

#[test]
fn fail_settlement_records_failure_outcome() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let outcome = f
        .settle(
            &project.run.capability_token,
            Settlement::Fail {
                reason: "失败原因".into(),
            },
        )
        .unwrap();
    assert_eq!(
        outcome,
        TokenSettleOutcome::Applied {
            agent_run: crate::handles::AgentRunHandle::parse(project.run.public_handle.clone())
                .unwrap()
        }
    );
    let settled = project.store.run_view(project.run.id).unwrap().unwrap();
    assert_eq!(settled.outcome.as_deref(), Some("fail"));
    assert_eq!(settled.outcome_payload.as_deref(), Some("失败原因"));
}

#[test]
fn same_settlement_replay_is_idempotent() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    f.settle(&token, Settlement::complete("首次")).unwrap();
    let replay = f.settle(&token, Settlement::complete("重复")).unwrap();
    assert_eq!(
        replay,
        TokenSettleOutcome::AlreadyApplied {
            agent_run: crate::handles::AgentRunHandle::parse(project.run.public_handle.clone())
                .unwrap()
        }
    );
    // 权威状态保持首次结算。
    let settled = project.store.run_view(project.run.id).unwrap().unwrap();
    assert_eq!(settled.outcome_payload.as_deref(), Some("首次"));
}

#[test]
fn conflicting_settlement_is_rejected() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    f.settle(&token, Settlement::complete("ok")).unwrap();
    let conflict = f.settle(
        &token,
        Settlement::Fail {
            reason: "反向".into(),
        },
    );
    assert_eq!(
        conflict,
        Err(TokenSettleProblem::Conflict {
            existing: "complete".into(),
            attempted: "fail".into(),
        })
    );
    let settled = project.store.run_view(project.run.id).unwrap().unwrap();
    assert_eq!(settled.outcome.as_deref(), Some("complete"));
}

#[test]
fn empty_and_unknown_tokens_fail_without_commands() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let before = f.command_counts(&project.store);

    assert_eq!(
        f.settle("", Settlement::complete("s")),
        Err(TokenSettleProblem::MissingToken)
    );
    assert_eq!(
        f.settle("mft-definitely-unknown", Settlement::complete("s")),
        Err(TokenSettleProblem::UnknownToken)
    );
    let problem = f
        .settle("mft-definitely-unknown", Settlement::complete("s"))
        .unwrap_err();
    assert!(!problem.to_string().contains("mft-definitely-unknown"));

    assert_eq!(f.command_counts(&project.store), before);
}

#[test]
fn token_matching_two_projects_is_rejected_not_first_match() {
    let f = RunControlFixture::new(1);
    let duplicated = f.projects[0].run.capability_token.clone();
    // 防御性契约:两个 Project 出现同一 token 时拒绝,绝不结算第一个。
    f.projects[1]
        .store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE agent_runs SET capability_token=?1 WHERE id=?2",
                rusqlite::params![duplicated, f.projects[1].run.id],
            )
            .map_err(Into::into)
        })
        .unwrap();

    let problem = f
        .settle(&duplicated, Settlement::complete("s"))
        .unwrap_err();
    assert_eq!(
        problem,
        TokenSettleProblem::AmbiguousToken { matches: 2 },
        "错误不得回显 token:{problem}"
    );
    for project in &f.projects {
        let run = project.store.run_view(project.run.id).unwrap().unwrap();
        assert!(run.outcome.is_none(), "歧义令牌不得结算任何项目");
    }
}

#[test]
fn indexed_target_is_rejected_when_a_later_legacy_run_reuses_the_token() {
    let f = RunControlFixture::new(1);
    let token = f.projects[0].run.capability_token.clone();
    let key = RunCapabilityKey::for_test(vec![0x52; 32]).unwrap();
    let indexed_run =
        crate::handles::AgentRunHandle::parse(f.projects[0].run.public_handle.clone()).unwrap();
    f.service
        .register_run_capability(&key, token.as_bytes(), &f.projects[0].project, &indexed_run)
        .unwrap();

    // 模拟 authority 建立后，尚未 eager-index 的 legacy 创建路径在另一个
    // Project 产生相同 plaintext token。已有 One 绝不能直接路由 A。
    f.projects[1]
        .store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE agent_runs SET capability_token=?1 WHERE id=?2",
                rusqlite::params![token, f.projects[1].run.id],
            )?;
            Ok(())
        })
        .unwrap();

    assert_eq!(
        f.settle(&token, Settlement::complete("must-not-settle")),
        Err(TokenSettleProblem::AmbiguousToken { matches: 2 })
    );
    for project in &f.projects {
        assert!(
            project
                .store
                .run_view(project.run.id)
                .unwrap()
                .unwrap()
                .outcome
                .is_none(),
            "indexed/legacy 歧义不得结算任何目标"
        );
    }
    assert!(matches!(
        f.service
            .resolve_run_capability(&key, token.as_bytes())
            .unwrap(),
        RunCapabilityResolution::Many
    ));
}

#[test]
fn token_only_settles_its_own_project_run() {
    let f = RunControlFixture::new(1);
    let (a, b) = (&f.projects[0], &f.projects[1]);
    f.settle(&a.run.capability_token, Settlement::complete("A 完成"))
        .unwrap();
    let settled = a.store.run_view(a.run.id).unwrap().unwrap();
    assert_eq!(settled.outcome.as_deref(), Some("complete"));
    let untouched = b.store.run_view(b.run.id).unwrap().unwrap();
    assert!(untouched.outcome.is_none(), "A 的令牌不得影响 B 的 run");
}

#[test]
fn closing_project_fails_closed() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    f.kernel.prepare_project_close(&project.project).unwrap();
    assert_eq!(
        f.settle(&token, Settlement::complete("s")),
        Err(TokenSettleProblem::ProjectClosing)
    );
    let run = project.store.run_view(project.run.id).unwrap().unwrap();
    assert!(run.outcome.is_none(), "关闭中的项目不得被结算");
}

#[test]
fn close_revokes_legacy_capability_and_reopen_never_reactivates_it() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    let close = f.kernel.prepare_project_close(&project.project).unwrap();
    f.kernel.finalize_project_close(close);
    assert_eq!(
        f.settle(&token, Settlement::complete("closed")),
        Err(TokenSettleProblem::UnknownToken)
    );

    let reopened = f
        .kernel
        .register_project_store(project._tmp.path(), project.store.clone())
        .unwrap();
    assert_eq!(reopened, project.project);
    f.kernel
        .register_run_lifecycle_port(&reopened, project.port.clone())
        .unwrap();
    assert_eq!(
        f.settle(&token, Settlement::complete("reopened")),
        Err(TokenSettleProblem::UnknownToken),
        "reopen 不得把 revoked capability 复活"
    );
    let key = RunCapabilityKey::for_test(vec![0x52; 32]).unwrap();
    assert!(matches!(
        f.service.resolve_run_capability(&key, token.as_bytes()).unwrap(),
        RunCapabilityResolution::One(capability)
            if capability.state == RunCapabilityState::Revoked
    ));
}

#[test]
fn kernel_facade_rejects_token_taint_without_persisting_or_echoing_it() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    let problem = f
        .client
        .settle_agent_run_by_token(
            &token,
            Settlement::complete_with_output(
                "summary",
                serde_json::json!({"nested":{"credential":token.clone()}}),
            ),
            Some(CommandId::new()),
        )
        .unwrap_err();
    assert_eq!(problem, TokenSettleProblem::SensitiveSettlement);
    assert!(!problem.to_string().contains(&token));
    assert_eq!(f.command_counts(&project.store), (0, 0));
    assert!(project
        .store
        .run_view(project.run.id)
        .unwrap()
        .unwrap()
        .outcome
        .is_none());
}

#[test]
fn post_commit_action_failure_keeps_receipt_and_replays() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    let command_id = CommandId::new();
    project
        .port
        .fail_next_delivery
        .store(true, Ordering::SeqCst);

    let first = f.client.settle_agent_run_by_token(
        &token,
        Settlement::complete("提交后失败"),
        Some(command_id.clone()),
    );
    assert!(
        matches!(first, Err(TokenSettleProblem::Kernel(_))),
        "post-commit 失败必须向客户端报错:{first:?}"
    );
    // Store 事务已提交:结算权威状态与 receipt 均已落库。
    let settled = project.store.run_view(project.run.id).unwrap().unwrap();
    assert_eq!(settled.outcome.as_deref(), Some("complete"));
    let (_, receipts) = f.command_counts(&project.store);
    assert_eq!(receipts, 1);

    // 同 command 重试先重新认证，再命中 receipt 并补投 durable action；
    // 返回首次持久化的原结果，而不是根据当前状态重新推导。
    let replay = f
        .client
        .settle_agent_run_by_token(&token, Settlement::complete("提交后失败"), Some(command_id))
        .unwrap();
    assert_eq!(
        replay,
        TokenSettleOutcome::Applied {
            agent_run: crate::handles::AgentRunHandle::parse(project.run.public_handle.clone())
                .unwrap()
        }
    );
    assert!(project.port.after_settlement_count() >= 1);
}

#[test]
fn process_exit_is_not_settlement_until_explicit() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    // Agent CLI 已退出:run 进入 awaiting-outcome,但 outcome 保持空,
    // 退出本身绝不等于成功结算。
    project
        .store
        .set_run_status(project.run.id, RunStatus::AwaitingOutcome)
        .unwrap();
    let exited = project.store.run_view(project.run.id).unwrap().unwrap();
    assert!(exited.outcome.is_none());

    // 显式 fail 结算仍可提交(exit ≠ 自动成功,也不自动失败)。
    let outcome = f
        .settle(
            &token,
            Settlement::Fail {
                reason: "人工确认失败".into(),
            },
        )
        .unwrap();
    assert_eq!(
        outcome,
        TokenSettleOutcome::Applied {
            agent_run: crate::handles::AgentRunHandle::parse(project.run.public_handle.clone())
                .unwrap()
        }
    );
    let settled = project.store.run_view(project.run.id).unwrap().unwrap();
    assert_eq!(settled.outcome.as_deref(), Some("fail"));
}

#[test]
fn cancelled_run_rejects_settlement_as_not_active() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    project
        .store
        .set_run_status(project.run.id, RunStatus::Cancelled)
        .unwrap();
    let problem = f
        .settle(&project.run.capability_token, Settlement::complete("s"))
        .unwrap_err();
    assert!(
        matches!(problem, TokenSettleProblem::RunNotActive(_)),
        "非活动 run 不是结算冲突:{problem}"
    );
}

#[test]
fn explicit_command_id_replays_original_result_and_rejects_other_payload() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    let command_id = CommandId::new();

    let first = f
        .client
        .settle_agent_run_by_token(
            &token,
            Settlement::complete("命令重试"),
            Some(command_id.clone()),
        )
        .unwrap();
    assert_eq!(
        first,
        TokenSettleOutcome::Applied {
            agent_run: crate::handles::AgentRunHandle::parse(project.run.public_handle.clone())
                .unwrap()
        }
    );
    let replay = f
        .client
        .settle_agent_run_by_token(
            &token,
            Settlement::complete("命令重试"),
            Some(command_id.clone()),
        )
        .unwrap();
    assert_eq!(replay, first, "receipt replay 必须返回首次原结果");
    assert_eq!(f.command_counts(&project.store), (1, 1));

    // 同 id 不同 settlement payload 在重新认证后稳定冲突。
    let reused = f
        .client
        .settle_agent_run_by_token(&token, Settlement::complete("不同摘要"), Some(command_id))
        .unwrap_err();
    assert!(
        matches!(
            reused,
            TokenSettleProblem::Kernel(KernelProblem::CommandIdReused)
        ),
        "同 id 异 digest 必须拒绝:{reused}"
    );
    assert!(!reused.to_string().contains(&token));
}

#[test]
fn controller_takeover_does_not_revoke_run_capability_authority() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    f.kernel
        .grant_controller_checked(
            &ClientId::parse("new-web-controller").unwrap(),
            &Principal::parse("different-user").unwrap(),
        )
        .unwrap();
    let outcome = f
        .client
        .settle_agent_run_by_token(
            &project.run.capability_token,
            Settlement::complete("controller-independent"),
            None,
        )
        .unwrap();
    assert_eq!(
        outcome,
        TokenSettleOutcome::Applied {
            agent_run: crate::handles::AgentRunHandle::parse(project.run.public_handle.clone())
                .unwrap()
        }
    );
}

#[test]
fn legacy_backfill_is_hmac_only_and_commit_transitions_to_settled() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    f.settle(&token, Settlement::complete("indexed")).unwrap();

    let key = RunCapabilityKey::for_test(vec![0x52; 32]).unwrap();
    let resolved = f
        .service
        .resolve_run_capability(&key, token.as_bytes())
        .unwrap();
    assert!(matches!(
        resolved,
        RunCapabilityResolution::One(ref capability)
            if capability.state == RunCapabilityState::Settled
                && capability.project == project.project
                && capability.agent_run.as_str() == project.run.public_handle
    ));
    let service_text = f
        .service
        .with_conn(|conn| -> anyhow::Result<String> {
            Ok(conn.query_row(
            "SELECT token_hmac || project_handle || agent_run_handle || state FROM run_capability",
            [],
            |row| row.get(0),
        )?)
        })
        .unwrap();
    assert!(
        !service_text.contains(&token),
        "service authority 不得落 token 明文"
    );
}

#[test]
fn quarantined_duplicate_is_a_stable_scan_sink() {
    let f = RunControlFixture::new(1);
    let token = f.projects[0].run.capability_token.clone();
    f.projects[1]
        .store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE agent_runs SET capability_token=?1 WHERE id=?2",
                rusqlite::params![token, f.projects[1].run.id],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        f.settle(&token, Settlement::complete("first")),
        Err(TokenSettleProblem::AmbiguousToken { .. })
    ));
    // 即使随后 legacy Store 只剩一个明文命中，quarantine tombstone 仍
    // 直接返回 Many；不能重新扫描后猜 winner。
    f.projects[1]
        .store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE agent_runs SET capability_token='replacement' WHERE id=?1",
                [f.projects[1].run.id],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        f.settle(&token, Settlement::complete("second")),
        Err(TokenSettleProblem::AmbiguousToken { .. })
    ));
}

#[test]
fn target_transaction_rejects_index_to_plaintext_token_swap() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    let token = project.run.capability_token.clone();
    let key = RunCapabilityKey::for_test(vec![0x52; 32]).unwrap();
    let run_handle =
        crate::handles::AgentRunHandle::parse(project.run.public_handle.clone()).unwrap();
    f.service
        .register_run_capability(&key, token.as_bytes(), &project.project, &run_handle)
        .unwrap();
    project
        .store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE agent_runs SET capability_token='swapped-token' WHERE id=?1",
                [project.run.id],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.settle(&token, Settlement::complete("must reject")),
        Err(TokenSettleProblem::UnknownToken)
    );
    assert_eq!(f.command_counts(&project.store), (0, 0));
}

#[test]
fn settle_leaves_finalized_receipt_on_agent_run_aggregate() {
    let f = RunControlFixture::new(0);
    let project = &f.projects[0];
    f.settle(&project.run.capability_token, Settlement::complete("s"))
        .unwrap();
    // 结算必须以权威 kernel 命令落库:receipt applied+finalized,
    // 目标 aggregate 是该 Agent Run,而不是任何直写路径。
    let receipt = project
        .store
        .with_conn(|conn| -> anyhow::Result<(String, String, Option<String>)> {
            Ok(conn.query_row(
                "SELECT state, aggregate_handle, finalized_at FROM command_receipt",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        })
        .unwrap();
    assert_eq!(receipt.0, "applied");
    assert_eq!(receipt.1, project.run.public_handle);
    assert!(receipt.2.is_some(), "receipt 必须 finalized");
}


// ---------- #88 B 自主版:EvolveWorkflow ----------

#[test]
fn evolve_workflow_method_is_stable() {
    let command = crate::run_control::RunControlCommand::EvolveWorkflow {
        key: "build".into(),
        title: "构建".into(),
        instructions: "构建项目".into(),
        agent_instance_id: "codex".into(),
        deps: vec!["check".into()],
    };
    assert_eq!(command.method(), "workflow.evolve");
}

#[test]
fn evolve_workflow_rejects_token_taint_in_node_fields() {
    let command = crate::run_control::RunControlCommand::EvolveWorkflow {
        key: "sk-123".into(),
        title: "标题".into(),
        instructions: "".into(),
        agent_instance_id: "codex".into(),
        deps: vec![],
    };
    assert!(crate::run_control::command_contains_token(&command, "sk-123"));
    let clean = crate::run_control::RunControlCommand::EvolveWorkflow {
        key: "build".into(),
        title: "标题".into(),
        instructions: "".into(),
        agent_instance_id: "codex".into(),
        deps: vec![],
    };
    assert!(!crate::run_control::command_contains_token(&clean, "sk-123"));
}
