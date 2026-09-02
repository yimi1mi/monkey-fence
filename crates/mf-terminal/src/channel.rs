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
