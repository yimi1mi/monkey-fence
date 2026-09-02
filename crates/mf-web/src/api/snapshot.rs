//! `mf.snapshot.v1` envelope(T7b,Issue #39;spec §7.3)。

use serde::{Deserialize, Serialize};

use super::u64_str;

/// 快照 cursor(stream_epoch opaque + 字符串化 through_seq)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotCursor {
    pub stream_epoch: String,
    #[serde(with = "u64_str")]
    pub through_seq: u64,
}

/// Snapshot envelope;`data` 为领域快照(per-workflow/workspace)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotEnvelope {
    pub schema: String,
    pub server_instance_id: String,
    pub cursor: SnapshotCursor,
    pub data: serde_json::Value,
}

impl SnapshotEnvelope {
    pub fn new(server_instance_id: &str, through_seq: u64, data: serde_json::Value) -> Self {
        Self {
            schema: "mf.snapshot.v1".to_string(),
            server_instance_id: server_instance_id.to_string(),
            cursor: SnapshotCursor {
                stream_epoch: format!("ep_{}", uuid::Uuid::now_v7().simple()),
                through_seq,
            },
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_with_string_u64() {
        let envelope = SnapshotEnvelope::new("srv_1", 1_842, serde_json::json!({"projects": []}));
        let json = serde_json::to_value(&envelope).unwrap();
        // wire 上 through_seq 是字符串(JS Number 安全)
        assert_eq!(json["cursor"]["through_seq"], "1842");
        let back: SnapshotEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back, envelope);
        assert_eq!(back.cursor.through_seq, 1_842);
    }
}
