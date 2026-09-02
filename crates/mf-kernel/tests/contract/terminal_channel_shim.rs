//! TerminalChannel shim 契约(Issue #28):`attach_terminal` 委托注入的
//! `TerminalHost`;未注入 fail-closed,未知会话 ResourceNotFound,
//! 存活会话返回可用通道。调用者拿不到 host/raw writer。

use std::sync::Mutex;

use crate::command::ServiceIdempotencyKey;
use crate::handles::SessionHandle;
use crate::kernel::{CoreKernel, InProcessCoreKernel, KernelProblem, TerminalAttach};
use crate::project_registry::ServiceStore;
use mf_terminal::{TerminalHost, TerminalProblem, TerminalSessionRef};

struct FakeHost {
    inputs: Mutex<Vec<Vec<u8>>>,
}

impl TerminalHost for FakeHost {
    fn session_alive(&self, session: &TerminalSessionRef) -> bool {
        session.as_str() != "sess-missing"
    }

    fn send_input(
        &self,
        _session: &TerminalSessionRef,
        bytes: &[u8],
    ) -> Result<(), TerminalProblem> {
        self.inputs.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }

    fn terminate_session(&self, _session: &TerminalSessionRef) -> Result<(), TerminalProblem> {
        Ok(())
    }

    fn tail_lines(&self, _session: &TerminalSessionRef, _lines: usize) -> Vec<String> {
        Vec::new()
    }
}

fn kernel_without_host() -> InProcessCoreKernel {
    let tmp = tempfile::tempdir().unwrap();
    let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    // tempdir 生命周期:ServiceStore 打开 SQLite 后句柄自持文件,Windows
    // 上目录删除推迟到进程退出,不影响测试。
    std::mem::forget(tmp);
    InProcessCoreKernel::new(service, ServiceIdempotencyKey::new(vec![0x28; 32]).unwrap())
}

#[test]
fn attach_terminal_fails_closed_without_host() {
    let kernel = kernel_without_host();
    let problem = kernel
        .attach_terminal(
            SessionHandle::from_opaque("sess-anything"),
            TerminalAttach { after_seq: 0 },
        )
        .unwrap_err();
    assert!(
        matches!(problem, KernelProblem::ServiceUnavailable(_)),
        "未注入宿主必须 fail-closed:{problem:?}"
    );
}

#[test]
fn attach_terminal_rejects_unknown_session() {
    let kernel = kernel_without_host();
    kernel.ensure_terminal_host(|| {
        std::sync::Arc::new(FakeHost {
            inputs: Mutex::new(Vec::new()),
        })
    });
    let problem = kernel
        .attach_terminal(
            SessionHandle::from_opaque("sess-missing"),
            TerminalAttach { after_seq: 0 },
        )
        .unwrap_err();
    assert_eq!(problem, KernelProblem::ResourceNotFound);
}

#[test]
fn attach_terminal_returns_channel_that_reaches_host() {
    let kernel = kernel_without_host();
    let host = std::sync::Arc::new(FakeHost {
        inputs: Mutex::new(Vec::new()),
    });
    let host_inputs = std::sync::Arc::clone(&host);
    kernel.ensure_terminal_host(move || host_inputs);

    let channel = kernel
        .attach_terminal(
            SessionHandle::from_opaque("sess-live"),
            TerminalAttach { after_seq: 42 },
        )
        .expect("存活会话 attach 必须成功");
    assert_eq!(channel.session().as_str(), "sess-live");
    assert!(channel.is_alive());
    channel.send_input(b"/model x\r").expect("输入发送失败");
    channel.terminate().expect("终止失败");
    assert_eq!(
        host.inputs.lock().unwrap().last().unwrap(),
        b"/model x\r".to_vec().as_slice()
    );
}

#[test]
fn ensure_terminal_host_is_first_writer_wins() {
    let kernel = kernel_without_host();
    let host = std::sync::Arc::new(FakeHost {
        inputs: Mutex::new(Vec::new()),
    });
    let second = std::sync::Arc::new(FakeHost {
        inputs: Mutex::new(Vec::new()),
    });
    let second_for_closure = std::sync::Arc::clone(&second);
    kernel.ensure_terminal_host(move || host);
    kernel.ensure_terminal_host(move || second_for_closure);
    let channel = kernel
        .attach_terminal(
            SessionHandle::from_opaque("sess-live"),
            TerminalAttach { after_seq: 0 },
        )
        .unwrap();
    channel.send_input(b"k").unwrap();
    assert_eq!(
        second.inputs.lock().unwrap().len(),
        0,
        "重复注入不得换掉已生效宿主"
    );
}
