//! `mf-terminal.v1` WS adapter(T7d,Issue #42;spec §8)。
//!
//! TerminalChannel 的无损 wire 映射:升级后首帧 attach(附 existence 复
//! 验与 replay)、32-byte binary frame、cumulative ACK/outstanding 预算、
//! writer lease(request/renew/release/revoke)、resize 合并、exit durable
//! 顺序、history gap → 4409(连接不得申请 writer,Web 改读 transcript)、
//! ping/frame/rate/queue limits、拒绝 permessage-deflate。
//!
//! 组件(journal/ack/writer lease/resize/exit)由 mf-terminal 提供
//! (#30–#32);本模块是 transport 无关的会话状态机——axum WebSocket
//! 只做 IO。Web 拿不到 PTY master/raw writer:一切经 TerminalHost。

use std::sync::Arc;
use std::time::Duration;

use mf_terminal::channel::{decode_frame, encode_output_frame, FrameProblem, FRAME_KIND_INPUT};
use mf_terminal::journal::{AttachProblem, TerminalJournal};
use mf_terminal::limits::TerminalLimits;
use mf_terminal::session::{
    plan_attach, AckProblem, AttachPlan, ClientOutputState, SlowClientDecision,
};
use mf_terminal::writer_lease::{
    ConnectionId, InputDecision, ResizeCoalescer, WriterLeaseManager, WriterRequestOutcome,
};
use mf_terminal::{TerminalChannel, TerminalHost};

use crate::problem::{close_code, Problem, ProblemCode, Retry};

/// 服务端 → 客户端 control 帧。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerControl {
    Hello {
        terminal_epoch: String,
        first_available_seq: String,
        last_seq: String,
    },
    WriterGranted {
        writer_lease_id: String,
        ttl_ms: u64,
        renew_after_ms: u64,
    },
    WriterDenied,
    WriterRevoked {
        reason: String,
    },
    InputAck {
        input_seq: String,
        ack_id: String,
    },
    OutOfOrder {
        expected_input_seq: String,
    },
    Exit {
        final_seq: String,
        code: Option<i64>,
    },
    Problem {
        code: String,
        detail: String,
    },
}

/// 客户端 → 服务端 control 帧(升级后首帧必须是 attach)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientControl {
    Attach {
        session_handle: String,
        after_seq: String,
    },
    Ack {
        through_seq: String,
    },
    RequestWriter,
    WriterRenew {
        writer_lease_id: String,
    },
    ReleaseWriter {
        writer_lease_id: String,
    },
    Resize {
        resize_seq: u64,
        cols: u16,
        rows: u16,
    },
    Detach,
}

/// 单个 WS 连接的会话状态机(驱动方逐帧调用)。
pub struct TerminalWsSession {
    limits: TerminalLimits,
    /// journal 由 host 投影驱动(host 是唯一事实源;本地仅缓存 hello)。
    hello: Option<ServerControl>,
    output: Option<ClientOutputState>,
    writer: WriterLeaseManager,
    resize: ResizeCoalescer,
    connection: ConnectionId,
    attached: bool,
    /// exit 已通知(重连先 replay 再重复同一 exit)。
    exit_notified: Option<(u64, Option<i64>)>,
    /// 帧速率(输入字节/秒;A2 input_rate 64KiB/s,burst 4×)。
    input_budget: f64,
    input_last: std::time::Instant,
}

impl TerminalWsSession {
    pub fn new() -> Self {
        let limits = TerminalLimits::default();
        Self {
            input_budget: 4.0 * 64.0 * 1024.0,
            input_last: std::time::Instant::now(),
            limits,
            hello: None,
            output: None,
            writer: WriterLeaseManager::new(Duration::from_secs(10)),
            resize: ResizeCoalescer::new(10),
            connection: ConnectionId(1),
            attached: false,
            exit_notified: None,
        }
    }

    /// 升级后首帧:attach(session 存在性由 kernel `attach_terminal` 复
    /// 验;gap → 4409 且不得 writer)。返回:hello + replay frames 或
    /// 关闭。
    pub fn attach(
        &mut self,
        host: &dyn TerminalHost,
        session_handle: &str,
        after_seq: u64,
        controller_epoch: u64,
    ) -> AttachResult {
        use mf_terminal::TerminalSessionRef;
        let reference = TerminalSessionRef::new(session_handle.to_string());
        // host 必须存活(Web 拿不到 master;唯一入口是 TerminalHost)
        if !host.session_alive(&reference) {
            return AttachResult::Close {
                close_code: close_code::UNAUTHENTICATED,
                problem: Problem::new(
                    ProblemCode::ResourceNotFound,
                    "终端会话不存在或已结束",
                    Some(Retry::Never),
                ),
            };
        }
        // journal 事实(同 epoch)
        let facts = match host.output_facts(&reference) {
            Ok(facts) => facts,
            Err(_) => {
                return AttachResult::Close {
                    close_code: close_code::UNAUTHENTICATED,
                    problem: Problem::new(
                        ProblemCode::ResourceNotFound,
                        "终端会话不可用",
                        Some(Retry::Never),
                    ),
                }
            }
        };
        // gap 判定(§8.3)
        let mut journal = TerminalJournal::with_epoch(facts.terminal_epoch, 16 * 1024 * 1024);
        let _ = &mut journal;
        match plan_attach_facts(facts.first_available_seq, facts.last_seq, after_seq) {
            FactsVerdict::ProtocolError => {
                return AttachResult::Close {
                    close_code: close_code::INVALID_ENVELOPE,
                    problem: Problem::new(
                        ProblemCode::InvalidEnvelope,
                        "after_seq 超过 last_seq",
                        Some(Retry::Never),
                    ),
                }
            }
            FactsVerdict::Gap { first, last } => return AttachResult::Close {
                close_code: close_code::RESYNC_OR_HISTORY_GAP,
                problem: Problem::new(
                    ProblemCode::TerminalHistoryGap,
                    format!(
                        "history gap:first_available={first} last={last};改读 terminal-transcript"
                    ),
                    Some(Retry::AfterResync),
                ),
            },
            FactsVerdict::Ok => {}
        }
        let replay = host
            .replay_output(&reference, after_seq)
            .unwrap_or_default();
        let hello = ServerControl::Hello {
            terminal_epoch: facts.terminal_epoch.as_uuid().to_string(),
            first_available_seq: facts.first_available_seq.to_string(),
            last_seq: facts.last_seq.to_string(),
        };
        let frames: Vec<Vec<u8>> = replay
            .iter()
            .map(|chunk| encode_output_frame(chunk.seq, &chunk.bytes).unwrap())
            .collect();
        self.attached = true;
        self.hello = Some(hello.clone());
        let limits = self.limits.clone();
        self.output = Some(ClientOutputState::new(limits, after_seq));
        let _ = controller_epoch;
        AttachResult::Attached { hello, frames }
    }

    /// 客户端 binary input 帧(writer lease 复验 + input_seq 幂等 +
    /// 字节速率;成功字节经 host 写入——绝不暴露 master)。
    pub fn binary_input(
        &mut self,
        host: &dyn TerminalHost,
        frame: &[u8],
        controller_epoch: u64,
        session_handle: &str,
    ) -> InputOutcome {
        // frame 校验(4413/kind)
        let decoded = match decode_frame(frame) {
            Ok(decoded) => decoded,
            Err(problem) => {
                let (code, close) = match &problem {
                    FrameProblem::TooLarge { .. } => (ProblemCode::FrameTooLarge, true),
                    _ => (ProblemCode::InvalidEnvelope, true),
                };
                return InputOutcome::Rejected {
                    problem: Problem::new(code, problem.to_string(), Some(Retry::Never)),
                    close,
                };
            }
        };
        if decoded.kind != FRAME_KIND_INPUT {
            return InputOutcome::Rejected {
                problem: Problem::new(
                    ProblemCode::InvalidEnvelope,
                    "期望 input 帧",
                    Some(Retry::Never),
                ),
                close: true,
            };
        }
        // 字节速率(A2:64 KiB/s,burst 4×)
        let elapsed = self.input_last.elapsed().as_secs_f64();
        self.input_last = std::time::Instant::now();
        self.input_budget = (self.input_budget + elapsed * 64.0 * 1024.0).min(4.0 * 64.0 * 1024.0);
        if self.input_budget < decoded.payload.len() as f64 {
            return InputOutcome::Rejected {
                problem: Problem::new(
                    ProblemCode::RateLimited,
                    "输入字节速率超限",
                    Some(Retry::AfterRetryAfter),
                ),
                close: false,
            };
        }
        self.input_budget -= decoded.payload.len() as f64;

        // input_seq 幂等 + lease 复验(lease id = frame 的 writer_lease_id)
        let lease_id = decoded.writer_lease_id;
        let digest = sha2_digest(decoded.payload);
        match self
            .writer
            .submit_input(lease_id, self.connection, decoded.seq, digest)
        {
            InputDecision::Admitted => {
                use mf_terminal::TerminalSessionRef;
                let result =
                    host.send_input(&TerminalSessionRef::new(session_handle), decoded.payload);
                let write_ok = result.is_ok();
                let ack = self
                    .writer
                    .complete_input(lease_id, decoded.seq, digest, write_ok);
                match ack {
                    Ok(Some(ack_id)) => InputOutcome::Acked {
                        input_seq: decoded.seq,
                        ack_id,
                    },
                    Ok(None) => InputOutcome::Rejected {
                        problem: Problem::new(
                            ProblemCode::InternalError,
                            "write_all 未产生 ack",
                            None,
                        ),
                        close: true,
                    },
                    Err(reason) => InputOutcome::Rejected {
                        problem: Problem::new(
                            ProblemCode::WriterLeaseExpired,
                            format!("writer 撤销:{reason:?}"),
                            Some(Retry::AfterReauth),
                        ),
                        close: true,
                    },
                }
            }
            InputDecision::DuplicateAck { ack_id } => InputOutcome::Acked {
                input_seq: decoded.seq,
                ack_id,
            },
            InputDecision::OutOfOrder { expected_seq } => InputOutcome::OutOfOrder { expected_seq },
            InputDecision::Conflict => InputOutcome::Rejected {
                problem: Problem::new(
                    ProblemCode::InputSeqConflict,
                    "同 input_seq 异 payload",
                    Some(Retry::Never),
                ),
                close: true,
            },
            InputDecision::NoWriter { reason } => InputOutcome::Rejected {
                problem: Problem::new(
                    ProblemCode::WriterRequired,
                    format!("无有效 writer:{reason:?}"),
                    Some(Retry::AfterReauth),
                ),
                close: false,
            },
        }
        .with_epoch(controller_epoch)
    }

    /// 客户端 control 帧。
    pub fn control(
        &mut self,
        host: &dyn TerminalHost,
        session_handle: &str,
        control: ClientControl,
        controller_epoch: u64,
    ) -> ControlOutcome {
        match control {
            ClientControl::Attach {
                session_handle,
                after_seq,
            } => {
                let after: u64 = match after_seq.parse() {
                    Ok(value) => value,
                    Err(_) => {
                        return ControlOutcome::Close {
                            close_code: close_code::INVALID_ENVELOPE,
                            problem: Problem::new(
                                ProblemCode::InvalidEnvelope,
                                "after_seq 必须是 u64 字符串",
                                Some(Retry::Never),
                            ),
                        }
                    }
                };
                match self.attach(host, &session_handle, after, controller_epoch) {
                    AttachResult::Attached { hello, frames } => {
                        ControlOutcome::Attached(hello, frames)
                    }
                    AttachResult::Close {
                        close_code,
                        problem,
                    } => ControlOutcome::Close {
                        close_code,
                        problem,
                    },
                }
            }
            ClientControl::Ack { through_seq } => {
                let Some(output) = self.output.as_mut() else {
                    return ControlOutcome::Close {
                        close_code: close_code::INVALID_ENVELOPE,
                        problem: Problem::new(
                            ProblemCode::InvalidEnvelope,
                            "ack 先于 attach",
                            Some(Retry::Never),
                        ),
                    };
                };
                let through: u64 = match through_seq.parse() {
                    Ok(value) => value,
                    Err(_) => {
                        return ControlOutcome::Close {
                            close_code: close_code::INVALID_ENVELOPE,
                            problem: Problem::new(
                                ProblemCode::InvalidEnvelope,
                                "through_seq 必须是 u64 字符串",
                                Some(Retry::Never),
                            ),
                        }
                    }
                };
                match output.ack(through) {
                    Ok(()) => ControlOutcome::Continued(None),
                    Err(AckProblem::BeyondHighestSent { highest_sent, .. }) => {
                        ControlOutcome::Close {
                            close_code: close_code::INVALID_ENVELOPE,
                            problem: Problem::new(
                                ProblemCode::InvalidEnvelope,
                                format!("ack 超过已发送最高 seq({highest_sent})"),
                                Some(Retry::Never),
                            ),
                        }
                    }
                }
            }
            ClientControl::RequestWriter => {
                if self.hello.is_none() {
                    return ControlOutcome::Close {
                        close_code: close_code::INVALID_ENVELOPE,
                        problem: Problem::new(
                            ProblemCode::InvalidEnvelope,
                            "writer 先于 attach",
                            Some(Retry::Never),
                        ),
                    };
                }
                match self
                    .writer
                    .request_writer(controller_epoch, self.connection)
                {
                    WriterRequestOutcome::Granted {
                        lease_id,
                        ttl_ms,
                        renew_after_ms,
                    } => ControlOutcome::Continued(Some(ServerControl::WriterGranted {
                        writer_lease_id: lease_id.iter().map(|b| format!("{b:02x}")).collect(),
                        ttl_ms,
                        renew_after_ms,
                    })),
                    WriterRequestOutcome::Denied => {
                        ControlOutcome::Continued(Some(ServerControl::WriterDenied))
                    }
                }
            }
            ClientControl::WriterRenew { writer_lease_id } => {
                let lease = parse_lease(&writer_lease_id);
                match self.writer.renew(lease, controller_epoch) {
                    mf_terminal::writer_lease::WriterRenewOutcome::Renewed { .. } => {
                        ControlOutcome::Continued(None)
                    }
                    mf_terminal::writer_lease::WriterRenewOutcome::Revoked { reason } => {
                        ControlOutcome::Continued(Some(ServerControl::WriterRevoked {
                            reason: format!("{reason:?}"),
                        }))
                    }
                }
            }
            ClientControl::ReleaseWriter { writer_lease_id } => {
                let lease = parse_lease(&writer_lease_id);
                let _ = self.writer.release(lease);
                ControlOutcome::Continued(None)
            }
            ClientControl::Resize {
                resize_seq,
                cols,
                rows,
            } => {
                // 合并窗口:洪泛 resize 只保留最新;flush 应用真实 PTY
                match self.resize.submit(resize_seq, cols, rows) {
                    mf_terminal::writer_lease::ResizeDecision::InvalidBounds => {
                        ControlOutcome::Close {
                            close_code: close_code::INVALID_ENVELOPE,
                            problem: Problem::new(
                                ProblemCode::InvalidEnvelope,
                                "resize 尺寸越界(cols 2-500/rows 2-300)",
                                Some(Retry::Never),
                            ),
                        }
                    }
                    _ => {
                        if let Some((seq, cols, rows)) = self.resize.flush() {
                            let _ = seq;
                            use mf_terminal::TerminalSessionRef;
                            let _ = host.resize_session(
                                &TerminalSessionRef::new(session_handle),
                                cols,
                                rows,
                            );
                        }
                        ControlOutcome::Continued(None)
                    }
                }
            }
            ClientControl::Detach => ControlOutcome::Close {
                close_code: 1000,
                problem: Problem::new(ProblemCode::InternalError, "detach", None),
            },
        }
    }

    /// 连接关闭:撤销 writer(§8.4 connection_closed)。
    pub fn connection_closed(&mut self) {
        self.writer.connection_closed(self.connection);
    }

    /// 输出预算轮询(发送循环;慢客户端只关自身)。
    pub fn poll_output(&mut self) -> Option<Problem> {
        let output = self.output.as_mut()?;
        match output.poll_slow_client() {
            SlowClientDecision::Continue | SlowClientDecision::Paused { .. } => None,
            SlowClientDecision::ShouldClose {
                outstanding_bytes, ..
            } => Some(Problem::new(
                ProblemCode::RateLimited,
                format!("慢客户端宽限耗尽(outstanding={outstanding_bytes}B)"),
                Some(Retry::AfterResync),
            )),
        }
    }

    /// 已 attach 且收到全部 replay 后的 live channel 供发送循环取用。
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// 会话存活轮询:PTY 结束 → durable 顺序的 exit 通知(journal 的
    /// final_seq 已由 T3d ExitGate durable-before-notify 保证;重复
    /// poll 重复同一 exit,§8.5)。
    pub fn poll_exit(
        &mut self,
        host: &dyn TerminalHost,
        session_handle: &str,
    ) -> Option<ServerControl> {
        if !self.attached {
            return None;
        }
        use mf_terminal::TerminalSessionRef;
        let reference = TerminalSessionRef::new(session_handle.to_string());
        if host.session_alive(&reference) {
            return None;
        }
        if let Some((final_seq, code)) = self.exit_notified {
            return Some(exit_control(final_seq, code));
        }
        let facts = host.output_facts(&reference).ok()?;
        self.exit_notified = Some((facts.last_seq, None));
        Some(exit_control(facts.last_seq, None))
    }
}

impl Default for TerminalWsSession {
    fn default() -> Self {
        Self::new()
    }
}

fn exit_control(final_seq: u64, code: Option<i64>) -> ServerControl {
    ServerControl::Exit {
        final_seq: final_seq.to_string(),
        code,
    }
}

fn parse_lease(text: &str) -> [u8; 16] {
    let mut lease = [0u8; 16];
    let bytes: Vec<u8> = (0..16)
        .filter_map(|i| {
            text.get(i * 2..i * 2 + 2)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        })
        .collect();
    if bytes.len() == 16 {
        lease.copy_from_slice(&bytes);
    }
    lease
}

fn sha2_digest(payload: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(payload);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

enum FactsVerdict {
    Ok,
    Gap { first: u64, last: u64 },
    ProtocolError,
}

fn plan_attach_facts(first_available: u64, last_seq: u64, after_seq: u64) -> FactsVerdict {
    if after_seq > last_seq {
        return FactsVerdict::ProtocolError;
    }
    if after_seq + 1 < first_available {
        return FactsVerdict::Gap {
            first: first_available,
            last: last_seq,
        };
    }
    FactsVerdict::Ok
}

/// attach 结果。
#[derive(Debug)]
pub enum AttachResult {
    Attached {
        hello: ServerControl,
        frames: Vec<Vec<u8>>,
    },
    Close {
        close_code: u16,
        problem: Problem,
    },
}

/// binary input 处置。
#[derive(Debug)]
pub enum InputOutcome {
    Acked { input_seq: u64, ack_id: u64 },
    OutOfOrder { expected_seq: u64 },
    Rejected { problem: Problem, close: bool },
}

impl InputOutcome {
    fn with_epoch(self, _controller_epoch: u64) -> Self {
        // epoch 复验点在 lease(request/renew 绑定);此处携带仅为诊断
        self
    }
}

/// control 处置。
#[derive(Debug)]
pub enum ControlOutcome {
    /// attach 成功(hello + replay)。
    Attached(ServerControl, Vec<Vec<u8>>),
    /// 继续会话(可选服务端 control 回执)。
    Continued(Option<ServerControl>),
    Close {
        close_code: u16,
        problem: Problem,
    },
}

/// 拒绝 permessage-deflate(§8.1):upgrade 头检查。
pub fn accepts_compression(extensions_header: Option<&str>) -> bool {
    // 返回 true 表示"可接受升级"(即未请求 deflate);请求了任何
    // permessage-deflate 形态 → false(拒绝升级)
    match extensions_header {
        None => true,
        Some(header) => !header.to_ascii_lowercase().contains("permessage-deflate"),
    }
}
