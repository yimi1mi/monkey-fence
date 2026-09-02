//! T3b 契约(Issue #30;附录 A2):frame 上限与 limits 钳制。

use mf_terminal::channel::{encode_frame, FrameProblem, FRAME_KIND_OUTPUT};
use mf_terminal::limits::{
    TerminalLimits, FRAME_MAX_BYTES, RESIZE_COLS_MAX, RESIZE_COLS_MIN, RESIZE_ROWS_MAX,
    RESIZE_ROWS_MIN,
};

const MIB: usize = 1024 * 1024;

#[test]
fn frame_max_bytes_is_fixed_256k() {
    assert_eq!(FRAME_MAX_BYTES, 256 * 1024);
    // 头部 + payload 恰好等于上限:合法
    let payload = vec![0u8; FRAME_MAX_BYTES - 32];
    assert!(encode_frame(FRAME_KIND_OUTPUT, 0, 1, [0u8; 16], &payload).is_ok());
    // 超 1 字节:拒绝
    let oversized = vec![0u8; FRAME_MAX_BYTES - 31];
    assert_eq!(
        encode_frame(FRAME_KIND_OUTPUT, 0, 1, [0u8; 16], &oversized),
        Err(FrameProblem::TooLarge {
            len: FRAME_MAX_BYTES + 1,
            max: FRAME_MAX_BYTES
        })
    );
}

#[test]
fn resize_bounds_are_fixed() {
    assert_eq!((RESIZE_COLS_MIN, RESIZE_COLS_MAX), (2, 500));
    assert_eq!((RESIZE_ROWS_MIN, RESIZE_ROWS_MAX), (2, 300));
}

#[test]
fn out_of_range_limits_are_clamped() {
    let clamped = TerminalLimits {
        outstanding_output_max_bytes: 512 * MIB,
        slow_client_grace_ms: 1,
        replay_ring_max_bytes: 1,
        pty_drain_max_block_ms: 10,
    }
    .clamp();
    assert_eq!(clamped.outstanding_output_max_bytes, 32 * MIB);
    assert_eq!(clamped.slow_client_grace_ms, 5_000);
    assert_eq!(clamped.replay_ring_max_bytes, MIB);
}

#[test]
fn in_range_limits_pass_through() {
    let limits = TerminalLimits {
        outstanding_output_max_bytes: 4 * MIB,
        slow_client_grace_ms: 45_000,
        replay_ring_max_bytes: 8 * MIB,
        pty_drain_max_block_ms: 10,
    };
    assert_eq!(limits.clamp(), limits);
}
