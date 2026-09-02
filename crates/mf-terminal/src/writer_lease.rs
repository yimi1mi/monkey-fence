//! Writer lease、input 幂等与 resize 合并(T3c,Issue #31;§8.4)。
//!
//! 单一 Controller 可写、Observer 永远只读:lease 绑定
//! `controller_epoch + connection + session`,不可转移。L-INPUT 语义:
//! 字节进入单线程有序 PTY 写队列时原子复验 Controller epoch 与 writer
//! lease;takeover 不撤销此前已线性化字节。网络不确定时**绝不自动重放**
//! 未确认 input。`input_ack` 只在底层 `write_all` 完整成功后产生。
//!
//! 本模块是 transport 无关的核心状态机;WS 帧解码(#42)与 GPUI 接线
//! (#34)在其后。全部内存态,Core 重启即失效,无持久格式。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::limits::{RESIZE_COLS_MAX, RESIZE_COLS_MIN, RESIZE_ROWS_MAX, RESIZE_ROWS_MIN};

/// WS 连接的进程内标识(transport 注入;重连即新值)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub u64);

/// writer lease 撤销原因(§8.6)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterRevokeReason {
    Released,
    Timeout,
    Takeover,
    ConnectionClosed,
    /// 同 input_seq 异 payload(`input_seq_conflict`,§8.4):撤销并关闭。
    InputSeqConflict,
}

/// `request_writer` 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterRequestOutcome {
    /// 新授予(或同连接幂等再授予同一 lease)。
    Granted {
        lease_id: [u8; 16],
        ttl_ms: u64,
        renew_after_ms: u64,
    },
    /// 同 epoch 下已有其它连接持有 writer(§8.1 `writer_denied.v1`)。
    Denied,
}

/// `writer_renew` 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterRenewOutcome {
    Renewed { expires_at_ms_since_start: u128 },
    Revoked { reason: WriterRevokeReason },
}

/// `release_writer` 幂等终态:重复 release 返回相同结果,不误伤其后
/// 新授予的 lease。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterReleaseOutcome {
    Released,
    AlreadyReleased,
}

/// input 提交决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDecision {
    /// 进入写队列;`write_all` 成功后调 `complete_input` 产生 input_ack。
    Admitted,
    /// 同 input_seq 同 payload digest 重发:幂等返回原 ack。
    DuplicateAck { ack_id: u64 },
    /// 同 input_seq 异 payload:撤销 writer 并关闭连接
    /// (`input_seq_conflict`,§8.4)。
    Conflict,
    /// 乱序(窗口外旧 seq 或跳 seq):不写入,返回期望 seq。
    OutOfOrder { expected_seq: u64 },
    /// 无有效 writer(未授予/已撤销/过期/连接不符)。
    NoWriter { reason: WriterRevokeReason },
}

/// 单会话的 writer lease 状态机。
pub struct WriterLeaseManager {
    ttl: Duration,
    /// 已撤销 lease 的墓碑(重复 release/renew 的幂等终态判定)。
    tombstones: VecDeque<([u8; 16], WriterRevokeReason)>,
    active: Option<ActiveWriterLease>,
}

struct ActiveWriterLease {
    lease_id: [u8; 16],
    controller_epoch: u64,
    connection: ConnectionId,
    expires_at: Instant,
    renew_after_at: Instant,
    input: InputDedupe,
}

/// 每 lease 的 input_seq 幂等记录(§8.4)。
struct InputDedupe {
    next_seq: u64,
    ack_seq: u64,
    /// 最近已 ack 的 (input_seq, digest, ack_id),有界滑窗。
    acked: VecDeque<(u64, [u8; 32], u64)>,
    next_ack_id: u64,
}

/// 滑窗容量:覆盖重放/重连场景的最近确认;更早的 seq 一律 OutOfOrder。
const ACK_WINDOW: usize = 64;

impl WriterLeaseManager {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl: ttl.max(Duration::from_millis(4_000)),
            tombstones: VecDeque::new(),
            active: None,
        }
    }

    /// ttl 的 60% 续租提示(附录 A2 派生,不可独立配置)。
    fn renew_after(&self) -> Duration {
        self.ttl.mul_f64(0.6)
    }

    fn revoke_active(
        &mut self,
        reason: WriterRevokeReason,
    ) -> Option<([u8; 16], WriterRevokeReason)> {
        let lease = self.active.take()?;
        self.push_tombstone(lease.lease_id, reason);
        Some((lease.lease_id, reason))
    }

    fn push_tombstone(&mut self, lease_id: [u8; 16], reason: WriterRevokeReason) {
        self.tombstones.push_back((lease_id, reason));
        while self.tombstones.len() > 64 {
            self.tombstones.pop_front();
        }
    }

    /// 过期检查(发送循环/心跳前调用)。
    pub fn poll_expiry(&mut self) -> Option<WriterRevokeReason> {
        if let Some(active) = self.active.as_ref() {
            if Instant::now() >= active.expires_at {
                return self
                    .revoke_active(WriterRevokeReason::Timeout)
                    .map(|(_, reason)| reason);
            }
        }
        None
    }

    /// 仅当前 Controller(epoch 匹配)可申请 writer。
    /// - 更高 controller_epoch 到来 → 旧 lease takeover 撤销后授予;
    /// - 同 epoch 已有其它连接的 writer → Denied;
    /// - 同连接重复申请 → 幂等返回同一 lease。
    pub fn request_writer(
        &mut self,
        controller_epoch: u64,
        connection: ConnectionId,
    ) -> WriterRequestOutcome {
        self.poll_expiry();
        if let Some(active) = self.active.as_mut() {
            if active.controller_epoch < controller_epoch {
                self.revoke_active(WriterRevokeReason::Takeover);
            } else if active.controller_epoch == controller_epoch {
                if active.connection == connection {
                    return WriterRequestOutcome::Granted {
                        lease_id: active.lease_id,
                        ttl_ms: self.ttl.as_millis() as u64,
                        renew_after_ms: self.renew_after().as_millis() as u64,
                    };
                }
                return WriterRequestOutcome::Denied;
            }
        }
        let lease_id = *uuid::Uuid::now_v7().as_bytes();
        let now = Instant::now();
        self.active = Some(ActiveWriterLease {
            lease_id,
            controller_epoch,
            connection,
            expires_at: now + self.ttl,
            renew_after_at: now + self.renew_after(),
            input: InputDedupe {
                next_seq: 1,
                ack_seq: 0,
                acked: VecDeque::new(),
                next_ack_id: 1,
            },
        });
        WriterRequestOutcome::Granted {
            lease_id,
            ttl_ms: self.ttl.as_millis() as u64,
            renew_after_ms: self.renew_after().as_millis() as u64,
        }
    }

    /// 续租复验 Controller epoch(§8.4):epoch 不匹配或已过期 → 撤销。
    /// 对已撤销 lease 的续租幂等返回其终态原因。
    pub fn renew(&mut self, lease_id: [u8; 16], controller_epoch: u64) -> WriterRenewOutcome {
        self.poll_expiry();
        let matches_active = self.active.as_ref().is_some_and(|active| {
            active.lease_id == lease_id && active.controller_epoch == controller_epoch
        });
        let takeover = self.active.as_ref().is_some_and(|active| {
            active.lease_id == lease_id && active.controller_epoch != controller_epoch
        });
        if takeover {
            self.revoke_active(WriterRevokeReason::Takeover);
            return WriterRenewOutcome::Revoked {
                reason: WriterRevokeReason::Takeover,
            };
        }
        if matches_active {
            let now = Instant::now();
            let ttl = self.ttl;
            let renew_after = self.renew_after();
            if let Some(active) = self.active.as_mut() {
                active.expires_at = now + ttl;
                active.renew_after_at = now + renew_after;
            }
            return WriterRenewOutcome::Renewed {
                expires_at_ms_since_start: ttl.as_millis(),
            };
        }
        // 未知 lease(从未授予/墓碑淘汰):按已释放终态幂等应答。
        let reason = self
            .tombstones
            .iter()
            .rev()
            .find(|(id, _)| *id == lease_id)
            .map(|(_, reason)| *reason)
            .unwrap_or(WriterRevokeReason::Released);
        WriterRenewOutcome::Revoked { reason }
    }

    /// 显式 release:幂等,不影响其后新 lease。
    pub fn release(&mut self, lease_id: [u8; 16]) -> WriterReleaseOutcome {
        if let Some(active) = self.active.as_ref() {
            if active.lease_id == lease_id {
                self.revoke_active(WriterRevokeReason::Released);
                return WriterReleaseOutcome::Released;
            }
        }
        WriterReleaseOutcome::AlreadyReleased
    }

    /// 连接关闭 → 该连接的 writer 撤销。
    pub fn connection_closed(&mut self, connection: ConnectionId) {
        if let Some(active) = self.active.as_ref() {
            if active.connection == connection {
                self.revoke_active(WriterRevokeReason::ConnectionClosed);
            }
        }
    }

    /// L-INPUT 提交:lease/connection 复验 + input_seq 幂等。
    /// `digest` 为 payload 的 SHA-256(调用方计算)。
    pub fn submit_input(
        &mut self,
        lease_id: [u8; 16],
        connection: ConnectionId,
        input_seq: u64,
        digest: [u8; 32],
    ) -> InputDecision {
        self.poll_expiry();
        let Some(active) = self.active.as_mut() else {
            // 无 active:按墓碑给出该 lease 的真实终态(诊断/协议上报),
            // 从未授予的 lease 按 Timeout 口径 fail-closed。
            let reason = self
                .tombstones
                .iter()
                .rev()
                .find(|(id, _)| *id == lease_id)
                .map(|(_, reason)| *reason)
                .unwrap_or(WriterRevokeReason::Timeout);
            return InputDecision::NoWriter { reason };
        };
        if active.lease_id != lease_id {
            return InputDecision::NoWriter {
                reason: WriterRevokeReason::Takeover,
            };
        }
        if active.connection != connection {
            return InputDecision::NoWriter {
                reason: WriterRevokeReason::ConnectionClosed,
            };
        }
        let decision = {
            let input = &mut active.input;
            if input_seq == input.next_seq {
                input.next_seq += 1;
                InputDecision::Admitted
            } else if let Some((_, acked_digest, ack_id)) = input
                .acked
                .iter()
                .rev()
                .find(|(seq, _, _)| *seq == input_seq)
            {
                if *acked_digest == digest {
                    InputDecision::DuplicateAck { ack_id: *ack_id }
                } else {
                    // 同 seq 异 payload:撤销 writer,transport 关闭连接
                    InputDecision::Conflict
                }
            } else {
                // 窗口外旧 seq 一律乱序(不写、不撤销)。
                InputDecision::OutOfOrder {
                    expected_seq: input.next_seq,
                }
            }
        };
        if matches!(decision, InputDecision::Conflict) {
            self.revoke_active(WriterRevokeReason::InputSeqConflict);
        }
        decision
    }

    /// 底层 `write_all` 完成:成功才产生 input_ack(§8.4);失败撤销
    /// writer(部分写/错误不得假装成功)。
    pub fn complete_input(
        &mut self,
        lease_id: [u8; 16],
        input_seq: u64,
        digest: [u8; 32],
        write_ok: bool,
    ) -> Result<Option<u64>, WriterRevokeReason> {
        let Some(active) = self.active.as_mut() else {
            return Err(WriterRevokeReason::Timeout);
        };
        if active.lease_id != lease_id {
            return Err(WriterRevokeReason::Takeover);
        }
        if !write_ok {
            // 部分写/错误:绝不 ACK;按服务端主动终止撤销(terminal
            // problem 的具体形态由 transport 层给出)。
            self.revoke_active(WriterRevokeReason::Released);
            return Err(WriterRevokeReason::Released);
        }
        let ack_id = active.input.next_ack_id;
        active.input.next_ack_id += 1;
        active.input.ack_seq = input_seq;
        active.input.acked.push_back((input_seq, digest, ack_id));
        while active.input.acked.len() > ACK_WINDOW {
            active.input.acked.pop_front();
        }
        Ok(Some(ack_id))
    }

    /// 当前未确认(已 Admitted 未 complete)的 input 数量提示;调用方
    /// 只能用它做诊断/用户提示,不得自动重放(§8.4)。
    pub fn pending_input_hint(&self) -> u64 {
        match self.active.as_ref() {
            Some(active) => active.input.next_seq - 1 - active.input.ack_seq,
            None => 0,
        }
    }
}

/// resize 决策(§8.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeDecision {
    /// 应用到 PTY(真实 `PtyMaster::resize`)。
    Applied { seq: u64, cols: u16, rows: u16 },
    /// 陈旧 seq(≤ 已应用)丢弃。
    DroppedStale,
    /// 合并窗口内被更新的请求取代。
    Superseded,
    /// 尺寸越界(§A2 fixed 边界)。
    InvalidBounds,
}

/// resize 序列/边界/合并(仅 writer 可达;Observer 在 transport 层拒绝)。
pub struct ResizeCoalescer {
    last_applied_seq: u64,
    window: Option<(Instant, (u64, u16, u16))>,
    /// 合并窗口 = 1000 / rate(附录 A2 派生;rate 默认 10/s)。
    window_len: Duration,
}

impl ResizeCoalescer {
    pub fn new(resize_max_rate_per_sec: u32) -> Self {
        let rate = resize_max_rate_per_sec.clamp(1, 20);
        Self {
            last_applied_seq: 0,
            window: None,
            window_len: Duration::from_millis(1000 / u64::from(rate)),
        }
    }

    /// 提交 resize:窗口内保留最新(后被前替);窗口过期时应用最新值。
    /// 真正落到 PTY 的动作由调用方按 `Applied` 执行。
    pub fn submit(&mut self, seq: u64, cols: u16, rows: u16) -> ResizeDecision {
        if !(RESIZE_COLS_MIN..=RESIZE_COLS_MAX).contains(&cols)
            || !(RESIZE_ROWS_MIN..=RESIZE_ROWS_MAX).contains(&rows)
        {
            return ResizeDecision::InvalidBounds;
        }
        if seq <= self.last_applied_seq {
            return ResizeDecision::DroppedStale;
        }
        let now = Instant::now();
        match self.window {
            Some((opened_at, latest)) if now.duration_since(opened_at) < self.window_len => {
                let _ = latest;
                self.window = Some((opened_at, (seq, cols, rows)));
                ResizeDecision::Superseded
            }
            Some((_, (pending_seq, pending_cols, pending_rows))) => {
                // 上一窗口结束:应用其最新值,本请求开新窗口
                self.window = Some((now, (seq, cols, rows)));
                self.last_applied_seq = pending_seq;
                ResizeDecision::Applied {
                    seq: pending_seq,
                    cols: pending_cols,
                    rows: pending_rows,
                }
            }
            None => {
                self.window = Some((now, (seq, cols, rows)));
                ResizeDecision::Superseded
            }
        }
    }

    /// 冲刷当前窗口(连接关闭/空闲时):返回待应用的最新值。
    pub fn flush(&mut self) -> Option<(u64, u16, u16)> {
        let (_, pending) = self.window.take()?;
        self.last_applied_seq = pending.0;
        Some(pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ttl() -> Duration {
        Duration::from_millis(4_000)
    }

    #[test]
    fn grant_then_takeover_revokes_old_controller() {
        let mut manager = WriterLeaseManager::new(ttl());
        let WriterRequestOutcome::Granted { lease_id: old, .. } =
            manager.request_writer(7, ConnectionId(1))
        else {
            panic!("首次授予");
        };
        // 同 epoch 其它连接:拒绝
        assert_eq!(
            manager.request_writer(7, ConnectionId(2)),
            WriterRequestOutcome::Denied
        );
        // 更高 epoch(takeover):旧 lease 撤销,新 Controller 获得写权
        let WriterRequestOutcome::Granted { lease_id: new, .. } =
            manager.request_writer(8, ConnectionId(2))
        else {
            panic!("takeover 授予");
        };
        assert_ne!(old, new);
        // 旧 Controller 续租:takeover 终态
        assert_eq!(
            manager.renew(old, 7),
            WriterRenewOutcome::Revoked {
                reason: WriterRevokeReason::Takeover
            }
        );
    }

    #[test]
    fn release_is_idempotent_and_harmless_to_new_lease() {
        let mut manager = WriterLeaseManager::new(ttl());
        let WriterRequestOutcome::Granted { lease_id, .. } =
            manager.request_writer(1, ConnectionId(1))
        else {
            panic!()
        };
        assert_eq!(manager.release(lease_id), WriterReleaseOutcome::Released);
        assert_eq!(
            manager.release(lease_id),
            WriterReleaseOutcome::AlreadyReleased
        );
        // 重复 release 不影响新 lease
        let WriterRequestOutcome::Granted { lease_id: next, .. } =
            manager.request_writer(1, ConnectionId(2))
        else {
            panic!()
        };
        assert_ne!(lease_id, next);
        assert_eq!(
            manager.release(lease_id),
            WriterReleaseOutcome::AlreadyReleased
        );
    }

    #[test]
    fn connection_close_and_timeout_revoke() {
        let mut manager = WriterLeaseManager::new(Duration::from_millis(4_000));
        let WriterRequestOutcome::Granted { lease_id, .. } =
            manager.request_writer(1, ConnectionId(9))
        else {
            panic!()
        };
        manager.connection_closed(ConnectionId(9));
        assert_eq!(
            manager.renew(lease_id, 1),
            WriterRenewOutcome::Revoked {
                reason: WriterRevokeReason::ConnectionClosed
            }
        );
        let WriterRequestOutcome::Granted { lease_id: l2, .. } =
            manager.request_writer(1, ConnectionId(10))
        else {
            panic!()
        };
        assert_eq!(
            manager.submit_input(l2, ConnectionId(10), 1, [0; 32]),
            InputDecision::Admitted
        );
    }

    #[test]
    fn input_dedupe_same_seq_same_digest_idempotent() {
        let mut manager = WriterLeaseManager::new(ttl());
        let WriterRequestOutcome::Granted { lease_id, .. } =
            manager.request_writer(1, ConnectionId(1))
        else {
            panic!()
        };
        let digest = [7u8; 32];
        assert_eq!(
            manager.submit_input(lease_id, ConnectionId(1), 1, digest),
            InputDecision::Admitted
        );
        let ack = manager
            .complete_input(lease_id, 1, digest, true)
            .unwrap()
            .unwrap();
        // 重发同 seq 同 digest:返回原 ack
        assert_eq!(
            manager.submit_input(lease_id, ConnectionId(1), 1, digest),
            InputDecision::DuplicateAck { ack_id: ack }
        );
        // 同 seq 异 digest:冲突撤销
        assert_eq!(
            manager.submit_input(lease_id, ConnectionId(1), 1, [9u8; 32]),
            InputDecision::Conflict
        );
        assert_eq!(
            manager.renew(lease_id, 1).canonical_reason(),
            Some(WriterRevokeReason::InputSeqConflict)
        );
    }

    #[test]
    fn out_of_order_returns_expected_and_does_not_write() {
        let mut manager = WriterLeaseManager::new(ttl());
        let WriterRequestOutcome::Granted { lease_id, .. } =
            manager.request_writer(1, ConnectionId(1))
        else {
            panic!()
        };
        // 乱序:future seq
        assert_eq!(
            manager.submit_input(lease_id, ConnectionId(1), 3, [1; 32]),
            InputDecision::OutOfOrder { expected_seq: 1 }
        );
        assert_eq!(
            manager.submit_input(lease_id, ConnectionId(1), 1, [1; 32]),
            InputDecision::Admitted
        );
        assert_eq!(
            manager.pending_input_hint(),
            1,
            "未确认 input 仅为提示,不重放"
        );
    }

    #[test]
    fn failed_write_all_never_acks_and_revokes() {
        let mut manager = WriterLeaseManager::new(ttl());
        let WriterRequestOutcome::Granted { lease_id, .. } =
            manager.request_writer(1, ConnectionId(1))
        else {
            panic!()
        };
        assert_eq!(
            manager.submit_input(lease_id, ConnectionId(1), 1, [1; 32]),
            InputDecision::Admitted
        );
        let err = manager
            .complete_input(lease_id, 1, [1; 32], false)
            .unwrap_err();
        assert_eq!(err, WriterRevokeReason::Released);
    }

    #[test]
    fn resize_stale_and_bounds_and_coalescing() {
        let mut coalescer = ResizeCoalescer::new(10);
        assert_eq!(coalescer.submit(1, 100, 30), ResizeDecision::Superseded);
        assert_eq!(coalescer.submit(2, 120, 40), ResizeDecision::Superseded);
        // 越界拒绝(fixed 边界:cols 2–500、rows 2–300)
        assert_eq!(coalescer.submit(3, 501, 30), ResizeDecision::InvalidBounds);
        assert_eq!(coalescer.submit(3, 100, 301), ResizeDecision::InvalidBounds);
        assert_eq!(coalescer.submit(3, 1, 30), ResizeDecision::InvalidBounds);
        // 冲刷:应用窗口内最新(seq=2);此后 seq≤2 一律陈旧丢弃
        assert_eq!(coalescer.flush(), Some((2, 120, 40)));
        assert_eq!(coalescer.submit(2, 90, 20), ResizeDecision::DroppedStale);
        assert_eq!(coalescer.submit(1, 90, 20), ResizeDecision::DroppedStale);
        // 窗口后再提交:新窗口开启
        assert_eq!(coalescer.submit(4, 80, 24), ResizeDecision::Superseded);
    }
}

/// 测试辅助:renew 结果的撤销原因统一读取。
trait CanonReason {
    fn canonical_reason(&self) -> Option<WriterRevokeReason>;
}
impl CanonReason for WriterRenewOutcome {
    fn canonical_reason(&self) -> Option<WriterRevokeReason> {
        match self {
            WriterRenewOutcome::Revoked { reason } => Some(*reason),
            _ => None,
        }
    }
}
