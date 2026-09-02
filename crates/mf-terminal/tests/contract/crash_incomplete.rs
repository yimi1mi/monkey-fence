//! T3d 契约(Issue #32):Core crash 恢复语义(§2.5/§8.6)——恢复到
//! durable_through_seq 且 complete=false,普通 PTY lost/Needs You。

use mf_terminal::transcript::{
    recover_after_crash, ExitGate, FINAL_STATE_CRASH_INCOMPLETE, FINAL_STATE_LOST,
};

#[test]
fn crash_recovery_restores_only_durable_prefix() {
    // crash 前会话仍在输出(live),durable 只到 seq 41;42+ 的 journal
    // 字节随进程消失。
    let recovery = recover_after_crash(41, false);
    assert_eq!(recovery.durable_through_seq, 41);
    assert!(!recovery.complete, "crash 恢复不得声称 complete");
    assert!(recovery.needs_you, "未终结的普通 PTY → lost/Needs You");
}

#[test]
fn crashed_complete_session_does_not_need_you() {
    // 已终结会话 crash:终态 durable,无 Needs You
    let recovery = recover_after_crash(99, true);
    assert!(!recovery.complete);
    assert!(!recovery.needs_you);
}

#[test]
fn pending_exit_at_crash_is_terminal_failure_not_recoverable_exit() {
    // durable commit 未完成即 crash:恢复方读到的是 crash_incomplete,
    // 不是正常 exit——重放 ExitGate 语义:PendingDurable 不产生 exit。
    let mut gate = ExitGate::new();
    gate.begin_exit(55, Some(0));
    // (无 commit;进程消失)
    assert!(!gate.may_notify_exit());
    assert_eq!(FINAL_STATE_CRASH_INCOMPLETE, "crash_incomplete");
    assert_eq!(FINAL_STATE_LOST, "lost");
}

#[test]
fn durable_failure_never_turns_into_complete() {
    let mut gate = ExitGate::new();
    gate.begin_exit(10, Some(0));
    gate.commit(false);
    // 失败后即便再次 commit 成功(不该发生),门闩也不得翻转为可通知:
    gate.commit(true);
    assert!(!gate.may_notify_exit());
    assert!(matches!(gate, ExitGate::TerminalFailure { final_seq: 10 }));
}
