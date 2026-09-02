//! T3e 契约(Issue #33):Core 消失后的 bounded orphan grace 与
//! Root process group 终止(§10.3/A6)。

use std::time::Duration;

use mf_elevated::limits::ElevatedLimits;
use mf_elevated::protocol::{
    CoreIdentity, HostEvent, HostMessage, OwnerEpoch, RootEpoch, SessionCapability,
};
use mf_elevated::root_host::{FakeRootHost, HostDecision, HostPhase};
use mf_elevated::spool::SessionSpool;

fn host_with_grace(grace_ms: u64) -> FakeRootHost {
    let capability = SessionCapability {
        session_handle: "sess-root-1".into(),
        core: CoreIdentity::new(777),
        root_epoch: RootEpoch(1),
    };
    let spool = SessionSpool::create(tempfile::tempdir().unwrap().keep(), 1024 * 1024).unwrap();
    FakeRootHost::new(
        "sess-root-1",
        capability,
        OwnerEpoch(1),
        spool,
        ElevatedLimits {
            root_host_orphan_grace_ms: grace_ms,
            ..ElevatedLimits::default()
        },
    )
}

#[test]
fn core_loss_rejects_control_but_spools_output() {
    let mut host = host_with_grace(60_000);
    let capability = SessionCapability {
        session_handle: "sess-root-1".into(),
        core: CoreIdentity::new(777),
        root_epoch: RootEpoch(1),
    };
    // channel 断开:立即拒绝新 input/resize/control
    host.core_channel_closed();
    assert!(matches!(host.phase(), HostPhase::Orphan { .. }));
    assert_eq!(
        host.handle(&HostMessage::Input {
            owner_epoch: OwnerEpoch(1),
            capability,
            bytes: b"whoami\r".to_vec()
        }),
        HostDecision::RejectedChannelClosed
    );
    // 已脱敏输出继续进 spool
    assert_eq!(
        host.orphan_output(b"redacted-output"),
        HostDecision::Accepted
    );
    assert!(host.spool_written_bytes() > 0);
}

#[test]
fn grace_expiry_terminates_root_process_group() {
    let mut host = host_with_grace(60_000);
    host.core_channel_closed();
    host.rewind_detached_since(Duration::from_secs(61));
    let event = host.poll_grace().expect("grace 耗尽必须终止");
    match event {
        HostEvent::OrphanTerminated { reason } => assert_eq!(reason, "orphan_grace_expired"),
        other => panic!("期望 OrphanTerminated,得到 {other:?}"),
    }
    assert!(!host.is_process_alive(), "无人控制的 Root 进程组不得存活");
    assert!(matches!(
        host.phase(),
        HostPhase::Terminated {
            orphan_expiry: true
        }
    ));
    // 终止后输出不再入 spool,且留下记录
    assert_eq!(
        host.orphan_output(b"late"),
        HostDecision::RejectedChannelClosed
    );
}

#[test]
fn grace_not_expired_keeps_host_alive() {
    let mut host = host_with_grace(3_600_000);
    host.core_channel_closed();
    assert!(host.poll_grace().is_none(), "宽限期内不终止");
    assert!(host.is_process_alive());
}
