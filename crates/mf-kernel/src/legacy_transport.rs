//! `mf.legacy-transport.v1` 本地 transport(T6a,Issue #37)。
//!
//! GPUI/launcher/tray/测试 harness 经 versioned 本地 IPC(NDJSON over
//! Named Pipe/UDS,当前用户 ACL)以**普通 Client** 身份访问 CoreKernel
//! 五接口——没有"可信内置调用"特权;opaque handle/problem/idempotency/
//! CAS 全部不旁路(与 Web transport 只差认证方式,§2.2)。协议帧即
//! kernel 内部 DTO 的 serde 形态(封闭命令族/snapshot query/cursor)。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::handles::CommandId;
use crate::handles::{ClientId, Principal, SessionHandle};
use crate::kernel::{
    CoreKernel, KernelCommand, KernelCommandRequest, KernelOutcome, KernelProblem, TerminalAttach,
    TerminalChannel,
};
use crate::projection::{EventCursor, SnapshotQuery};

/// transport 协议版本(NDJSON 帧)。
pub const LEGACY_TRANSPORT_PROTOCOL: &str = "mf.legacy-transport.v1";

/// 请求帧(每行一个 JSON)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LegacyRequest {
    /// 客户端注册(角色/lease 由 Core 判定;transport 只携带身份)。
    Hello {
        protocol: String,
        client_id: String,
        principal: String,
        controller_lease_epoch: u64,
    },
    Dispatch {
        command_id: String,
        command: KernelCommand,
    },
    Snapshot {
        query: SnapshotQuery,
    },
    SubscribeEvents {
        cursor: EventCursor,
    },
    AttachTerminal {
        session: String,
        after_seq: u64,
    },
    Shutdown,
}

/// 响应帧。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LegacyResponse {
    HelloAccepted,
    Outcome {
        result: KernelOutcomeWire,
    },
    SnapshotEnvelope {
        envelope: serde_json::Value,
    },
    Subscribed {
        /// live 流由调用方持 subscription 轮询(NDJSON 同步模型:
        /// transport 只做 cursor 注册与首批)。
        hello: serde_json::Value,
    },
    TerminalChannelAttached,
    Problem {
        code: String,
        message: String,
    },
    ProtocolMismatch {
        expected: String,
    },
    ShuttingDown,
}

/// KernelOutcome 的 wire 形态(serde 化)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum KernelOutcomeWire {
    Accepted {
        operation_handle: String,
    },
    Applied {
        semantic_revision: u64,
        presentation_revision: u64,
        replayed: bool,
    },
    RunApplied {
        revision: u64,
        replayed: bool,
    },
}

/// transport 会话(单连接):注册身份后逐请求路由到 CoreKernel。
pub struct LegacyTransportSession {
    kernel: Arc<dyn CoreKernel>,
    client: Option<(ClientId, Principal, u64)>,
}

impl LegacyTransportSession {
    pub fn new(kernel: Arc<dyn CoreKernel>) -> Self {
        Self {
            kernel,
            client: None,
        }
    }

    /// 处理一行请求,返回一行响应(None = 静默忽略空行)。
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let request: LegacyRequest = match serde_json::from_str(trimmed) {
            Ok(request) => request,
            Err(error) => {
                return Some(
                    serde_json::to_string(&LegacyResponse::Problem {
                        code: "invalid_envelope".into(),
                        message: error.to_string(),
                    })
                    .expect("serde"),
                );
            }
        };
        let response = self.route(request);
        Some(serde_json::to_string(&response).expect("serde"))
    }

    fn route(&mut self, request: LegacyRequest) -> LegacyResponse {
        match request {
            LegacyRequest::Hello {
                protocol,
                client_id,
                principal,
                controller_lease_epoch,
            } => {
                if protocol != LEGACY_TRANSPORT_PROTOCOL {
                    return LegacyResponse::ProtocolMismatch {
                        expected: LEGACY_TRANSPORT_PROTOCOL.into(),
                    };
                }
                match (ClientId::parse(client_id), Principal::parse(principal)) {
                    (Ok(client_id), Ok(principal)) => {
                        self.client = Some((client_id, principal, controller_lease_epoch));
                        LegacyResponse::HelloAccepted
                    }
                    (Err(error), _) | (_, Err(error)) => LegacyResponse::Problem {
                        code: "invalid_envelope".into(),
                        message: error.to_string(),
                    },
                }
            }
            LegacyRequest::Dispatch {
                command_id,
                command,
            } => {
                let Some((client_id, principal, epoch)) = self.client.clone() else {
                    return unauthenticated();
                };
                let command_id = match CommandId::parse(command_id) {
                    Ok(id) => id,
                    Err(error) => {
                        return LegacyResponse::Problem {
                            code: "invalid_envelope".into(),
                            message: error.to_string(),
                        }
                    }
                };
                // 唯一写路径:transport 不旁路 idempotency/CAS/lease
                let request =
                    KernelCommandRequest::new(command_id, client_id, principal, epoch, command);
                match self.kernel.dispatch(request) {
                    Ok(outcome) => LegacyResponse::Outcome {
                        result: outcome_to_wire(outcome),
                    },
                    Err(problem) => problem_response(problem),
                }
            }
            LegacyRequest::Snapshot { query } => {
                if self.client.is_none() {
                    return unauthenticated();
                }
                match self.kernel.snapshot(query) {
                    Ok(envelope) => LegacyResponse::SnapshotEnvelope {
                        envelope: serde_json::to_value(&envelope).unwrap_or_default(),
                    },
                    Err(problem) => problem_response(problem),
                }
            }
            LegacyRequest::SubscribeEvents { cursor } => {
                if self.client.is_none() {
                    return unauthenticated();
                }
                match self.kernel.subscribe_events(cursor) {
                    Ok(subscription) => {
                        // hello 投影(serde 形态);live 轮询由连接循环
                        // 持 subscription 驱动(#48 standalone 接线)。
                        let hello = serde_json::to_value(subscription.hello()).unwrap_or_default();
                        LegacyResponse::Subscribed { hello }
                    }
                    Err(problem) => problem_response(problem),
                }
            }
            LegacyRequest::AttachTerminal { session, after_seq } => {
                if self.client.is_none() {
                    return unauthenticated();
                }
                let session = match SessionHandle::parse(session) {
                    Ok(handle) => handle,
                    Err(error) => {
                        return LegacyResponse::Problem {
                            code: "invalid_envelope".into(),
                            message: error.to_string(),
                        }
                    }
                };
                match self
                    .kernel
                    .attach_terminal(session, TerminalAttach { after_seq })
                {
                    Ok(_channel) => LegacyResponse::TerminalChannelAttached,
                    Err(problem) => problem_response(problem),
                }
            }
            LegacyRequest::Shutdown => {
                if self.client.is_none() {
                    unauthenticated()
                } else {
                    LegacyResponse::ShuttingDown
                }
            }
        }
    }
}

fn unauthenticated() -> LegacyResponse {
    LegacyResponse::Problem {
        code: "unauthenticated".into(),
        message: "transport 未注册(先 Hello)".into(),
    }
}

fn problem_response(problem: KernelProblem) -> LegacyResponse {
    LegacyResponse::Problem {
        code: problem.code().into(),
        message: problem.to_string(),
    }
}

fn outcome_to_wire(outcome: KernelOutcome) -> KernelOutcomeWire {
    match outcome {
        KernelOutcome::Accepted { operation_handle } => KernelOutcomeWire::Accepted {
            operation_handle: operation_handle.as_str().to_string(),
        },
        KernelOutcome::Applied {
            revisions,
            replayed,
        } => KernelOutcomeWire::Applied {
            semantic_revision: revisions.semantic_revision,
            presentation_revision: revisions.presentation_revision,
            replayed,
        },
        KernelOutcome::RunApplied { revision, replayed } => {
            KernelOutcomeWire::RunApplied { revision, replayed }
        }
    }
}

/// Named Pipe / UDS 端点路径(当前用户目录下;OS ACL 由平台层保证:
/// Windows 自定义 DACL / Unix socket 文件权限 0600)。
pub fn default_endpoint() -> PathBuf {
    let base = std::env::temp_dir();
    #[cfg(windows)]
    {
        // Windows Named Pipe 名(\\\\.\\pipe\\...);文件系统路径仅用于
        // discovery 记录。端点随机后缀由调用方追加。
        base.join("monkeyfence-legacy-transport")
    }
    #[cfg(not(windows))]
    {
        base.join("monkeyfence-legacy-transport.sock")
    }
}

/// 同步帧循环(reader → session → writer)。断线即重连(Hello 重注册;
/// command idempotency 由 Core 保证同 id 同结果)。
pub fn serve_connection<R: std::io::Read, W: std::io::Write>(
    session: &mut LegacyTransportSession,
    reader: R,
    mut writer: W,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(()); // 对端关闭
        }
        if let Some(response) = session.handle_line(&line) {
            writer.write_all(response.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_mismatch_is_rejected() {
        let kernel = fake_kernel();
        let mut session = LegacyTransportSession::new(kernel);
        let response = session
            .handle_line(
                &serde_json::to_string(&LegacyRequest::Hello {
                    protocol: "mf.legacy-transport.v9".into(),
                    client_id: "cl_a".into(),
                    principal: "user".into(),
                    controller_lease_epoch: 1,
                })
                .unwrap(),
            )
            .unwrap();
        let parsed: LegacyResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            parsed,
            LegacyResponse::ProtocolMismatch { expected } if expected == LEGACY_TRANSPORT_PROTOCOL
        ));
    }

    #[test]
    fn requests_before_hello_are_unauthenticated() {
        let kernel = fake_kernel();
        let mut session = LegacyTransportSession::new(kernel);
        let response = session
            .handle_line(&serde_json::to_string(&LegacyRequest::Shutdown).unwrap())
            .unwrap();
        let parsed: LegacyResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            parsed,
            LegacyResponse::Problem { code, .. } if code == "unauthenticated"
        ));
    }

    #[test]
    fn hello_then_dispatch_routes_through_kernel() {
        let kernel = fake_kernel();
        let mut session = LegacyTransportSession::new(kernel.clone());
        let hello = hello_line();
        let _ = session.handle_line(&hello);
        // dispatch 一个 rename:fake kernel 返回固定 outcome
        let request = serde_json::to_string(&LegacyRequest::Dispatch {
            command_id: uuid::Uuid::now_v7().to_string(),
            command: rename_command(),
        })
        .unwrap();
        let response = session.handle_line(&request).unwrap();
        let parsed: LegacyResponse = serde_json::from_str(&response).unwrap();
        match parsed {
            LegacyResponse::Outcome { result } => {
                assert!(matches!(
                    result,
                    KernelOutcomeWire::Applied {
                        replayed: false,
                        ..
                    }
                ));
            }
            other => panic!("期望 outcome:{other:?}"),
        }
    }

    #[test]
    fn garbage_line_yields_problem_not_crash() {
        let kernel = fake_kernel();
        let mut session = LegacyTransportSession::new(kernel);
        let response = session.handle_line("not json").unwrap();
        let parsed: LegacyResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            parsed,
            LegacyResponse::Problem { code, .. } if code == "invalid_envelope"
        ));
    }

    fn hello_line() -> String {
        serde_json::to_string(&LegacyRequest::Hello {
            protocol: LEGACY_TRANSPORT_PROTOCOL.into(),
            client_id: "cl_a".into(),
            principal: "user".into(),
            controller_lease_epoch: 1,
        })
        .unwrap()
    }

    fn rename_command() -> KernelCommand {
        KernelCommand::workflow_rename(
            crate::handles::ProjectStoreHandle::parse(format!("proj_{}", uuid::Uuid::now_v7()))
                .unwrap(),
            crate::handles::WorkflowHandle::parse(uuid::Uuid::now_v7().simple().to_string())
                .unwrap(),
            "重命名",
            1,
        )
    }

    fn fake_kernel() -> Arc<dyn CoreKernel> {
        struct FakeKernel;
        impl CoreKernel for FakeKernel {
            fn dispatch(
                &self,
                _request: KernelCommandRequest,
            ) -> Result<KernelOutcome, KernelProblem> {
                Ok(KernelOutcome::Applied {
                    revisions: crate::projection::RevisionVector {
                        semantic_revision: 1,
                        presentation_revision: 2,
                    },
                    replayed: false,
                })
            }
            fn snapshot(
                &self,
                _query: SnapshotQuery,
            ) -> Result<crate::projection::SnapshotEnvelope, KernelProblem> {
                Err(KernelProblem::ServiceUnavailable("fake".into()))
            }
            fn subscribe_events(
                &self,
                _cursor: EventCursor,
            ) -> Result<crate::projection::EventSubscription, KernelProblem> {
                Err(KernelProblem::ServiceUnavailable("fake".into()))
            }
            fn attach_terminal(
                &self,
                _session: SessionHandle,
                _attach: TerminalAttach,
            ) -> Result<TerminalChannel, KernelProblem> {
                Err(KernelProblem::ServiceUnavailable("fake".into()))
            }
            fn shutdown(
                &self,
                _intent: crate::shutdown::ShutdownIntent,
            ) -> crate::shutdown::ShutdownAssessment {
                crate::shutdown::ShutdownAssessment::default()
            }
            fn grant_controller(
                &self,
                _client_id: &str,
                _principal: &str,
            ) -> Result<u64, KernelProblem> {
                Err(KernelProblem::ServiceUnavailable("fake".into()))
            }
            fn controller_epoch(&self) -> u64 {
                0
            }
            fn attach_project(&self, _root: &std::path::Path) -> Result<String, KernelProblem> {
                Err(KernelProblem::ServiceUnavailable("fake".into()))
            }
            fn detach_project(&self, _project_handle: &str) -> Result<(), KernelProblem> {
                Err(KernelProblem::ServiceUnavailable("fake".into()))
            }
        }
        Arc::new(FakeKernel)
    }
}
