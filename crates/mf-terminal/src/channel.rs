//! TerminalChannel 与 TerminalHost(T2f shim,Issue #28)。
//!
//! `TerminalChannel` 是 `CoreKernel::attach_terminal` 返回的终端通道:
//! 只暴露输入字节、终止与只读 tail 查询,不暴露 `PtyMaster`、raw writer
//! 或任何可直接 mutation SessionRegistry 的句柄。`TerminalHost` 由拥有
//! SessionRuntime 的装配件实现;T2 阶段唯一实现是 legacy SessionRegistry
//! 的 shim,T3 迁移后由本 crate 的 session runtime 直接实现。

/// 终端会话的 opaque 引用(legacy SessionRegistry public handle 形态,
/// `sess_` UUIDv7;T3 起由 mf-terminal 自己签发)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalSessionRef(String);

impl TerminalSessionRef {
    pub fn new(handle: impl Into<String>) -> Self {
        Self(handle.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 终端通道问题。不携带内核级 lease/CAS 语义(那属于 KernelProblem);
/// T3 扩展为 §8 的协议问题族(writer lease、history gap 等)。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TerminalProblem {
    #[error("终端会话不存在或已结束: {0}")]
    SessionNotFound(String),
    #[error("终端宿主不可用: {0}")]
    HostUnavailable(String),
    #[error("终端写入失败: {0}")]
    WriteFailed(String),
}

/// 终端宿主缝隙:由拥有 SessionRuntime/PTY 的装配件实现。
///
/// T2 阶段唯一生产实现位于 legacy `SessionRegistry`(同文件语义的
/// shim);实现方保证:
///
/// - `send_input` 与旧 `send_prompt_raw` 一样按字节透传(行规程由真实
///   CLI 侧处理),T3 之前不引入 seq/ACK;
/// - `terminate` 与旧 `kill_session` 一样走进程组/Job Object 清理;
/// - 读方法只返回渲染后 tail,不泄漏内部 PTY 句柄。
pub trait TerminalHost: Send + Sync {
    /// 会话是否仍存活(attach 前的存在性校验)。
    fn session_alive(&self, session: &TerminalSessionRef) -> bool;

    /// 发送原始输入字节到会话 PTY(唯一写入口)。
    fn send_input(&self, session: &TerminalSessionRef, bytes: &[u8])
        -> Result<(), TerminalProblem>;

    /// 终止会话(进程组/Job Object 清理由宿主负责)。
    fn terminate_session(&self, session: &TerminalSessionRef) -> Result<(), TerminalProblem>;

    /// 只读 tail 查询(渲染后行;shim 兼容旧 `pty_tail`)。
    fn tail_lines(&self, session: &TerminalSessionRef, lines: usize) -> Vec<String>;

    /// T3f:journal 增量 replay(reconnect;8.3)。默认不可用——宿主
    /// 未接 journal 管线时明确拒绝,不给半吊子 replay。
    fn replay_output(
        &self,
        _session: &TerminalSessionRef,
        _after_seq: u64,
    ) -> Result<Vec<crate::journal::JournalChunk>, TerminalProblem> {
        Err(TerminalProblem::HostUnavailable(
            "该宿主未接入 journal replay".into(),
        ))
    }

    /// T3f:会话输出事实(hello 投影;8.1)。默认不可用。
    fn output_facts(
        &self,
        _session: &TerminalSessionRef,
    ) -> Result<crate::journal::HelloFacts, TerminalProblem> {
        Err(TerminalProblem::HostUnavailable(
            "该宿主未接入 journal".into(),
        ))
    }

    /// T3f:真实 resize 到 PTY + Screen 投影(仅 PTY 会话;HTTP 会话
    /// 与未接线的宿主明确拒绝)。边界由实现复验(cols 2-500/rows 2-300)。
    fn resize_session(
        &self,
        _session: &TerminalSessionRef,
        _cols: u16,
        _rows: u16,
    ) -> Result<(), TerminalProblem> {
        Err(TerminalProblem::HostUnavailable(
            "该宿主不支持 resize".into(),
        ))
    }
}

/// `attach_terminal` 返回的终端通道。
///
/// 调用者只能经此通道与终端交互;构造函数私有,唯一来源是 kernel 的
/// `attach_terminal`(宿主注入由装配件完成,不经本类型)。
#[derive(Clone)]
pub struct TerminalChannel {
    host: std::sync::Arc<dyn TerminalHost>,
    session: TerminalSessionRef,
}

impl TerminalChannel {
    /// 构造仅限宿主装配件:生产路径是 `CoreKernel::attach_terminal`
    /// (mf-kernel)在注入的 `TerminalHost` 上调用本方法。仓库级
    /// mutation bypass audit 禁止 UI/Companion 直接调用。
    pub fn attach(host: std::sync::Arc<dyn TerminalHost>, session: TerminalSessionRef) -> Self {
        Self { host, session }
    }

    pub fn session(&self) -> &TerminalSessionRef {
        &self.session
    }

    /// 发送原始输入字节。这是 T2 阶段唯一的终端写入口,替代旧
    /// `send_prompt`/`send_prompt_raw` 旁路。
    pub fn send_input(&self, bytes: &[u8]) -> Result<(), TerminalProblem> {
        self.host.send_input(&self.session, bytes)
    }

    /// 终止会话。
    pub fn terminate(&self) -> Result<(), TerminalProblem> {
        self.host.terminate_session(&self.session)
    }

    /// 会话是否仍存活。
    pub fn is_alive(&self) -> bool {
        self.host.session_alive(&self.session)
    }

    /// 只读 tail 查询(shim 兼容旧渲染 tail;T3 起由 journal/replay 取代)。
    pub fn tail_lines(&self, lines: usize) -> Vec<String> {
        self.host.tail_lines(&self.session, lines)
    }

    /// T3f:journal 增量 replay(reconnect 后恢复屏幕/状态;8.3)。
    pub fn replay_output(
        &self,
        after_seq: u64,
    ) -> Result<Vec<crate::journal::JournalChunk>, TerminalProblem> {
        self.host.replay_output(&self.session, after_seq)
    }

    /// T3f:输出事实(next/last/first_available seq + epoch)。
    pub fn output_facts(&self) -> Result<crate::journal::HelloFacts, TerminalProblem> {
        self.host.output_facts(&self.session)
    }

    /// T3f:真实 resize(到达 PTY 与 Screen;8.5)。
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), TerminalProblem> {
        self.host.resize_session(&self.session, cols, rows)
    }
}

impl std::fmt::Debug for TerminalChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalChannel")
            .field("session", &self.session.as_str())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct FakeHost {
        alive: bool,
        inputs: Mutex<Vec<Vec<u8>>>,
        terminated: AtomicUsize,
    }

    impl TerminalHost for FakeHost {
        fn session_alive(&self, _session: &TerminalSessionRef) -> bool {
            self.alive
        }

        fn send_input(
            &self,
            _session: &TerminalSessionRef,
            bytes: &[u8],
        ) -> Result<(), TerminalProblem> {
            if !self.alive {
                return Err(TerminalProblem::SessionNotFound("已结束".into()));
            }
            self.inputs.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }

        fn terminate_session(&self, _session: &TerminalSessionRef) -> Result<(), TerminalProblem> {
            self.terminated.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn tail_lines(&self, _session: &TerminalSessionRef, _lines: usize) -> Vec<String> {
            vec!["tail".to_string()]
        }
    }

    #[test]
    fn channel_routes_input_terminate_and_tail_to_host() {
        let host = std::sync::Arc::new(FakeHost {
            alive: true,
            inputs: Mutex::new(Vec::new()),
            terminated: AtomicUsize::new(0),
        });
        let channel = TerminalChannel::attach(host.clone(), TerminalSessionRef::new("sess-test"));
        assert!(channel.is_alive());
        channel.send_input(b"/model x\r").unwrap();
        channel.terminate().unwrap();
        assert_eq!(host.inputs.lock().unwrap()[0], b"/model x\r");
        assert_eq!(host.terminated.load(Ordering::SeqCst), 1);
        assert_eq!(channel.tail_lines(4), vec!["tail".to_string()]);
        assert_eq!(channel.session().as_str(), "sess-test");
    }

    #[test]
    fn dead_session_input_fails_closed() {
        let host = std::sync::Arc::new(FakeHost {
            alive: false,
            inputs: Mutex::new(Vec::new()),
            terminated: AtomicUsize::new(0),
        });
        let channel = TerminalChannel::attach(host, TerminalSessionRef::new("sess-dead"));
        assert!(!channel.is_alive());
        assert!(channel.send_input(b"\x1b").is_err());
    }
}

// ---------------------------------------------------------------------------
// Terminal v1 binary frame codec(§8.1;T3b,Issue #30)
// ---------------------------------------------------------------------------

/// 固定 32-byte network-order header;v1 双向 frame 上限 256 KiB。
pub const FRAME_HEADER_BYTES: usize = 32;
pub const FRAME_MAGIC: [u8; 4] = *b"MFT1";
/// kind 1:server → client 的脱敏输出。
pub const FRAME_KIND_OUTPUT: u8 = 1;
/// kind 2:client → server 的原始输入(writer lease 绑定)。
pub const FRAME_KIND_INPUT: u8 = 2;
/// v1 不发送 checkpoint(§8.3):kind 3..255 保留,解码即拒绝。
pub const FRAME_KIND_CHECKPOINT_RESERVED: u8 = 3;

/// frame 解码问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameProblem {
    #[error("frame 太短({len} bytes,头部需 32)")]
    TooShort { len: usize },
    #[error("frame magic 不符")]
    BadMagic,
    #[error("frame kind {kind} 保留/未知(v1 仅 1=output、2=input;不发送 checkpoint)")]
    UnknownKind { kind: u8 },
    #[error("reserved 字节非零")]
    ReservedNotZero,
    #[error("frame 超过 frame_max_bytes({len} > {max})")]
    TooLarge { len: usize, max: usize },
}

/// 解码后的 frame 头 + payload 切片。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub kind: u8,
    pub flags: u8,
    pub seq: u64,
    /// writer lease UUID 原始字节(output 全 0)。
    pub writer_lease_id: [u8; 16],
    pub payload: &'a [u8],
}

/// 编码 binary frame(kind/flags/seq/lease + payload,network order)。
/// 超过 `FRAME_MAX_BYTES` 由调用方(transport 分帧策略)保证;本函数
/// 仍做防御性断言。
pub fn encode_frame(
    kind: u8,
    flags: u8,
    seq: u64,
    writer_lease_id: [u8; 16],
    payload: &[u8],
) -> Result<Vec<u8>, FrameProblem> {
    let total = FRAME_HEADER_BYTES + payload.len();
    if total > crate::limits::FRAME_MAX_BYTES {
        return Err(FrameProblem::TooLarge {
            len: total,
            max: crate::limits::FRAME_MAX_BYTES,
        });
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&FRAME_MAGIC);
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&[0u8; 2]);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&writer_lease_id);
    out.extend_from_slice(payload);
    Ok(out)
}

/// 编码 output frame(lease 全 0)。
pub fn encode_output_frame(seq: u64, payload: &[u8]) -> Result<Vec<u8>, FrameProblem> {
    encode_frame(FRAME_KIND_OUTPUT, 0, seq, [0u8; 16], payload)
}

/// 解码 frame(校验 magic/kind/reserved/长度)。
pub fn decode_frame(bytes: &[u8]) -> Result<Frame<'_>, FrameProblem> {
    if bytes.len() < FRAME_HEADER_BYTES {
        return Err(FrameProblem::TooShort { len: bytes.len() });
    }
    if bytes.len() > crate::limits::FRAME_MAX_BYTES {
        return Err(FrameProblem::TooLarge {
            len: bytes.len(),
            max: crate::limits::FRAME_MAX_BYTES,
        });
    }
    if bytes[0..4] != FRAME_MAGIC {
        return Err(FrameProblem::BadMagic);
    }
    let kind = bytes[4];
    if kind != FRAME_KIND_OUTPUT && kind != FRAME_KIND_INPUT {
        return Err(FrameProblem::UnknownKind { kind });
    }
    if bytes[6] != 0 || bytes[7] != 0 {
        return Err(FrameProblem::ReservedNotZero);
    }
    let mut seq_bytes = [0u8; 8];
    seq_bytes.copy_from_slice(&bytes[8..16]);
    let mut writer_lease_id = [0u8; 16];
    writer_lease_id.copy_from_slice(&bytes[16..32]);
    Ok(Frame {
        kind,
        flags: bytes[5],
        seq: u64::from_be_bytes(seq_bytes),
        writer_lease_id,
        payload: &bytes[FRAME_HEADER_BYTES..],
    })
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    #[test]
    fn output_frame_round_trip() {
        let encoded = encode_output_frame(7, b"hello").unwrap();
        assert_eq!(encoded.len(), 32 + 5);
        let frame = decode_frame(&encoded).unwrap();
        assert_eq!(frame.kind, FRAME_KIND_OUTPUT);
        assert_eq!(frame.seq, 7);
        assert_eq!(frame.payload, b"hello");
        assert_eq!(frame.writer_lease_id, [0u8; 16]);
        assert_eq!(frame.flags, 0);
    }

    #[test]
    fn input_frame_carries_lease_and_be_seq() {
        let lease = [9u8; 16];
        let encoded =
            encode_frame(FRAME_KIND_INPUT, 0, 0x0102_0304_0506_0708, lease, b"\r").unwrap();
        // seq 大端:bytes[8..16]
        assert_eq!(&encoded[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
        let frame = decode_frame(&encoded).unwrap();
        assert_eq!(frame.seq, 0x0102_0304_0506_0708);
        assert_eq!(frame.writer_lease_id, lease);
    }

    #[test]
    fn reserved_checkpoint_kind_is_rejected() {
        let mut encoded = encode_output_frame(1, b"x").unwrap();
        encoded[4] = FRAME_KIND_CHECKPOINT_RESERVED;
        assert_eq!(
            decode_frame(&encoded),
            Err(FrameProblem::UnknownKind { kind: 3 })
        );
    }

    #[test]
    fn bad_magic_short_and_oversize_rejected() {
        assert_eq!(decode_frame(b"MFT"), Err(FrameProblem::TooShort { len: 3 }));
        let mut bad_magic = encode_output_frame(1, b"x").unwrap();
        bad_magic[0] = b'X';
        assert_eq!(decode_frame(&bad_magic), Err(FrameProblem::BadMagic));
        let oversized = vec![0u8; crate::limits::FRAME_MAX_BYTES + 1];
        assert!(matches!(
            decode_frame(&oversized),
            Err(FrameProblem::TooLarge { .. })
        ));
    }

    #[test]
    fn nonzero_reserved_rejected() {
        let mut encoded = encode_output_frame(1, b"x").unwrap();
        encoded[6] = 1;
        assert_eq!(decode_frame(&encoded), Err(FrameProblem::ReservedNotZero));
    }
}
