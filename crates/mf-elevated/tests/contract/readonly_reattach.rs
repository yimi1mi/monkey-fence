//! T3e 契约(Issue #33):新 Core 只读 reattach,不恢复 writer/control
//! (§10.3);spool 不越 hard cap。

use mf_elevated::limits::ElevatedLimits;
use mf_elevated::protocol::{CoreIdentity, HostMessage, OwnerEpoch, RootEpoch, SessionCapability};
use mf_elevated::root_host::{FakeRootHost, HostDecision};
use mf_elevated::spool::{SessionSpool, SpoolError};

fn spool_at(dir: &std::path::Path, max: u64) -> SessionSpool {
    SessionSpool::create(dir.to_path_buf(), max).unwrap()
}

#[test]
fn new_core_reattaches_read_only_without_writer() {
    let capability = SessionCapability {
        session_handle: "sess-root-2".into(),
        core: CoreIdentity::new(3141),
        root_epoch: RootEpoch(2),
    };
    let tmp = tempfile::tempdir().unwrap();
    let mut host = FakeRootHost::new(
        "sess-root-2",
        capability,
        OwnerEpoch(1),
        spool_at(tmp.path(), 1024 * 1024),
        ElevatedLimits::default(),
    );
    host.core_channel_closed();
    assert_eq!(
        host.orphan_output(b"orphaned-redacted"),
        HostDecision::Accepted
    );

    // 新 Core 用持久 receipt 只读 reattach:能读 spool,不能写
    let receipt = host.receipt();
    let written = host
        .reattach_read_only(&receipt, CoreIdentity::new(9999))
        .expect("合法 receipt 应可只读重附");
    assert_eq!(written, host.spool_written_bytes());
    // reattach 后 writer/control 依然全拒
    assert_eq!(
        host.handle(&HostMessage::Input {
            owner_epoch: OwnerEpoch(9),
            capability: SessionCapability {
                session_handle: "sess-root-2".into(),
                core: CoreIdentity::new(9999),
                root_epoch: RootEpoch(2),
            },
            bytes: b"nope".to_vec()
        }),
        HostDecision::RejectedChannelClosed
    );
    // 错误 receipt 拒绝
    let wrong = mf_elevated::protocol::HostReceipt {
        session_handle: "sess-other".into(),
        original_core: receipt.original_core,
        root_epoch: receipt.root_epoch,
    };
    assert!(host
        .reattach_read_only(&wrong, CoreIdentity::new(1))
        .is_err());
}

#[test]
fn terminated_host_cannot_reattach() {
    let capability = SessionCapability {
        session_handle: "sess-root-3".into(),
        core: CoreIdentity::new(1),
        root_epoch: RootEpoch(1),
    };
    let tmp = tempfile::tempdir().unwrap();
    let mut host = FakeRootHost::new(
        "sess-root-3",
        capability,
        OwnerEpoch(1),
        spool_at(tmp.path(), 1024 * 1024),
        ElevatedLimits {
            root_host_orphan_grace_ms: 60_000,
            ..ElevatedLimits::default()
        },
    );
    host.core_channel_closed();
    // 模拟 grace 到期
    host.rewind_detached_since(std::time::Duration::from_secs(61));
    let _ = host.poll_grace();
    let receipt = host.receipt();
    assert!(
        host.reattach_read_only(&receipt, CoreIdentity::new(2))
            .is_err(),
        "已终止 host 不可重附(只剩 spool 记录可导入)"
    );
}

#[test]
fn spool_never_exceeds_hard_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spool = spool_at(tmp.path(), 64);
    assert!(spool.append(&[0u8; 32]).is_ok());
    assert!(spool.append(&[0u8; 31]).is_ok()); // 63 ≤ 64
    match spool.append(&[0u8; 2]) {
        Err(SpoolError::OverCapacity { needed, max }) => assert_eq!((needed, max), (65, 64)),
        other => panic!("期望 OverCapacity,得到 {other:?}"),
    }
    // 只读 spool 拒绝一切写
    let mut ro = SessionSpool::attach_read_only(tmp.path().to_path_buf(), 1024).unwrap();
    assert!(matches!(
        ro.append(b"x"),
        Err(SpoolError::Io(_)) | Err(SpoolError::OverCapacity { .. })
    ));
}
