//! T3e 契约(Issue #33):owner/root epoch 与 capability 复验、
//! 浏览器/Plugin Worker 无法取得 Broker capability(§10.2/10.3)。

use mf_elevated::limits::ElevatedLimits;
use mf_elevated::protocol::{
    BrokerGate, BrokerReject, BrokerRequest, CoreIdentity, HostMessage, OwnerEpoch, RequestNonce,
    RootEpoch, SessionCapability, PROTOCOL_VERSION,
};
use mf_elevated::root_host::{FakeRootHost, HostDecision};
use mf_elevated::spool::SessionSpool;
use uuid::Uuid;

fn fixture() -> (FakeRootHost, SessionCapability, OwnerEpoch) {
    let core = CoreIdentity::new(4242);
    let capability = SessionCapability {
        session_handle: "sess-root-1".into(),
        core,
        root_epoch: RootEpoch(5),
    };
    let spool = SessionSpool::create(tempfile::tempdir().unwrap().keep(), 1024 * 1024).unwrap();
    let host = FakeRootHost::new(
        "sess-root-1",
        capability.clone(),
        OwnerEpoch(1),
        spool,
        ElevatedLimits::default(),
    );
    (host, capability, OwnerEpoch(1))
}

#[test]
fn stale_owner_epoch_cannot_write() {
    let (mut host, capability, owner) = fixture();
    assert_eq!(
        host.handle(&HostMessage::Input {
            owner_epoch: owner,
            capability: capability.clone(),
            bytes: b"ls".to_vec()
        }),
        HostDecision::Accepted
    );
    assert_eq!(host.accepted_input(), b"ls");
    // 旧 Core(更小 owner epoch):拒绝
    assert_eq!(
        host.handle(&HostMessage::Input {
            owner_epoch: OwnerEpoch(0),
            capability: capability.clone(),
            bytes: b"dangerous".to_vec()
        }),
        HostDecision::RejectedStaleOwner
    );
    assert_eq!(host.accepted_input(), b"ls", "被拒输入绝不入队");
}

#[test]
fn mismatched_capability_rejected() {
    let (mut host, capability, owner) = fixture();
    let wrong_session = SessionCapability {
        session_handle: "sess-other".into(),
        ..capability.clone()
    };
    assert_eq!(
        host.handle(&HostMessage::Input {
            owner_epoch: owner,
            capability: wrong_session,
            bytes: vec![0x03]
        }),
        HostDecision::RejectedCapability
    );
    let wrong_epoch = SessionCapability {
        root_epoch: RootEpoch(4),
        ..capability.clone()
    };
    assert_eq!(
        host.handle(&HostMessage::Resize {
            owner_epoch: owner,
            capability: wrong_epoch,
            cols: 80,
            rows: 24
        }),
        HostDecision::RejectedCapability
    );
}

#[test]
fn broker_gate_rejects_wrong_identity_epoch_and_replay() {
    let core = CoreIdentity::new(1000);
    let mut gate = BrokerGate::new(core, RootEpoch(9));
    let capability = SessionCapability {
        session_handle: "sess-root-1".into(),
        core,
        root_epoch: RootEpoch(9),
    };
    let request =
        |core: CoreIdentity, epoch: RootEpoch, nonce: RequestNonce| BrokerRequest::LaunchRootHost {
            protocol: PROTOCOL_VERSION,
            core,
            root_epoch: epoch,
            nonce,
            request_id: Uuid::now_v7(),
            capability: capability.clone(),
        };
    gate.verify(&request(core, RootEpoch(9), RequestNonce::new()))
        .unwrap();
    let mut bad_protocol = request(core, RootEpoch(9), RequestNonce::new());
    if let BrokerRequest::LaunchRootHost { protocol, .. } =
        (&mut bad_protocol) as &mut BrokerRequest
    {
        *protocol = 2;
    }
    assert_eq!(
        gate.verify(&bad_protocol),
        Err(BrokerReject::ProtocolVersion)
    );
    assert_eq!(
        gate.verify(&request(
            CoreIdentity::new(2000),
            RootEpoch(9),
            RequestNonce::new()
        )),
        Err(BrokerReject::CoreIdentity)
    );
    assert_eq!(
        gate.verify(&request(core, RootEpoch(8), RequestNonce::new())),
        Err(BrokerReject::RootEpochStale)
    );
    let nonce = RequestNonce::new();
    gate.verify(&request(core, RootEpoch(9), nonce)).unwrap();
    assert_eq!(
        gate.verify(&request(core, RootEpoch(9), nonce)),
        Err(BrokerReject::NonceReplayOrExpired)
    );
}

#[test]
fn browser_or_plugin_worker_cannot_forge_capability() {
    // capability 绑定 Core PID+start_id+Root epoch:浏览器/Worker 既不知
    // Core start_id 也不持 Root epoch;伪造任意字段都被 host 复验拒绝。
    let (mut host, real, owner) = fixture();
    let forged = SessionCapability {
        session_handle: real.session_handle.clone(),
        core: CoreIdentity::new(9999),
        root_epoch: real.root_epoch,
    };
    assert_eq!(
        host.handle(&HostMessage::Terminate {
            owner_epoch: owner,
            capability: forged
        }),
        HostDecision::RejectedCapability
    );
    assert!(host.is_process_alive(), "伪造终止不得杀死 Root 进程组");
}
