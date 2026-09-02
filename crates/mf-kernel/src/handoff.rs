//! Bridge A freeze/drain/handoff 集成(T5c,Issue #47;spec §2.3/§13)。
//!
//! 状态机:`owning → freezing(拒绝新命令/会话/安装/Root;旋转
//! Controller/Root/writer epoch)→ draining(publication barrier 等已
//! 线性化命令/PTY 输入队列/outbox/可中断 Operation)→ stores_closed
//! (flush transcript/outbox/receipt;关闭 Store 句柄)→ handed_off
//! (写 handoff manifest;释放 CoreOwnerLock;**永不再自行 reopen**)`。
//! 新 owner 在 reacquire 窗口内可更高 epoch 重取;窗口外/不兼容保持
//! 停止并给恢复诊断。pre-Bridge 二进制(无 handoff 能力)不是合法
//! rollback target——manifest 携带最低接管版本。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Bridge A 阶段(§2.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BridgePhase {
    Owning,
    Freezing,
    Draining,
    StoresClosed,
    HandedOff,
}

/// Bridge 判定输入(各面 drain 状态)。
#[derive(Debug, Clone, Default)]
pub struct DrainInputs {
    /// publication barrier 上未发布 outbox 事件数。
    pub pending_outbox_events: usize,
    /// 未终结 command intent 数。
    pub unfinished_intents: usize,
    /// 存活 Agent Session 数(live PTY;不可中断 → 延期)。
    pub live_sessions: usize,
    /// 进行中 Installation Job 数。
    pub active_installation_jobs: usize,
    /// 不可中断 Operation 数(如 frozen plan 执行中段)。
    pub uninterruptible_operations: usize,
}

/// handoff manifest(新 owner 的接管凭据;§13)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffManifest {
    pub schema: String,
    /// 移交时的 service/controller epoch 组。
    pub controller_epoch: u64,
    pub root_epoch: Option<u64>,
    pub stream_epoch: String,
    /// bundle id/版本(handoff 方)。
    pub bundle_id: String,
    /// 最低接管版本(pre-Bridge 二进制 < 此值 → 不是 rollback target)。
    pub min_successor_version: String,
    pub handed_off_at: String,
}

/// handoff 问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BridgeProblem {
    #[error("阶段不能回退({from:?} → {to:?}):handed-off 永不 reopen")]
    PhaseRegression { from: BridgePhase, to: BridgePhase },
    #[error("drain 未完成:outbox={outbox}, intents={intents}, sessions={sessions}, installs={installs}, uninterruptible={uninterruptible}")]
    DrainIncomplete {
        outbox: usize,
        intents: usize,
        sessions: usize,
        installs: usize,
        uninterruptible: usize,
    },
    #[error("已 handed-off:本进程不再自行 reopen(新 owner 经 owner lock 重取)")]
    AlreadyHandedOff,
    #[error("接管版本 {successor} 低于最低要求 {required}:pre-Bridge 二进制不是 rollback target")]
    SuccessorTooOld { successor: String, required: String },
    #[error("reacquire 窗口已过:保持停止并输出恢复诊断")]
    ReacquireWindowExpired,
}

/// Bridge A 状态机(单进程;owner lock 由调用方持有并在 handed_off 释放)。
pub struct BridgeAState {
    phase: BridgePhase,
    /// Controller/Root/writer epoch 旋转(freeze 时一次性)。
    pub frozen_controller_epoch: Option<u64>,
    /// reacquire 窗口(§2.3;窗口外保持停止)。
    reacquire_window: Duration,
    handed_off_at: Option<Instant>,
    manifest: Option<HandoffManifest>,
    /// 已旋转(freeze 幂等防重复旋转)。
    rotated: bool,
}

impl BridgeAState {
    pub fn new(reacquire_window_ms: u64) -> Self {
        Self {
            phase: BridgePhase::Owning,
            frozen_controller_epoch: None,
            reacquire_window: Duration::from_millis(reacquire_window_ms),
            handed_off_at: None,
            manifest: None,
            rotated: false,
        }
    }

    pub fn phase(&self) -> BridgePhase {
        self.phase
    }

    pub fn manifest(&self) -> Option<&HandoffManifest> {
        self.manifest.as_ref()
    }

    /// 是否接受新工作(命令/会话/安装/Root enable)。
    pub fn accepts_new_work(&self) -> bool {
        self.phase == BridgePhase::Owning
    }

    /// freeze:拒绝新工作 + 一次性旋转全部 epoch。
    pub fn freeze(&mut self, controller_epoch: u64) -> Result<(), BridgeProblem> {
        if self.phase == BridgePhase::HandedOff {
            return Err(BridgeProblem::AlreadyHandedOff);
        }
        if self.phase > BridgePhase::Owning {
            // freeze 幂等:重复 freeze 允许(不重复旋转)
            return Ok(());
        }
        self.phase = BridgePhase::Freezing;
        if !self.rotated {
            self.frozen_controller_epoch = Some(controller_epoch + 1);
            self.rotated = true;
        }
        Ok(())
    }

    /// drain:零活动 gate——全部 drain 面清零才前进(不可中断活动 →
    /// 延期,保持 draining)。
    pub fn drain(&mut self, inputs: &DrainInputs) -> Result<(), BridgeProblem> {
        if self.phase == BridgePhase::HandedOff {
            return Err(BridgeProblem::AlreadyHandedOff);
        }
        if self.phase < BridgePhase::Freezing {
            return Err(BridgeProblem::PhaseRegression {
                from: self.phase,
                to: BridgePhase::Draining,
            });
        }
        self.phase = BridgePhase::Draining;
        let DrainInputs {
            pending_outbox_events,
            unfinished_intents,
            live_sessions,
            active_installation_jobs,
            uninterruptible_operations,
        } = inputs;
        let empty = *pending_outbox_events == 0
            && *unfinished_intents == 0
            && *live_sessions == 0
            && *active_installation_jobs == 0
            && *uninterruptible_operations == 0;
        if !empty {
            return Err(BridgeProblem::DrainIncomplete {
                outbox: *pending_outbox_events,
                intents: *unfinished_intents,
                sessions: *live_sessions,
                installs: *active_installation_jobs,
                uninterruptible: *uninterruptible_operations,
            });
        }
        Ok(())
    }

    /// stores_closed:flush 全部 durable 面(transcript/outbox/receipt)
    /// 并关闭 Store 句柄(由调用方执行物理动作;此处只推阶段)。
    pub fn close_stores(&mut self) -> Result<(), BridgeProblem> {
        match self.phase {
            BridgePhase::Draining => {
                self.phase = BridgePhase::StoresClosed;
                Ok(())
            }
            BridgePhase::HandedOff => Err(BridgeProblem::AlreadyHandedOff),
            other => Err(BridgeProblem::PhaseRegression {
                from: other,
                to: BridgePhase::StoresClosed,
            }),
        }
    }

    /// handed_off:写 manifest + 释放 owner lock(调用方)+ 永不 reopen。
    pub fn hand_off(&mut self, mut manifest: HandoffManifest) -> Result<(), BridgeProblem> {
        match self.phase {
            BridgePhase::StoresClosed => {
                manifest.schema = "mf.handoff.v1".into();
                self.phase = BridgePhase::HandedOff;
                self.handed_off_at = Some(Instant::now());
                self.manifest = Some(manifest);
                Ok(())
            }
            BridgePhase::HandedOff => Err(BridgeProblem::AlreadyHandedOff),
            other => Err(BridgeProblem::PhaseRegression {
                from: other,
                to: BridgePhase::HandedOff,
            }),
        }
    }

    /// 新 owner 接管判定(窗口内 + 版本达标;窗口由 handoff 时刻起算)。
    pub fn successor_may_reacquire(
        &self,
        successor_version: &str,
        window_elapsed: Duration,
    ) -> Result<(), BridgeProblem> {
        let manifest = self
            .manifest
            .as_ref()
            .ok_or(BridgeProblem::AlreadyHandedOff)?;
        if successor_version < manifest.min_successor_version.as_str() {
            return Err(BridgeProblem::SuccessorTooOld {
                successor: successor_version.to_string(),
                required: manifest.min_successor_version.clone(),
            });
        }
        if window_elapsed > self.reacquire_window {
            return Err(BridgeProblem::ReacquireWindowExpired);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_drain() -> DrainInputs {
        DrainInputs::default()
    }

    #[test]
    fn happy_path_freeze_drain_close_handoff() {
        let mut bridge = BridgeAState::new(10_000);
        assert!(bridge.accepts_new_work());
        bridge.freeze(7).unwrap();
        assert!(!bridge.accepts_new_work(), "freeze 后无新工作");
        assert_eq!(bridge.frozen_controller_epoch, Some(8), "epoch 旋转");
        // 重复 freeze 幂等(不重复旋转)
        bridge.freeze(8).unwrap();
        assert_eq!(bridge.frozen_controller_epoch, Some(8));
        bridge.drain(&empty_drain()).unwrap();
        bridge.close_stores().unwrap();
        bridge.hand_off(manifest("bundle-a")).unwrap();
        assert_eq!(bridge.phase(), BridgePhase::HandedOff);
    }

    #[test]
    fn phase_regression_and_already_handed_off_rejected() {
        let mut bridge = BridgeAState::new(1_000);
        // 未 freeze 先 drain → 回退拒绝
        assert!(matches!(
            bridge.drain(&empty_drain()),
            Err(BridgeProblem::PhaseRegression { .. })
        ));
        bridge.freeze(1).unwrap();
        bridge.drain(&empty_drain()).unwrap();
        bridge.close_stores().unwrap();
        bridge.hand_off(manifest("b")).unwrap();
        // handed-off 之后任何动作都拒绝
        assert_eq!(bridge.freeze(9), Err(BridgeProblem::AlreadyHandedOff));
        assert_eq!(
            bridge.drain(&empty_drain()),
            Err(BridgeProblem::AlreadyHandedOff)
        );
        assert_eq!(bridge.close_stores(), Err(BridgeProblem::AlreadyHandedOff));
        assert_eq!(
            bridge.hand_off(manifest("b")),
            Err(BridgeProblem::AlreadyHandedOff)
        );
    }

    #[test]
    fn zero_active_gate_defers_until_drained() {
        let mut bridge = BridgeAState::new(1_000);
        bridge.freeze(1).unwrap();
        let busy = DrainInputs {
            pending_outbox_events: 3,
            unfinished_intents: 1,
            live_sessions: 2,
            active_installation_jobs: 1,
            uninterruptible_operations: 1,
        };
        assert!(matches!(
            bridge.drain(&busy),
            Err(BridgeProblem::DrainIncomplete { .. })
        ));
        // 活动清空(已线性化命令/输入/outbox 不丢——它们完成后面数清零)
        bridge.drain(&empty_drain()).unwrap();
    }

    #[test]
    fn reacquire_window_and_successor_version() {
        let mut bridge = BridgeAState::new(5_000);
        bridge.freeze(1).unwrap();
        bridge.drain(&empty_drain()).unwrap();
        bridge.close_stores().unwrap();
        bridge.hand_off(manifest("bundle-a")).unwrap();
        // 达标版本 + 窗口内 → 可重取
        bridge
            .successor_may_reacquire("0.2.0", Duration::from_millis(4_999))
            .unwrap();
        // pre-Bridge 版本太旧 → 不是 rollback target
        assert!(matches!(
            bridge.successor_may_reacquire("0.1.0", Duration::from_millis(0)),
            Err(BridgeProblem::SuccessorTooOld { .. })
        ));
        // 窗口外 → 保持停止
        assert_eq!(
            bridge.successor_may_reacquire("0.2.0", Duration::from_millis(5_001)),
            Err(BridgeProblem::ReacquireWindowExpired)
        );
    }

    fn manifest(bundle: &str) -> HandoffManifest {
        HandoffManifest {
            schema: "mf.handoff.v1".into(),
            controller_epoch: 8,
            root_epoch: None,
            stream_epoch: "ep_golden".into(),
            bundle_id: bundle.into(),
            min_successor_version: "0.2.0".into(),
            handed_off_at: "2026-09-02T00:00:00Z".into(),
        }
    }
}
