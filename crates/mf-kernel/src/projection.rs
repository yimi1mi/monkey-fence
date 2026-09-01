//! Projection seam:Snapshot/事件 envelope、进程内 event journal 与
//! 最小 subscription(canonical spec §5,L-PUBLISH)。
//!
//! T2a(Issue #23)建立首条 facade tracer；T2b(Issue #24)把它深化为：
//! - 事件只在目标事务 commit + target receipt + outbox 之后、
//!   `published_at` 标记成功之后才对外可见(publication barrier);
//! - Snapshot 直接投影 Store 权威状态,journal 只携带 cursor,不建立
//!   第二状态机;
//! - 不可解释的 outbox 行 → fail-closed 旋转 stream_epoch,全部客户端
//!   resync；journal hard-cap/min-age、resume/gap 与每客户端有界队列由
//!   `journal.rs` 封装。
//!
//! outbox `event_json` 是 #21 冻结的 store-local 形状
//! (`{type, aggregate, caused_by_command_id, projection}`);本模块在
//! publication 时把它编译为对外的 `mf.event.v1` envelope(§5.4)。

use crate::handles::{AggregateKind, AggregateRef, ServerInstanceId, StreamEpoch, WorkflowHandle};
use crate::journal::EventJournal;
use crate::kernel::KernelProblem;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

pub use crate::journal::{EventHello, EventSubscription, JournalStats as ProjectionDiagnostics};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ScalarRevision {
    #[serde(serialize_with = "serialize_u64_decimal")]
    pub revision: u64,
}

/// Workflow 使用 semantic/presentation 双轴；其它 aggregate 使用单轴。
/// `untagged` 保持 wire 与 canonical envelope 同构。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum AggregateRevision {
    Workflow(RevisionVector),
    Scalar(ScalarRevision),
}

impl PartialEq<RevisionVector> for AggregateRevision {
    fn eq(&self, other: &RevisionVector) -> bool {
        matches!(self, Self::Workflow(value) if value == other)
    }
}

impl PartialEq<AggregateRevision> for RevisionVector {
    fn eq(&self, other: &AggregateRevision) -> bool {
        other == self
    }
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
    pub base_revision: AggregateRevision,
    pub aggregate_revision: AggregateRevision,
    pub caused_by_command_id: Option<String>,
    /// 事件类型(如 `workflow.rename`),不含 store-local `.applied` 后缀。
    #[serde(rename = "type")]
    pub event_type: String,
    pub projection_critical: bool,
    /// `{"mode":"typed_delta","delta_type":...,"data":...}`;禁止 JSON Patch。
    pub projection: Value,
}

/// Snapshot 查询(当前 workflow tracer；Workspace 快照随完整投影扩展)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotQuery {
    Workflow {
        project: crate::handles::ProjectStoreHandle,
        workflow: WorkflowHandle,
    },
}

/// Snapshot data:Store 权威状态的只读投影(不缓存、不推演)。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkflowSnapshotData {
    pub workflow: WorkflowHandle,
    pub name: String,
    pub allow_unsafe_parallel: bool,
    pub revisions: RevisionVector,
    pub nodes: Vec<WorkflowSnapshotNode>,
    pub edges: Vec<WorkflowSnapshotEdge>,
    #[serde(serialize_with = "serialize_u64_decimal")]
    pub workflow_collection_revision: u64,
    pub viewport: Option<Value>,
    pub collapse: Option<Value>,
    pub layout: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkflowSnapshotNode {
    pub handle: String,
    pub key: String,
    pub title: String,
    pub instructions: String,
    pub agent_instance_id: String,
    pub deps: Vec<String>,
    pub position: Option<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkflowSnapshotEdge {
    pub handle: String,
    pub upstream_node_handle: String,
    pub downstream_node_handle: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// 全局跨 Project 投影流的深模块入口。journal 只提供恢复窗口，权威状态
/// 始终来自各 Store；Hub 的 publication mutex 把业务 commit、这里的
/// publication 与 Snapshot 的 cursor+Store read 串在同一个 L-PUBLISH 上。
pub(crate) struct ProjectionHub {
    publication: parking_lot::Mutex<()>,
    journal: Arc<EventJournal>,
}

impl ProjectionHub {
    pub(crate) fn new() -> Self {
        Self {
            publication: parking_lot::Mutex::new(()),
            journal: Arc::new(EventJournal::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(max_events: usize, max_bytes: usize) -> Self {
        Self {
            publication: parking_lot::Mutex::new(()),
            journal: Arc::new(EventJournal::for_test(max_events, max_bytes)),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_limits(limits: crate::limits::JournalLimits) -> Self {
        Self {
            publication: parking_lot::Mutex::new(()),
            journal: Arc::new(EventJournal::for_test_limits(limits)),
        }
    }

    /// L-PUBLISH 的唯一入口。调用方在进入前只可克隆 registry handles，
    /// 不得持有 Project/Store/client queue 锁。Recovering phase 会在业务
    /// closure 执行前收口，因而 poison 期间不可能提交新业务事务。
    pub(crate) fn linearize<T>(
        &self,
        targets: &[crate::command::TargetDatabase],
        action: impl FnOnce(&Self) -> Result<T, KernelProblem>,
    ) -> Result<T, KernelProblem> {
        let _barrier = self.publication.lock();
        self.journal.ensure_configured()?;
        self.recover_poisoned(targets)?;
        action(self)
    }

    /// closing 已在一次成功 linearize 中建立后，finalize 只需要等待当前
    /// L-PUBLISH 临界区结束并执行不可失败的内存移除；其它 target 的
    /// Recovering 不应把已完成 legacy teardown 的 Project 卡在半关闭态。
    pub(crate) fn finalize_close(&self, action: impl FnOnce()) {
        let _barrier = self.publication.lock();
        action();
    }

    /// Snapshot cursor 与权威 Store reader 同处 L-PUBLISH。
    pub(crate) fn snapshot<T>(
        &self,
        targets: &[crate::command::TargetDatabase],
        read_authority: impl FnOnce() -> Result<T, KernelProblem>,
    ) -> Result<(EventCursor, T), KernelProblem> {
        self.linearize(targets, |hub| {
            let cursor = hub.cursor();
            let value = read_authority()?;
            Ok((cursor, value))
        })
    }

    pub(crate) fn subscribe_live(
        &self,
        targets: &[crate::command::TargetDatabase],
        cursor: &EventCursor,
    ) -> Result<EventSubscription, KernelProblem> {
        self.linearize(targets, |hub| hub.subscribe(cursor))
    }

    pub(crate) fn server_instance_id(&self) -> &ServerInstanceId {
        self.journal.server_instance_id()
    }

    pub(crate) fn cursor(&self) -> EventCursor {
        self.journal.cursor()
    }

    pub(crate) fn stats(&self) -> crate::journal::JournalStats {
        self.journal.stats()
    }

    #[cfg(test)]
    pub(crate) fn append_probe(&self) -> Result<u64, KernelProblem> {
        self.journal.append_probe()
    }

    pub(crate) fn subscribe(
        &self,
        cursor: &EventCursor,
    ) -> Result<EventSubscription, KernelProblem> {
        self.journal.subscribe(cursor)
    }

    pub(crate) fn publish_pending(
        &self,
        target: &crate::command::TargetDatabase,
    ) -> Result<(), KernelProblem> {
        self.journal.publish_pending(target)
    }

    pub(crate) fn abort_publication(
        &self,
        target: &crate::command::TargetDatabase,
    ) -> Result<(), KernelProblem> {
        self.journal.abort_publication(target)
    }

    pub(crate) fn recover_poisoned(
        &self,
        targets: &[crate::command::TargetDatabase],
    ) -> Result<(), KernelProblem> {
        self.journal.recover_poisoned(targets)
    }
}

/// store-local outbox 行 → `mf.event.v1` envelope。
pub(crate) fn compile_event(
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
    let base_revision = revision_at(&projection, "base_revision", aggregate.kind)?;
    let aggregate_revision = revision_at(&projection, "aggregate_revision", aggregate.kind)?;
    let delta = projection
        .get("delta")
        .cloned()
        .ok_or_else(|| "outbox 事件缺少 projection.delta".to_string())?;
    let projection_critical = value
        .get("projection_critical")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    validate_projection(
        &event_type,
        &delta,
        base_revision,
        aggregate_revision,
        projection_critical,
    )?;
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
        projection_critical,
        projection: delta,
    })
}

/// 服务端只发布能够被 v1 投影器确定解释的事件。typed delta 使用封闭
/// 白名单且必须精确推进一个 revision 分量；replace/tombstone 仍需保持
/// revision 单调。任何未知 critical delta 或不连续 revision 都由调用方
/// 旋转 epoch，不能进入 journal。
fn validate_projection(
    event_type: &str,
    delta: &Value,
    base: AggregateRevision,
    aggregate: AggregateRevision,
    projection_critical: bool,
) -> Result<(), String> {
    let mode = delta
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| "projection.delta 缺少 mode".to_string())?;
    ensure_not_regressed(base, aggregate)?;
    match mode {
        "typed_delta" => {
            let delta_type = delta
                .get("delta_type")
                .and_then(Value::as_str)
                .ok_or_else(|| "typed_delta 缺少 delta_type".to_string())?;
            if delta_type != event_type {
                return Err(format!(
                    "typed_delta 类型 {delta_type} 与事件 {event_type} 不一致"
                ));
            }
            // T2b 的封闭白名单先含首条 tracer；后续 T2c/T2d 每新增命令
            // 必须在同一提交显式扩表并补 projection contract。
            if !matches!(
                delta_type,
                "workflow.rename"
                    | "workflow.add_node"
                    | "workflow.update_node"
                    | "workflow.remove_node"
                    | "workflow.connect"
                    | "workflow.disconnect"
                    | "workflow.node_position_set"
                    | "workflow.viewport_set"
                    | "workflow.set_unsafe_parallel"
                    | "project.workflow_collection_changed"
            ) {
                let critical = if projection_critical {
                    "critical"
                } else {
                    "non-critical"
                };
                return Err(format!("未知 {critical} typed delta:{delta_type}"));
            }
            let data = delta
                .get("data")
                .cloned()
                .ok_or_else(|| "typed_delta 缺少 data".to_string())?;
            if delta_type == "workflow.rename" {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct WorkflowRenameDelta {
                    name: String,
                }
                let decoded: WorkflowRenameDelta = serde_json::from_value(data)
                    .map_err(|error| format!("workflow.rename data 非法:{error}"))?;
                if decoded.name.trim().is_empty() {
                    return Err("workflow.rename name 不能为空".into());
                }
            } else if !data.is_object() {
                return Err(format!("{delta_type} data 必须是 object"));
            }
            if !is_single_step(base, aggregate) {
                return Err("typed_delta revision 不连续".into());
            }
        }
        "replace" => {
            if delta.get("data").is_none() {
                return Err("replace 缺少 data".into());
            }
            if aggregate == base {
                return Err("replace 未推进 revision".into());
            }
        }
        "tombstone" => {
            // tombstone 携带删除前最终 revision；允许与 base 相同，也允许
            // 删除命令把语义轴推进一步，但绝不允许倒退。
        }
        other => return Err(format!("未知 projection mode:{other}")),
    }
    Ok(())
}

fn revision_at(
    projection: &Value,
    field: &str,
    aggregate_kind: AggregateKind,
) -> Result<AggregateRevision, String> {
    let value = projection
        .get(field)
        .ok_or_else(|| format!("outbox projection 缺少 {field}"))?;
    if aggregate_kind == AggregateKind::ProjectWorkflow {
        let semantic_revision = value
            .get("semantic_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("{field}.semantic_revision 缺失或非 u64"))?;
        let presentation_revision = value
            .get("presentation_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("{field}.presentation_revision 缺失或非 u64"))?;
        Ok(AggregateRevision::Workflow(RevisionVector {
            semantic_revision,
            presentation_revision,
        }))
    } else {
        let revision = value
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("{field}.revision 缺失或非 u64"))?;
        Ok(AggregateRevision::Scalar(ScalarRevision { revision }))
    }
}

fn ensure_not_regressed(
    base: AggregateRevision,
    aggregate: AggregateRevision,
) -> Result<(), String> {
    match (base, aggregate) {
        (AggregateRevision::Workflow(base), AggregateRevision::Workflow(aggregate))
            if aggregate.semantic_revision >= base.semantic_revision
                && aggregate.presentation_revision >= base.presentation_revision =>
        {
            Ok(())
        }
        (AggregateRevision::Scalar(base), AggregateRevision::Scalar(aggregate))
            if aggregate.revision >= base.revision =>
        {
            Ok(())
        }
        (AggregateRevision::Workflow(_), AggregateRevision::Workflow(_))
        | (AggregateRevision::Scalar(_), AggregateRevision::Scalar(_)) => {
            Err("aggregate revision 倒退".into())
        }
        _ => Err("aggregate revision 形状不一致".into()),
    }
}

fn is_single_step(base: AggregateRevision, aggregate: AggregateRevision) -> bool {
    match (base, aggregate) {
        (AggregateRevision::Workflow(base), AggregateRevision::Workflow(aggregate)) => {
            let semantic_step = aggregate.semantic_revision - base.semantic_revision;
            let presentation_step = aggregate.presentation_revision - base.presentation_revision;
            semantic_step.saturating_add(presentation_step) == 1
        }
        (AggregateRevision::Scalar(base), AggregateRevision::Scalar(aggregate)) => {
            aggregate.revision == base.revision.saturating_add(1)
        }
        _ => false,
    }
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
