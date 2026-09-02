//! T3c 契约(Issue #31):input_seq digest 幂等与 input_ack 语义(§8.4)。

use mf_terminal::writer_lease::{
    ConnectionId, InputDecision, WriterLeaseManager, WriterRequestOutcome,
};
use std::time::Duration;

fn manager_with_writer() -> (WriterLeaseManager, [u8; 16]) {
    let mut manager = WriterLeaseManager::new(Duration::from_millis(4_000));
    let WriterRequestOutcome::Granted { lease_id, .. } = manager.request_writer(1, ConnectionId(1))
    else {
        panic!()
    };
    (manager, lease_id)
}

#[test]
fn duplicate_same_payload_returns_original_ack() {
    let (mut manager, lease) = manager_with_writer();
    let digest = [0xAB; 32];
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 1, digest),
        InputDecision::Admitted
    );
    let ack = manager
        .complete_input(lease, 1, digest, true)
        .unwrap()
        .expect("write_all 成功必须产生 input_ack");
    // 网络重发同 seq 同 payload:幂等,返回原 ack,不重复入队
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 1, digest),
        InputDecision::DuplicateAck { ack_id: ack }
    );
    // 后续 seq 正常推进
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 2, [2; 32]),
        InputDecision::Admitted
    );
}

#[test]
fn same_seq_different_payload_conflicts_and_revokes() {
    let (mut manager, lease) = manager_with_writer();
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 1, [1; 32]),
        InputDecision::Admitted
    );
    manager.complete_input(lease, 1, [1; 32], true).unwrap();
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 1, [2; 32]),
        InputDecision::Conflict
    );
    // lease 已撤销:同连接再无写入可能
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 2, [1; 32]),
        InputDecision::NoWriter {
            reason: mf_terminal::writer_lease::WriterRevokeReason::InputSeqConflict
        }
    );
}

#[test]
fn partial_or_failed_write_never_acks() {
    let (mut manager, lease) = manager_with_writer();
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 1, [1; 32]),
        InputDecision::Admitted
    );
    // write_all 失败:无 ack、writer 撤销
    assert!(manager.complete_input(lease, 1, [1; 32], false).is_err());
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 1, [1; 32]),
        InputDecision::NoWriter {
            reason: mf_terminal::writer_lease::WriterRevokeReason::Released
        }
    );
}

#[test]
fn unacked_input_is_never_auto_replayed() {
    let (mut manager, lease) = manager_with_writer();
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 1, [1; 32]),
        InputDecision::Admitted
    );
    // 未 complete(网络不确定):模块只提供只读提示,不存在重放入队路径
    assert_eq!(manager.pending_input_hint(), 1);
    // 重发同 seq 同 digest 但尚未 ack:仍幂等按 Duplicate 处理吗?
    // ——未 ack 的 seq 不在 acked 窗口,且 next_seq 已推进:乱序拒绝,
    // 绝不重复入队(§8.4:不自动重放未确认 input)。
    assert_eq!(
        manager.submit_input(lease, ConnectionId(1), 1, [1; 32]),
        InputDecision::OutOfOrder { expected_seq: 2 }
    );
}
