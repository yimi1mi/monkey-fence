//! Terminal v1 数值上限(canonical spec 附录 A2;T3b,Issue #30)。
//!
//! 全部参数集中于此;允许范围与 hard cap 以代码表达,配置值越界时
//! 钳制到范围内(不静默接受越界值)。`frame_max_bytes` 与 resize 边界
//! 为 fixed,不可配置。派生值(pause/resume 水位、input burst、resize
//! 合并窗口、renew_after)不允许独立配置。

/// 双向 binary frame 的固定上限(256 KiB;超出 → 协议 4413)。
pub const FRAME_MAX_BYTES: usize = 256 * 1024;

/// resize 列数固定边界。
pub const RESIZE_COLS_MIN: u16 = 2;
pub const RESIZE_COLS_MAX: u16 = 500;
/// resize 行数固定边界。
pub const RESIZE_ROWS_MIN: u16 = 2;
pub const RESIZE_ROWS_MAX: u16 = 300;

/// attach 必须在升级后此时限内到达,否则 4400 关闭(附录 A2)。
pub const ATTACH_TIMEOUT_MS: u64 = 5_000;
/// terminal WS server 发起 ping 的周期。
pub const TERMINAL_WS_PING_INTERVAL_MS: u64 = 20_000;
/// terminal WS 空闲超时。
pub const TERMINAL_WS_IDLE_TIMEOUT_MS: u64 = 90_000;

/// 可配置的 Terminal v1 上限(附录 A2)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLimits {
    /// 单客户端未确认输出字节预算(pause=75%、resume=25% 水位派生)。
    pub outstanding_output_max_bytes: usize,
    /// 慢客户端宽限;超时 → 4409 关闭。
    pub slow_client_grace_ms: u64,
    /// 每 Agent Session 内存 replay ring 容量。
    pub replay_ring_max_bytes: usize,
    /// PTY reader 因客户端反压可阻塞的工程上限(不可配,仅常量披露)。
    pub pty_drain_max_block_ms: u64,
}

impl Default for TerminalLimits {
    fn default() -> Self {
        Self {
            outstanding_output_max_bytes: 8 * 1024 * 1024,
            slow_client_grace_ms: 30_000,
            replay_ring_max_bytes: 16 * 1024 * 1024,
            pty_drain_max_block_ms: 10,
        }
    }
}

impl TerminalLimits {
    /// pause 水位(75% × outstanding 预算;附录 A2 派生)。
    pub fn pause_watermark_bytes(&self) -> usize {
        self.outstanding_output_max_bytes / 4 * 3
    }

    /// resume 水位(25% × outstanding 预算)。
    pub fn resume_watermark_bytes(&self) -> usize {
        self.outstanding_output_max_bytes / 4
    }

    /// 越界钳制到允许范围(1–32 MiB / 5–120 s / 1–64 MiB)。
    pub fn clamp(&self) -> Self {
        let clamp_val = |v: usize, lo: usize, hi: usize| v.clamp(lo, hi);
        Self {
            outstanding_output_max_bytes: clamp_val(
                self.outstanding_output_max_bytes,
                1024 * 1024,
                32 * 1024 * 1024,
            ),
            slow_client_grace_ms: self.slow_client_grace_ms.clamp(5_000, 120_000),
            replay_ring_max_bytes: clamp_val(
                self.replay_ring_max_bytes,
                1024 * 1024,
                64 * 1024 * 1024,
            ),
            pty_drain_max_block_ms: self.pty_drain_max_block_ms,
        }
    }
}
