//! `mf-workflow.v1` 的有界进程内恢复 journal（canonical spec §5.3–§5.6）。
//!
//! 这里不保存权威领域状态。Store/outbox 是事实源；journal 只在
//! L-PUBLISH 内分配全局 seq、提供同 epoch resume window，并为每个客户端
//! 维护独立有界队列。容量、publication 或协议不可解释时旋转 epoch，旧
//! subscription 确定返回 `resync_required`。

use crate::command::{CommandProblem, TargetDatabase};
use crate::handles::{ServerInstanceId, StreamEpoch};
use crate::kernel::KernelProblem;
use crate::limits::JournalLimits;
use crate::projection::{compile_event, EventCursor, EventEnvelope};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::params;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// T7 WebGateway 配置落地前的安全 hard cap；单客户端队列之外还必须限制
/// 总 fan-out 倍数，避免连接洪泛放大 CPU/引用内存。
const ACTIVE_EVENT_SUBSCRIPTIONS_HARD_CAP: usize = 256;

trait MonotonicClock: Send + Sync {
    fn now(&self) -> Duration;
}

struct SystemMonotonicClock {
    origin: Instant,
}

impl SystemMonotonicClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl MonotonicClock for SystemMonotonicClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

#[cfg(test)]
pub(crate) struct ManualClock {
    millis: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            millis: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub(crate) fn advance(&self, duration: Duration) {
        self.millis.fetch_add(
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

#[cfg(test)]
impl MonotonicClock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.millis.load(std::sync::atomic::Ordering::Relaxed))
    }
}

#[derive(Debug, Clone)]
struct JournalEntry {
    event: Arc<EventEnvelope>,
    encoded_bytes: usize,
    inserted_at: Duration,
}

#[derive(Debug)]
struct ClientQueue {
    /// subscribe 时冻结的 journal 尾部；该范围直接从 journal 分批 replay，
    /// 不占用较小的 live send queue。
    replay_until: u64,
    last_delivered: u64,
    events: VecDeque<JournalEntry>,
    bytes: usize,
}

#[derive(Debug)]
struct JournalInner {
    stream_epoch: StreamEpoch,
    next_seq: u64,
    bytes: usize,
    entries: VecDeque<JournalEntry>,
    aggregate_heads: HashMap<String, crate::projection::AggregateRevision>,
    tombstoned: HashSet<String>,
    clients: HashMap<u64, ClientQueue>,
    next_client_id: u64,
    rotations: u64,
    capacity_rotations: u64,
    publication_rotations: u64,
    protocol_rotations: u64,
    evicted: u64,
    /// publication 失败且 outbox 无法当场 reconciled 时禁止新 epoch 发布；
    /// 下次调用必须先收口旧 outbox。
    poisoned_targets: HashSet<String>,
    resyncs: u64,
}

/// 运行时可观测水位；不暴露 event payload，也不是第二事实源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalStats {
    pub events: usize,
    pub bytes: usize,
    pub first_available_seq: u64,
    pub clients: usize,
    pub rotations: u64,
    pub capacity_rotations: u64,
    pub publication_rotations: u64,
    pub protocol_rotations: u64,
    pub evicted: u64,
    pub resyncs: u64,
    pub max_client_queue_events: usize,
    pub max_client_queue_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EventHello {
    pub schema: &'static str,
    pub stream_epoch: StreamEpoch,
    #[serde(serialize_with = "serialize_u64_decimal")]
    pub first_available_seq: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    pub last_seq: u64,
}

fn serialize_u64_decimal<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

/// 每个订阅只拥有自己的队列。队列超限时 journal 移除该 client；下一次
/// poll 返回 resync，不会拖慢其它 client 或持有其资源。
pub struct EventSubscription {
    journal: Arc<EventJournal>,
    client_id: u64,
    stream_epoch: StreamEpoch,
    last_seq: u64,
    hello: EventHello,
}

impl std::fmt::Debug for EventSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSubscription")
            .field("client_id", &self.client_id)
            .field("stream_epoch", &self.stream_epoch)
            .field("last_seq", &self.last_seq)
            .finish_non_exhaustive()
    }
}

impl EventSubscription {
    /// 非阻塞清空该 client 当前队列。epoch 已旋转、gap 或慢 client 被逐出
    /// 都统一返回 `resync_required`。
    pub fn poll(&mut self) -> Result<Vec<EventEnvelope>, KernelProblem> {
        let events = self
            .journal
            .poll_client(self.client_id, &self.stream_epoch)?;
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

    pub fn hello(&self) -> &EventHello {
        &self.hello
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        self.journal
            .release_client(self.client_id, &self.stream_epoch);
    }
}

/// 全局跨 Project journal。外层 [`crate::projection::ProjectionHub`] 独占
/// L-PUBLISH；内部 mutex 保证 resume、append 与 fan-out 不观察半发布批次。
pub(crate) struct EventJournal {
    server_instance_id: ServerInstanceId,
    limits: JournalLimits,
    config_error: Option<String>,
    clock: Arc<dyn MonotonicClock>,
    inner: Mutex<JournalInner>,
}

impl EventJournal {
    pub(crate) fn new() -> Self {
        #[cfg(any(test, feature = "test-support"))]
        let loaded: Result<JournalLimits, crate::limits::JournalLimitsLoadError> =
            Ok(JournalLimits::default());
        #[cfg(not(any(test, feature = "test-support")))]
        let loaded = JournalLimits::load_default_path();

        match loaded {
            Ok(limits) => Self::with_limits(limits),
            Err(error) => {
                let mut journal = Self::with_limits(JournalLimits::default());
                journal.config_error = Some(error.to_string());
                journal
            }
        }
    }

    fn with_limits(limits: JournalLimits) -> Self {
        Self::with_clock(limits, Arc::new(SystemMonotonicClock::new()))
    }

    fn with_clock(limits: JournalLimits, clock: Arc<dyn MonotonicClock>) -> Self {
        Self {
            server_instance_id: ServerInstanceId::new(),
            limits,
            config_error: None,
            clock,
            inner: Mutex::new(JournalInner {
                stream_epoch: StreamEpoch::new(),
                next_seq: 1,
                bytes: 0,
                entries: VecDeque::new(),
                aggregate_heads: HashMap::new(),
                tombstoned: HashSet::new(),
                clients: HashMap::new(),
                next_client_id: 1,
                rotations: 0,
                capacity_rotations: 0,
                publication_rotations: 0,
                protocol_rotations: 0,
                evicted: 0,
                poisoned_targets: HashSet::new(),
                resyncs: 0,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(max_events: usize, max_bytes: usize) -> Self {
        let mut limits = JournalLimits::default();
        limits.journal_max_events = max_events;
        limits.journal_max_bytes = max_bytes;
        // 小容量 seam 只用于契约测试，不能用 production 的配置下限校验。
        limits.journal_event_max_bytes = limits.journal_event_max_bytes.min(max_bytes.max(1));
        limits.client_event_queue_max_events =
            limits.client_event_queue_max_events.min(max_events.max(1));
        limits.client_event_queue_max_bytes =
            limits.client_event_queue_max_bytes.min(max_bytes.max(1));
        Self::with_limits(limits)
    }

    #[cfg(test)]
    pub(crate) fn for_test_limits(limits: JournalLimits) -> Self {
        Self::with_limits(limits)
    }

    #[cfg(test)]
    pub(crate) fn for_test_limits_and_clock(
        limits: JournalLimits,
        clock: Arc<ManualClock>,
    ) -> Self {
        Self::with_clock(limits, clock)
    }

    pub(crate) fn server_instance_id(&self) -> &ServerInstanceId {
        &self.server_instance_id
    }

    pub(crate) fn ensure_configured(&self) -> Result<(), KernelProblem> {
        match &self.config_error {
            Some(error) => Err(KernelProblem::ServiceUnavailable(error.clone())),
            None => Ok(()),
        }
    }

    pub(crate) fn cursor(&self) -> EventCursor {
        let inner = self.inner.lock();
        EventCursor {
            stream_epoch: inner.stream_epoch.clone(),
            through_seq: inner.next_seq.saturating_sub(1),
        }
    }

    pub(crate) fn stats(&self) -> JournalStats {
        let inner = self.inner.lock();
        JournalStats {
            events: inner.entries.len(),
            bytes: inner.bytes,
            first_available_seq: first_available_seq(&inner),
            clients: inner.clients.len(),
            rotations: inner.rotations,
            capacity_rotations: inner.capacity_rotations,
            publication_rotations: inner.publication_rotations,
            protocol_rotations: inner.protocol_rotations,
            evicted: inner.evicted,
            resyncs: inner.resyncs,
            max_client_queue_events: inner
                .clients
                .values()
                .map(|queue| queue.events.len())
                .max()
                .unwrap_or(0),
            max_client_queue_bytes: inner
                .clients
                .values()
                .map(|queue| queue.bytes)
                .max()
                .unwrap_or(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn append_probe(&self) -> Result<u64, KernelProblem> {
        use crate::handles::{AggregateKind, AggregateRef};
        use crate::projection::{AggregateRevision, RevisionVector};

        let mut inner = self.inner.lock();
        if !inner.poisoned_targets.is_empty() {
            return Err(KernelProblem::ResyncRequired);
        }
        let seq = inner.next_seq;
        let previous = inner
            .aggregate_heads
            .get("project_workflow:perf-probe")
            .copied()
            .unwrap_or(AggregateRevision::Workflow(RevisionVector {
                semantic_revision: 1,
                presentation_revision: 1,
            }));
        let AggregateRevision::Workflow(previous_vector) = previous else {
            return Err(KernelProblem::ResyncRequired);
        };
        let next = AggregateRevision::Workflow(RevisionVector {
            semantic_revision: previous_vector.semantic_revision,
            presentation_revision: previous_vector
                .presentation_revision
                .checked_add(1)
                .ok_or(KernelProblem::ResyncRequired)?,
        });
        let event = EventEnvelope {
            schema: crate::projection::EVENT_SCHEMA,
            stream_epoch: inner.stream_epoch.clone(),
            seq,
            occurred_at: Utc::now().to_rfc3339(),
            aggregate: AggregateRef::new(AggregateKind::ProjectWorkflow, "perf-probe")
                .map_err(|error| KernelProblem::Internal(error.to_string()))?,
            base_revision: previous,
            aggregate_revision: next,
            caused_by_command_id: None,
            event_type: "workflow.rename".into(),
            projection_critical: true,
            projection: serde_json::json!({
                "mode": "typed_delta",
                "delta_type": "workflow.rename",
                "data": {"name": "perf"},
            }),
        };
        let encoded_bytes = serde_json::to_vec(&event)
            .map_err(|error| KernelProblem::Internal(error.to_string()))?
            .len();
        let now = self.clock.now();
        if encoded_bytes > self.limits.journal_event_max_bytes
            || !make_capacity(&mut inner, &self.limits, 1, encoded_bytes, now)
        {
            rotate_epoch(&mut inner, RotationReason::Capacity);
            return Err(KernelProblem::ResyncRequired);
        }
        let entry = JournalEntry {
            event: Arc::new(event),
            encoded_bytes,
            inserted_at: now,
        };
        inner.bytes = inner.bytes.saturating_add(encoded_bytes);
        inner.entries.push_back(entry.clone());
        inner
            .aggregate_heads
            .insert("project_workflow:perf-probe".into(), next);
        inner.next_seq = seq.checked_add(1).ok_or(KernelProblem::ResyncRequired)?;
        fan_out(&mut inner, &self.limits, std::slice::from_ref(&entry));
        Ok(seq)
    }

    pub(crate) fn subscribe(
        self: &Arc<Self>,
        cursor: &EventCursor,
    ) -> Result<EventSubscription, KernelProblem> {
        let mut inner = self.inner.lock();
        let last_seq = inner.next_seq.saturating_sub(1);
        let first_seq = first_available_seq(&inner);
        if inner.stream_epoch != cursor.stream_epoch
            || cursor.through_seq > last_seq
            || cursor.through_seq.saturating_add(1) < first_seq
        {
            inner.resyncs = inner.resyncs.saturating_add(1);
            return Err(KernelProblem::ResyncRequired);
        }
        if inner.clients.len() >= ACTIVE_EVENT_SUBSCRIPTIONS_HARD_CAP {
            return Err(KernelProblem::ServiceUnavailable(
                "event_subscription_limit".into(),
            ));
        }

        let client_id = inner.next_client_id;
        inner.next_client_id = inner
            .next_client_id
            .checked_add(1)
            .ok_or_else(|| KernelProblem::ServiceUnavailable("event_client_id_exhausted".into()))?;
        inner.clients.insert(
            client_id,
            ClientQueue {
                replay_until: last_seq,
                last_delivered: cursor.through_seq,
                events: VecDeque::new(),
                bytes: 0,
            },
        );
        Ok(EventSubscription {
            journal: self.clone(),
            client_id,
            stream_epoch: inner.stream_epoch.clone(),
            last_seq: cursor.through_seq,
            hello: EventHello {
                schema: "events.hello.v1",
                stream_epoch: inner.stream_epoch.clone(),
                first_available_seq: first_seq,
                last_seq,
            },
        })
    }

    fn poll_client(
        &self,
        client_id: u64,
        stream_epoch: &StreamEpoch,
    ) -> Result<Vec<EventEnvelope>, KernelProblem> {
        let mut inner = self.inner.lock();
        if &inner.stream_epoch != stream_epoch {
            return Err(KernelProblem::ResyncRequired);
        }
        let Some(queue_view) = inner.clients.get(&client_id) else {
            return Err(KernelProblem::ResyncRequired);
        };
        if queue_view.last_delivered < queue_view.replay_until {
            let after = queue_view.last_delivered;
            let replay_until = queue_view.replay_until;
            if after.saturating_add(1) < first_available_seq(&inner) {
                inner.clients.remove(&client_id);
                inner.resyncs = inner.resyncs.saturating_add(1);
                return Err(KernelProblem::ResyncRequired);
            }
            let mut bytes = 0usize;
            let mut replay = Vec::new();
            let mut oversized = false;
            for entry in inner
                .entries
                .iter()
                .filter(|entry| entry.event.seq > after && entry.event.seq <= replay_until)
            {
                if entry.encoded_bytes > self.limits.client_event_queue_max_bytes {
                    oversized = true;
                    break;
                }
                if !replay.is_empty()
                    && (replay.len() >= self.limits.client_event_queue_max_events
                        || bytes.saturating_add(entry.encoded_bytes)
                            > self.limits.client_event_queue_max_bytes)
                {
                    break;
                }
                bytes = bytes.saturating_add(entry.encoded_bytes);
                replay.push((*entry.event).clone());
            }
            if oversized {
                inner.clients.remove(&client_id);
                inner.resyncs = inner.resyncs.saturating_add(1);
                return Err(KernelProblem::ResyncRequired);
            }
            let Some(last) = replay.last().map(|event| event.seq) else {
                inner.clients.remove(&client_id);
                inner.resyncs = inner.resyncs.saturating_add(1);
                return Err(KernelProblem::ResyncRequired);
            };
            inner
                .clients
                .get_mut(&client_id)
                .expect("client checked")
                .last_delivered = last;
            return Ok(replay);
        }
        let queue = inner.clients.get_mut(&client_id).expect("client checked");
        let mut events = Vec::with_capacity(queue.events.len());
        while let Some(entry) = queue.events.pop_front() {
            queue.bytes = queue.bytes.saturating_sub(entry.encoded_bytes);
            queue.last_delivered = entry.event.seq;
            events.push((*entry.event).clone());
        }
        Ok(events)
    }

    fn release_client(&self, client_id: u64, stream_epoch: &StreamEpoch) {
        let mut inner = self.inner.lock();
        if &inner.stream_epoch == stream_epoch {
            inner.clients.remove(&client_id);
        }
    }

    /// dispatch 在 target commit 后（或无法证明未 commit 时）失败的统一
    /// fail-closed 收口：旋转 epoch，并把旧 outbox 标为 reconciled。
    pub(crate) fn abort_publication(&self, target: &TargetDatabase) -> Result<(), KernelProblem> {
        let mut inner = self.inner.lock();
        rotate_and_reconcile(&mut inner, target, Utc::now(), RotationReason::Publication)
    }

    /// 全局 Recovering phase：所有故障 target 的旧 outbox 都收口后，
    /// 才允许新 epoch 继续发布或返回 Snapshot。
    pub(crate) fn recover_poisoned(&self, targets: &[TargetDatabase]) -> Result<(), KernelProblem> {
        let mut inner = self.inner.lock();
        if inner.poisoned_targets.is_empty() {
            return Ok(());
        }
        let poisoned: Vec<String> = inner.poisoned_targets.iter().cloned().collect();
        for store_key in poisoned {
            let Some(target) = targets
                .iter()
                .find(|target| target.store_key() == store_key)
            else {
                return Err(KernelProblem::ResyncRequired);
            };
            mark_all_pending_reconciled(target, Utc::now())?;
            inner.poisoned_targets.remove(&store_key);
        }
        Ok(())
    }

    /// L-PUBLISH：在 journal mutex 内完成 outbox 编译、容量决策、
    /// `published_at` 收口、append 与 fan-out。Store 标记成功前队列不可见；
    /// 任一步失败都旋转 epoch，旧客户端确定 resync。
    pub(crate) fn publish_pending(&self, target: &TargetDatabase) -> Result<(), KernelProblem> {
        let mut inner = self.inner.lock();
        if !inner.poisoned_targets.is_empty() {
            return Err(KernelProblem::ResyncRequired);
        }
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
            .map_err(|error| publication_failure(&mut inner, target, "读取 outbox 水位", error))?;
        let pending_count = usize::try_from(pending_count).unwrap_or(usize::MAX);
        let pending_bytes = usize::try_from(pending_bytes).unwrap_or(usize::MAX);
        if pending_count == 0 {
            return Ok(());
        }
        if pending_count > self.limits.journal_max_events
            || pending_bytes > self.limits.journal_max_bytes
        {
            rotate_and_reconcile(&mut inner, target, Utc::now(), RotationReason::Capacity)?;
            return Err(KernelProblem::ResyncRequired);
        }

        let rows: Vec<(i64, String)> = target
            .with_conn(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT outbox_id, event_json FROM projection_outbox
                         WHERE published_at IS NULL ORDER BY outbox_id LIMIT ?1",
                    )
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                let rows = stmt
                    .query_map(
                        [
                            i64::try_from(self.limits.journal_max_events.saturating_add(1))
                                .unwrap_or(i64::MAX),
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                Ok(rows)
            })
            .map_err(|error| publication_failure(&mut inner, target, "读取 outbox", error))?;
        if rows.len() != pending_count {
            rotate_and_reconcile(&mut inner, target, Utc::now(), RotationReason::Publication)?;
            return Err(KernelProblem::ResyncRequired);
        }

        let occurred_at = Utc::now();
        let occurred_at_wire = occurred_at.to_rfc3339();
        let inserted_at = self.clock.now();
        let mut staged = Vec::with_capacity(rows.len());
        for (index, (outbox_id, event_json)) in rows.into_iter().enumerate() {
            let seq = inner
                .next_seq
                .checked_add(u64::try_from(index).unwrap_or(u64::MAX))
                .ok_or_else(|| {
                    let _ = rotate_and_reconcile(
                        &mut inner,
                        target,
                        occurred_at,
                        RotationReason::Publication,
                    );
                    KernelProblem::ResyncRequired
                })?;
            let event =
                match compile_event(&inner.stream_epoch, seq, &occurred_at_wire, &event_json) {
                    Ok(event) => event,
                    Err(error) => {
                        log::error!("projection publication 不可解释:{error}");
                        rotate_and_reconcile(
                            &mut inner,
                            target,
                            occurred_at,
                            RotationReason::Protocol,
                        )?;
                        return Err(KernelProblem::ResyncRequired);
                    }
                };
            let encoded_bytes = match serde_json::to_vec(&event) {
                Ok(bytes) => bytes.len(),
                Err(error) => {
                    log::error!("projection event 序列化失败:{error}");
                    rotate_and_reconcile(
                        &mut inner,
                        target,
                        occurred_at,
                        RotationReason::Publication,
                    )?;
                    return Err(KernelProblem::ResyncRequired);
                }
            };
            if encoded_bytes > self.limits.journal_event_max_bytes {
                rotate_and_reconcile(&mut inner, target, occurred_at, RotationReason::Capacity)?;
                return Err(KernelProblem::ResyncRequired);
            }
            staged.push((
                outbox_id,
                JournalEntry {
                    event: Arc::new(event),
                    encoded_bytes,
                    inserted_at,
                },
            ));
        }
        let staged_bytes = staged.iter().fold(0usize, |total, (_, entry)| {
            total.saturating_add(entry.encoded_bytes)
        });
        if staged.len() > self.limits.journal_max_events
            || staged_bytes > self.limits.journal_max_bytes
        {
            rotate_and_reconcile(&mut inner, target, occurred_at, RotationReason::Capacity)?;
            return Err(KernelProblem::ResyncRequired);
        }

        let mut staged_heads = inner.aggregate_heads.clone();
        let mut staged_tombstones = inner.tombstoned.clone();
        for (_, entry) in &staged {
            let key = aggregate_key(&entry.event);
            if staged_tombstones.contains(&key)
                || staged_heads
                    .get(&key)
                    .is_some_and(|head| *head != entry.event.base_revision)
            {
                rotate_and_reconcile(&mut inner, target, occurred_at, RotationReason::Protocol)?;
                return Err(KernelProblem::ResyncRequired);
            }
            staged_heads.insert(key.clone(), entry.event.aggregate_revision);
            if entry.event.projection["mode"] == "tombstone" {
                staged_tombstones.insert(key);
            }
        }

        if !make_capacity(
            &mut inner,
            &self.limits,
            staged.len(),
            staged_bytes,
            inserted_at,
        ) {
            rotate_and_reconcile(&mut inner, target, occurred_at, RotationReason::Capacity)?;
            return Err(KernelProblem::ResyncRequired);
        }

        let ids: Vec<i64> = staged.iter().map(|(outbox_id, _)| *outbox_id).collect();
        let mark_result = target.with_tx(|tx| {
            for id in &ids {
                let changed = tx
                    .execute(
                        "UPDATE projection_outbox SET published_at=?2
                         WHERE outbox_id=?1 AND published_at IS NULL",
                        params![id, &occurred_at_wire],
                    )
                    .map_err(|error| CommandProblem::Internal(error.to_string()))?;
                if changed != 1 {
                    return Err(CommandProblem::Internal(format!(
                        "outbox {id} publication CAS 未命中"
                    )));
                }
            }
            Ok(())
        });
        if let Err(error) = mark_result {
            log::error!("标记 projection_outbox 已发布失败:{error}");
            rotate_and_reconcile(&mut inner, target, occurred_at, RotationReason::Publication)?;
            return Err(KernelProblem::ResyncRequired);
        }

        let next_seq = staged
            .last()
            .and_then(|(_, entry)| entry.event.seq.checked_add(1))
            .ok_or_else(|| {
                let _ = rotate_and_reconcile(
                    &mut inner,
                    target,
                    occurred_at,
                    RotationReason::Publication,
                );
                KernelProblem::ResyncRequired
            })?;
        let published: Vec<JournalEntry> = staged.into_iter().map(|(_, entry)| entry).collect();
        inner.bytes = inner.bytes.saturating_add(staged_bytes);
        inner.entries.extend(published.iter().cloned());
        inner.aggregate_heads = staged_heads;
        inner.tombstoned = staged_tombstones;
        inner.next_seq = next_seq;
        fan_out(&mut inner, &self.limits, &published);
        debug_assert!(inner.entries.len() <= self.limits.journal_max_events);
        debug_assert!(inner.bytes <= self.limits.journal_max_bytes);
        Ok(())
    }
}

fn first_available_seq(inner: &JournalInner) -> u64 {
    inner
        .entries
        .front()
        .map(|entry| entry.event.seq)
        .unwrap_or(inner.next_seq)
}

fn make_capacity(
    inner: &mut JournalInner,
    limits: &JournalLimits,
    append_events: usize,
    append_bytes: usize,
    now: Duration,
) -> bool {
    while inner.entries.len().saturating_add(append_events) > limits.journal_max_events
        || inner.bytes.saturating_add(append_bytes) > limits.journal_max_bytes
    {
        let Some(front) = inner.entries.front() else {
            return false;
        };
        let age_secs = now.saturating_sub(front.inserted_at).as_secs();
        if age_secs < limits.journal_min_age_secs {
            return false;
        }
        let removed = inner.entries.pop_front().expect("front checked");
        inner.bytes = inner.bytes.saturating_sub(removed.encoded_bytes);
        inner.evicted = inner.evicted.saturating_add(1);
    }
    true
}

fn fan_out(inner: &mut JournalInner, limits: &JournalLimits, events: &[JournalEntry]) {
    let appended_bytes = events.iter().fold(0usize, |total, entry| {
        total.saturating_add(entry.encoded_bytes)
    });
    let mut slow_clients = Vec::new();
    for (client_id, queue) in &mut inner.clients {
        if queue.events.len().saturating_add(events.len()) > limits.client_event_queue_max_events
            || queue.bytes.saturating_add(appended_bytes) > limits.client_event_queue_max_bytes
        {
            slow_clients.push(*client_id);
            continue;
        }
        queue.events.extend(events.iter().cloned());
        queue.bytes = queue.bytes.saturating_add(appended_bytes);
    }
    for client_id in slow_clients {
        inner.clients.remove(&client_id);
        inner.resyncs = inner.resyncs.saturating_add(1);
    }
}

#[derive(Debug, Clone, Copy)]
enum RotationReason {
    Capacity,
    Publication,
    Protocol,
}

fn rotate_epoch(inner: &mut JournalInner, reason: RotationReason) {
    inner.resyncs = inner
        .resyncs
        .saturating_add(u64::try_from(inner.clients.len()).unwrap_or(u64::MAX));
    inner.stream_epoch = StreamEpoch::new();
    inner.next_seq = 1;
    inner.bytes = 0;
    inner.entries.clear();
    inner.aggregate_heads.clear();
    inner.tombstoned.clear();
    inner.clients.clear();
    inner.rotations = inner.rotations.saturating_add(1);
    match reason {
        RotationReason::Capacity => {
            inner.capacity_rotations = inner.capacity_rotations.saturating_add(1)
        }
        RotationReason::Publication => {
            inner.publication_rotations = inner.publication_rotations.saturating_add(1)
        }
        RotationReason::Protocol => {
            inner.protocol_rotations = inner.protocol_rotations.saturating_add(1)
        }
    }
}

fn aggregate_key(event: &EventEnvelope) -> String {
    format!(
        "{}:{}",
        event.aggregate.kind.as_str(),
        event.aggregate.handle
    )
}

fn rotate_and_reconcile(
    inner: &mut JournalInner,
    target: &TargetDatabase,
    occurred_at: DateTime<Utc>,
    reason: RotationReason,
) -> Result<(), KernelProblem> {
    rotate_epoch(inner, reason);
    if let Err(error) = mark_all_pending_reconciled(target, occurred_at) {
        inner
            .poisoned_targets
            .insert(target.store_key().to_string());
        return Err(error);
    }
    Ok(())
}

fn publication_failure(
    inner: &mut JournalInner,
    target: &TargetDatabase,
    action: &str,
    error: CommandProblem,
) -> KernelProblem {
    log::error!("{action}失败，旋转 stream epoch:{error}");
    rotate_epoch(inner, RotationReason::Publication);
    inner
        .poisoned_targets
        .insert(target.store_key().to_string());
    KernelProblem::ResyncRequired
}

fn mark_all_pending_reconciled(
    target: &TargetDatabase,
    occurred_at: DateTime<Utc>,
) -> Result<(), KernelProblem> {
    let reconciled = format!(
        "{}{}",
        crate::reconcile::OUTBOX_RECONCILED_PREFIX,
        occurred_at.to_rfc3339()
    );
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
            log::error!("journal rotate 后 outbox reconcile 失败:{error}");
            KernelProblem::ResyncRequired
        })
}
