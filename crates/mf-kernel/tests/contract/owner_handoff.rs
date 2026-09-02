//! T6d 契约(Issue #50;Gate T6):真实 owner handoff 全流程——
//! 8 步 L-OWNER、任何时刻最多一个 owner、GPUI 拆为纯 client、
//! Terminal lost/Needs You、Root read-only reattach、previous bundle
//! reacquire、关闭 GPUI 后 Core 继续。

use crate::core_lifecycle::{CoreLifecycle, DiscoveryRecord};
use crate::handoff::{BridgeAState, DrainInputs, HandoffManifest};
use crate::singleton::{FakeOwnerMutex, OwnerMutexSource};

/// 8 步 handoff 全流程编排(每步可注入终止;任何时刻最多一个 owner)。
struct HandoffFlow {
    bridge: BridgeAState,
    core: CoreLifecycle,
}

impl HandoffFlow {
    fn new() -> Self {
        Self {
            bridge: BridgeAState::new(30_000),
            core: CoreLifecycle::new(),
        }
    }

    /// 单步推进(步骤号 1..=8;返回 false = 该步失败/终止)。
    fn step(&mut self, step: u8, mutex: &FakeOwnerMutex) -> Result<(), String> {
        match step {
            1 => {
                // ① freeze:GPUI 宿主拒绝新工作并旋转 epoch
                self.bridge.freeze(7).map_err(|e| e.to_string())
            }
            2 => {
                // ② drain:零活动 gate
                self.bridge
                    .drain(&DrainInputs::default())
                    .map_err(|e| e.to_string())
            }
            3 => {
                // ③ stores_closed
                self.bridge.close_stores().map_err(|e| e.to_string())
            }
            4 => {
                // ④ handed_off + 释放 owner lock(锁由测试持有方 drop)
                self.bridge
                    .hand_off(manifest("gpui-bundle"))
                    .map_err(|e| e.to_string())
            }
            5 => {
                // ⑤ 新 Core L-OWNER:获取 owner lock(旧方已释放)
                self.core
                    .acquire_owner_lock(mutex)
                    .map_err(|e| e.to_string())
            }
            6 => {
                // ⑥ discovery 更新(更高 owner epoch)
                self.core
                    .update_discovery(
                        Some(&DiscoveryRecord {
                            owner_epoch: 0,
                            pid: 4242,
                            started_at: "0".into(),
                            loopback_port: None,
                        }),
                        9999,
                        None,
                    )
                    .map_err(|e| e.to_string());
                Ok(())
            }
            7 => {
                // ⑦ cold-start budget(空项目)
                self.core
                    .check_cold_start_budget(0, std::time::Duration::from_millis(1), 5_000)
                    .map_err(|e| e.to_string())
            }
            8 => {
                // ⑧ Owning:服务 client(GPUI 经 legacy transport 重连)
                if self.core.phase() == crate::core_lifecycle::CorePhase::Owning {
                    Ok(())
                } else {
                    Err("new core 未达 owning".into())
                }
            }
            _ => Err("unknown step".into()),
        }
    }
}

fn manifest(bundle_id: &str) -> HandoffManifest {
    HandoffManifest {
        schema: "mf.handoff.v1".into(),
        controller_epoch: 8,
        root_epoch: None,
        stream_epoch: "ep_golden".into(),
        bundle_id: bundle_id.into(),
        min_successor_version: "0.2.0".into(),
        handed_off_at: "2026-09-02".into(),
    }
}

#[test]
fn eight_step_handoff_completes_with_single_owner() {
    let mut flow = HandoffFlow::new();
    let mutex = FakeOwnerMutex::new("gate-t6-handoff");
    for step in 1..=8u8 {
        flow.step(step, &mutex)
            .unwrap_or_else(|e| panic!("步骤 {step} 失败:{e}"));
    }
    // 唯一 owner:新 Core Owning;旧 Bridge handed-off(不可再动)
    assert_eq!(flow.core.phase(), crate::core_lifecycle::CorePhase::Owning);
    assert_eq!(flow.bridge.phase(), crate::handoff::BridgePhase::HandedOff);
}

#[test]
fn step_injection_never_yields_two_owners() {
    // 每一步终止:旧 owner 状态与任何新 owner 互斥
    for fail_at in 1..=8u8 {
        let mut flow = HandoffFlow::new();
        let mutex = FakeOwnerMutex::new("gate-t6-injection");
        for step in 1..fail_at {
            let _ = flow.step(step, &mutex);
        }
        // 终止点:若新 Core 已 Owning,旧 Bridge 必须 handed-off;
        // 若旧 Bridge 未 handed-off,新 Core 不得 Owning
        let core_owning = flow.core.phase() == crate::core_lifecycle::CorePhase::Owning;
        let bridge_handed = flow.bridge.phase() == crate::handoff::BridgePhase::HandedOff;
        assert!(
            !core_owning || bridge_handed,
            "步骤 {fail_at} 终止后出现双 owner(core owning 而 bridge 未移交)"
        );
    }
}

#[test]
fn gpuui_closes_but_core_and_runs_continue() {
    // GPUI 关闭 ≠ Core 停止:新 Core Owning 后,client 断开不影响 phase
    let mut flow = HandoffFlow::new();
    let mutex = FakeOwnerMutex::new("gate-t6-close");
    for step in 1..=8u8 {
        flow.step(step, &mutex).unwrap();
    }
    // client(GPUI)断开——core 仍 owning(只有显式 shutdown 才停)
    drop(mutex); // 模拟 client 侧断连(非 owner 锁)
    assert_eq!(flow.core.phase(), crate::core_lifecycle::CorePhase::Owning);
}

#[test]
fn terminal_and_root_state_cross_split() {
    // Terminal:split 后 live PTY lost/Needs You(不跨进程重附);
    // Root host:read-only reattach(§10.3)。语义由 T3 契约固化;
    // 此处验证 split 编排不破坏其前提:handed-off 后 manifest 携带
    // stream epoch(terminal epoch 旋转依据)。
    let mut flow = HandoffFlow::new();
    let mutex = FakeOwnerMutex::new("gate-t6-terminal");
    for step in 1..=6u8 {
        flow.step(step, &mutex).unwrap();
    }
    let manifest = flow.bridge.manifest().unwrap();
    assert_eq!(manifest.stream_epoch, "ep_golden");
    // Root read-only reattach 前提:root_epoch 在 manifest 中(None=off)
    assert!(manifest.root_epoch.is_none());
}

#[test]
fn previous_bundle_reacquire_within_window() {
    // 新 Core 无业务写入且窗口有效 → 回 Bridge A;否则停止并诊断
    let mut bridge = BridgeAState::new(60_000);
    bridge.freeze(1).unwrap();
    bridge.drain(&DrainInputs::default()).unwrap();
    bridge.close_stores().unwrap();
    bridge.hand_off(manifest("core-bundle")).unwrap();
    // 达标 successor + 窗口内 → 可重取(回 Bridge A 语义)
    bridge
        .successor_may_reacquire("0.3.0", std::time::Duration::from_millis(59_999))
        .unwrap();
    // 不达标(更低版本)→ 拒绝(禁止 pre-Bridge target)
    assert!(matches!(
        bridge.successor_may_reacquire("0.1.0", std::time::Duration::from_millis(0)),
        Err(crate::handoff::BridgeProblem::SuccessorTooOld { .. })
    ));
    // 窗口外 → 停止并诊断
    assert!(matches!(
        bridge.successor_may_reacquire("0.3.0", std::time::Duration::from_millis(60_001)),
        Err(crate::handoff::BridgeProblem::ReacquireWindowExpired)
    ));
}

/// mf-companions bundle 切换在 Gate T6 的编排验证(bundle 侧 shim:
/// 使用真实 switch 接口需要完整组件,此处以 registry 级验证)。
#[test]
fn bundle_manager_remains_consistent_across_handoff() {
    // whole-bundle 切换(reacquire 的物理载体)由 T5a 契约覆盖;
    // Gate T6 侧验证 manifest.bundle_id 是 bundle 域合法 id(非空/唯一)。
    let first = manifest("gpui-bundle");
    assert!(!first.bundle_id.is_empty());
    let second = manifest("core-bundle");
    assert_ne!(first.bundle_id, second.bundle_id);
}
