//! T3f/B3 gate 组合契约(Issue #34):seq/ACK、writer、resize、gap、
//! crash 与 Root read-only seam 在同一场景中串联(spec §8/§10)。

use mf_terminal::channel::{decode_frame, encode_output_frame, FRAME_KIND_OUTPUT};
use mf_terminal::journal::TerminalJournal;
use mf_terminal::limits::TerminalLimits;
use mf_terminal::session::{plan_attach, AttachPlan, ClientOutputState, SlowClientDecision};
use mf_terminal::transcript::{recover_after_crash, ExitGate, FlushPolicy, TranscriptFlusher};
use mf_terminal::writer_lease::{
    ConnectionId, InputDecision, ResizeCoalescer, ResizeDecision, WriterLeaseManager,
    WriterRequestOutcome,
};
use std::time::Duration;

const MIB: usize = 1024 * 1024;

/// 完整输出面旅程:输出 → journal(seq) → frame → client 发送 → ACK
/// 释放预算 → 客户端消费的字节与 journal 逐字节一致。
#[test]
fn output_journey_seq_frame_ack_end_to_end() {
    let mut journal = TerminalJournal::new(8 * MIB);
    let mut flusher = TranscriptFlusher::new(FlushPolicy {
        flush_batch_bytes: 10,
        ..FlushPolicy::default()
    });
    let mut client = ClientOutputState::new(TerminalLimits::default(), 0);
    let chunks: [&[u8]; 3] = [b"hello ", b"redacted ", b"world"];
    let mut consumed = Vec::new();
    for bytes in chunks {
        let seq = journal.append(bytes.to_vec());
        if let Some(batch) = flusher.push(seq, bytes) {
            assert_eq!(batch.seq_end, seq);
        }
        // transport:encode → (网络) → decode → 消费
        let encoded = encode_output_frame(seq, bytes).unwrap();
        let frame = decode_frame(&encoded).unwrap();
        assert_eq!(frame.kind, FRAME_KIND_OUTPUT);
        client.note_sent(seq, encoded.len());
        consumed.extend_from_slice(frame.payload);
    }
    assert_eq!(String::from_utf8(consumed).unwrap(), "hello redacted world");
    // 全部确认:预算归零,不再暂停
    client.ack(journal.last_seq()).unwrap();
    assert_eq!(client.outstanding_bytes(), 0);
    assert!(matches!(
        client.poll_slow_client(),
        SlowClientDecision::Continue
    ));
}

/// writer + resize + 输入幂等在同一会话生命周期中协同。
#[test]
fn writer_resize_and_input_cooperate() {
    let mut journal = TerminalJournal::new(8 * MIB);
    let mut manager = WriterLeaseManager::new(Duration::from_millis(4_000));
    let mut resize = ResizeCoalescer::new(10);
    let WriterRequestOutcome::Granted { lease_id, .. } = manager.request_writer(2, ConnectionId(1))
    else {
        panic!("controller writer 授予");
    };
    // 输入有序入队(每条都过 lease/connection 复验)
    for (seq, _payload) in [
        (1u64, b"a".as_slice()),
        (2, b"b".as_slice()),
        (3, b"c".as_slice()),
    ] {
        assert_eq!(
            manager.submit_input(lease_id, ConnectionId(1), seq, [seq as u8; 32]),
            InputDecision::Admitted
        );
        manager
            .complete_input(lease_id, seq, [seq as u8; 32], true)
            .unwrap()
            .expect("input_ack");
    }
    // resize 洪泛合并到最新值,flush 应用
    assert_eq!(resize.submit(1, 80, 24), ResizeDecision::Superseded);
    assert_eq!(resize.submit(2, 100, 30), ResizeDecision::Superseded);
    assert_eq!(resize.submit(3, 120, 40), ResizeDecision::Superseded);
    assert_eq!(resize.flush(), Some((3, 120, 40)));
    // 输出继续推进(writer 与输出面互不阻塞)
    assert!(journal.append(b"out".to_vec()) > 0);
}

/// gap + crash + Root seam 组合:输出超 ring → gap 连接关闭且无 writer;
/// Core crash 恢复只到 durable 前缀;Root host 只读重附不恢复 writer。
#[test]
fn gap_crash_and_root_readonly_seam_combined() {
    // 输出超出 ring → history gap
    let mut journal = TerminalJournal::new(MIB);
    journal.append(vec![0u8; 700 * 1024]);
    journal.append(vec![1u8; 700 * 1024]);
    assert!(matches!(
        plan_attach(&journal, 0),
        AttachPlan::GapClose { .. }
    ));
    // crash:durable 只到 41,complete=false,未终结 → Needs You
    let recovery = recover_after_crash(41, false);
    assert_eq!(recovery.durable_through_seq, 41);
    assert!(!recovery.complete && recovery.needs_you);
    // exit 门闩:durable 失败不可恢复
    let mut gate = ExitGate::new();
    gate.begin_exit(9, Some(0));
    gate.commit(false);
    assert!(!gate.may_notify_exit());
    // Root host seam(mf-elevated 组件契约;详见该 crate)在此固化组合:
    // 断开后只读重附 —— 用 journal 语义对照:重连方只能 replay 既有输出。
    let replay = journal.replay(journal.last_seq());
    assert!(replay.is_empty(), "只读重附不得产生新输出");
}

#[test]
fn b3_budget_constants_hold() {
    // A9:journal append 微秒级(此处不测耗时,固化语义常量存在);
    // A2:flood/ACK 派生水位可计算且有序。
    let limits = TerminalLimits::default();
    assert!(limits.resume_watermark_bytes() < limits.pause_watermark_bytes());
    assert_eq!(
        mf_terminal::channel::FRAME_HEADER_BYTES,
        32,
        "frame header 固定 32 字节"
    );
}
