//! T3c 契约(Issue #31):resize 单调 seq、边界与合并窗口(§8.4/A2)。

use mf_terminal::writer_lease::{ResizeCoalescer, ResizeDecision};

#[test]
fn coalescing_keeps_latest_in_window() {
    let mut coalescer = ResizeCoalescer::new(10);
    // 一连串拖拽尺寸进入同一合并窗口(100ms):只保留最新
    assert_eq!(coalescer.submit(1, 80, 24), ResizeDecision::Superseded);
    assert_eq!(coalescer.submit(2, 100, 30), ResizeDecision::Superseded);
    assert_eq!(coalescer.submit(3, 120, 40), ResizeDecision::Superseded);
    assert_eq!(coalescer.submit(4, 160, 50), ResizeDecision::Superseded);
    assert_eq!(coalescer.flush(), Some((4, 160, 50)));
}

#[test]
fn stale_resize_seq_dropped_after_apply() {
    let mut coalescer = ResizeCoalescer::new(10);
    coalescer.submit(10, 100, 30);
    assert_eq!(coalescer.flush(), Some((10, 100, 30)));
    // 迟到的旧 seq(乱序网络)不影响 PTY
    assert_eq!(coalescer.submit(9, 90, 20), ResizeDecision::DroppedStale);
    assert_eq!(coalescer.submit(10, 95, 25), ResizeDecision::DroppedStale);
    assert_eq!(coalescer.flush(), None);
}

#[test]
fn bounds_are_fixed_and_fail_closed() {
    let mut coalescer = ResizeCoalescer::new(10);
    // cols 2–500、rows 2–300(§A2 fixed)
    assert_eq!(coalescer.submit(1, 1, 30), ResizeDecision::InvalidBounds);
    assert_eq!(coalescer.submit(1, 501, 30), ResizeDecision::InvalidBounds);
    assert_eq!(coalescer.submit(1, 100, 1), ResizeDecision::InvalidBounds);
    assert_eq!(coalescer.submit(1, 100, 301), ResizeDecision::InvalidBounds);
    // 全部被拒:窗口为空
    assert_eq!(coalescer.flush(), None);
    // 合法边界值接受
    assert_eq!(coalescer.submit(2, 2, 2), ResizeDecision::Superseded);
    assert_eq!(coalescer.flush(), Some((2, 2, 2)));
}
