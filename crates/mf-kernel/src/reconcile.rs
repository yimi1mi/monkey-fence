//! 本模块在 T2 CoreKernel facade 落位前是 dark seam(仅契约测试使用)。

#![allow(dead_code)] // Dark seam until the T2 CoreKernel tracer.

//! Core 重启 reconcile 与 retention/GC(canonical spec §4.1 步骤 4/§4.6/
//! §5.6/附录 B)。
//!
//! 重启矩阵(fault matrix):
//!
//! - step/command intent 无 target receipt:只终结/标记失败(`revoked`,
//!   epoch 已失效),绝不重做业务写;
//! - 已有 target receipt:只补 service 终态(step `succeeded`/intent
//!   `applied`)与 outbox 收尾,不重做 target effect;
//! - 只 reconcile 新协议 intent/operation(operation_step 行即新协议凭
//!   证);legacy/外部行不重放、不补造陈旧 delta;
//! - 全程幂等,可重复 crash/restart(每个阶段独立事务,状态守卫收敛)。
//!
//! outbox reconciled 标记(§5.6):旧 epoch 未发布的 `projection_outbox`
//! 行以 `published_at='reconciled:<rfc3339>'` 收尾——不向新 epoch 重放
//! Snapshot 已包含的陈旧 delta,也不把它们伪装成正常 publication 时间戳。
//! Project v7 / Catalog v2 的 schema 已冻结(只有 `published_at` 列),
//! 因此前缀标记是唯一可区分且保持 `published_at IS NULL` 深度口径的写法。
//!
//! retention/GC(§4.6/附录 A4):按天龄与行数上限清理终态 receipt /
//! operation / audit / intent;未终结对象与被 audit 引用的 receipt 永不
//! 删除。

use crate::command::{finish_intent_tx, intent_tx, CommandProblem, IntentState, TargetDatabase};
use crate::handles::CommandId;
use crate::limits::RetentionLimits;
use crate::operation::{
    mark_reconciling, open_operations, reconcile_operation, OperationHandle, OperationOutcome,
    StepId,
};
use crate::project_registry::ServiceStore;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::sync::Arc;

/// `published_at` 的 reconciled 标记前缀(§5.6)。
pub const OUTBOX_RECONCILED_PREFIX: &str = "reconciled:";

/// 一次启动 reconcile 的结果摘要(可观测性:reconcile 数,§4.7)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 无 receipt 被终结 `revoked` 的单命令 intent 数。
    pub intents_revoked: usize,
    /// 依 receipt 补 `applied` 的单命令 intent 数。
    pub intents_applied: usize,
    /// 目标 store 未打开等原因被跳过、保持原状的 intent 数。
    pub intents_skipped: usize,
    /// reconcile 终结为 completed 的 operation 数(含完整回滚)。
    pub operations_completed: usize,
    /// reconcile 终结为 needs_you 的 operation 数。
    pub operations_needs_you: usize,
    /// acceptance target receipt 从未提交、因此从未对外返回 202 的 operation 数。
    pub operations_not_accepted: usize,
    /// legacy/无 step 行被跳过(不重放、不补造)的 operation 数。
    pub operations_skipped: usize,
    /// 各 store 被标记 reconciled 的 outbox 行数。
    pub outbox_reconciled: Vec<(String, usize)>,
}

/// 一次 retention GC 的删除摘要。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub operations_deleted: usize,
    pub intents_deleted: usize,
    /// 各 store 删除的 terminal receipt 数(按天龄 + 行数上限)。
    pub receipts_deleted: Vec<(String, usize)>,
    pub audit_deleted: usize,
}

/// 调用方从活动 Workflow Run、显式 replay lease 与安装 pin 投影出的 GC
/// 保护集。T1 尚未装配这些领域 reader，但 GC API 必须显式接收保护事实，
/// 不能在不知道 active/pin 状态时删除对应 receipt/Operation。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcProtection {
    receipt_ids: HashSet<String>,
    operation_handles: HashSet<String>,
}

impl GcProtection {
    /// 调用方已完成 active/pin reader，确认本轮没有保护对象。刻意不实现
    /// Default，避免“reader 尚未装配”被误当成空保护集而 fail-open。
    pub fn confirmed_empty() -> Self {
        Self {
            receipt_ids: HashSet::new(),
            operation_handles: HashSet::new(),
        }
    }

    pub fn protect_command(mut self, command_id: &CommandId) -> Self {
        self.receipt_ids.insert(command_id.as_str().to_string());
        self
    }

    pub fn protect_step(mut self, step_id: &StepId) -> Self {
        self.receipt_ids.insert(step_id.as_str().to_string());
        self
    }

    pub fn protect_operation(mut self, handle: &OperationHandle) -> Self {
        self.operation_handles.insert(handle.as_str().to_string());
        self
    }

    #[cfg(test)]
    pub(crate) fn for_contract(
        receipt_ids: impl IntoIterator<Item = String>,
        operation_handles: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            receipt_ids: receipt_ids.into_iter().collect(),
            operation_handles: operation_handles.into_iter().collect(),
        }
    }
}

/// 启动 reconcile:单命令 intent 终结 → 未终结 operation 推入
/// `reconciling` → 逐个依 receipt 终态化 → 旧 epoch outbox 标 reconciled。
/// 阶段间可崩溃;重复调用幂等收敛。
pub(crate) fn reconcile_startup(
    service: &Arc<ServiceStore>,
    targets: &[TargetDatabase],
    now: DateTime<Utc>,
) -> Result<ReconcileReport, CommandProblem> {
    let mut report = ReconcileReport::default();
    let _gate = service.command_gate();
    reconcile_intents(service, targets, &mut report)?;
    mark_reconciling(service).map_err(op_problem)?;
    reconcile_operations(service, targets, &mut report)?;
    for target in targets {
        let marked = mark_outbox_reconciled(target, now)?;
        if marked > 0 {
            report
                .outbox_reconciled
                .push((target.store_key().to_string(), marked));
        }
    }
    Ok(report)
}

/// 只处理不被 operation 引用的单命令 intent(§4.1 步骤 4):
/// reserved + 无 receipt → revoked;reserved + receipt → applied;
/// receipt 身份不符 → fail-closed 保持 reserved(计入 skipped)。
fn reconcile_intents(
    service: &Arc<ServiceStore>,
    targets: &[TargetDatabase],
    report: &mut ReconcileReport,
) -> Result<(), CommandProblem> {
    let reserved: Vec<(String, String, String)> = service
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT command_id, target_store, aggregate FROM command_intent
                     WHERE state='reserved'
                       AND NOT EXISTS (
                           SELECT 1 FROM operation o WHERE o.command_id = command_intent.command_id
                       )
                     ORDER BY created_at, command_id",
                )
                .map_err(anyhow::Error::new)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(anyhow::Error::new)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(anyhow::Error::new)?;
            Ok(rows)
        })
        .map_err(command_from_anyhow)?;
    for (command_id, store_key, _aggregate) in reserved {
        let Some(target) = targets.iter().find(|t| t.store_key() == store_key) else {
            report.intents_skipped += 1;
            continue;
        };
        let receipt: Option<(String, String, String, Option<String>)> = target
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT semantic_digest, aggregate_handle, state, finalized_at
                     FROM command_receipt
                     WHERE command_id=?1",
                    [&command_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .optional()
                .map_err(internal)
            })
            .map_err(internal)?;
        let has_receipt = receipt.is_some();
        let apply = match receipt {
            Some((digest, aggregate_handle, state, finalized_at)) => service
                .with_conn(|conn| {
                    let intent = intent_tx(conn, &command_id)?
                        .ok_or_else(|| anyhow::anyhow!("command intent 消失:{command_id}"))?;
                    anyhow::ensure!(
                        intent.state == IntentState::Reserved,
                        "intent {command_id} 在 reconcile 期间离开 reserved"
                    );
                    Ok(intent.semantic_digest == digest
                        && intent.aggregate_handle == aggregate_handle
                        && state == "applied"
                        && finalized_at.is_some())
                })
                .map_err(|error| CommandProblem::Internal(format!("{error:#}")))?,
            None => false,
        };
        if has_receipt && !apply {
            // receipt 身份不符:fail-closed,保持 reserved 供诊断,
            // 绝不重做业务写、绝不误标 applied。
            report.intents_skipped += 1;
            continue;
        }
        service
            .with_tx(|tx| {
                let intent = intent_tx(tx, &command_id)
                    .map_err(anyhow::Error::new)?
                    .ok_or_else(|| anyhow::anyhow!("command intent 消失:{command_id}"))?;
                anyhow::ensure!(
                    intent.state == IntentState::Reserved,
                    "intent {} 在 reconcile 期间进入非 reserved 状态",
                    command_id
                );
                if apply {
                    finish_intent_tx(tx, &command_id, IntentState::Applied, None)
                        .map_err(anyhow::Error::new)?;
                } else {
                    finish_intent_tx(
                        tx,
                        &command_id,
                        IntentState::Revoked,
                        Some(CommandProblem::ControllerLeaseExpired.code()),
                    )
                    .map_err(anyhow::Error::new)?;
                }
                Ok(())
            })
            .map_err(command_from_anyhow)?;
        if apply {
            report.intents_applied += 1;
        } else {
            report.intents_revoked += 1;
        }
    }
    Ok(())
}

/// 逐个终结 reconciling operation(每 op 一个事务,部分进度可崩溃续跑)。
fn reconcile_operations(
    service: &Arc<ServiceStore>,
    targets: &[TargetDatabase],
    report: &mut ReconcileReport,
) -> Result<(), CommandProblem> {
    for handle in open_operations(service).map_err(op_problem)? {
        match reconcile_operation(service, &handle, targets).map_err(op_problem)? {
            Some(OperationOutcome::Completed { .. }) => report.operations_completed += 1,
            Some(OperationOutcome::NeedsYou { .. }) => report.operations_needs_you += 1,
            Some(OperationOutcome::NotAccepted { .. }) => report.operations_not_accepted += 1,
            None => {
                // legacy/外部行:无 operation_step 凭证,不重放、不补造。
                report.operations_skipped += 1;
            }
        }
    }
    Ok(())
}

/// 旧 epoch outbox 收尾:未发布行标记 `reconciled:<now>`(§5.6)。
/// 幂等:已标记/已发布行不满足 `IS NULL`,不会重复处理。
pub(crate) fn mark_outbox_reconciled(
    target: &TargetDatabase,
    now: DateTime<Utc>,
) -> Result<usize, CommandProblem> {
    let mark = format!("{OUTBOX_RECONCILED_PREFIX}{}", now.to_rfc3339());
    target.with_conn(|conn| {
        conn.execute(
            "UPDATE projection_outbox SET published_at=?1 WHERE published_at IS NULL",
            [&mark],
        )
        .map_err(internal)
    })
}

/// `published_at` 是否为 reconciled 标记(而非正常 publication 时间戳)。
pub fn is_reconciled_mark(published_at: &str) -> bool {
    published_at.starts_with(OUTBOX_RECONCILED_PREFIX)
}

// ───────────────────────────── retention GC ─────────────────────────────

/// 按 §4.6/附录 A4 执行一轮 GC。顺序:终态 operation(级联 step)→
/// 终态 intent → 各 store terminal receipt(天龄 + 行数上限)→ audit。
/// 未终结对象与被 audit 引用的 receipt 永不删除;重复执行幂等。
/// 与 dispatch/accept/reconcile 同一 command_gate 串行,消除与在途
/// L-CMD 的候选集竞态。
pub(crate) fn run_retention_gc(
    service: &Arc<ServiceStore>,
    targets: &[TargetDatabase],
    limits: &RetentionLimits,
    protection: &GcProtection,
    now: DateTime<Utc>,
) -> Result<GcReport, CommandProblem> {
    limits
        .validate()
        .map_err(|error| CommandProblem::Internal(error.to_string()))?;
    let _gate = service.command_gate();
    let mut report = GcReport::default();
    let receipt_cutoff = cutoff(&now, limits.receipt_retention_days)?;
    let operation_cutoff = cutoff(&now, limits.operation_retention_days)?;
    let audit_cutoff = cutoff(&now, limits.audit_retention_days)?;

    report.operations_deleted = gc_operations(service, &operation_cutoff, protection)?;
    report.intents_deleted = gc_intents(service, &receipt_cutoff, protection)?;
    for target in targets {
        let deleted = gc_receipts(
            service,
            target,
            &receipt_cutoff,
            limits.receipt_max_rows_per_store,
            protection,
        )?;
        if deleted > 0 {
            report
                .receipts_deleted
                .push((target.store_key().to_string(), deleted));
        }
    }
    report.audit_deleted = service
        .with_conn(|conn| {
            conn.execute("DELETE FROM audit WHERE created_at < ?1", [audit_cutoff])
                .map_err(anyhow::Error::new)
        })
        .map_err(command_from_anyhow)?;
    Ok(report)
}

fn cutoff(now: &DateTime<Utc>, days: u64) -> Result<String, CommandProblem> {
    let days = chrono::Duration::try_days(
        i64::try_from(days).map_err(|e| CommandProblem::Internal(e.to_string()))?,
    )
    .ok_or_else(|| CommandProblem::Internal("retention days 溢出".into()))?;
    Ok((*now - days).to_rfc3339())
}

/// 终态 operation 到期删除;被 audit 以 operation handle 或 initiating
/// command id 引用的行保留。`operation_step` 经 FK 级联清理(其 receipt
/// 是否可删由 receipt GC 依自身天龄/引用另行判定)。
fn gc_operations(
    service: &Arc<ServiceStore>,
    cutoff: &str,
    protection: &GcProtection,
) -> Result<usize, CommandProblem> {
    let candidates: Vec<(String, String)> = service
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT operation_handle, command_id FROM operation
                     WHERE state IN ('completed','needs_you') AND updated_at < ?1
                     ORDER BY updated_at, operation_handle",
                )
                .map_err(anyhow::Error::new)?;
            let rows = stmt
                .query_map([cutoff], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(anyhow::Error::new)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(anyhow::Error::new)?;
            Ok(rows)
        })
        .map_err(command_from_anyhow)?;
    let mut deleted = 0usize;
    for (handle, command_id) in candidates {
        if protection.operation_handles.contains(&handle)
            || protection.receipt_ids.contains(&command_id)
        {
            continue;
        }
        if audit_references(service, &handle)? || audit_references(service, &command_id)? {
            continue;
        }
        deleted += service
            .with_conn(|conn| {
                conn.execute(
                    "DELETE FROM operation
                     WHERE operation_handle=?1 AND state IN ('completed','needs_you')",
                    [&handle],
                )
                .map_err(anyhow::Error::new)
            })
            .map_err(command_from_anyhow)?;
    }
    Ok(deleted)
}

/// 终态 intent 到期删除;仍被 operation 引用(FK)的不动。
fn gc_intents(
    service: &Arc<ServiceStore>,
    cutoff: &str,
    protection: &GcProtection,
) -> Result<usize, CommandProblem> {
    let candidates: Vec<String> = service
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT command_id FROM command_intent
                     WHERE state IN ('applied','failed','cancelled','revoked')
                       AND resolved_at IS NOT NULL AND resolved_at < ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM operation o WHERE o.command_id = command_intent.command_id
                       )
                     ORDER BY resolved_at, command_id",
                )
                .map_err(anyhow::Error::new)?;
            let rows = stmt
                .query_map([cutoff], |row| row.get::<_, String>(0))
                .map_err(anyhow::Error::new)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(anyhow::Error::new)?;
            Ok(rows)
        })
        .map_err(command_from_anyhow)?;
    let mut deleted = 0usize;
    for command_id in candidates {
        if protection.receipt_ids.contains(&command_id) {
            continue;
        }
        if audit_references(service, &command_id)? {
            continue;
        }
        deleted += service
            .with_conn(|conn| {
                conn.execute(
                    "DELETE FROM command_intent
                     WHERE command_id=?1
                       AND NOT EXISTS (SELECT 1 FROM operation o WHERE o.command_id=?1)",
                    [&command_id],
                )
                .map_err(anyhow::Error::new)
            })
            .map_err(command_from_anyhow)?;
    }
    Ok(deleted)
}

/// terminal receipt 清理:天龄超限删除;再按行数上限删最旧终态。
/// 绝不删除:未终结 receipt(state != 'applied' 或 finalized_at NULL)、
/// 未终结 operation 的 step receipt、被 audit 引用的 receipt。
fn gc_receipts(
    service: &Arc<ServiceStore>,
    target: &TargetDatabase,
    cutoff: &str,
    max_rows: u64,
    protection: &GcProtection,
) -> Result<usize, CommandProblem> {
    let mut protected = protected_step_ids(service)?;
    protected.extend(protection.receipt_ids.iter().cloned());
    let mut deleted = 0usize;
    // 天龄轴:到期终态先删(受保护者保留)。
    let aged: Vec<String> = target
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT command_id FROM command_receipt
                     WHERE state='applied' AND finalized_at IS NOT NULL AND finalized_at < ?1
                     ORDER BY finalized_at, rowid",
                )
                .map_err(internal)?;
            let rows = stmt
                .query_map([cutoff], |row| row.get::<_, String>(0))
                .map_err(internal)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(internal)?;
            Ok(rows)
        })
        .map_err(internal)?;
    for command_id in aged {
        if protected.contains(&command_id) || audit_references(service, &command_id)? {
            continue;
        }
        deleted += delete_receipt(target, &command_id)?;
    }
    // 行数轴:总量超上限时删最旧终态,直到回到上限内。
    let total: i64 = target
        .with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM command_receipt", [], |row| row.get(0))
                .map_err(internal)
        })
        .map_err(internal)?;
    let max_rows = i64::try_from(max_rows).map_err(|e| CommandProblem::Internal(e.to_string()))?;
    if total <= max_rows {
        return Ok(deleted);
    }
    let excess = total - max_rows;
    let oldest: Vec<String> = target
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT command_id FROM command_receipt
                     WHERE state='applied' AND finalized_at IS NOT NULL
                     ORDER BY finalized_at, rowid",
                )
                .map_err(internal)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(internal)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(internal)?;
            Ok(rows)
        })
        .map_err(internal)?;
    let mut removed = 0i64;
    for command_id in oldest {
        if removed >= excess {
            break;
        }
        if protected.contains(&command_id) || audit_references(service, &command_id)? {
            continue;
        }
        removed += i64::try_from(delete_receipt(target, &command_id)?)
            .map_err(|e| CommandProblem::Internal(e.to_string()))?;
    }
    Ok(deleted + usize::try_from(removed).map_err(|e| CommandProblem::Internal(e.to_string()))?)
}

fn delete_receipt(target: &TargetDatabase, command_id: &str) -> Result<usize, CommandProblem> {
    target.with_conn(|conn| {
        conn.execute(
            "DELETE FROM command_receipt WHERE command_id=?1 AND state='applied'
             AND finalized_at IS NOT NULL",
            [command_id],
        )
        .map_err(internal)
    })
}

/// 未终结 operation 的 acceptance command id 与全部 step id(receipt
/// 保护集,§4.6)。
fn protected_step_ids(service: &Arc<ServiceStore>) -> Result<HashSet<String>, CommandProblem> {
    service
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT s.step_id FROM operation_step s
                      JOIN operation o ON o.operation_handle = s.operation_handle
                      WHERE o.state NOT IN ('completed','needs_you')
                      UNION
                      SELECT o.command_id FROM operation o
                      WHERE o.state NOT IN ('completed','needs_you')",
                )
                .map_err(anyhow::Error::new)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(anyhow::Error::new)?
                .collect::<Result<HashSet<_>, _>>()
                .map_err(anyhow::Error::new)?;
            Ok(rows)
        })
        .map_err(command_from_anyhow)
}

/// audit 对某 command/step id 的引用判定:audit summary 是 canonical JSON,
/// 引用形如 `"command_id":"<uuid>"`;以带引号 token 精确匹配,不做会误伤
/// 子串的裸 LIKE。
fn audit_references(service: &Arc<ServiceStore>, command_id: &str) -> Result<bool, CommandProblem> {
    service
        .with_conn(|conn| audit_references_conn(conn, command_id).map_err(anyhow::Error::new))
        .map_err(command_from_anyhow)
}

fn audit_references_conn(conn: &Connection, command_id: &str) -> Result<bool, rusqlite::Error> {
    let quoted = format!("\"{command_id}\"");
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM audit WHERE instr(summary_json, ?1) > 0",
        params![quoted],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn op_problem(problem: crate::operation::OperationProblem) -> CommandProblem {
    CommandProblem::Internal(problem.to_string())
}

fn command_from_anyhow(error: anyhow::Error) -> CommandProblem {
    error
        .downcast_ref::<CommandProblem>()
        .cloned()
        .unwrap_or_else(|| CommandProblem::Internal(format!("{error:#}")))
}

fn internal(error: impl std::fmt::Display) -> CommandProblem {
    CommandProblem::Internal(error.to_string())
}
