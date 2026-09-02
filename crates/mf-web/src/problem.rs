//! `mf.problem.v1` 与版本协商(T7b,Issue #39;spec §7.2/§7.5)。
//!
//! 稳定错误码全集、retry 语义、HTTP status 大类映射与 WS close code
//! 表。HTTP status 只表达大类,`code + retry` 才是客户端分支依据。

use serde::{Deserialize, Serialize};

/// 当前 HTTP API major(`v1`)。
pub const API_VERSIONS: &[&str] = &["v1"];
/// 当前 WS subprotocol 集。
pub const WS_SUBPROTOCOLS: &[&str] = &["mf-workflow.v1", "mf-terminal.v1"];

/// 稳定错误码(§7.5 全集;字符串形式即 wire 值)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemCode {
    // 协议
    UnsupportedApiVersion,
    UnsupportedWsSubprotocol,
    InvalidEnvelope,
    // 认证
    Unauthenticated,
    OriginRejected,
    CsrfRejected,
    // 角色
    ControllerRequired,
    ControllerLeaseExpired,
    // 资源
    ResourceNotFound,
    #[serde(rename = "resource_not_found")]
    ResourceScopeMismatchInternal,
    // CAS
    RevisionConflict,
    CommandIdReused,
    CommandInProgress,
    // DAG
    ValidationFailed,
    WorkflowCycle,
    UnknownDependency,
    // Agent
    AgentInstanceUnavailable,
    PluginVersionUnavailable,
    CliVersionMismatch,
    // Terminal
    WriterRequired,
    WriterLeaseExpired,
    InputSeqConflict,
    TerminalEpochMismatch,
    TerminalHistoryGap,
    FrameTooLarge,
    RateLimited,
    // Root/安装
    RootModeRequired,
    RootEpochExpired,
    RootAuthorizationDenied,
    BrokerUnavailable,
    ElevationRequired,
    InstallationFailed,
    // 服务
    ResyncRequired,
    ServiceUnavailable,
    InternalError,
    SchemaFutureVersion,
}

impl ProblemCode {
    /// HTTP status 大类(§7.5:大类表达,精细分支看 code+retry)。
    pub fn http_status(self) -> u16 {
        match self {
            Self::UnsupportedApiVersion | Self::UnsupportedWsSubprotocol => 400,
            Self::InvalidEnvelope => 400,
            Self::Unauthenticated | Self::OriginRejected | Self::CsrfRejected => 401,
            Self::ControllerRequired | Self::ControllerLeaseExpired => 403,
            Self::ResourceNotFound | Self::ResourceScopeMismatchInternal => 404,
            Self::RevisionConflict | Self::CommandIdReused | Self::CommandInProgress => 409,
            Self::ValidationFailed | Self::WorkflowCycle | Self::UnknownDependency => 422,
            Self::AgentInstanceUnavailable
            | Self::PluginVersionUnavailable
            | Self::CliVersionMismatch => 409,
            Self::WriterRequired | Self::WriterLeaseExpired | Self::InputSeqConflict => 409,
            Self::TerminalEpochMismatch | Self::TerminalHistoryGap => 409,
            Self::FrameTooLarge => 413,
            Self::RateLimited => 429,
            Self::RootModeRequired
            | Self::RootEpochExpired
            | Self::RootAuthorizationDenied
            | Self::BrokerUnavailable
            | Self::ElevationRequired
            | Self::InstallationFailed => 409,
            Self::ResyncRequired => 409,
            Self::ServiceUnavailable | Self::InternalError | Self::SchemaFutureVersion => 500,
        }
    }
}

/// retry 语义(客户端分支依据)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retry {
    Never,
    SameCommandId,
    AfterResync,
    AfterReauth,
    AfterRetryAfter,
}

/// `mf.problem.v1` envelope。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    pub schema: String,
    pub code: ProblemCode,
    pub message: String,
    #[serde(default)]
    pub trace_id: String,
    #[serde(default)]
    pub command_id: Option<String>,
    #[serde(default)]
    pub retry: Option<Retry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<serde_json::Value>,
}

impl Problem {
    pub fn new(code: ProblemCode, message: impl Into<String>, retry: Option<Retry>) -> Self {
        Self {
            schema: "mf.problem.v1".to_string(),
            code,
            message: message.into(),
            trace_id: String::new(),
            command_id: None,
            retry,
            current: None,
        }
    }
}

/// WS close code 表(§7.5)。
pub mod close_code {
    pub const INVALID_ENVELOPE: u16 = 4400;
    pub const UNAUTHENTICATED: u16 = 4401;
    pub const ROLE_OR_LEASE: u16 = 4403;
    pub const RESYNC_OR_HISTORY_GAP: u16 = 4409;
    pub const FRAME_TOO_LARGE: u16 = 4413;
    pub const RATE_LIMITED: u16 = 4429;
    pub const INTERNAL: u16 = 4500;
}

/// HTTP API 版本协商:无交集明确拒绝(不模糊兼容;§7.2)。
pub fn negotiate_api(requested: &[String]) -> Result<&'static str, ProblemCode> {
    for version in requested {
        if API_VERSIONS.contains(&version.as_str()) {
            return Ok("v1");
        }
    }
    Err(ProblemCode::UnsupportedApiVersion)
}

/// WS subprotocol 协商:客户端请求列表与 Core 支持集求交;无交集拒绝。
pub fn negotiate_ws_subprotocol(requested: &[String]) -> Result<&'static str, ProblemCode> {
    for protocol in requested {
        if let Some(supported) = WS_SUBPROTOCOLS
            .iter()
            .find(|candidate| **candidate == protocol.as_str())
        {
            return Ok(supported);
        }
    }
    Err(ProblemCode::UnsupportedWsSubprotocol)
}
