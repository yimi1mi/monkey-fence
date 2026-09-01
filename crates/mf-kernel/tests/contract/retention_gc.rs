//! T1g 契约(Issue #22):command receipt / Operation / audit 的 retention
//! 与 GC(§4.6/附录 A4)。未终结、活动中、被 audit 引用的对象绝不删除;
//! 时间与行数上限全部确定性注入(固定 now + 直写 fixture 行)。

use crate::command::CommandProblem;
use crate::command_support::*;
use crate::limits::RetentionLimits;
use crate::operation_saga::saga_service;
use crate::project_registry::ServiceStore;
use crate::reconcile::{run_retention_gc, GcProtection};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::params;
use std::sync::Arc;

/// 全部 GC 测试共享的「当前时间」。
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap()
}

fn days_ago(days: i64) -> String {
    (now() - chrono::Duration::try_days(days).unwrap()).to_rfc3339()
}

fn insert_receipt(
    target: &crate::command::TargetDatabase,
    command_id: &str,
    state: &str,
    finalized: Option<&str>,
) {
    target
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO command_receipt
                     (command_id, semantic_digest, aggregate_handle, result_revisions,
                      state, created_at, finalized_at)
                 VALUES (?1, 'digest-gc', 'wf-gc', '{}', ?2, ?3, ?4)",
                params![
                    command_id,
                    state,
                    finalized.unwrap_or("2026-01-01T00:00:00Z"),
                    finalized
                ],
            )
            .map_err(|e| CommandProblem::Internal(e.to_string()))?;
            Ok(())
        })
        .unwrap();
}

fn receipt_count(target: &crate::command::TargetDatabase) -> i64 {
    target
        .with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM command_receipt", [], |row| row.get(0))
                .map_err(|e| CommandProblem::Internal(e.to_string()))
        })
        .unwrap()
}

fn receipt_exists(target: &crate::command::TargetDatabase, command_id: &str) -> bool {
    target
        .with_conn(|conn| {
            conn.query_row(
                "SELECT 1 FROM command_receipt WHERE command_id=?1",
                [command_id],
                |_| Ok(()),
            )
            .map_err(|e| CommandProblem::Internal(e.to_string()))
        })
        .is_ok()
}

/// 写一个 operation(+intent+steps)fixture;steps 经 id 关联 receipt 保护。
#[allow(clippy::too_many_arguments)]
fn insert_operation(
    service: &Arc<ServiceStore>,
    handle: &str,
    command_id: &str,
    state: &str,
    intent_state: &str,
    updated_at: &str,
    step_ids: &[&str],
) {
    service
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO command_intent
                     (command_id, semantic_digest, target_store, aggregate, principal,
                      client_id, controller_epoch, root_epoch, state, created_at,
                      resolved_at, problem_code)
                 VALUES (?1, 'digest-gc', 'project:proj_gc', 'wf-gc', 'user', 'client', 1, NULL,
                         ?2, ?3, ?3, NULL)",
                params![command_id, intent_state, updated_at],
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            conn.execute(
                "INSERT INTO operation
                     (operation_handle, command_id, kind, state, saga_state, progress_json,
                      created_at, updated_at)
                 VALUES (?1, ?2, 'gc.kind', ?3, '', '{}', ?4, ?4)",
                params![handle, command_id, state, updated_at],
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            for (index, step_id) in step_ids.iter().enumerate() {
                conn.execute(
                    "INSERT INTO operation_step
                         (operation_handle, step_index, role, step_id, target_store, aggregate,
                          semantic_digest, expected_json, compensates, state, created_at, updated_at)
                     VALUES (?1, ?2, 'forward', ?3, 'project:proj_gc', 'wf-gc', 'digest-gc',
                             '[]', NULL, 'succeeded', ?4, ?4)",
                    params![handle, index, step_id, updated_at],
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            Ok(())
        })
        .unwrap();
}

fn counts(service: &Arc<ServiceStore>, table: &str) -> i64 {
    service
        .with_conn(|conn| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap()
}

/// 到期 terminal receipt 删除;未终结(unfinalized/failed)与年轻 receipt 保留。
#[test]
fn aged_terminal_receipts_deleted_others_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    insert_receipt(&target, "r-aged", "applied", Some(&days_ago(40)));
    insert_receipt(&target, "r-young", "applied", Some(&days_ago(1)));
    insert_receipt(&target, "r-open", "applied", None);
    insert_receipt(&target, "r-failed", "failed", Some(&days_ago(40)));

    let report = run_retention_gc(
        &service,
        std::slice::from_ref(&target),
        &RetentionLimits::default(),
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap();
    assert_eq!(
        report.receipts_deleted,
        vec![(target.store_key().to_string(), 1)]
    );
    assert!(!receipt_exists(&target, "r-aged"));
    assert!(receipt_exists(&target, "r-young"), "未到期不删");
    assert!(
        receipt_exists(&target, "r-open"),
        "finalized_at 为空(未终结)不删"
    );
    assert!(receipt_exists(&target, "r-failed"), "非终态 receipt 不删");
    assert_eq!(report.audit_deleted, 0);
}

/// 被 audit 引用的 receipt(带引号 token 匹配)绝不删除。
#[test]
fn audit_referenced_receipt_is_never_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    insert_receipt(
        &target,
        "018f0000-0000-7000-8000-0000000000a1",
        "applied",
        Some(&days_ago(90)),
    );
    service
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO audit (kind, summary_json, created_at)
                 VALUES ('installation_receipt',
                         '{\"command_id\":\"018f0000-0000-7000-8000-0000000000a1\",\"outcome\":\"ok\"}',
                         ?1)",
                [days_ago(1)],
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();
    // 子串不误伤:只包含 id 前缀的 audit 行不构成引用。
    insert_receipt(
        &target,
        "018f0000-0000-7000-8000-0000000000a2",
        "applied",
        Some(&days_ago(90)),
    );
    service
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO audit (kind, summary_json, created_at)
                 VALUES ('note', '{\"note\":\"prefix 018f0000-0000-7000-8000-0000000000a\"}', ?1)",
                [days_ago(1)],
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();

    let report = run_retention_gc(
        &service,
        std::slice::from_ref(&target),
        &RetentionLimits::default(),
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap();
    assert_eq!(report.receipts_deleted[0].1, 1);
    assert!(
        receipt_exists(&target, "018f0000-0000-7000-8000-0000000000a1"),
        "被 audit 引用的 receipt 不清理(§4.6)"
    );
    assert!(!receipt_exists(
        &target,
        "018f0000-0000-7000-8000-0000000000a2"
    ));
}

/// 未终结 operation 的 step receipt 受保护,即使已到期。
#[test]
fn open_operation_step_receipt_is_protected() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    let step_id = "018f0000-0000-7000-8000-0000000000b1";
    insert_receipt(&target, step_id, "applied", Some(&days_ago(120)));
    insert_operation(
        &service,
        "op_018f0000-0000-7000-8000-0000000000b1",
        "018f0000-0000-7000-8000-0000000000b2",
        "running",
        "reserved",
        &days_ago(120),
        &[step_id],
    );

    let report = run_retention_gc(
        &service,
        std::slice::from_ref(&target),
        &RetentionLimits::default(),
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap();
    assert!(report.receipts_deleted.is_empty(), "无一行可删");
    assert!(
        receipt_exists(&target, step_id),
        "未终结 operation 的 receipt 不清理"
    );
    assert_eq!(counts(&service, "operation"), 1, "活动 operation 不清理");
}

/// 行数上限:超限时删最旧终态 receipt,直到回到上限内。
#[test]
fn receipt_row_cap_deletes_oldest_terminal_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    // 10_005 行终态 receipt,全部在 7 天保留期内(秒级间隔,序号越大越旧),
    // 只触发行数上限轴(显式用 min 上限 10_000)。
    target
        .with_tx(|tx| {
            for index in 0..10_005i64 {
                let finalized =
                    (now() - chrono::Duration::try_seconds((index + 1) * 47).unwrap()).to_rfc3339();
                tx.execute(
                    "INSERT INTO command_receipt
                         (command_id, semantic_digest, aggregate_handle, result_revisions,
                          state, created_at, finalized_at)
                     VALUES (?1, 'digest-gc', 'wf-gc', '{}', 'applied', ?2, ?2)",
                    params![format!("018f0000-0000-7000-8000-{index:012x}"), finalized],
                )
                .map_err(|e| CommandProblem::Internal(e.to_string()))?;
            }
            Ok(())
        })
        .unwrap();
    let limits = RetentionLimits {
        receipt_max_rows_per_store: 10_000,
        receipt_retention_days: 7,
        ..Default::default()
    };
    let report = run_retention_gc(
        &service,
        std::slice::from_ref(&target),
        &limits,
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap();
    assert_eq!(report.receipts_deleted[0].1, 5, "恰好删到上限");
    assert_eq!(receipt_count(&target), 10_000);
    // finalized_at 最旧的 5 行(hours 最大,即 index 最大)被删。
    // 10004=0x2714(最旧)被删;9999=0x270f 是保留侧最老一行。
    assert!(!receipt_exists(
        &target,
        "018f0000-0000-7000-8000-000000002714"
    ));
    assert!(receipt_exists(
        &target,
        "018f0000-0000-7000-8000-00000000270f"
    ));
}

/// Operation GC:只删终态且到期;活动中的任何年龄都不删;step 级联清理。
#[test]
fn operations_gc_only_terminal_and_aged() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    insert_operation(
        &service,
        "op_aged_completed",
        "cmd_aged_completed",
        "completed",
        "applied",
        &days_ago(100),
        &["s1", "s2"],
    );
    insert_operation(
        &service,
        "op_aged_needs_you",
        "cmd_aged_needs_you",
        "needs_you",
        "failed",
        &days_ago(100),
        &["s3"],
    );
    insert_operation(
        &service,
        "op_aged_running",
        "cmd_aged_running",
        "running",
        "reserved",
        &days_ago(100),
        &["s4"],
    );
    insert_operation(
        &service,
        "op_young_completed",
        "cmd_young_completed",
        "completed",
        "applied",
        &days_ago(1),
        &["s5"],
    );

    let report = run_retention_gc(
        &service,
        &[],
        &RetentionLimits::default(),
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap();
    assert_eq!(report.operations_deleted, 2);
    assert_eq!(counts(&service, "operation"), 2);
    assert_eq!(
        counts(&service, "operation_step"),
        2,
        "step 随 operation 级联清理"
    );
    // 到期终态 operation 释放其 intent(FK 不再引用)。
    assert_eq!(report.intents_deleted, 2);
    for kept in ["op_aged_running", "op_young_completed"] {
        let state: String = service
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT state FROM operation WHERE operation_handle=?1",
                    [kept],
                    |row| row.get(0),
                )
                .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .unwrap();
        assert_eq!(
            state,
            if kept == "op_aged_running" {
                "running"
            } else {
                "completed"
            }
        );
    }
}

/// audit 引用的终态 Operation 即使已过 operation retention 也不删除。
#[test]
fn audit_referenced_operation_is_protected() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let handle = "op_audit_protected";
    insert_operation(
        &service,
        handle,
        "cmd_audit_protected",
        "completed",
        "applied",
        &days_ago(120),
        &["step_audit_protected"],
    );
    service
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO audit(kind, summary_json, created_at) VALUES (?1, ?2, ?3)",
                params![
                    "operation_provenance",
                    serde_json::json!({"operation_handle": handle}).to_string(),
                    now().to_rfc3339(),
                ],
            )
            .map_err(|error| anyhow::anyhow!(error))?;
            Ok(())
        })
        .unwrap();

    let report = run_retention_gc(
        &service,
        &[],
        &RetentionLimits::default(),
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap();
    assert_eq!(report.operations_deleted, 0);
    assert_eq!(report.intents_deleted, 0);
    assert_eq!(counts(&service, "operation"), 1);
    assert_eq!(counts(&service, "operation_step"), 1);
}

/// 活动 Workflow Run / replay lease 的调用方 pin 必须覆盖跨库 GC：被 pin
/// 的 receipt 与 Operation 即使到期也保留。
#[test]
fn active_and_replay_pins_protect_receipts_and_operations() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    insert_receipt(
        &target,
        "receipt_replay_pinned",
        "applied",
        Some(&days_ago(120)),
    );
    insert_operation(
        &service,
        "op_active_pinned",
        "cmd_active_pinned",
        "completed",
        "applied",
        &days_ago(120),
        &["step_active_pinned"],
    );
    let protection = GcProtection::for_contract(
        [
            "receipt_replay_pinned".to_string(),
            "cmd_active_pinned".to_string(),
        ],
        ["op_active_pinned".to_string()],
    );
    let report = run_retention_gc(
        &service,
        std::slice::from_ref(&target),
        &RetentionLimits::default(),
        &protection,
        now(),
    )
    .unwrap();
    assert_eq!(report.operations_deleted, 0);
    assert_eq!(report.intents_deleted, 0);
    assert!(report.receipts_deleted.is_empty());
    assert!(receipt_exists(&target, "receipt_replay_pinned"));
    assert_eq!(counts(&service, "operation"), 1);
}

/// intent GC:只删不被 operation 引用、不被 audit 引用的到期终态;
/// reserved 永不删。
#[test]
fn intents_gc_respects_references_and_state() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    // 到期终态、无引用 → 删。
    service
        .with_conn(|conn| {
            for (id, state) in [
                ("018f0000-0000-7000-8000-0000000000c1", "applied"),
                ("018f0000-0000-7000-8000-0000000000c2", "reserved"),
                ("018f0000-0000-7000-8000-0000000000c3", "applied"),
            ] {
                conn.execute(
                    "INSERT INTO command_intent
                         (command_id, semantic_digest, target_store, aggregate, principal,
                          client_id, controller_epoch, root_epoch, state, created_at, resolved_at)
                     VALUES (?1, 'd', 'project:proj_gc', 'wf', 'u', 'c', 1, NULL, ?2, ?3, ?3)",
                    params![id, state, days_ago(60)],
                )?;
            }
            Ok(())
        })
        .unwrap();
    // c3 被 audit 引用。
    service
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO audit (kind, summary_json, created_at)
                 VALUES ('ref', '{\"command_id\":\"018f0000-0000-7000-8000-0000000000c3\"}', ?1)",
                [days_ago(1)],
            )?;
            Ok(())
        })
        .unwrap();
    // c2(reserved)即使到期也不删;c1 删。
    let report = run_retention_gc(
        &service,
        &[],
        &RetentionLimits::default(),
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap();
    assert_eq!(report.intents_deleted, 1);
    assert_eq!(counts(&service, "command_intent"), 2);
}

/// audit 按 append-only 之外唯一的 GC 通道(§4.6)按天龄清理。
#[test]
fn audit_gc_by_age() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    service
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO audit (kind, summary_json, created_at)
                 VALUES ('old', '{}', ?1)",
                [days_ago(400)],
            )?;
            conn.execute(
                "INSERT INTO audit (kind, summary_json, created_at)
                 VALUES ('young', '{}', ?1)",
                [days_ago(10)],
            )?;
            Ok(())
        })
        .unwrap();
    let report = run_retention_gc(
        &service,
        &[],
        &RetentionLimits::default(),
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap();
    assert_eq!(report.audit_deleted, 1);
    assert_eq!(counts(&service, "audit"), 1);
}

/// 全部年轻/未终结时 GC 是 no-op。
#[test]
fn gc_is_noop_when_nothing_eligible() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let target = project_target(&tmp.path().join("workflow-v1.db"));
    insert_receipt(&target, "r-young", "applied", Some(&days_ago(1)));
    insert_operation(
        &service,
        "op_young",
        "cmd_young",
        "completed",
        "applied",
        &days_ago(1),
        &[],
    );
    service
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO audit (kind, summary_json, created_at) VALUES ('a', '{}', ?1)",
                [days_ago(1)],
            )?;
            Ok(())
        })
        .unwrap();
    let report = run_retention_gc(
        &service,
        &[target],
        &RetentionLimits::default(),
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap();
    assert_eq!(report, crate::reconcile::GcReport::default());
}

/// 越界 limits 拒绝执行 GC(装配层 fail fast,不静默钳制)。
#[test]
fn gc_rejects_out_of_range_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let service = saga_service(&tmp);
    let limits = RetentionLimits {
        receipt_retention_days: 6, // 低于 min=7
        ..Default::default()
    };
    let error = run_retention_gc(
        &service,
        &[],
        &limits,
        &GcProtection::confirmed_empty(),
        now(),
    )
    .unwrap_err();
    assert!(matches!(error, CommandProblem::Internal(_)));
}
