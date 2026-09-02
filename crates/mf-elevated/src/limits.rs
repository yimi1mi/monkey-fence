//! Root/Elevated 数值上限(附录 A6;T3e,Issue #33)。

/// Root/Elevated 可配置上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElevatedLimits {
    /// Core↔Broker 心跳周期。
    pub broker_heartbeat_interval_ms: u64,
    /// 连失判定断开(fixed,不可配置)。
    pub broker_heartbeat_miss_limit: u32,
    /// Core 消失后 Root process group 的 bounded grace。
    pub root_host_orphan_grace_ms: u64,
    /// 一次性 nonce 有效期。
    pub broker_request_ttl_ms: u64,
    /// 每会话 ACL 保护 spool 容量。
    pub root_spool_max_bytes: usize,
}

/// `broker_heartbeat_miss_limit` 的 fixed 值(附录 A6)。
pub const BROKER_HEARTBEAT_MISS_LIMIT: u32 = 3;

impl Default for ElevatedLimits {
    fn default() -> Self {
        Self {
            broker_heartbeat_interval_ms: 2_000,
            broker_heartbeat_miss_limit: BROKER_HEARTBEAT_MISS_LIMIT,
            root_host_orphan_grace_ms: 300_000,
            broker_request_ttl_ms: 30_000,
            root_spool_max_bytes: 32 * 1024 * 1024,
        }
    }
}

impl ElevatedLimits {
    /// 越界钳制到允许范围(心跳 1–10 s、grace 60–1800 s、ttl 10–120 s、
    /// spool 4–256 MiB);miss_limit 强制为 fixed 值。
    pub fn clamp(&self) -> Self {
        Self {
            broker_heartbeat_interval_ms: self.broker_heartbeat_interval_ms.clamp(1_000, 10_000),
            broker_heartbeat_miss_limit: BROKER_HEARTBEAT_MISS_LIMIT,
            root_host_orphan_grace_ms: self.root_host_orphan_grace_ms.clamp(60_000, 1_800_000),
            broker_request_ttl_ms: self.broker_request_ttl_ms.clamp(10_000, 120_000),
            root_spool_max_bytes: self
                .root_spool_max_bytes
                .clamp(4 * 1024 * 1024, 256 * 1024 * 1024),
        }
    }
}
