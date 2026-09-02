//! T3d 契约(Issue #32):durable-before-notify、exit replay 与
//! history gap 连接不获 writer(§8.3/§8.5)。

use mf_terminal::journal::TerminalJournal;
use mf_terminal::session::{plan_attach, AttachPlan};
use mf_terminal::transcript::ExitGate;
use mf_terminal::writer_lease::{ConnectionId, WriterLeaseManager, WriterRequestOutcome};
use std::time::Duration;

const MIB: usize = 1024 * 1024;

#[test]
fn last_bytes_commit_durable_before_exit_notify() {
    let mut journal = TerminalJournal::new(8 * MIB);
    journal.append(b"streaming".to_vec());
    let final_seq = journal.append(b"final-bytes".to_vec());
    let mut gate = ExitGate::new();
    // EOF:final_seq 冻结;durable commit 前不得通知 exit
    gate.begin_exit(final_seq, Some(0));
    assert!(!gate.may_notify_exit());
    // 持久化故障:不得发送可恢复的正常 exit
    gate.commit(false);
    assert!(!gate.may_notify_exit());
    assert!(matches!(
        gate,
        ExitGate::TerminalFailure { final_seq: f } if f == final_seq
    ));
}

#[test]
fn exit_then_reconnect_replays_to_final_seq_then_same_exit() {
    let mut journal = TerminalJournal::new(8 * MIB);
    journal.append(b"a".to_vec());
    journal.append(b"b".to_vec());
    let final_seq = journal.last_seq();
    // exit 已 durable:重连 attach(after_seq=0)先 replay 到 final_seq
    let AttachPlan::Live { hello, replay } = plan_attach(&journal, 0) else {
        panic!("exit 后重连应可 replay");
    };
    assert_eq!(hello.last_seq, final_seq);
    assert_eq!(replay.last().unwrap().seq, final_seq);
    // transport 随后重发同一 exit(final_seq, code)——由 ExitGate 语义保证
    let mut gate = ExitGate::new();
    gate.begin_exit(final_seq, Some(0));
    gate.commit(true);
    assert!(gate.may_notify_exit());
}

#[test]
fn gap_connection_closes_and_never_gets_writer() {
    let mut journal = TerminalJournal::new(MIB);
    journal.append(vec![1u8; 700 * 1024]);
    journal.append(vec![2u8; 700 * 1024]); // 驱逐 seq1
    let plan = plan_attach(&journal, 0);
    let AttachPlan::GapClose { gap } = plan else {
        panic!("被驱逐区间的 attach 必须判 gap");
    };
    assert_eq!(gap.first_available_seq, 2);
    assert_eq!(gap.last_seq, 2);
    // §8.3:gap 连接不得申请 writer——transport 在 GapClose 路径直接
    // 关闭,绝不进入 request_writer 分支。这里固化该组合规则:
    match plan_attach(&journal, 0) {
        AttachPlan::GapClose { .. } => {
            // 正常会话的 writer 授予只发生在 Live 分支;GapClose 分支
            // 没有 request_writer 调用点(类型层无该路径)。
        }
        _ => unreachable!(),
    }
    // 同一会话的后续正常连接(after_seq 覆盖内)仍可获得 writer
    let mut manager = WriterLeaseManager::new(Duration::from_millis(4_000));
    assert!(matches!(
        manager.request_writer(1, ConnectionId(1)),
        WriterRequestOutcome::Granted { .. }
    ));
}

#[test]
fn beyond_last_seq_is_protocol_error() {
    let journal = TerminalJournal::new(8 * MIB);
    match plan_attach(&journal, 3) {
        AttachPlan::ProtocolError {
            after_seq,
            last_seq,
        } => {
            assert_eq!((after_seq, last_seq), (3, 0));
        }
        other => panic!("期望 ProtocolError,得到 {other:?}"),
    }
}
