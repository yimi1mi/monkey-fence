//! T3e 契约(Issue #33):附录 A6 默认值、fixed 项与钳制。

use mf_elevated::limits::{ElevatedLimits, BROKER_HEARTBEAT_MISS_LIMIT};

#[test]
fn defaults_match_appendix_a6() {
    let defaults = ElevatedLimits::default();
    assert_eq!(defaults.broker_heartbeat_interval_ms, 2_000);
    assert_eq!(defaults.broker_heartbeat_miss_limit, 3);
    assert_eq!(defaults.root_host_orphan_grace_ms, 300_000);
    assert_eq!(defaults.broker_request_ttl_ms, 30_000);
    assert_eq!(defaults.root_spool_max_bytes, 32 * 1024 * 1024);
    assert_eq!(BROKER_HEARTBEAT_MISS_LIMIT, 3, "miss limit fixed");
}

#[test]
fn out_of_range_values_are_clamped_and_miss_limit_fixed() {
    let clamped = ElevatedLimits {
        broker_heartbeat_interval_ms: 1,
        broker_heartbeat_miss_limit: 99, // 必须被 fixed 值覆盖
        root_host_orphan_grace_ms: 1,
        broker_request_ttl_ms: 1,
        root_spool_max_bytes: 1,
    }
    .clamp();
    assert_eq!(clamped.broker_heartbeat_interval_ms, 1_000);
    assert_eq!(
        clamped.broker_heartbeat_miss_limit,
        BROKER_HEARTBEAT_MISS_LIMIT
    );
    assert_eq!(clamped.root_host_orphan_grace_ms, 60_000);
    assert_eq!(clamped.broker_request_ttl_ms, 10_000);
    assert_eq!(clamped.root_spool_max_bytes, 4 * 1024 * 1024);
}
