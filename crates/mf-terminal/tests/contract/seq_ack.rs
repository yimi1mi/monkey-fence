//! T3b 契约(Issue #30):journal seq/replay 与 per-client ACK 反压的
//! 组合行为(§8.2/§8.3)。

use mf_terminal::channel::{decode_frame, encode_output_frame};
use mf_terminal::journal::{AttachProblem, TerminalJournal};
use mf_terminal::limits::TerminalLimits;
use mf_terminal::session::{AckProblem, ClientOutputState, SlowClientDecision};

const MIB: usize = 1024 * 1024;

#[test]
fn empty_session_attach_sees_boundary_facts() {
    let journal = TerminalJournal::new(8 * MIB);
    let facts = journal.check_attach(0).expect("空会话 attach(0) 合法");
    assert_eq!(facts.next_seq, 1);
    assert_eq!(facts.last_seq, 0);
    assert_eq!(facts.first_available_seq, 1);
    assert!(journal.replay(0).is_empty());
}

#[test]
fn illegal_after_seq_rejected_and_frame_seq_round_trips() {
    let mut journal = TerminalJournal::new(8 * MIB);
    journal.append(b"chunk-a".to_vec());
    journal.append(b"chunk-b".to_vec());
    match journal.check_attach(3) {
        Err(AttachProblem::AfterSeqBeyondLast { requested, last }) => {
            assert_eq!((requested, last), (3, 2));
        }
        other => panic!("期望 AfterSeqBeyondLast,得到 {other:?}"),
    }
    // 合法 attach:replay 的每个 chunk 编码为 frame 后 seq 原样到达
    let facts = journal.check_attach(0).unwrap();
    assert_eq!(facts.last_seq, 2);
    for chunk in journal.replay(0) {
        let encoded = encode_output_frame(chunk.seq, &chunk.bytes).unwrap();
        let frame = decode_frame(&encoded).unwrap();
        assert_eq!(frame.seq, chunk.seq);
        assert_eq!(frame.payload, chunk.bytes.as_ref());
    }
}

#[test]
fn same_epoch_replay_but_new_pty_requires_new_epoch() {
    let mut journal = TerminalJournal::new(8 * MIB);
    journal.append(b"out-1".to_vec());
    journal.append(b"out-2".to_vec());
    // 同 epoch:断线重连(after_seq=1)增量 replay
    let replayed = journal.replay(1);
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].bytes.as_ref(), b"out-2");
    // 新 PTY:新 journal 必须携带不同 epoch,不得复用 seq 空间
    let mut successor = TerminalJournal::new(8 * MIB);
    assert_ne!(journal.epoch(), successor.epoch());
    assert_eq!(successor.check_attach(0).unwrap().first_available_seq, 1);
    successor.append(b"fresh".to_vec());
    assert_eq!(successor.last_seq(), 1, "新 epoch 的 seq 从 1 重新开始");
}

#[test]
fn ring_overflow_reports_history_gap_and_blocks_writer_hint() {
    let mut journal = TerminalJournal::new(MIB);
    journal.append(vec![1u8; 700 * 1024]); // seq 1
    journal.append(vec![2u8; 700 * 1024]); // seq 2:超限驱逐 seq 1
    assert_eq!(journal.first_available_seq(), 2);
    match journal.check_attach(0) {
        Err(AttachProblem::HistoryGap(gap)) => {
            assert_eq!(gap.first_available_seq, 2);
            assert_eq!(gap.last_seq, 2);
        }
        other => panic!("期望 HistoryGap,得到 {other:?}"),
    }
}

#[test]
fn ack_errors_close_protocol_but_reader_and_peer_unaffected() {
    let mut journal = TerminalJournal::new(8 * MIB);
    journal.append(b"data".to_vec());
    let mut slow = ClientOutputState::new(default_limits(), 0);
    let mut peer = ClientOutputState::new(default_limits(), 0);
    slow.note_sent(1, 4);
    peer.note_sent(1, 4);
    // 非法 ACK:高于已发最高 seq → 协议错误(transport 关闭该连接)
    assert_eq!(
        slow.ack(2),
        Err(AckProblem::BeyondHighestSent {
            through_seq: 2,
            highest_sent: 1
        })
    );
    // 重复/旧 ACK 幂等
    slow.ack(1).unwrap();
    slow.ack(1).unwrap();
    slow.ack(0).unwrap();
    assert_eq!(slow.outstanding_bytes(), 0);
    // 另一 client 与 journal(PTY reader 侧)完全不受影响
    assert_eq!(peer.outstanding_bytes(), 4);
    assert_eq!(journal.last_seq(), 1);
    journal.append(b"more".to_vec());
    assert_eq!(journal.last_seq(), 2);
}

#[test]
fn slow_client_times_out_4409_without_blocking_anyone() {
    let mut journal = TerminalJournal::new(8 * MIB);
    let mut slow = ClientOutputState::new(default_limits(), 0);
    let mut peer = ClientOutputState::new(default_limits(), 0);
    // 慢 client 挂起 7 条 1 MiB chunk(7 MiB ≥ 默认 pause 水位 6 MiB;
    // journal 照常 append,reader 不等待任何 ACK)
    for seq in 1..=7u64 {
        let bytes = vec![0u8; MIB];
        journal.append(bytes.clone());
        slow.note_sent(seq, bytes.len());
        peer.note_sent(seq, bytes.len());
    }
    assert!(slow.is_paused());
    // peer 正常 ACK 排空,永不暂停
    peer.ack(7).unwrap();
    assert!(matches!(
        peer.poll_slow_client(),
        SlowClientDecision::Continue
    ));
    // 慢 client 宽限耗尽 → 4409;journal/reader 无感
    slow.rewind_pause_since(std::time::Duration::from_secs(61));
    match slow.poll_slow_client() {
        SlowClientDecision::ShouldClose { close_code, .. } => assert_eq!(close_code, 4409),
        other => panic!("期望 ShouldClose,得到 {other:?}"),
    }
    assert_eq!(journal.last_seq(), 7);
}

fn default_limits() -> TerminalLimits {
    TerminalLimits::default()
}
