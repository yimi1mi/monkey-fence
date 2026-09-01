//! Projection seam:Snapshot/事件 envelope、进程内 event journal 与
//! 最小 subscription(canonical spec §5,L-PUBLISH)。
//!
//! T2a(Issue #23)只交付 facade tracer 所需的最小真实链路:
//! - 事件只在目标事务 commit + target receipt + outbox 之后、
//!   `published_at` 标记成功之后才对外可见(publication barrier);
//! - Snapshot 直接投影 Store 权威状态,journal 只携带 cursor,不建立
//!   第二状态机;
//! - 不可解释的 outbox 行 → fail-closed 旋转 stream_epoch,全部客户端
//!   resync;有界 journal/容量 fail-closed 属后续 journal ticket(§5.7),
//!   本模块不做容量驱逐。
//!
//! outbox `event_json` 是 #21 冻结的 store-local 形状
//! (`{type, aggregate, caused_by_command_id, projection}`);本模块在
//! publication 时把它编译为对外的 `mf.event.v1` envelope(§5.4)。

use crate::command::{CommandProblem, TargetDatabase};
use crate::handles::{AggregateKind, AggregateRef, ServerInstanceId, StreamEpoch, WorkflowHandle};
use crate::kernel::KernelProblem;
use crate::limits::{JOURNAL_MAX_BYTES_DEFAULT, JOURNAL_MAX_EVENTS_DEFAULT};
use parking_lot::Mutex;
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;

pub const EVENT_SCHEMA: &str = "mf.event.v1";
pub const SNAPSHOT_SCHEMA: &str = "mf.snapshot.v1";

fn serialize_u64_decimal<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

/// Project Workflow 的 semantic/presentation 双 revision 向量(§5.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RevisionVector {
    #[serde(serialize_with = "serialize_u64_decimal")]
    pub semantic_revision: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    pub presentation_revision: u64,
}

/// 事件流游标:Snapshot 与 resume 都携带(epoch, through_seq)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventCursor {
    pub stream_epoch: StreamEpoch,
    #[serde(serialize_with = "serialize_u64_decimal")]
    pub through_seq: u64,
}

/// `mf.event.v1` envelope(§5.4):typed delta 携带 base/aggregate 双
/// revision;未知 delta_type 或 revision 不连续时客户端必须 resync。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventEnvelope {
    pub schema: &'static str,
    pub stream_epoch: StreamEpoch,
    #[serde(serialize_with = "serialize_u64_decimal")]
    pub seq: u64,
    pub occurred_at: String,
    pub aggregate: AggregateRef,
    pub base_revision: RevisionVector,
    pub aggregate_revision: RevisionVector,
    pub caused_by_command_id: Option<String>,
    /// 事件类型(如 `workflow.rename`),不含 store-local `.applied` 后缀。
    #[serde(rename = "type")]
    pub event_type: String,
    pub projection_critical: bool,
    /// `{"mode":"typed_delta","delta_type":...,"data":...}`;禁止 JSON Patch。
    pub projection: Value,
}

/// Snapshot 查询(T2a 只冻结 workflow 单资源;Workspace 快照属后续)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotQuery {
    Workflow {
        project: crate::handles::ProjectStoreHandle,
        workflow: WorkflowHandle,
    },
}

/// Snapshot data:Store 权威状态的只读投影(不缓存、不推演)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkflowSnapshotData {
    pub workflow: WorkflowHandle,
    pub name: String,
    pub revisions: RevisionVector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SnapshotData {
    Workflow(WorkflowSnapshotData),
}

/// `mf.snapshot.v1`(§7.3):cursor 与事件流同 barrier 读取。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotEnvelope {
    pub schema: &'static str,
    pub server_instance_id: ServerInstanceId,
    pub cursor: EventCursor,
    pub data: SnapshotData,
}

/// 最小 projection subscription(进程内同步拉取;WS fan-out 属 T7)。
/// epoch 旋转后 poll 返回 `resync_required`,客户端重取 Snapshot。
pub struct EventSubscription {
    journal: Arc<EventJournal>,
    stream_epoch: StreamEpoch,
    last_seq: u64,
}

impl std::fmt::Debug for EventSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSubscription")
            .field("stream_epoch", &self.stream_epoch)
            .field("last_seq", &self.last_seq)
            .finish_non_exhaustive()
    }
}

impl EventSubscription {
    /// 非阻塞拉取 `seq > last_seq` 的新事件(按 seq 升序)。
    pub fn poll(&mut self) -> Result<Vec<EventEnvelope>, KernelProblem> {
        let inner = self.journal.inner.lock();
        if inner.stream_epoch != self.stream_epoch {
            return Err(KernelProblem::ResyncRequired);
        }
        let mut events = Vec::new();
        for entry in &inner.entries {
            if entry.seq > self.last_seq {
                events.push(entry.clone());
            }
        }
        if let Some(last) = events.last() {
            self.last_seq = last.seq;
        }
        Ok(events)
    }

    pub fn cursor(&self) -> EventCursor {
        EventCursor {
            stream_epoch: self.stream_epoch.clone(),
            through_seq: self.last_seq,
        }
    }
}

/// 进程内事件 journal:seq 分配、append 与 fan-out 的串行 barrier。
/// 权威状态在 Store;journal 只是有界恢复流(本 ticket 不做容量驱逐)。
pub(crate) struct EventJournal {
    server_instance_id: ServerInstanceId,
    max_events: usize,
    max_bytes: usize,
    inner: Mutex<JournalInner>,
}

struct JournalInner {
    stream_epoch: StreamEpoch,
    next_seq: u64,
    bytes: usize,
    entries: VecDeque<EventEnvelope>,
}

impl EventJournal {
    pub(crate) fn new() -> Self {
        Self::with_limits(JOURNAL_MAX_EVENTS_DEFAULT, JOURNAL_MAX_BYTES_DEFAULT)
    }

    fn with_limits(max_events: usize, max_bytes: usize) -> Self {
        Self {
            server_instance_id: ServerInstanceId::new(),
            max_events,
            max_bytes,
            inner: Mutex::new(JournalInner {
                stream_epoch: StreamEpoch::new(),
                next_seq: 1,
                bytes: 0,
                entries: VecDeque::new(),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(max_events: usize, max_bytes: usize) -> Self {
        Self::with_limits(max_events, max_bytes)
    }

    pub(crate) fn server_instance_id(&self) -> &ServerInstanceId {
        &self.server_instance_id
    }

    /// barrier 上的当前游标(through_seq = 已 append 的最大 seq;空流为 0)。
    pub(crate) fn cursor(&self) -> EventCursor {
        let inner = self.inner.lock();
        EventCursor {
            stream_epoch: inner.stream_epoch.clone(),
            through_seq: inner.next_seq.saturating_sub(1),
        }
    }

    pub(crate) fn subscribe(
        self: &Arc<Self>,
        cursor: &EventCursor,
    ) -> Result<EventSubscription, KernelProblem> {
        let inner = self.inner.lock();
        if inner.stream_epoch != cursor.stream_epoch {
            return Err(KernelProblem::ResyncRequired);
        }
        if cursor.through_seq > inner.next_seq.saturating_sub(1) {
            return Err(KernelProblem::ResyncRequired);
        }
        Ok(EventSubscription {
            journal: self.clone(),
            stream_epoch: inner.stream_epoch.clone(),
            last_seq: cursor.through_seq,
        })
    }

    /// L-PUBLISH:把目标 Store 全部未发布 outbox 行编译为 `mf.event.v1`
    /// 并对外可见。顺序固定为「journal 锁内读取 → 组装 → 同事务标记
    /// `published_at` → 追加对外可见队列」:标记失败则不对外可见,下次
    /// dispatch/reconcile 重试;不可解释的 outbox 行旋转 epoch(fail-closed)。
    pub(crate) fn publish_pending(&self, target: &TargetDatabase) -> Result<(), KernelProblem> {
        let mut inner = self.inner.lock();
        // 先读聚合水位，不把任意 backlog 全量分配进内存。
        let (pending_count, pending_bytes): (i64, i64) = target
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(length(CAST(event_json AS BLOB))), 0)
                     FROM projection_outbox WHERE published_at IS NULL",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|error| CommandProblem::Internal(error.to_string()))
            })
            .map_err(|error| {
                rotate_epoch(&mut inner);
                log::error!("读取 projection_outbox 水位失败:{error}");
                KernelProblem::ResyncRequired
            })?;
        let pending_count = usize::try_from(pending_count).unwrap_or(usize::MAX);
        let pending_bytes = usize::try_from(pending_bytes).unwrap_or(usize::MAX);
        if inner.entries.len().saturating_add(pending_count) > self.max_events
            || inner.bytes.saturating_add(pending_bytes) > self.max_bytes
        {
            rotate_epoch(&mut inner);
            mark_all_pending_reconciled(target, &chrono::Utc::now().to_rfc3339())?;
            return Err(KernelProblem::ResyncRequired);
        }
        let rows: Vec<(i64, String)> = target
            .with_conn(|conn| {
                let read =
                    || -> Result<Vec<(i64, String)>, rusqlite::Error> {
                        let mut stmt = conn.prepare(
                            "SELECT outbox_id, event_json FROM projection_outbox
                         WHERE published_at IS NULL ORDER BY outbox_id LIMIT ?1",
                        )?;
                        let rows = stmt
                            .query_map(
                                [i64::try_from(self.max_events.saturating_add(1))
                                    .unwrap_or(i64::MAX)],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )?
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok(rows)
                    };
                read().map_err(|error| CommandProblem::Internal(error.to_string()))
            })
            .map_err(|error| {
                rotate_epoch(&mut inner);
                log::error!("读取 projection_outbox 失败，旋转 stream epoch:{error}");
                KernelProblem::ResyncRequired
            })?;
        if rows.is_empty() {
            return Ok(());
        }
        let occurred_at = chrono::Utc::now().to_rfc3339();
        let mut staged: Vec<(i64, EventEnvelope)> = Vec::with_capacity(rows.len());
        for (outbox_id, event_json) in rows {
            let seq = inner.next_seq;
            match compile_event(&inner.stream_epoch, seq, &occurred_at, &event_json) {
                Ok(event) => {
                    inner.next_seq += 1;
                    staged.push((outbox_id, event));
                }
                Err(error) => {
                    rotate_epoch(&mut inner);
                    log::error!("projection publication 失败，旋转 stream epoch:{error}");
                    return Err(KernelProblem::ResyncRequired);
                }
            }
        }
        // 先在目标 Store 同事务收口 published_at(权威链),成功后才可见。
        let staged_bytes = staged.iter().fold(0usize, |total, (_, event)| {
            total.saturating_add(
                serde_json::to_vec(event)
                    .map(|bytes| bytes.len())
                    .unwrap_or(self.max_bytes),
            )
        });
        if inner.bytes.saturating_add(staged_bytes) > self.max_bytes {
            rotate_epoch(&mut inner);
            mark_all_pending_reconciled(target, &occurred_at)?;
            return Err(KernelProblem::ResyncRequired);
        }
        let ids: Vec<i64> = staged.iter().map(|(id, _)| *id).collect();
        let marked = target
            .with_tx(|tx| {
                let mark = || -> Result<(), rusqlite::Error> {
                    for id in &ids {
                        tx.execute(
                            "UPDATE projection_outbox SET published_at = ?2
                             WHERE outbox_id = ?1 AND published_at IS NULL",
                            params![id, &occurred_at],
                        )?;
                    }
                    Ok(())
                };
                mark().map_err(|error| CommandProblem::Internal(error.to_string()))
            })
            .map_err(|error| KernelProblem::Internal(format!("标记 published_at 失败:{error}")));
        if let Err(error) = marked {
            rotate_epoch(&mut inner);
            log::error!("标记 published_at 失败，旋转 stream epoch:{error}");
            return Err(KernelProblem::ResyncRequired);
        }
        inner.bytes = inner.bytes.saturating_add(staged_bytes);
        for (_, event) in staged {
            inner.entries.push_back(event);
        }
        Ok(())
    }
}

fn rotate_epoch(inner: &mut JournalInner) {
    inner.stream_epoch = StreamEpoch::new();
    inner.next_seq = 1;
    inner.bytes = 0;
    inner.entries.clear();
}

fn mark_all_pending_reconciled(
    target: &TargetDatabase,
    occurred_at: &str,
) -> Result<(), KernelProblem> {
    let reconciled = format!("reconciled:{occurred_at}");
    target
        .with_tx(|tx| {
            tx.execute(
                "UPDATE projection_outbox SET published_at=?1 WHERE published_at IS NULL",
                [&reconciled],
            )
            .map_err(|error| CommandProblem::Internal(error.to_string()))?;
            Ok(())
        })
        .map_err(|error| {
            log::error!("journal overflow reconcile 失败:{error}");
            KernelProblem::ResyncRequired
        })
}

/// store-local outbox 行 → `mf.event.v1` envelope。
fn compile_event(
    stream_epoch: &StreamEpoch,
    seq: u64,
    occurred_at: &str,
    event_json: &str,
) -> Result<EventEnvelope, String> {
    let value: Value = serde_json::from_str(event_json).map_err(|e| e.to_string())?;
    let store_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "outbox 事件缺少 type".to_string())?;
    let event_type = store_type
        .strip_suffix(".applied")
        .unwrap_or(store_type)
        .to_string();
    let aggregate_kind = value
        .pointer("/aggregate/kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "outbox 事件缺少 aggregate.kind".to_string())?;
    let aggregate_handle = value
        .pointer("/aggregate/handle")
        .and_then(Value::as_str)
        .ok_or_else(|| "outbox 事件缺少 aggregate.handle".to_string())?;
    let aggregate = AggregateRef::new(
        aggregate_kind_from_str(aggregate_kind)?,
        aggregate_handle.to_string(),
    )
    .map_err(|e| e.to_string())?;
    let caused_by_command_id = value
        .get("caused_by_command_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let projection = value
        .get("projection")
        .cloned()
        .ok_or_else(|| "outbox 事件缺少 projection".to_string())?;
    let base_revision = revision_vector_at(&projection, "base_revision")?;
    let aggregate_revision = revision_vector_at(&projection, "aggregate_revision")?;
    let delta = projection
        .get("delta")
        .cloned()
        .ok_or_else(|| "outbox 事件缺少 projection.delta".to_string())?;
    Ok(EventEnvelope {
        schema: EVENT_SCHEMA,
        stream_epoch: stream_epoch.clone(),
        seq,
        occurred_at: occurred_at.to_string(),
        aggregate,
        base_revision,
        aggregate_revision,
        caused_by_command_id,
        event_type,
        // typed delta 属投影关键:客户端无法应用未知 delta 时必须 resync,
        // 不得静默显示旧状态(§5.4)。
        projection_critical: true,
        projection: delta,
    })
}

fn revision_vector_at(projection: &Value, field: &str) -> Result<RevisionVector, String> {
    let value = projection
        .get(field)
        .ok_or_else(|| format!("outbox projection 缺少 {field}"))?;
    let semantic_revision = value
        .get("semantic_revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{field}.semantic_revision 缺失或非 u64"))?;
    let presentation_revision = value
        .get("presentation_revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{field}.presentation_revision 缺失或非 u64"))?;
    Ok(RevisionVector {
        semantic_revision,
        presentation_revision,
    })
}

fn aggregate_kind_from_str(value: &str) -> Result<AggregateKind, String> {
    match value {
        "project" => Ok(AggregateKind::Project),
        "project_workflow" => Ok(AggregateKind::ProjectWorkflow),
        "workflow_run" => Ok(AggregateKind::WorkflowRun),
        "step" => Ok(AggregateKind::Step),
        "agent_session" => Ok(AggregateKind::AgentSession),
        "agent_instance" => Ok(AggregateKind::AgentInstance),
        "provider_profile" => Ok(AggregateKind::ProviderProfile),
        "installation" => Ok(AggregateKind::Installation),
        "root_state" => Ok(AggregateKind::RootState),
        other => Err(format!("未知 aggregate.kind:{other}")),
    }
}
