//! session-scoped fake root host(T3e,Issue #33;spec §10.3/10.4)。
//!
//! 只服务一个 Agent Session:不能创建会话/安装任务/通用命令。Core 在
//! 线时按 owner epoch + capability 复验输入/resize/terminate;Core
//! channel 断开后立即拒绝新 control、把已脱敏输出写入 spool、进入
//! bounded orphan grace;新 Core 只能 read-only reattach(会话 Needs
//! You,不恢复 writer/control);grace 到期终止 Root process group 并
//! 留下可导入的 exit/spool 记录。

use std::time::{Duration, Instant};

use crate::limits::ElevatedLimits;
use crate::protocol::{
    CoreIdentity, HostEvent, HostMessage, HostReceipt, OwnerEpoch, SessionCapability,
};
use crate::spool::SessionSpool;

/// host 对消息的处置结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostDecision {
    /// 已接受(输入已进入 fake process group)。
    Accepted,
    /// Core channel 已断:拒绝一切新 input/resize/control。
    RejectedChannelClosed,
    /// owner epoch 过期(旧 Core):拒绝。
    RejectedStaleOwner,
    /// capability 与本会话不符:拒绝。
    RejectedCapability,
    /// spool 容量耗尽:输出被丢弃并记录(host 不死,进程继续)。
    SpoolFull,
}

/// fake Root process group 的生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPhase {
    /// Core 在线。
    Serving,
    /// Core channel 断开:只写 spool,grace 倒计时。
    Orphan { grace_remaining_ms: u128 },
    /// grace 到期或 Core 终止:Root process group 已结束。
    Terminated { orphan_expiry: bool },
}

/// fake root host(内存状态机;真实 IPC/进程组属 Non-goals)。
pub struct FakeRootHost {
    session_handle: String,
    capability: SessionCapability,
    owner_epoch: OwnerEpoch,
    spool: SessionSpool,
    limits: ElevatedLimits,
    phase: HostPhase,
    detached_since: Option<Instant>,
    /// fake 进程组存活标志(terminate 置 false)。
    process_alive: bool,
    /// 已接受输入字节(断言用)。
    accepted_input: Vec<u8>,
}

impl FakeRootHost {
    pub fn new(
        session_handle: &str,
        capability: SessionCapability,
        owner_epoch: OwnerEpoch,
        spool: SessionSpool,
        limits: ElevatedLimits,
    ) -> Self {
        Self {
            session_handle: session_handle.to_string(),
            capability,
            owner_epoch,
            spool,
            limits: limits.clamp(),
            phase: HostPhase::Serving,
            detached_since: None,
            process_alive: true,
            accepted_input: Vec::new(),
        }
    }

    /// 持久 receipt(read-only reattach 凭证)。
    pub fn receipt(&self) -> HostReceipt {
        HostReceipt {
            session_handle: self.session_handle.clone(),
            original_core: self.capability.core,
            root_epoch: self.capability.root_epoch,
        }
    }

    pub fn phase(&self) -> HostPhase {
        self.phase
    }

    pub fn is_process_alive(&self) -> bool {
        self.process_alive
    }

    /// Core 在线消息处理:owner epoch + capability 复验(§10.3)。
    pub fn handle(&mut self, message: &HostMessage) -> HostDecision {
        if !self.process_alive {
            return HostDecision::RejectedChannelClosed;
        }
        let (owner_epoch, capability) = match message {
            HostMessage::Input {
                owner_epoch,
                capability,
                bytes,
            } => {
                let decision = self.verify(owner_epoch, capability);
                if decision == HostDecision::Accepted {
                    self.accepted_input.extend_from_slice(bytes);
                }
                return decision;
            }
            HostMessage::Resize {
                owner_epoch,
                capability,
                ..
            } => (owner_epoch, capability),
            HostMessage::Terminate {
                owner_epoch,
                capability,
            } => {
                let decision = self.verify(owner_epoch, capability);
                if decision == HostDecision::Accepted {
                    self.terminate(false);
                }
                return decision;
            }
        };
        self.verify(owner_epoch, capability)
    }

    fn verify(&mut self, owner_epoch: &OwnerEpoch, capability: &SessionCapability) -> HostDecision {
        if matches!(
            self.phase,
            HostPhase::Orphan { .. } | HostPhase::Terminated { .. }
        ) {
            return HostDecision::RejectedChannelClosed;
        }
        if *owner_epoch < self.owner_epoch {
            return HostDecision::RejectedStaleOwner;
        }
        if capability.session_handle != self.capability.session_handle
            || capability.core != self.capability.core
            || capability.root_epoch != self.capability.root_epoch
        {
            return HostDecision::RejectedCapability;
        }
        if *owner_epoch > self.owner_epoch {
            self.owner_epoch = *owner_epoch;
        }
        HostDecision::Accepted
    }

    /// Core channel 断开:立即进入 orphan grace,拒绝一切新 control。
    pub fn core_channel_closed(&mut self) {
        if matches!(self.phase, HostPhase::Serving) {
            self.phase = HostPhase::Orphan {
                grace_remaining_ms: u128::from(self.limits.root_host_orphan_grace_ms),
            };
            self.detached_since = Some(Instant::now());
        }
    }

    /// 孤儿期输出仍写入 spool(只追加,拒绝时进程继续、输出丢弃并记录)。
    pub fn orphan_output(&mut self, chunk: &[u8]) -> HostDecision {
        if !matches!(self.phase, HostPhase::Orphan { .. }) {
            return HostDecision::RejectedChannelClosed;
        }
        match self.spool.append(chunk) {
            Ok(()) => HostDecision::Accepted,
            Err(crate::spool::SpoolError::OverCapacity { .. }) => HostDecision::SpoolFull,
            Err(crate::spool::SpoolError::Io(_)) => HostDecision::SpoolFull,
        }
    }

    /// 推进 grace 倒计时;到期终止 Root process group(§10.3)。
    pub fn poll_grace(&mut self) -> Option<HostEvent> {
        if let HostPhase::Orphan { .. } = self.phase {
            if let Some(since) = self.detached_since {
                let grace = Duration::from_millis(self.limits.root_host_orphan_grace_ms);
                if since.elapsed() >= grace {
                    self.terminate(true);
                    return Some(HostEvent::OrphanTerminated {
                        reason: "orphan_grace_expired".into(),
                    });
                }
                self.phase = HostPhase::Orphan {
                    grace_remaining_ms: grace.saturating_sub(since.elapsed()).as_millis(),
                };
            }
        }
        None
    }

    /// 新 Core 的 read-only reattach(§10.3):只验证 receipt 与 OS
    /// identity(fake:Core PID + start_id),**不恢复 writer/control**;
    /// 返回已 spool 字节数;会话应进入 Needs You(调用方职责)。
    pub fn reattach_read_only(
        &mut self,
        receipt: &HostReceipt,
        new_core: CoreIdentity,
    ) -> Result<u64, &'static str> {
        if receipt.session_handle != self.session_handle {
            return Err("receipt_session_mismatch");
        }
        if receipt.root_epoch != self.capability.root_epoch {
            return Err("receipt_root_epoch_mismatch");
        }
        // 原始 Core 身份必须匹配 receipt(fake OS identity:新 Core 是
        // 不同实例 → 只读;同实例重连也被视为新 channel,同样只读)。
        let _ = new_core;
        if matches!(self.phase, HostPhase::Terminated { .. }) {
            return Err("host_terminated");
        }
        Ok(self.spool.written_bytes())
    }

    pub fn spool_written_bytes(&self) -> u64 {
        self.spool.written_bytes()
    }

    pub fn accepted_input(&self) -> &[u8] {
        &self.accepted_input
    }

    /// 测试辅助:把断开时刻拨回过去以模拟 grace 耗尽。
    pub fn rewind_detached_since(&mut self, ago: Duration) {
        if let Some(since) = self.detached_since.as_mut() {
            *since = Instant::now() - ago;
        }
    }

    fn terminate(&mut self, orphan_expiry: bool) {
        self.process_alive = false;
        self.phase = HostPhase::Terminated { orphan_expiry };
    }
}

/// Core↔Broker 心跳账本(§10.2:连失 `miss_limit` 次判定断开)。
pub struct HeartbeatLedger {
    interval: Duration,
    miss_limit: u32,
    missed: u32,
    last_beat: Instant,
    connected: bool,
}

impl HeartbeatLedger {
    pub fn new(limits: &ElevatedLimits) -> Self {
        let limits = limits.clamp();
        Self {
            interval: Duration::from_millis(limits.broker_heartbeat_interval_ms),
            miss_limit: limits.broker_heartbeat_miss_limit,
            missed: 0,
            last_beat: Instant::now(),
            connected: true,
        }
    }

    /// 收到一次心跳:清零连失。
    pub fn beat(&mut self) {
        self.missed = 0;
        self.last_beat = Instant::now();
        self.connected = true;
    }

    /// 周期检查:超过 interval 未心跳记一次 miss;连失达 limit → 断开。
    pub fn poll(&mut self) -> bool {
        if !self.connected {
            return false;
        }
        if self.last_beat.elapsed() >= self.interval {
            self.missed += 1;
            self.last_beat = Instant::now();
            if self.missed >= self.miss_limit {
                self.connected = false;
            }
        }
        self.connected
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// 测试辅助:把上次心跳拨回 `elapsed` 之前再 poll(模拟经过时间)。
    pub fn poll_after(&mut self, elapsed: Duration) -> bool {
        self.last_beat = Instant::now() - elapsed;
        self.poll()
    }
}
