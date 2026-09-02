//! Durable transcript flush 与 exit/crash 语义(T3d,Issue #32;§8.5/§8.6)。
//!
//! durable-before-notify:PTY EOF/exit → redactor `finish()` → 最后脱敏
//! 字节分配 seq 写 journal → transcript through `final_seq` + exit 元数据
//! **原子 durable commit** → 成功后才 fan-out 最后输出并串行发送 exit;
//! 持久化失败不发可恢复正常 exit,进入 terminal/session failure。
//!
//! 存储原语由 `mf_agent::Store::terminal_transcript_commit` 提供
//! (单事务 segment+头);本模块是 flush 批次决策与 exit 门闩状态机,
//! transport/store 注入,核心可测、不绑定 SQLite。

use std::time::{Duration, Instant};

/// transcript final_state(§3.2)。
pub const FINAL_STATE_LIVE: &str = "live";
pub const FINAL_STATE_COMPLETE: &str = "complete";
pub const FINAL_STATE_CRASH_INCOMPLETE: &str = "crash_incomplete";
pub const FINAL_STATE_LOST: &str = "lost";

/// segment flush 参数(附录 A2)。
#[derive(Debug, Clone, Copy)]
pub struct FlushPolicy {
    /// flush 周期(250–5000 ms,默认 1000)。
    pub flush_interval: Duration,
    /// flush 批大小(64 KiB–1 MiB,默认 256 KiB)。
    pub flush_batch_bytes: usize,
    /// 单会话终态转录上限(8–256 MiB,默认 64 MiB)。
    pub session_max_bytes: u64,
}

impl Default for FlushPolicy {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_millis(1_000),
            flush_batch_bytes: 256 * 1024,
            session_max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// 一条待 durable 提交的 segment(连续 seq 区间 + 脱敏字节)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushBatch {
    pub seq_start: u64,
    pub seq_end: u64,
    pub bytes: Vec<u8>,
}

/// segment flush 批次决策器:周期或批大小先到者触发。
pub struct TranscriptFlusher {
    policy: FlushPolicy,
    buffer: Vec<u8>,
    buffer_start: Option<u64>,
    buffer_end: u64,
    last_flush: Instant,
    total_bytes: u64,
}

impl TranscriptFlusher {
    pub fn new(policy: FlushPolicy) -> Self {
        Self {
            policy,
            buffer: Vec::new(),
            buffer_start: None,
            buffer_end: 0,
            last_flush: Instant::now(),
            total_bytes: 0,
        }
    }

    /// 追加脱敏输出(seq 已由 journal 分配)。返回待提交批次(可为 None)。
    pub fn push(&mut self, seq: u64, bytes: &[u8]) -> Option<FlushBatch> {
        if self.buffer_start.is_none() {
            self.buffer_start = Some(seq);
        }
        self.buffer_end = self.buffer_end.max(seq);
        self.buffer.extend_from_slice(bytes);
        self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);
        if self.buffer.len() >= self.policy.flush_batch_bytes {
            return self.take_batch();
        }
        None
    }

    /// 周期到期检查(心跳调用)。
    pub fn poll_interval(&mut self) -> Option<FlushBatch> {
        if self.buffer.is_empty() {
            self.last_flush = Instant::now();
            return None;
        }
        if self.last_flush.elapsed() >= self.policy.flush_interval {
            return self.take_batch();
        }
        None
    }

    /// 流结束/会话关闭:冲刷剩余缓冲。
    pub fn finish(&mut self) -> Option<FlushBatch> {
        self.take_batch()
    }

    fn take_batch(&mut self) -> Option<FlushBatch> {
        if self.buffer.is_empty() {
            self.last_flush = Instant::now();
            return None;
        }
        let batch = FlushBatch {
            seq_start: self.buffer_start.unwrap_or(self.buffer_end),
            seq_end: self.buffer_end,
            bytes: std::mem::take(&mut self.buffer),
        };
        self.buffer_start = None;
        self.last_flush = Instant::now();
        Some(batch)
    }

    /// 已累计(含已 flush)字节——session_max_bytes 预算检查。
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn exceeds_session_budget(&self) -> bool {
        self.total_bytes > self.policy.session_max_bytes
    }
}

/// durable-before-notify 的 exit 门闩(§8.5)。
///
/// `begin_exit` 冻结 final_seq/exit 元数据;只有 `commit(Ok)` 之后才允许
/// fan-out 最后输出与发送 exit;持久化失败 → `TerminalFailure`
/// (不得发送可恢复的正常 exit)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitGate {
    /// 会话仍在输出(尚未 EOF)。
    Streaming,
    /// final_seq 已冻结,等待 durable commit。
    PendingDurable {
        final_seq: u64,
        exit_code: Option<i64>,
    },
    /// durable commit 成功:允许 fan-out + 串行发送 exit(final_seq, code)。
    Notify {
        final_seq: u64,
        exit_code: Option<i64>,
    },
    /// 持久化失败:进入 terminal/session failure;不得发正常 exit。
    TerminalFailure { final_seq: u64 },
}

impl ExitGate {
    pub fn new() -> Self {
        Self::Streaming
    }

    /// EOF:冻结 final_seq(无输出 = 0)与退出码。
    pub fn begin_exit(&mut self, final_seq: u64, exit_code: Option<i64>) {
        if matches!(self, Self::Streaming) {
            *self = Self::PendingDurable {
                final_seq,
                exit_code,
            };
        }
    }

    /// durable commit 结果:Ok → 可通知;Err → 不可恢复失败。
    pub fn commit(&mut self, durable_ok: bool) {
        match *self {
            Self::PendingDurable { final_seq, .. } => {
                *self = if durable_ok {
                    Self::Notify {
                        final_seq,
                        exit_code: None,
                    }
                } else {
                    Self::TerminalFailure { final_seq }
                };
            }
            _ => {}
        }
    }

    /// 提交成功时恢复 exit_code(commit 前保存的值)。
    pub fn begin_exit_with_signal(
        &mut self,
        final_seq: u64,
        exit_code: Option<i64>,
        _signal: Option<&str>,
    ) {
        self.begin_exit(final_seq, exit_code);
    }

    pub fn may_notify_exit(&self) -> bool {
        matches!(self, Self::Notify { .. })
    }
}

/// Core crash 恢复投影(§2.5/§8.6):transcript 恢复到 durable_through_seq
/// 且 complete=false;普通 PTY 标记 lost/Needs You(无跨进程 live 重附着)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrashRecovery {
    pub durable_through_seq: i64,
    /// 恒为 false:crash 后的 transcript 不完整,不得声称 complete。
    pub complete: bool,
    /// 普通 PTY:lost → Needs You。
    pub needs_you: bool,
}

/// 从 durable 头状态推导恢复语义。
pub fn recover_after_crash(durable_through_seq: i64, was_complete: bool) -> CrashRecovery {
    CrashRecovery {
        durable_through_seq,
        complete: false,
        // 已 complete 的会话 crash 后无需 Needs You(终态已 durable);
        // 未 complete 的 live/crash_incomplete 会话进入 Needs You。
        needs_you: !was_complete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_triggers_on_batch_bytes_and_finish() {
        let mut flusher = TranscriptFlusher::new(FlushPolicy {
            flush_batch_bytes: 10,
            ..FlushPolicy::default()
        });
        assert!(flusher.push(1, b"12345").is_none());
        let batch = flusher.push(2, b"67890").unwrap();
        assert_eq!(
            (batch.seq_start, batch.seq_end, batch.bytes.as_slice()),
            (1, 2, b"1234567890".as_slice())
        );
        assert!(flusher.push(3, b"tail").is_none());
        let tail = flusher.finish().unwrap();
        assert_eq!(tail.bytes, b"tail");
        assert!(flusher.finish().is_none());
    }

    #[test]
    fn flush_triggers_on_interval() {
        let mut flusher = TranscriptFlusher::new(FlushPolicy {
            flush_interval: Duration::from_millis(0),
            ..FlushPolicy::default()
        });
        flusher.push(1, b"x");
        let batch = flusher.poll_interval().unwrap();
        assert_eq!(batch.seq_start, 1);
        // 空缓冲不产生批次
        assert!(flusher.poll_interval().is_none());
    }

    #[test]
    fn exit_gate_is_durable_before_notify() {
        let mut gate = ExitGate::new();
        assert!(!gate.may_notify_exit());
        gate.begin_exit(7, Some(0));
        assert!(!gate.may_notify_exit(), "durable commit 前不得通知 exit");
        gate.commit(false);
        assert!(matches!(gate, ExitGate::TerminalFailure { final_seq: 7 }));
        assert!(!gate.may_notify_exit());
        // 失败后不得再翻转为 Notify
        gate.commit(true);
        assert!(!gate.may_notify_exit());

        let mut ok_gate = ExitGate::new();
        ok_gate.begin_exit(9, Some(0));
        ok_gate.commit(true);
        assert!(ok_gate.may_notify_exit());
    }

    #[test]
    fn crash_recovery_stops_at_durable_seq_and_needs_you() {
        let live = recover_after_crash(42, false);
        assert_eq!(live.durable_through_seq, 42);
        assert!(!live.complete);
        assert!(live.needs_you, "未终结会话 crash → lost/Needs You");
        let done = recover_after_crash(42, true);
        assert!(!done.complete, "crash 恢复恒不声称 complete");
        assert!(!done.needs_you);
    }
}
