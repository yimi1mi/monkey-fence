//! T1g 契约(Issue #22):多 Store 故障矩阵——step1 后终结、step2 后只补
//! 结果、legacy 不重放、reconcile 幂等可续跑、outbox reconciled 标记。
//!
//! 「重启」= 丢弃全部连接后重开 service/project/catalog(fixture 全在
//! tempfile);fault point 提供确定性崩溃窗口(§14.2 fault harness)。

use crate::command::{
    CommandCoordinator, CommandProblem, FaultPoint, IntentState, ReconcileOutcome,
};
use crate::command_support::*;
use crate::handles::{CommandId, TargetStoreKind};
use crate::operation::{
    mark_reconciling, operation_of, steps_of, OperationAcceptFaultPoint, OperationCoordinator,
    OperationFaultPoint, OperationHandle, OperationOutcome, OperationState, StepRole, StepState,
    CODE_COMPENSATION_MISSING, CODE_COMPENSATION_REQUIRED,
};
use crate::operation_saga::*;
use crate::project_registry::ServiceStore;
use crate::reconcile::{is_reconciled_mark, reconcile_startup, ReconcileReport};
use chrono::Utc;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

fn run_with_fault(
    service: &Arc<ServiceStore>,
    targets: &[crate::command::TargetDatabase],
    handle: &crate::operation::OperationHandle,
    plan: &crate::operation::OperationPlan,
    epoch: u64,
    effects: Vec<crate::operation::StepEffect>,
    fault: Option<OperationFaultPoint>,
) -> Result<OperationOutcome, crate::operation::OperationProblem> {
    let coordinator = OperationCoordinator::new(service.clone(), key());
    coordinator.run(
        handle,
        plan,
        targets,
        &TestAuthorizer::new(epoch),
        effects,
        fault,
    )
}

/// crash 窗口:step1(target commit)后崩溃——receipt 已在、service 未记。
/// 重启后只依 receipt 补 service 结果,绝不重做业务写。
#[test]
fn crash_after_first_step_target_commit_recovers_from_receipt_only() {
    let tmp = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let plan = plan_of(vec![
        saga_step(StepRole::Forward, project_step_target(), 1, "step-a", None),
        saga_step(
            StepRole::Compensate,
            project_step_target(),
            2,
            "undo-a",
            Some(0),
        ),
        saga_step(StepRole::Forward, catalog_step_target(), 1, "step-b", None),
    ]);
    let handle;
    {
        let service = saga_service(&tmp);
        let targets = open_targets(&tmp);
        let coordinator = OperationCoordinator::new(service.clone(), key());
        handle = accept_operation(
            &coordinator,
            &saga_envelope(command_id.clone(), 21),
            &plan,
            &targets,
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let error = run_with_fault(
            &service,
            &targets,
            &handle,
            &plan,
            21,
            vec![
                bump_effect(1, "step-a".into(), calls.clone()),
                bump_effect(2, "undo-a".into(), calls.clone()),
                bump_effect(1, "step-b".into(), calls.clone()),
            ],
            Some(OperationFaultPoint::AfterStepTargetCommit(0)),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            crate::operation::OperationProblem::FaultInjected("after_step_target_commit")
        ));
        // 崩溃现场:step0 的 receipt/outbox 已持久,service step 行仍 pending。
        let steps = steps_of(&service, &handle).unwrap();
        assert_eq!(steps[0].state, StepState::Pending);
        let (value, revision, receipts, outbox, _) = target_snapshot(&targets[0]);
        assert_eq!(
            (value.as_str(), revision, receipts, outbox),
            ("step-a", 2, 2, 2)
        );
    }

    // 模拟 Core 重启:重开全部库,startup reconcile。
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(report.operations_needs_you, 1);
    assert_eq!(
        report.intents_revoked, 0,
        "operation intent 由 op reconcile 终结"
    );

    let record = operation_of(&service, &handle).unwrap();
    assert_eq!(record.operation_handle, handle, "handle 跨重启稳定");
    assert_eq!(record.state, OperationState::NeedsYou);
    assert_eq!(
        record.progress.problem.as_ref().unwrap().code,
        CODE_COMPENSATION_REQUIRED,
        "重启后 lease/epoch 失效,补偿无法自动完成"
    );
    let steps = steps_of(&service, &handle).unwrap();
    assert_eq!(
        steps[0].state,
        StepState::Succeeded,
        "receipt 在 → 只补 service 结果"
    );
    assert_eq!(
        steps[0].result,
        serde_json::json!({"revision": 2}),
        "service step result 必须从 target receipt 恢复"
    );
    assert_eq!(
        steps[1].state,
        StepState::Revoked,
        "无 receipt 的补偿 step 只终结,不执行"
    );
    assert_eq!(
        steps[2].state,
        StepState::Revoked,
        "无 receipt 的 forward step 只终结"
    );
    // 绝不重做业务写:receipt 数不变、值不变。
    let (value, revision, receipts, _, unpublished) = target_snapshot_detail(&targets[0]);
    assert_eq!((value.as_str(), revision, receipts), ("step-a", 2, 2));
    assert_eq!(unpublished, 0, "旧 outbox 已全部标记 reconciled");
    assert!(outbox_all_marked_reconciled(&targets[0]));
    assert_eq!(
        intent_of(&service, &command_id).1.as_deref(),
        Some(CODE_COMPENSATION_REQUIRED)
    );
}

/// service 已把 step0 记为 succeeded、step1 尚未执行时崩溃；重启仍只读
/// receipt/step 状态，不执行下一 effect。
#[test]
fn crash_after_step_finalized_reconciles_without_next_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let plan = plan_of(two_forward_steps());
    let handle;
    {
        let service = saga_service(&tmp);
        let targets = open_targets(&tmp);
        let coordinator = OperationCoordinator::new(service.clone(), key());
        handle = accept_operation(
            &coordinator,
            &saga_envelope(command_id.clone(), 26),
            &plan,
            &targets,
        )
        .unwrap();
        run_with_fault(
            &service,
            &targets,
            &handle,
            &plan,
            26,
            vec![
                bump_effect(1, "step-a".into(), Arc::new(AtomicUsize::new(0))),
                bump_effect(1, "must-not-run".into(), Arc::new(AtomicUsize::new(0))),
            ],
            Some(OperationFaultPoint::AfterStepFinalized(0)),
        )
        .unwrap_err();
        assert_eq!(
            steps_of(&service, &handle).unwrap()[0].state,
            StepState::Succeeded
        );
        assert_eq!(target_snapshot(&targets[1]).0, "initial");
    }
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(report.operations_needs_you, 1);
    let record = operation_of(&service, &handle).unwrap();
    assert_eq!(record.state, OperationState::NeedsYou);
    assert_eq!(
        record.progress.problem.unwrap().code,
        CODE_COMPENSATION_MISSING
    );
    assert_eq!(target_snapshot(&targets[0]).0, "step-a");
    assert_eq!(target_snapshot(&targets[1]).0, "initial");
}

/// operation 已提交 compensating、补偿尚未执行时崩溃；重启不得自动执行
/// compensation，只能进入 Needs You。
#[test]
fn crash_after_compensating_enters_needs_you_without_effect_replay() {
    let tmp = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let plan = plan_of(vec![
        saga_step(StepRole::Forward, project_step_target(), 1, "step-a", None),
        saga_step(
            StepRole::Compensate,
            project_step_target(),
            2,
            "undo-a",
            Some(0),
        ),
        saga_step(StepRole::Forward, catalog_step_target(), 1, "step-b", None),
    ]);
    let compensation_calls = Arc::new(AtomicUsize::new(0));
    let handle;
    {
        let service = saga_service(&tmp);
        let targets = open_targets(&tmp);
        let coordinator = OperationCoordinator::new(service.clone(), key());
        handle = accept_operation(
            &coordinator,
            &saga_envelope(command_id.clone(), 27),
            &plan,
            &targets,
        )
        .unwrap();
        run_with_fault(
            &service,
            &targets,
            &handle,
            &plan,
            27,
            vec![
                bump_effect(1, "step-a".into(), Arc::new(AtomicUsize::new(0))),
                bump_effect(2, "undo-a".into(), compensation_calls.clone()),
                failing_effect(Arc::new(AtomicUsize::new(0))),
            ],
            Some(OperationFaultPoint::AfterCompensating),
        )
        .unwrap_err();
        assert_eq!(
            operation_of(&service, &handle).unwrap().state,
            OperationState::Compensating
        );
        assert_eq!(
            compensation_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(report.operations_needs_you, 1);
    assert_eq!(
        operation_of(&service, &handle)
            .unwrap()
            .progress
            .problem
            .unwrap()
            .code,
        CODE_COMPENSATION_REQUIRED
    );
    assert_eq!(
        compensation_calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(target_snapshot(&targets[0]).0, "step-a");
}

/// operation 终态 service 事务提交后、响应前崩溃：重启不得重新推进或重放。
#[test]
fn crash_after_operation_finalized_is_terminal_noop_on_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let plan = plan_of(two_forward_steps());
    let handle;
    {
        let service = saga_service(&tmp);
        let targets = open_targets(&tmp);
        let coordinator = OperationCoordinator::new(service.clone(), key());
        handle = accept_operation(
            &coordinator,
            &saga_envelope(command_id.clone(), 28),
            &plan,
            &targets,
        )
        .unwrap();
        run_with_fault(
            &service,
            &targets,
            &handle,
            &plan,
            28,
            vec![
                bump_effect(1, "step-a".into(), Arc::new(AtomicUsize::new(0))),
                bump_effect(1, "step-b".into(), Arc::new(AtomicUsize::new(0))),
            ],
            Some(OperationFaultPoint::AfterOperationFinalized),
        )
        .unwrap_err();
        assert_eq!(
            operation_of(&service, &handle).unwrap().state,
            OperationState::Completed
        );
    }
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let first = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(first.operations_completed, 0);
    assert_eq!(first.operations_needs_you, 0);
    assert_eq!(
        operation_of(&service, &handle).unwrap().state,
        OperationState::Completed
    );
    assert_eq!(intent_of(&service, &command_id).0, IntentState::Applied);
    let receipts = (
        target_snapshot(&targets[0]).2,
        target_snapshot(&targets[1]).2,
    );
    let second = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(second, ReconcileReport::default());
    assert_eq!(
        (
            target_snapshot(&targets[0]).2,
            target_snapshot(&targets[1]).2
        ),
        receipts
    );
}

/// target_snapshot 附加未发布 outbox 计数(reconciled 标记断言用)。
fn target_snapshot_detail(target: &crate::command::TargetDatabase) -> (String, i64, i64, i64, i64) {
    target
        .with_conn(|conn| {
            let (value, revision): (String, i64) = conn
                .query_row(
                    "SELECT value, revision FROM command_test_effect WHERE aggregate_handle=?1",
                    [AGGREGATE],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|e| CommandProblem::Internal(e.to_string()))?;
            let receipts: i64 = conn
                .query_row("SELECT COUNT(*) FROM command_receipt", [], |row| row.get(0))
                .map_err(|e| CommandProblem::Internal(e.to_string()))?;
            let outbox: i64 = conn
                .query_row("SELECT COUNT(*) FROM projection_outbox", [], |row| {
                    row.get(0)
                })
                .map_err(|e| CommandProblem::Internal(e.to_string()))?;
            let unpublished: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM projection_outbox WHERE published_at IS NULL",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| CommandProblem::Internal(e.to_string()))?;
            Ok((value, revision, receipts, outbox, unpublished))
        })
        .unwrap()
}

fn outbox_all_marked_reconciled(target: &crate::command::TargetDatabase) -> bool {
    target
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT published_at FROM projection_outbox")
                .map_err(|e| CommandProblem::Internal(e.to_string()))?;
            let marks = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| CommandProblem::Internal(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| CommandProblem::Internal(e.to_string()))?;
            Ok(marks.iter().all(|mark| is_reconciled_mark(mark)))
        })
        .unwrap()
}

/// 全部 forward step 均有 receipt、只差终态化:重启后 completed,零 effect 重放。
#[test]
fn crash_after_last_step_target_commit_completes_from_receipts() {
    let tmp = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let plan = plan_of(two_forward_steps());
    let handle;
    {
        let service = saga_service(&tmp);
        let targets = open_targets(&tmp);
        let coordinator = OperationCoordinator::new(service.clone(), key());
        handle = accept_operation(
            &coordinator,
            &saga_envelope(command_id.clone(), 22),
            &plan,
            &targets,
        )
        .unwrap();
        run_with_fault(
            &service,
            &targets,
            &handle,
            &plan,
            22,
            vec![
                bump_effect(1, "step-a".into(), Arc::new(AtomicUsize::new(0))),
                bump_effect(1, "step-b".into(), Arc::new(AtomicUsize::new(0))),
            ],
            Some(OperationFaultPoint::AfterStepTargetCommit(1)),
        )
        .unwrap_err();
        assert_eq!(target_snapshot(&targets[0]).2, 2);
        assert_eq!(target_snapshot(&targets[1]).2, 1);
    }

    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(report.operations_completed, 1);
    let record = operation_of(&service, &handle).unwrap();
    assert_eq!(record.state, OperationState::Completed);
    assert_eq!(record.progress.outcome, None);
    assert_eq!(intent_of(&service, &command_id).0, IntentState::Applied);
    // 零 effect 重放:receipt 数与业务值保持崩溃现场。
    assert_eq!(target_snapshot(&targets[0]).2, 2);
    assert_eq!(target_snapshot(&targets[0]).0, "step-a");
    assert_eq!(target_snapshot(&targets[1]).2, 1);
}

/// 补偿 target commit 后崩溃:重启依 receipt 认定完整回滚,不重放补偿。
#[test]
fn crash_after_compensation_target_commit_completes_compensated() {
    let tmp = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let plan = plan_of(vec![
        saga_step(StepRole::Forward, project_step_target(), 1, "step-a", None),
        saga_step(
            StepRole::Compensate,
            project_step_target(),
            2,
            "undo-a",
            Some(0),
        ),
        saga_step(StepRole::Forward, catalog_step_target(), 1, "step-b", None),
    ]);
    let handle;
    {
        let service = saga_service(&tmp);
        let targets = open_targets(&tmp);
        let coordinator = OperationCoordinator::new(service.clone(), key());
        handle = accept_operation(
            &coordinator,
            &saga_envelope(command_id.clone(), 23),
            &plan,
            &targets,
        )
        .unwrap();
        run_with_fault(
            &service,
            &targets,
            &handle,
            &plan,
            23,
            vec![
                bump_effect(1, "step-a".into(), Arc::new(AtomicUsize::new(0))),
                bump_effect(2, "undo-a".into(), Arc::new(AtomicUsize::new(0))),
                failing_effect(Arc::new(AtomicUsize::new(0))), // step-b 失败触发补偿
            ],
            Some(OperationFaultPoint::AfterStepTargetCommit(1)),
        )
        .unwrap_err();
        // 现场:compensating + 补偿 receipt 已持久。
        assert_eq!(
            operation_of(&service, &handle).unwrap().state,
            OperationState::Compensating
        );
        assert_eq!(target_snapshot(&targets[0]).2, 3);
    }

    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(report.operations_completed, 1);
    let record = operation_of(&service, &handle).unwrap();
    assert_eq!(record.state, OperationState::Completed);
    assert_eq!(record.progress.outcome.as_deref(), Some("compensated"));
    assert_eq!(record.progress.forward_succeeded, 1);
    let steps = steps_of(&service, &handle).unwrap();
    assert_eq!(steps[1].state, StepState::Succeeded);
    assert_eq!(steps[2].state, StepState::Failed);
    assert_eq!(target_snapshot(&targets[0]).2, 3, "补偿不得重放");
    assert_eq!(target_snapshot(&targets[0]).0, "undo-a");
}

/// service reserve 后、acceptance target receipt 前崩溃：调用方未获得 202；
/// 重启只终结 intent/steps，零业务写。
#[test]
fn crash_before_acceptance_receipt_is_not_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let plan = plan_of(two_forward_steps());
    let handle;
    {
        let service = saga_service(&tmp);
        let targets = open_targets(&tmp);
        let coordinator = OperationCoordinator::new(service.clone(), key());
        let envelope = saga_envelope(command_id.clone(), 24);
        let error = coordinator
            .accept_with_fault(
                &envelope,
                &plan,
                &targets[0],
                &TestAuthorizer::new(24),
                OperationAcceptFaultPoint::AfterIntentReserve,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            crate::operation::OperationProblem::FaultInjected("after_intent_reserve")
        ));
        let raw = service
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT operation_handle FROM operation WHERE command_id=?1",
                    [command_id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| anyhow::anyhow!(error))
            })
            .unwrap();
        handle = OperationHandle::parse(raw).unwrap();
        assert_eq!(
            operation_of(&service, &handle).unwrap().state,
            OperationState::Accepted
        );
    }

    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(report.operations_not_accepted, 1);
    let steps = steps_of(&service, &handle).unwrap();
    assert_eq!(
        steps.iter().map(|step| step.state).collect::<Vec<_>>(),
        vec![StepState::Revoked, StepState::Revoked],
        "无 receipt 的 step 只终结,绝不重做业务写"
    );
    assert_eq!(
        steps[0].problem_code.as_deref(),
        Some("controller_lease_expired")
    );
    for target in &targets {
        let (value, revision, receipts, outbox, _) = target_snapshot_detail(target);
        assert_eq!(
            (value.as_str(), revision, receipts, outbox),
            ("initial", 1, 0, 0),
            "崩溃前无任何业务写,重启也不得产生"
        );
    }
    let (intent_state, problem) = intent_of(&service, &command_id);
    assert_eq!(intent_state, IntentState::Revoked);
    assert_eq!(problem.as_deref(), Some("controller_lease_expired"));
    let coordinator = OperationCoordinator::new(service.clone(), key());
    let retry = accept_operation(
        &coordinator,
        &saga_envelope(command_id, 24),
        &plan,
        &targets,
    )
    .unwrap_err();
    assert!(matches!(
        retry,
        crate::operation::OperationProblem::Command(CommandProblem::ControllerLeaseExpired)
    ));
    assert_eq!(target_snapshot_detail(&targets[0]).2, 0);
    assert_eq!(target_snapshot_detail(&targets[0]).3, 0);
}

/// reconcile 自身可崩溃续跑:先部分推进(mark reconciling),重启后收敛;
/// 重复执行幂等。
#[test]
fn reconcile_is_reentrant_and_idempotent_across_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let plan = plan_of(vec![
        saga_step(StepRole::Forward, project_step_target(), 1, "step-a", None),
        saga_step(
            StepRole::Compensate,
            project_step_target(),
            2,
            "undo-a",
            Some(0),
        ),
        saga_step(StepRole::Forward, catalog_step_target(), 1, "step-b", None),
    ]);
    let handle;
    {
        let service = saga_service(&tmp);
        let coordinator = OperationCoordinator::new(service.clone(), key());
        let targets = open_targets(&tmp);
        handle = accept_operation(
            &coordinator,
            &saga_envelope(command_id.clone(), 25),
            &plan,
            &targets,
        )
        .unwrap();
    }
    // 部分 reconcile:只推进到 reconciling 就「崩溃」。
    {
        let service = saga_service(&tmp);
        assert_eq!(mark_reconciling(&service).unwrap(), 1);
        assert_eq!(
            operation_of(&service, &handle).unwrap().state,
            OperationState::Reconciling
        );
    }
    // 重启后完整 reconcile 收敛(无已生效 step → 平凡回滚终结,不重放)。
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let first = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(first.operations_completed, 1);
    let record = operation_of(&service, &handle).unwrap();
    assert_eq!(record.state, OperationState::Completed);
    assert_eq!(record.progress.outcome.as_deref(), Some("compensated"));
    // 重复 reconcile:幂等 no-op,状态与标记不再变化。
    let second = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(
        second,
        ReconcileReport {
            intents_revoked: 0,
            intents_applied: 0,
            intents_skipped: 0,
            operations_completed: 0,
            operations_needs_you: 0,
            operations_not_accepted: 0,
            operations_skipped: 0,
            outbox_reconciled: vec![],
        },
        "第二次 reconcile 必须是 no-op"
    );
    assert_eq!(
        operation_of(&service, &handle).unwrap().state,
        OperationState::Completed
    );
    let steps = steps_of(&service, &handle).unwrap();
    assert!(steps.iter().all(|step| step.state == StepState::Revoked));
}

/// legacy 形状的 operation(无 operation_step 行)不重放、不补造;
/// store 未打开的 intent 保持 reserved 等待诊断。
#[test]
fn legacy_rows_are_skipped_not_replayed() {
    let tmp = tempfile::tempdir().unwrap();
    let legacy_command = "018f0000-0000-7000-8000-0000000000aa";
    let orphan_command = "018f0000-0000-7000-8000-0000000000bb";
    {
        let service = saga_service(&tmp);
        service
            .with_conn(|conn| {
                conn.execute_batch(&format!(
                    "INSERT INTO command_intent
                         (command_id, semantic_digest, target_store, aggregate, principal,
                          client_id, controller_epoch, root_epoch, state, created_at)
                     VALUES ('{legacy_command}', 'digest-l', 'project:proj_unknown', 'wf',
                             'user-test', 'client-test', 1, NULL, 'reserved',
                             '2026-09-01T00:00:00Z');
                     INSERT INTO command_intent
                         (command_id, semantic_digest, target_store, aggregate, principal,
                          client_id, controller_epoch, root_epoch, state, created_at)
                     VALUES ('{orphan_command}', 'digest-o', 'project:proj_018f0000-0000-7000-8000-000000000001',
                             'wf', 'user-test', 'client-test', 1, NULL, 'reserved',
                             '2026-09-01T00:00:00Z');
                     INSERT INTO operation
                         (operation_handle, command_id, kind, state, saga_state, progress_json,
                          created_at, updated_at)
                     VALUES ('op_018f0000-0000-7000-8000-0000000000cc', '{orphan_command}',
                             'legacy.kind', 'running', '', '{{}}',
                             '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z');"
                ))
                .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .unwrap();
        // legacy target 库里的未发布 outbox 行(非新协议链产物)。
        let targets = open_targets(&tmp);
        targets[0]
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO projection_outbox(event_json, published_at)
                     VALUES ('{\"type\":\"legacy.event\"}', NULL)",
                    [],
                )
                .map_err(|e| CommandProblem::Internal(e.to_string()))?;
                Ok(())
            })
            .unwrap();
    }

    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    // legacy operation:无 step 行 → skipped,不终结、不重放。
    assert_eq!(report.operations_skipped, 1);
    let legacy_state: String = service
        .with_conn(|conn| {
            conn.query_row(
                "SELECT state FROM operation WHERE operation_handle=?1",
                ["op_018f0000-0000-7000-8000-0000000000cc"],
                |row| row.get(0),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();
    assert_eq!(
        legacy_state, "reconciling",
        "legacy operation 只推进 reconciling 标记,不补造 step/终态"
    );
    // store 未打开的 reserved intent:跳过且保持 reserved。
    assert_eq!(report.intents_skipped, 1);
    let orphan_state: String = service
        .with_conn(|conn| {
            conn.query_row(
                "SELECT state FROM command_intent WHERE command_id=?1",
                [legacy_command],
                |row| row.get(0),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();
    assert_eq!(orphan_state, "reserved");
    // 被 operation 引用的 intent 不走单命令终结路径。
    let referenced_state: String = service
        .with_conn(|conn| {
            conn.query_row(
                "SELECT state FROM command_intent WHERE command_id=?1",
                [orphan_command],
                |row| row.get(0),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();
    assert_eq!(referenced_state, "reserved");
    // legacy outbox 行被 reconciled 收尾(不向新 epoch 重放陈旧 delta)。
    assert_eq!(report.outbox_reconciled[0].1, 1);
    assert!(outbox_all_marked_reconciled(&targets[0]));
    let events: i64 = targets[0]
        .with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM projection_outbox", [], |row| {
                row.get(0)
            })
            .map_err(|e| CommandProblem::Internal(e.to_string()))
        })
        .unwrap();
    assert_eq!(events, 1, "不补造任何新 delta");
}

/// 单命令 intent(#21 链)也由 startup reconcile 统一终结:
/// step1 后无 receipt → revoked;step2 后有 receipt → applied。
#[test]
fn startup_reconcile_finalizes_plain_command_intents() {
    let tmp = tempfile::tempdir().unwrap();
    let revoked = CommandId::new();
    let applied = CommandId::new();
    {
        let service = saga_service(&tmp);
        let coordinator = CommandCoordinator::new(service.clone(), key());
        let target = project_target(&tmp.path().join("workflow-v1.db"));
        let calls = Arc::new(AtomicUsize::new(0));
        coordinator
            .dispatch_with_fault(
                &envelope(
                    revoked.clone(),
                    TargetStoreKind::Project,
                    31,
                    plain_payload("never"),
                ),
                &target,
                &TestAuthorizer::new(31),
                set_value_effect("never"),
                Some(FaultPoint::AfterIntentReserve),
            )
            .unwrap_err();
        coordinator
            .dispatch_with_fault(
                &envelope(
                    applied.clone(),
                    TargetStoreKind::Project,
                    31,
                    plain_payload("done"),
                ),
                &target,
                &TestAuthorizer::new(31),
                update_effect("done".into(), calls.clone()),
                Some(FaultPoint::AfterTargetCommit),
            )
            .unwrap_err();
    }

    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(report.intents_revoked, 1);
    assert_eq!(report.intents_applied, 1);
    assert_eq!(
        coordinator_intent(&service, &revoked),
        Some(IntentState::Revoked)
    );
    assert_eq!(
        coordinator_intent(&service, &applied),
        Some(IntentState::Applied)
    );
    // 与 #21 逐条 reconcile 相同的终态。
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let coordinator = CommandCoordinator::new(service.clone(), key());
    assert_eq!(
        coordinator.reconcile(&revoked, &target).unwrap(),
        ReconcileOutcome::Terminal(IntentState::Revoked)
    );
}

fn coordinator_intent(service: &Arc<ServiceStore>, command_id: &CommandId) -> Option<IntentState> {
    CommandCoordinator::new(service.clone(), key())
        .intent_state(command_id)
        .unwrap()
}

/// 只有 `state='applied' AND finalized_at IS NOT NULL` 的 target receipt
/// 才能证明业务效果已线性化；failed 或未 finalized 的行不得提升 intent。
#[test]
fn startup_reconcile_rejects_nonterminal_target_receipts() {
    let tmp = tempfile::tempdir().unwrap();
    let failed = CommandId::new();
    let unfinalized = CommandId::new();
    let service = saga_service(&tmp);
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let coordinator = CommandCoordinator::new(service.clone(), key());
    for command_id in [&failed, &unfinalized] {
        coordinator
            .dispatch_with_fault(
                &envelope(
                    command_id.clone(),
                    TargetStoreKind::Project,
                    40,
                    plain_payload("not-terminal"),
                ),
                &target,
                &TestAuthorizer::new(40),
                set_value_effect("must-not-run"),
                Some(FaultPoint::AfterIntentReserve),
            )
            .unwrap_err();
    }
    for (command_id, state, finalized_at) in [
        (&failed, "failed", Some(Utc::now().to_rfc3339())),
        (&unfinalized, "applied", None),
    ] {
        let (digest, aggregate): (String, String) = service
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT semantic_digest, aggregate FROM command_intent WHERE command_id=?1",
                    [command_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|error| anyhow::anyhow!(error))
            })
            .unwrap();
        target
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO command_receipt
                     (command_id, semantic_digest, aggregate_handle, result_revisions,
                      state, created_at, finalized_at)
                     VALUES (?1, ?2, ?3, '{}', ?4, ?5, ?6)",
                    rusqlite::params![
                        command_id.as_str(),
                        digest,
                        aggregate,
                        state,
                        Utc::now().to_rfc3339(),
                        finalized_at,
                    ],
                )
                .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                Ok(())
            })
            .unwrap();
    }

    drop(target);
    drop(coordinator);
    drop(service);
    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(report.intents_skipped, 2);
    assert_eq!(
        coordinator_intent(&service, &failed),
        Some(IntentState::Reserved)
    );
    assert_eq!(
        coordinator_intent(&service, &unfinalized),
        Some(IntentState::Reserved)
    );
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let coordinator = CommandCoordinator::new(service, key());
    assert_eq!(
        coordinator.reconcile(&unfinalized, &target).unwrap_err(),
        CommandProblem::CommandInProgress
    );
    assert_eq!(
        coordinator
            .dispatch_contract(
                &envelope(
                    unfinalized,
                    TargetStoreKind::Project,
                    40,
                    plain_payload("not-terminal"),
                ),
                &target,
                &TestAuthorizer::new(40),
                panic_effect(),
            )
            .unwrap_err(),
        CommandProblem::CommandInProgress,
        "dispatch 重试也不得把未 finalized receipt 当成功"
    );
}

/// receipt 身份与 intent/step 冻结 digest 不符时 fail-closed:
/// 单命令 intent 保持 reserved;operation step 标 failed + needs_you。
#[test]
fn tampered_receipt_fails_closed_during_startup_reconcile() {
    let tmp = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let plan = plan_of(vec![saga_step(
        StepRole::Forward,
        project_step_target(),
        1,
        "step-a",
        None,
    )]);
    let handle;
    {
        let service = saga_service(&tmp);
        let targets = open_targets(&tmp);
        let coordinator = OperationCoordinator::new(service.clone(), key());
        handle = accept_operation(
            &coordinator,
            &saga_envelope(command_id.clone(), 41),
            &plan,
            &targets,
        )
        .unwrap();
        run_with_fault(
            &service,
            &targets,
            &handle,
            &plan,
            41,
            vec![bump_effect(
                1,
                "step-a".into(),
                Arc::new(AtomicUsize::new(0)),
            )],
            Some(OperationFaultPoint::AfterStepTargetCommit(0)),
        )
        .unwrap_err();
        // 只篡改 step receipt；acceptance receipt 保持有效，reconcile 必须
        // 在 step 层 fail-closed。
        let step_id = steps_of(&service, &handle).unwrap()[0]
            .step_id
            .as_str()
            .to_string();
        targets[0]
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE command_receipt SET semantic_digest='corrupt' WHERE command_id=?1",
                    [&step_id],
                )
                .map_err(|e| CommandProblem::Internal(e.to_string()))?;
                Ok(())
            })
            .unwrap();
    }

    let service = saga_service(&tmp);
    let targets = open_targets(&tmp);
    let report = reconcile_startup(&service, &targets, Utc::now()).unwrap();
    assert_eq!(report.operations_needs_you, 1);
    let steps = steps_of(&service, &handle).unwrap();
    assert_eq!(steps[0].state, StepState::Failed);
    assert_eq!(steps[0].problem_code.as_deref(), Some("command_id_reused"));
    assert_eq!(
        operation_of(&service, &handle).unwrap().state,
        OperationState::NeedsYou
    );
    // receipt 仍在但身份不符:不得被误删/误用;step-a 业务值保持现场。
    assert_eq!(target_snapshot(&targets[0]).0, "step-a");
}
