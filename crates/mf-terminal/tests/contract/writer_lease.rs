//! T3c 契约(Issue #31):writer lease 生命周期与 L-INPUT 绑定(§8.4)。

use mf_terminal::writer_lease::{
    ConnectionId, InputDecision, WriterLeaseManager, WriterReleaseOutcome, WriterRenewOutcome,
    WriterRequestOutcome, WriterRevokeReason,
};
use std::time::Duration;

fn manager() -> WriterLeaseManager {
    WriterLeaseManager::new(Duration::from_millis(4_000))
}

#[test]
fn only_one_writer_per_epoch_and_observer_never_writes() {
    let mut manager = manager();
    let WriterRequestOutcome::Granted { lease_id, .. } = manager.request_writer(3, ConnectionId(1))
    else {
        panic!("controller 连接应获写权");
    };
    // 同 epoch 的第二个连接(Observer)申请:拒绝
    assert_eq!(
        manager.request_writer(3, ConnectionId(2)),
        WriterRequestOutcome::Denied
    );
    // Observer 直接持 lease 提交输入:连接绑定复验失败,永不入队
    assert_eq!(
        manager.submit_input(lease_id, ConnectionId(2), 1, [1; 32]),
        InputDecision::NoWriter {
            reason: WriterRevokeReason::ConnectionClosed
        }
    );
    // Controller 自身正常
    assert_eq!(
        manager.submit_input(lease_id, ConnectionId(1), 1, [1; 32]),
        InputDecision::Admitted
    );
}

#[test]
fn takeover_new_controller_writes_old_cannot() {
    let mut manager = manager();
    let WriterRequestOutcome::Granted { lease_id: old, .. } =
        manager.request_writer(3, ConnectionId(1))
    else {
        panic!()
    };
    assert_eq!(
        manager.submit_input(old, ConnectionId(1), 1, [1; 32]),
        InputDecision::Admitted
    );
    let WriterRequestOutcome::Granted { lease_id: new, .. } =
        manager.request_writer(4, ConnectionId(2))
    else {
        panic!("takeover 必须授予新 Controller")
    };
    // 旧 Controller 的 lease 在新 epoch 下失效:提交被拒
    assert_eq!(
        manager.submit_input(old, ConnectionId(1), 2, [2; 32]),
        InputDecision::NoWriter {
            reason: WriterRevokeReason::Takeover
        }
    );
    // 新 Controller 可写
    assert_eq!(
        manager.submit_input(new, ConnectionId(2), 1, [9; 32]),
        InputDecision::Admitted
    );
}

#[test]
fn release_then_regrant_and_idempotent_release() {
    let mut manager = manager();
    let WriterRequestOutcome::Granted { lease_id, .. } = manager.request_writer(1, ConnectionId(5))
    else {
        panic!()
    };
    assert_eq!(manager.release(lease_id), WriterReleaseOutcome::Released);
    assert_eq!(
        manager.release(lease_id),
        WriterReleaseOutcome::AlreadyReleased
    );
    // 新连接可立即获授;旧 lease 的重复 release 不误伤
    let WriterRequestOutcome::Granted { lease_id: next, .. } =
        manager.request_writer(1, ConnectionId(6))
    else {
        panic!()
    };
    assert_eq!(
        manager.release(lease_id),
        WriterReleaseOutcome::AlreadyReleased
    );
    assert_eq!(
        manager.renew(next, 1),
        WriterRenewOutcome::Renewed {
            expires_at_ms_since_start: 4_000
        }
    );
}
