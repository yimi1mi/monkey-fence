//! workflow events wire DTO(T7b,Issue #39;spec §7.1/§7.2)。
//!
//! `mf-workflow.v1` 事件 envelope:字符串化 seq、封闭 critical 语义
//! (未知 critical 事件 → 客户端必须 resync;未知非 critical 可忽略,
//! v1 只允许 additive optional change)。

use serde::{Deserialize, Serialize};

use super::u64_str;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema: String,
    /// 事件类型(领域封闭集;未知类型的处置由 `critical` 决定)。
    #[serde(rename = "type")]
    pub event_type: String,
    /// critical = true:客户端必须理解;未知 critical → 断开并
    /// `resync_required`(不允许带病运行)。
    pub critical: bool,
    pub stream_epoch: String,
    #[serde(with = "u64_str")]
    pub seq: u64,
    pub data: serde_json::Value,
}

impl EventEnvelope {
    pub fn new(event_type: &str, critical: bool, seq: u64, data: serde_json::Value) -> Self {
        Self {
            schema: "mf.event.v1".to_string(),
            event_type: event_type.to_string(),
            critical,
            stream_epoch: format!("ep_{}", uuid::Uuid::now_v7().simple()),
            seq,
            data,
        }
    }
}

/// kernel EventEnvelope → wire(§7.1:seq 字符串化;projection_critical
/// → critical;typed_delta 投影整体作为 data 透传,禁止 JSON Patch)。
impl From<mf_kernel::projection::EventEnvelope> for EventEnvelope {
    fn from(event: mf_kernel::projection::EventEnvelope) -> Self {
        Self {
            schema: "mf.event.v1".to_string(),
            event_type: event.event_type.clone(),
            critical: event.projection_critical,
            stream_epoch: event.stream_epoch.as_str().to_string(),
            seq: event.seq,
            data: event.projection,
        }
    }
}

/// 未知事件的客户端处置(§7.2:additive optional change)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownEventPolicy {
    /// 未知 critical → 拒绝继续(4409 resync;客户端重连拉全量快照)。
    MustResync,
    /// 未知非 critical → 可安全忽略。
    Ignorable,
}

pub fn policy_for_unknown(event: &EventEnvelope) -> UnknownEventPolicy {
    if event.critical {
        UnknownEventPolicy::MustResync
    } else {
        UnknownEventPolicy::Ignorable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_wire_shape_and_string_seq() {
        let event = EventEnvelope::new(
            "workflow_run.needs_you",
            true,
            1_842,
            serde_json::json!({"run": "run_0123456789abcdef0123456789abcdef"}),
        );
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["schema"], "mf.event.v1");
        assert_eq!(json["seq"], "1842");
        assert_eq!(json["critical"], true);
        let back: EventEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn unknown_critical_requires_resync_but_optional_ignored() {
        let critical = EventEnvelope::new("future.critical.event", true, 1, serde_json::json!({}));
        assert_eq!(
            policy_for_unknown(&critical),
            UnknownEventPolicy::MustResync
        );
        let optional = EventEnvelope::new("future.hint.event", false, 2, serde_json::json!({}));
        assert_eq!(policy_for_unknown(&optional), UnknownEventPolicy::Ignorable);
    }
}
