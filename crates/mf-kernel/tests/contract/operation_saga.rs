//! T1g 契约(Issue #22):Operation saga——幂等 step receipt、
//! compensation/Needs You、accept 幂等与 plan 冻结。
//!
//! fixture 全部基于 tempfile 独立 service/project/catalog 库,不触碰
//! `~/.monkeyfence` 或真实用户数据。共享 saga 助手同时供
//! `multistore_crash_recovery`/`retention_gc` 复用。

use crate::command::{CommandProblem, CommandType, EffectOutput};
use crate::command_support::*;
use crate::handles::{
    AggregateKind, AggregateRef, CommandId, CommandTarget, ExpectedRevision, TargetStoreKind,
};
use crate::operation::{
    operation_of, steps_of, OperationAcceptFaultPoint, OperationCoordinator, OperationFaultPoint,
    OperationHandle, OperationKind, OperationOutcome, OperationPlan, OperationState, SagaStepPlan,
    StepEffect, StepId, StepRole, StepState, CODE_COMPENSATION_MISSING, CODE_OPERATION_COMPENSATED,
};
use crate::project_registry::ServiceStore;
use rusqlite::params;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const SAGA_KIND: &str = "test.saga.v1";

pub fn service_path(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("service-v1.db")
}

pub fn saga_service(tmp: &tempfile::TempDir) -> Arc<ServiceStore> {
    ServiceStore::open(&service_path(tmp)).unwrap()
}

/// saga initiating envelope(Project target;step 才是真正的多 Store 写)。
pub fn saga_envelope(command_id: CommandId, epoch: u64) -> crate::command::CommandEnvelope {
    envelope(
        command_id,
        TargetStoreKind::Project,
        epoch,
        plain_payload("saga"),
    )
}

pub fn project_step_target() -> CommandTarget {
    CommandTarget {
        store: TargetStoreKind::Project,
        store_handle: PROJECT_HANDLE.into(),
        aggregate: AggregateRef::new(AggregateKind::ProjectWorkflow, AGGREGATE).unwrap(),
    }
}

pub fn catalog_step_target() -> CommandTarget {
    CommandTarget {
        store: TargetStoreKind::Catalog,
        store_handle: "catalog".into(),
        aggregate: AggregateRef::new(AggregateKind::ProjectWorkflow, AGGREGATE).unwrap(),
    }
}

/// 冻结一个 step:expected 锚定 `revision` 轴,payload 进入 canonical digest。
pub fn saga_step(
    role: StepRole,
    target: CommandTarget,
    expected_revision: u64,
    payload_value: &str,
    compensates: Option<usize>,
) -> SagaStepPlan {
    let aggregate = target.aggregate.clone();
    SagaStepPlan::new(
        role,
        StepId::new(),
        CommandType::WorkflowMoveNode,
        target,
        vec![ExpectedRevision {
            aggregate,
            revisions: BTreeMap::from([("revision".to_string(), expected_revision)]),
        }],
        &json!({"value": payload_value}),
        compensates,
        &OperationKind::parse(SAGA_KIND).unwrap(),
    )
    .unwrap()
}

/// 计数 + 递增 revision 的确定性 step effect;revision 不匹配 → 终态冲突。
pub fn bump_effect(from_revision: u64, value: String, calls: Arc<AtomicUsize>) -> StepEffect {
    Box::new(move |tx| {
        calls.fetch_add(1, Ordering::SeqCst);
        let changed = tx
            .execute(
                "UPDATE command_test_effect SET value=?2, revision=revision+1
                 WHERE aggregate_handle=?1 AND revision=?3",
                params![AGGREGATE, value.as_str(), from_revision],
            )
            .map_err(|error| CommandProblem::Internal(error.to_string()))?;
        if changed != 1 {
            return Err(CommandProblem::RevisionConflict);
        }
        Ok(EffectOutput::for_contract(
            json!({"revision": from_revision + 1}),
            json!({"value": value, "revision": from_revision + 1}),
        ))
    })
}

/// 必定失败的 step effect(终态 RevisionConflict)。
pub fn failing_effect(calls: Arc<AtomicUsize>) -> StepEffect {
    Box::new(move |_tx| {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(CommandProblem::RevisionConflict)
    })
}

/// 带调用顺序日志的 effect(验证补偿逆序)。
pub fn ordered_effect(
    label: &'static str,
    from_revision: u64,
    order: Arc<Mutex<Vec<&'static str>>>,
    calls: Arc<AtomicUsize>,
) -> StepEffect {
    Box::new(move |tx| {
        order.lock().unwrap().push(label);
        bump_effect(from_revision, label.to_string(), calls)(tx)
    })
}

pub fn plan_of(steps: Vec<SagaStepPlan>) -> OperationPlan {
    OperationPlan {
        kind: OperationKind::parse(SAGA_KIND).unwrap(),
        steps,
    }
}

/// 双 Store 顺序 saga:project(rev1→2) → catalog(rev1→2)。
pub fn two_forward_steps() -> Vec<SagaStepPlan> {
    vec![
        saga_step(StepRole::Forward, project_step_target(), 1, "step-a", None),
        saga_step(StepRole::Forward, catalog_step_target(), 1, "step-b", None),
    ]
}

/// 跨 Store fixture:project + catalog target。
pub fn open_targets(tmp: &tempfile::TempDir) -> Vec<crate::command::TargetDatabase> {
    vec![
        project_target(&tmp.path().join("workflow-v1.db")),
        catalog_target(&tmp.path().join("catalog-v2.db")),
    ]
}

pub fn accept_operation(
    coordinator: &OperationCoordinator,
    envelope: &crate::command::CommandEnvelope,
    plan: &OperationPlan,
    targets: &[crate::command::TargetDatabase],
) -> Result<OperationHandle, crate::operation::OperationProblem> {
    coordinator.accept(
        envelope,
        plan,
        &targets[0],
        &TestAuthorizer::new(envelope.controller_epoch()),
    )
}

pub fn intent_of(
    service: &Arc<ServiceStore>,
    command_id: &CommandId,
) -> (crate::command::IntentState, Option<String>) {
    let state = crate::command::CommandCoordinator::new(service.clone(), key())
        .intent_state(command_id)
        .unwrap()
        .expect("intent 必须存在");
    let problem = service
        .with_conn(|conn| {
            conn.query_row(
                "SELECT problem_code FROM command_intent WHERE command_id=?1",
                [command_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();
    (state, problem)
}

fn step_states(service: &Arc<ServiceStore>, handle: &OperationHandle) -> Vec<StepState> {
    steps_of(service, handle)
        .unwrap()
        .into_iter()
        .map(|step| step.state)
        .collect()
}

#[test]
fn saga_runs_steps_across_stores_and_completes() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let project = project_target(&tmp.path().join("workflow-v1.db"));
    let catalog = catalog_target(&tmp.path().join("catalog-v2.db"));
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let auth = TestAuthorizer::new(7);
    let command_id = CommandId::new();
    let plan = plan_of(two_forward_steps());
    let targets = vec![project.clone(), catalog.clone()];
    let handle = accept_operation(
        &coordinator,
        &saga_envelope(command_id.clone(), 7),
        &plan,
        &targets,
    )
    .unwrap();
    assert!(handle.as_str().starts_with("op_"));
    assert!(
        OperationHandle::parse(handle.as_str()).is_ok(),
        "handle 必须是 op_ + UUIDv7"
    );
    assert_eq!(
        operation_of(&service, &handle).unwrap().state,
        OperationState::Accepted
    );
    let (value, revision, receipts, outbox, _) = target_snapshot(&project);
    assert_eq!((value.as_str(), revision), ("initial", 1));
    assert_eq!(
        (receipts, outbox),
        (1, 1),
        "返回 handle 前 acceptance target receipt/outbox 必须已提交"
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let outcome = coordinator
        .run(
            &handle,
            &plan,
            &targets,
            &auth,
            vec![
                bump_effect(1, "step-a".into(), calls.clone()),
                bump_effect(1, "step-b".into(), calls.clone()),
            ],
            None,
        )
        .unwrap();
    assert_eq!(outcome, OperationOutcome::Completed { compensated: false });
    assert_eq!(calls.load(Ordering::SeqCst), 2, "每个 step 恰好执行一次");

    let record = operation_of(&service, &handle).unwrap();
    assert_eq!(record.state, OperationState::Completed);
    assert_eq!(record.progress.forward_succeeded, 2);
    assert_eq!(record.progress.outcome, None);
    assert_eq!(
        step_states(&service, &handle),
        vec![StepState::Succeeded, StepState::Succeeded]
    );
    assert_eq!(
        intent_of(&service, &command_id).0,
        crate::command::IntentState::Applied
    );
    // initiating Project target 还有一条 acceptance receipt/outbox；随后
    // 两个 step 各写一条 target-local receipt/outbox。
    let (project_value, project_revision, project_receipts, project_outbox, _) =
        target_snapshot(&project);
    let (catalog_value, _, catalog_receipts, catalog_outbox, _) = target_snapshot(&catalog);
    assert_eq!((project_value.as_str(), project_revision), ("step-a", 2));
    assert_eq!(catalog_value, "step-b");
    assert_eq!((project_receipts, project_outbox), (2, 2));
    assert_eq!((catalog_receipts, catalog_outbox), (1, 1));
}

/// 已 succeeded 的 step 在 resume 时按 durable 状态跳过,不再执行 effect。
#[test]
fn fault_after_step_finalize_resumes_without_replaying_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let auth = TestAuthorizer::new(8);
    let plan = plan_of(two_forward_steps());
    let handle = accept_operation(
        &coordinator,
        &saga_envelope(CommandId::new(), 8),
        &plan,
        &targets,
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let error = coordinator
        .run(
            &handle,
            &plan,
            &targets,
            &auth,
            vec![
                bump_effect(1, "step-a".into(), calls.clone()),
                bump_effect(1, "step-b".into(), calls.clone()),
            ],
            Some(OperationFaultPoint::AfterStepFinalized(0)),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        crate::operation::OperationProblem::FaultInjected("after_step_finalized")
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        step_states(&service, &handle),
        vec![StepState::Succeeded, StepState::Pending]
    );

    // 模拟进程内恢复:重新 run,step0 必须由 durable 状态跳过。
    let outcome = coordinator
        .run(
            &handle,
            &plan,
            &targets,
            &auth,
            vec![
                failing_effect(calls.clone()), // step0 若重放会立即失败
                bump_effect(1, "step-b".into(), calls.clone()),
            ],
            None,
        )
        .unwrap();
    assert_eq!(outcome, OperationOutcome::Completed { compensated: false });
    assert_eq!(calls.load(Ordering::SeqCst), 2, "step0 effect 不得重放");
    assert_eq!(
        operation_of(&service, &handle).unwrap().state,
        OperationState::Completed
    );
}

/// forward 失败 → 声明的补偿按 forward 完成序逆序执行;完整回滚标
/// compensated,不伪造成功(intent = failed/operation_compensated)。
#[test]
fn failure_runs_declared_compensations_in_reverse_order() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let project = targets[0].clone();
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let auth = TestAuthorizer::new(9);
    let order = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let steps = vec![
        saga_step(StepRole::Forward, project_step_target(), 1, "step-a", None),
        saga_step(StepRole::Forward, catalog_step_target(), 1, "step-b", None),
        saga_step(
            StepRole::Compensate,
            catalog_step_target(),
            2,
            "undo-b",
            Some(1),
        ),
        saga_step(
            StepRole::Compensate,
            project_step_target(),
            2,
            "undo-a",
            Some(0),
        ),
        saga_step(StepRole::Forward, project_step_target(), 2, "step-c", None),
    ];
    let plan = plan_of(steps);
    let command_id = CommandId::new();
    let handle = accept_operation(
        &coordinator,
        &saga_envelope(command_id.clone(), 9),
        &plan,
        &targets,
    )
    .unwrap();
    let outcome = coordinator
        .run(
            &handle,
            &plan,
            &targets,
            &auth,
            vec![
                ordered_effect("step-a", 1, order.clone(), calls.clone()),
                ordered_effect("step-b", 1, order.clone(), calls.clone()),
                ordered_effect("undo-b", 2, order.clone(), calls.clone()),
                ordered_effect("undo-a", 2, order.clone(), calls.clone()),
                failing_effect(calls.clone()), // step-c 失败触发补偿
            ],
            None,
        )
        .unwrap();
    assert_eq!(outcome, OperationOutcome::Completed { compensated: true });
    assert_eq!(
        *order.lock().unwrap(),
        vec!["step-a", "step-b", "undo-b", "undo-a"],
        "补偿必须按 forward 完成序逆序执行"
    );
    let record = operation_of(&service, &handle).unwrap();
    assert_eq!(record.state, OperationState::Completed);
    assert_eq!(record.progress.outcome.as_deref(), Some("compensated"));
    assert_eq!(record.progress.problem, None);
    let (intent_state, problem) = intent_of(&service, &command_id);
    assert_eq!(intent_state, crate::command::IntentState::Failed);
    assert_eq!(problem.as_deref(), Some(CODE_OPERATION_COMPENSATED));
    let steps = steps_of(&service, &handle).unwrap();
    assert_eq!(steps[4].state, StepState::Failed);
    assert_eq!(steps[4].problem_code.as_deref(), Some("revision_conflict"));
    assert_eq!(steps[2].state, StepState::Succeeded);
    assert_eq!(steps[3].state, StepState::Succeeded);
    assert_eq!(target_snapshot(&project).0, "undo-a");
}

/// 已生效 step 无补偿声明 → Needs You + 稳定 problem,不得伪造成功。
#[test]
fn failure_without_compensation_declared_enters_needs_you() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let project = targets[0].clone();
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let auth = TestAuthorizer::new(10);
    let calls = Arc::new(AtomicUsize::new(0));
    let steps = vec![
        saga_step(StepRole::Forward, project_step_target(), 1, "step-a", None),
        saga_step(StepRole::Forward, catalog_step_target(), 1, "step-b", None),
        saga_step(StepRole::Forward, project_step_target(), 2, "step-c", None),
    ];
    let plan = plan_of(steps);
    let command_id = CommandId::new();
    let handle = accept_operation(
        &coordinator,
        &saga_envelope(command_id.clone(), 10),
        &plan,
        &targets,
    )
    .unwrap();
    let outcome = coordinator
        .run(
            &handle,
            &plan,
            &targets,
            &auth,
            vec![
                bump_effect(1, "step-a".into(), calls.clone()),
                failing_effect(calls.clone()), // step-b 失败,step-a 无补偿
                failing_effect(calls.clone()),
            ],
            None,
        )
        .unwrap();
    assert_eq!(
        outcome,
        OperationOutcome::NeedsYou {
            problem_code: CODE_COMPENSATION_MISSING.to_string(),
        }
    );
    let record = operation_of(&service, &handle).unwrap();
    assert_eq!(record.state, OperationState::NeedsYou);
    let problem = record
        .progress
        .problem
        .expect("Needs You 必须保留可诊断 problem");
    assert_eq!(problem.code, CODE_COMPENSATION_MISSING);
    assert_eq!(
        problem.step_index,
        Some(0),
        "problem 指向缺补偿的已生效 step"
    );
    let (intent_state, problem_code) = intent_of(&service, &command_id);
    assert_eq!(intent_state, crate::command::IntentState::Failed);
    assert_eq!(problem_code.as_deref(), Some(CODE_COMPENSATION_MISSING));
    // Needs You 不清理业务效果:step-a 的效果与 receipt 保留供诊断。
    assert_eq!(target_snapshot(&project).2, 2);
}

/// 补偿自身失败 → Needs You(compensation_failed),保留已生效效果。
#[test]
fn compensation_failure_enters_needs_you() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let auth = TestAuthorizer::new(11);
    let calls = Arc::new(AtomicUsize::new(0));
    let steps = vec![
        saga_step(StepRole::Forward, project_step_target(), 1, "step-a", None),
        saga_step(
            StepRole::Compensate,
            project_step_target(),
            2,
            "undo-a",
            Some(0),
        ),
        saga_step(StepRole::Forward, catalog_step_target(), 1, "step-b", None),
    ];
    let plan = plan_of(steps);
    let command_id = CommandId::new();
    let handle = accept_operation(
        &coordinator,
        &saga_envelope(command_id.clone(), 11),
        &plan,
        &targets,
    )
    .unwrap();
    let outcome = coordinator
        .run(
            &handle,
            &plan,
            &targets,
            &auth,
            vec![
                bump_effect(1, "step-a".into(), calls.clone()),
                failing_effect(calls.clone()), // 补偿失败
                failing_effect(calls.clone()), // step-b 失败触发补偿
            ],
            None,
        )
        .unwrap();
    assert_eq!(
        outcome,
        OperationOutcome::NeedsYou {
            problem_code: "compensation_failed".to_string(),
        }
    );
    let record = operation_of(&service, &handle).unwrap();
    assert_eq!(record.state, OperationState::NeedsYou);
    assert_eq!(
        record.progress.problem.as_ref().unwrap().code,
        "compensation_failed"
    );
    assert_eq!(
        record.progress.problem.as_ref().unwrap().step_index,
        Some(1)
    );
    let steps = steps_of(&service, &handle).unwrap();
    assert_eq!(steps[1].state, StepState::Failed);
    assert_eq!(steps[1].problem_code.as_deref(), Some("revision_conflict"));
    assert_eq!(
        intent_of(&service, &command_id).1.as_deref(),
        Some("compensation_failed")
    );
}

/// accept 幂等:同 command_id + 同 plan 返回原 handle;异 plan 拒绝。
#[test]
fn accept_is_idempotent_by_command_id_and_frozen_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let targets = open_targets(&tmp);
    let command_id = CommandId::new();
    let envelope = saga_envelope(command_id.clone(), 12);
    let plan = plan_of(two_forward_steps());
    let handle = accept_operation(&coordinator, &envelope, &plan, &targets).unwrap();
    assert_eq!(
        accept_operation(&coordinator, &envelope, &plan, &targets).unwrap(),
        handle
    );
    let changed_envelope = crate::command_support::envelope(
        command_id,
        TargetStoreKind::Project,
        12,
        plain_payload("different initiating semantics"),
    );
    assert_eq!(
        accept_operation(&coordinator, &changed_envelope, &plan, &targets).unwrap_err(),
        crate::operation::OperationProblem::CommandIdReused,
        "同 plan 也不能绕过 initiating command digest"
    );

    let mut other = two_forward_steps();
    other.push(saga_step(
        StepRole::Forward,
        project_step_target(),
        2,
        "extra",
        None,
    ));
    let error = accept_operation(&coordinator, &envelope, &plan_of(other), &targets).unwrap_err();
    assert_eq!(
        error,
        crate::operation::OperationProblem::CommandIdReused,
        "同 command_id 不得更换冻结 plan"
    );
    // 不同 command_id 可另起 saga,handle 互不相同。
    let second = accept_operation(
        &coordinator,
        &saga_envelope(CommandId::new(), 12),
        &plan_of(two_forward_steps()),
        &targets,
    )
    .unwrap();
    assert_ne!(second, handle);
}

/// acceptance target commit 后、handle 返回前崩溃：重试必须从 receipt
/// 恢复同一 handle，且不得写第二条 receipt/outbox。
#[test]
fn accept_retry_after_target_commit_returns_same_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let coordinator = OperationCoordinator::new(service, key());
    let envelope = saga_envelope(CommandId::new(), 16);
    let plan = plan_of(two_forward_steps());
    let error = coordinator
        .accept_with_fault(
            &envelope,
            &plan,
            &targets[0],
            &TestAuthorizer::new(16),
            OperationAcceptFaultPoint::AfterTargetCommit,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        crate::operation::OperationProblem::Command(CommandProblem::FaultInjected(
            "after_operation_accept_target_commit"
        ))
    ));
    let receipt_handle = targets[0]
        .with_conn(|conn| {
            let raw: String = conn
                .query_row(
                    "SELECT result_revisions FROM command_receipt WHERE command_id=?1",
                    [envelope.command_id().as_str()],
                    |row| row.get(0),
                )
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            let value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            Ok(value["operation_handle"].as_str().unwrap().to_string())
        })
        .unwrap();
    let retried = accept_operation(&coordinator, &envelope, &plan, &targets).unwrap();
    assert_eq!(retried.as_str(), receipt_handle);
    assert_eq!(target_snapshot(&targets[0]).2, 1);
    assert_eq!(target_snapshot(&targets[0]).3, 1);
}

/// saga payload 不承载明文凭据(§7.4 WriteOnlySecret 特例只属单命令)。
#[test]
fn plan_payload_with_credentials_is_rejected() {
    let error = SagaStepPlan::new(
        StepRole::Forward,
        StepId::new(),
        CommandType::WorkflowMoveNode,
        project_step_target(),
        vec![],
        &json!({"api_key": "sk-secret"}),
        None,
        &OperationKind::parse(SAGA_KIND).unwrap(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        crate::operation::OperationProblem::InvalidPlan(_)
    ));
}

/// effect 输出携带敏感字段时 fail-closed:step 终结 failed、不写 receipt。
#[test]
fn sensitive_step_output_fails_closed_without_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let project = targets[0].clone();
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let auth = TestAuthorizer::new(13);
    let plan = plan_of(vec![saga_step(
        StepRole::Forward,
        project_step_target(),
        1,
        "leaky",
        None,
    )]);
    let handle = accept_operation(
        &coordinator,
        &saga_envelope(CommandId::new(), 13),
        &plan,
        &targets,
    )
    .unwrap();
    let outcome = coordinator
        .run(
            &handle,
            &plan,
            &targets,
            &auth,
            vec![Box::new(|_tx| {
                Ok(EffectOutput::for_contract(
                    json!({"revision": 2}),
                    json!({"api_key": "sk-leak"}),
                ))
            })],
            None,
        )
        .unwrap();
    // 无已生效 step:补偿空集平凡完成,但目标未达成、intent failed。
    assert_eq!(outcome, OperationOutcome::Completed { compensated: true });
    let steps = steps_of(&service, &handle).unwrap();
    assert_eq!(steps[0].state, StepState::Failed);
    assert_eq!(steps[0].problem_code.as_deref(), Some("invalid_envelope"));
    assert_eq!(
        target_snapshot(&project).2,
        1,
        "只保留 acceptance receipt，敏感 step 不得留下 receipt"
    );
    assert_eq!(
        target_snapshot(&project).0,
        "initial",
        "敏感 step 不得产生任何业务写"
    );
}

/// run 必须与冻结 plan 身份一致;拿错 plan 直接拒绝。
#[test]
fn run_rejects_plan_that_does_not_match_frozen_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let auth = TestAuthorizer::new(14);
    let plan = plan_of(two_forward_steps());
    let handle = accept_operation(
        &coordinator,
        &saga_envelope(CommandId::new(), 14),
        &plan,
        &targets,
    )
    .unwrap();
    let wrong = plan_of(vec![
        saga_step(StepRole::Forward, project_step_target(), 1, "other", None),
        saga_step(StepRole::Forward, catalog_step_target(), 1, "other-b", None),
    ]);
    let error = coordinator
        .run(
            &handle,
            &wrong,
            &targets,
            &auth,
            vec![
                failing_effect(Arc::new(AtomicUsize::new(0))),
                failing_effect(Arc::new(AtomicUsize::new(0))),
            ],
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        crate::operation::OperationProblem::PlanConflict(_)
    ));
}

/// 已终结 operation 不得再执行 forward step(状态机守卫)。
#[test]
fn run_refuses_steps_after_terminal_state() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let auth = TestAuthorizer::new(15);
    let plan = plan_of(two_forward_steps());
    let handle = accept_operation(
        &coordinator,
        &saga_envelope(CommandId::new(), 15),
        &plan,
        &targets,
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    coordinator
        .run(
            &handle,
            &plan,
            &targets,
            &auth,
            vec![
                bump_effect(1, "step-a".into(), calls.clone()),
                bump_effect(1, "step-b".into(), calls.clone()),
            ],
            None,
        )
        .unwrap();
    let error = coordinator
        .run(
            &handle,
            &plan,
            &targets,
            &auth,
            vec![
                failing_effect(Arc::new(AtomicUsize::new(0))),
                failing_effect(Arc::new(AtomicUsize::new(0))),
            ],
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        crate::operation::OperationProblem::StateConflict(_)
    ));
}
