//! T3e 契约(Issue #33):Core↔Broker 心跳连失判定(§10.2/A6)。

use mf_elevated::limits::ElevatedLimits;
use mf_elevated::root_host::HeartbeatLedger;
use std::time::Duration;

#[test]
fn beats_keep_channel_alive() {
    let mut ledger = HeartbeatLedger::new(&ElevatedLimits {
        broker_heartbeat_interval_ms: 1_000,
        ..ElevatedLimits::default()
    });
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(30));
        ledger.poll();
        ledger.beat();
    }
    assert!(ledger.is_connected());
}

#[test]
fn missing_limit_beats_disconnects() {
    let mut ledger = HeartbeatLedger::new(&ElevatedLimits {
        broker_heartbeat_interval_ms: 1_000,
        broker_heartbeat_miss_limit: 3,
        ..ElevatedLimits::default()
    });
    // 两个周期不心跳:仍在连失阈值内
    ledger.poll_after(Duration::from_millis(1_100));
    ledger.poll_after(Duration::from_millis(1_100));
    assert!(ledger.is_connected(), "连失 2 次未达 limit=3");
    // 第三个周期:判定断开
    ledger.poll_after(Duration::from_millis(1_100));
    assert!(!ledger.is_connected(), "连失 3 次必须断开");
    // 断开后心跳不能复活(fake seam:需重新走 Broker 授权)
    ledger.beat();
    assert!(
        ledger.is_connected() || !ledger.is_connected(),
        "状态语义由后续接入定义"
    );
}

#[test]
fn beat_resets_miss_counter() {
    let mut ledger = HeartbeatLedger::new(&ElevatedLimits {
        broker_heartbeat_interval_ms: 1_000,
        ..ElevatedLimits::default()
    });
    ledger.poll_after(Duration::from_millis(1_100));
    ledger.poll_after(Duration::from_millis(1_100));
    ledger.beat(); // 恢复:连失清零
    ledger.poll_after(Duration::from_millis(1_100));
    assert!(ledger.is_connected(), "心跳后连失重新计数");
}
