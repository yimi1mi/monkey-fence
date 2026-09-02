//! Elevated 协议消息与 epoch/capability(T3e,Issue #33;spec §10.2/10.3)。
//!
//! fake seam:消息为可序列化 DTO(JSON golden 稳定),真实 IPC
//! (Named Pipe/UDS + OS ACL)属后续 ticket。浏览器或 Plugin Worker
//! 永远无法构造合法 capability——它由 Broker 签发、绑定 Core 实例与
//! Root epoch,验证时校验全部绑定字段。

use serde::{Deserialize, Serialize};

/// 协议版本。
pub const PROTOCOL_VERSION: u32 = 1;

/// Core 实例身份(PID + 启动身份;§10.2)。fake seam 用 PID + UUID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CoreIdentity {
    pub pid: u32,
    pub start_id: uuid::Uuid,
}

impl CoreIdentity {
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            start_id: uuid::Uuid::now_v7(),
        }
    }
}

/// Root epoch:Root Mode 生命周期标识(§10.1);关闭/重启即失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RootEpoch(pub u64);

/// owner epoch:Core↔host 通道的单调序(旧 Core 不能写,§10.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OwnerEpoch(pub u64);

/// Broker 签发的一次性 128-bit nonce。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestNonce(pub [u8; 16]);

impl RequestNonce {
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(bytes)
    }
}

/// 会话 capability:绑定 Agent Session + Core 实例 + Root epoch。
/// 浏览器/Plugin Worker 无法取得——只有 Broker 按 §10.2 签发。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionCapability {
    pub session_handle: String,
    pub core: CoreIdentity,
    pub root_epoch: RootEpoch,
}

/// Core → Broker 的 Root 请求(§10.2)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerRequest {
    /// 启动 session-scoped root host(携带能力与一次性 nonce)。
    LaunchRootHost {
        protocol: u32,
        core: CoreIdentity,
        root_epoch: RootEpoch,
        nonce: RequestNonce,
        request_id: uuid::Uuid,
        capability: SessionCapability,
    },
}

/// Broker 对请求的验证结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BrokerReject {
    #[error("protocol_version_mismatch")]
    ProtocolVersion,
    #[error("core_identity_mismatch")]
    CoreIdentity,
    #[error("root_epoch_stale")]
    RootEpochStale,
    #[error("nonce_replay_or_expired")]
    NonceReplayOrExpired,
}

/// Core → root host 的输入/control 消息(§10.3:host 复验 owner epoch
/// 与 session capability;旧 Core/旧 lease 不能写)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Input {
        owner_epoch: OwnerEpoch,
        capability: SessionCapability,
        bytes: Vec<u8>,
    },
    Resize {
        owner_epoch: OwnerEpoch,
        capability: SessionCapability,
        cols: u16,
        rows: u16,
    },
    /// Core 主动终止(host 立即结束 Root process group)。
    Terminate {
        owner_epoch: OwnerEpoch,
        capability: SessionCapability,
    },
}

/// host → Core 的应答/事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
    Output {
        bytes: Vec<u8>,
    },
    Exited {
        code: Option<i32>,
    },
    /// Core channel 断开后,新 Core 的 read-only reattach(§10.3)。
    ReattachedReadOnly {
        spool_written_bytes: u64,
    },
    /// orphan grace 到期:Root process group 已终止,留下 spool 记录。
    OrphanTerminated {
        reason: String,
    },
    Rejected {
        reason: String,
    },
}

/// host 持久 receipt:read-only reattach 的凭证(持久 host receipt +
/// OS identity;fake seam 以 Core identity 模拟 OS 身份)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReceipt {
    pub session_handle: String,
    pub original_core: CoreIdentity,
    pub root_epoch: RootEpoch,
}

/// Broker 请求验证(fake broker 状态机;真实 UAC 属 Non-goals)。
pub struct BrokerGate {
    active_core: CoreIdentity,
    active_root_epoch: RootEpoch,
    used_nonces: std::collections::HashSet<[u8; 16]>,
}

impl BrokerGate {
    pub fn new(active_core: CoreIdentity, active_root_epoch: RootEpoch) -> Self {
        Self {
            active_core,
            active_root_epoch,
            used_nonces: std::collections::HashSet::new(),
        }
    }

    /// §10.2 验证:协议版本、Core 实例、当前 Root epoch、nonce 一次性。
    /// (TTL 由调用方在 nonce 签发侧计时;fake gate 只管重放。)
    pub fn verify(&mut self, request: &BrokerRequest) -> Result<(), BrokerReject> {
        let BrokerRequest::LaunchRootHost {
            protocol,
            core,
            root_epoch,
            nonce,
            ..
        } = request;
        if *protocol != PROTOCOL_VERSION {
            return Err(BrokerReject::ProtocolVersion);
        }
        if *core != self.active_core {
            return Err(BrokerReject::CoreIdentity);
        }
        if *root_epoch != self.active_root_epoch {
            return Err(BrokerReject::RootEpochStale);
        }
        if self.used_nonces.contains(&nonce.0) {
            return Err(BrokerReject::NonceReplayOrExpired);
        }
        self.used_nonces.insert(nonce.0);
        Ok(())
    }
}
