//! Terminal 输出侧 per-client 状态(T3b,Issue #30;canonical spec §8.2)。
//!
//! 反压按客户端隔离:每个 attach 持有独立的 outstanding byte budget
//! 与 cumulative ACK 水位;ACK 只释放对应 client 的预算,绝不妨碍 PTY
//! reader 持续 drain(journal append 永不阻塞)或其它 client。慢客户端
//! 在 `slow_client_grace_ms` 内未排空 → 4409 关闭(由 transport 执行,
//! 本模块只给判定)。
//!
//! writer input、resize 与 WS transport 属后续 ticket(#31/#42)。

use std::collections::BTreeMap;
use std::time::Instant;

use crate::limits::TerminalLimits;

/// ACK 校验问题(§8.2)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AckProblem {
    /// ACK 高于该 client 已发送的最高 seq:协议错误,连接关闭。
    #[error("ack {through_seq} 超过已发送最高 seq {highest_sent}")]
    BeyondHighestSent { through_seq: u64, highest_sent: u64 },
}

/// 慢客户端判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowClientDecision {
    /// 正常(继续发送)。
    Continue,
    /// 已暂停发送;宽限期内继续等待 ACK。
    Paused { elapsed_ms: u128 },
    /// 宽限耗尽未排空 → transport 必须 4409 关闭该连接。
    ShouldClose {
        close_code: u16,
        outstanding_bytes: usize,
    },
}

/// 单个 attach 连接的输出状态。
///
/// 生命周期:attach(校验 after_seq、发 hello + replay)→ 每发一条
/// binary output 调 `note_sent` → 收到 `ack(through_seq)` 调 `ack`
/// → 发送循环用 `poll_slow_client` 决定继续/暂停/4409。
pub struct ClientOutputState {
    limits: TerminalLimits,
    attached_after: u64,
    highest_sent: u64,
    acked_through: u64,
    /// 已发送未确认的 (seq → bytes);cumulative ACK 释放前缀。
    outstanding: BTreeMap<u64, usize>,
    outstanding_bytes: usize,
    paused_since: Option<Instant>,
}

impl ClientOutputState {
    pub fn new(limits: TerminalLimits, attached_after: u64) -> Self {
        Self {
            limits: limits.clamp(),
            attached_after,
            highest_sent: attached_after,
            acked_through: attached_after,
            outstanding: BTreeMap::new(),
            outstanding_bytes: 0,
            paused_since: None,
        }
    }

    pub fn attached_after(&self) -> u64 {
        self.attached_after
    }

    pub fn highest_sent(&self) -> u64 {
        self.highest_sent
    }

    pub fn acked_through(&self) -> u64 {
        self.acked_through
    }

    pub fn outstanding_bytes(&self) -> usize {
        self.outstanding_bytes
    }

    pub fn is_paused(&self) -> bool {
        self.paused_since.is_some()
    }

    /// 记录一条已发送的 binary output(seq、字节数)。达到 pause 水位
    /// (75%)即暂停该 client 的后续发送(不阻塞 journal/reader)。
    pub fn note_sent(&mut self, seq: u64, byte_len: usize) {
        self.highest_sent = self.highest_sent.max(seq);
        *self.outstanding.entry(seq).or_insert(0) += byte_len;
        self.outstanding_bytes += byte_len;
        if self.outstanding_bytes >= self.limits.pause_watermark_bytes()
            && self.paused_since.is_none()
        {
            self.paused_since = Some(Instant::now());
        }
    }

    /// cumulative ACK(§8.2):只确认已发送且消费的连续 seq。
    /// - 高于本 client 已发最高 seq → 协议错误(关闭);
    /// - 旧/重复 ACK 幂等忽略;
    /// - 正常 ACK 释放至 through_seq 的 outstanding 预算,跌回 resume
    ///   水位(25%)即恢复发送。
    pub fn ack(&mut self, through_seq: u64) -> Result<(), AckProblem> {
        if through_seq > self.highest_sent {
            return Err(AckProblem::BeyondHighestSent {
                through_seq,
                highest_sent: self.highest_sent,
            });
        }
        if through_seq <= self.acked_through {
            return Ok(()); // 重复/旧 ACK:幂等
        }
        let mut released = 0usize;
        while let Some((&seq, &len)) = self.outstanding.iter().next() {
            if seq > through_seq {
                break;
            }
            released += len;
            self.outstanding.remove(&seq);
        }
        self.outstanding_bytes = self.outstanding_bytes.saturating_sub(released);
        self.acked_through = through_seq;
        if self.outstanding_bytes <= self.limits.resume_watermark_bytes() {
            self.paused_since = None;
        }
        Ok(())
    }

    /// 测试/诊断注入:把暂停时刻拨回过去以模拟宽限耗尽。生产不得调用
    /// (只影响本 client 的宽限判定,不触碰 journal/reader)。
    pub fn rewind_pause_since(&mut self, ago: std::time::Duration) {
        if let Some(since) = self.paused_since.as_mut() {
            *since = Instant::now() - ago;
        }
    }

    /// 发送循环的慢客户端决策(调用方按心跳/写前轮询)。
    pub fn poll_slow_client(&self) -> SlowClientDecision {
        match self.paused_since {
            None => SlowClientDecision::Continue,
            Some(since) => {
                let elapsed = since.elapsed();
                if elapsed.as_millis() >= u128::from(self.limits.slow_client_grace_ms) {
                    SlowClientDecision::ShouldClose {
                        close_code: 4409,
                        outstanding_bytes: self.outstanding_bytes,
                    }
                } else {
                    SlowClientDecision::Paused {
                        elapsed_ms: elapsed.as_millis(),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(outstanding: usize, grace_ms: u64) -> TerminalLimits {
        TerminalLimits {
            outstanding_output_max_bytes: outstanding,
            slow_client_grace_ms: grace_ms,
            replay_ring_max_bytes: 1024 * 1024,
            pty_drain_max_block_ms: 10,
        }
        .clamp()
    }

    #[test]
    fn ack_releases_budget_for_this_client_only() {
        let mut a = ClientOutputState::new(limits(1024, 30_000), 0);
        let mut b = ClientOutputState::new(limits(1024, 30_000), 0);
        a.note_sent(1, 100);
        a.note_sent(2, 100);
        b.note_sent(1, 100);
        a.ack(1).unwrap();
        assert_eq!(a.outstanding_bytes(), 100);
        assert_eq!(b.outstanding_bytes(), 100, "A 的 ACK 不得释放 B 的预算");
        a.ack(2).unwrap();
        assert_eq!(a.outstanding_bytes(), 0);
        assert_eq!(b.outstanding_bytes(), 100);
    }

    #[test]
    fn ack_beyond_highest_sent_is_protocol_error() {
        let mut client = ClientOutputState::new(limits(1024, 30_000), 0);
        client.note_sent(1, 10);
        match client.ack(2) {
            Err(AckProblem::BeyondHighestSent {
                through_seq,
                highest_sent,
            }) => assert_eq!((through_seq, highest_sent), (2, 1)),
            other => panic!("期望 BeyondHighestSent,得到 {other:?}"),
        }
    }

    #[test]
    fn duplicate_and_stale_acks_are_idempotent() {
        let mut client = ClientOutputState::new(limits(1024, 30_000), 0);
        client.note_sent(1, 50);
        client.ack(1).unwrap();
        client.ack(1).unwrap();
        client.ack(0).unwrap();
        assert_eq!(client.outstanding_bytes(), 0);
        assert_eq!(client.acked_through(), 1);
    }

    #[test]
    fn pause_at_high_watermark_resume_at_low() {
        // outstanding=4 MiB(clamp 下限之上)→ pause=3 MiB、resume=1 MiB
        let mib = 1024 * 1024;
        let mut client = ClientOutputState::new(limits(4 * mib, 30_000), 0);
        client.note_sent(1, 2 * mib);
        assert!(!client.is_paused());
        client.note_sent(2, mib + mib / 2); // 3.5 MiB ≥ 3 MiB → pause
        assert!(client.is_paused());
        assert!(matches!(
            client.poll_slow_client(),
            SlowClientDecision::Paused { .. }
        ));
        client.ack(1).unwrap(); // 剩 1.5 MiB > 1 MiB:仍暂停
        assert!(client.is_paused());
        client.ack(2).unwrap(); // 0 ≤ 1 MiB → resume
        assert!(!client.is_paused());
        assert!(matches!(
            client.poll_slow_client(),
            SlowClientDecision::Continue
        ));
    }

    #[test]
    fn exhausted_grace_closes_4409() {
        let mib = 1024 * 1024;
        let mut client = ClientOutputState::new(limits(2 * mib, 30_000), 0);
        client.note_sent(1, 2 * mib); // ≥ pause 水位(1.5 MiB)
        assert!(client.is_paused());
        // 宽限期内:仅暂停
        assert!(matches!(
            client.poll_slow_client(),
            SlowClientDecision::Paused { .. }
        ));
        client.rewind_pause_since(std::time::Duration::from_secs(31));
        match client.poll_slow_client() {
            SlowClientDecision::ShouldClose {
                close_code,
                outstanding_bytes: _,
            } => assert_eq!(close_code, 4409),
            other => panic!("期望 ShouldClose,得到 {other:?}"),
        }
    }
}
