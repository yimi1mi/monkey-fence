//! T3b 契约(Issue #30;附录 A2):默认值与派生水位。

use mf_terminal::limits::TerminalLimits;

const MIB: usize = 1024 * 1024;

#[test]
fn defaults_match_appendix_a2() {
    let defaults = TerminalLimits::default();
    assert_eq!(defaults.outstanding_output_max_bytes, 8 * MIB);
    assert_eq!(defaults.slow_client_grace_ms, 30_000);
    assert_eq!(defaults.replay_ring_max_bytes, 16 * MIB);
    assert_eq!(defaults.pty_drain_max_block_ms, 10);
}

#[test]
fn derived_watermarks_follow_documented_ratios() {
    let limits = TerminalLimits::default();
    // pause=75%、resume=25%(附录 A2 派生比例)
    assert_eq!(limits.pause_watermark_bytes(), 6 * MIB);
    assert_eq!(limits.resume_watermark_bytes(), 2 * MIB);
    // 派生水位必须在预算内且有序
    assert!(limits.resume_watermark_bytes() < limits.pause_watermark_bytes());
    assert!(limits.pause_watermark_bytes() < limits.outstanding_output_max_bytes);
}
