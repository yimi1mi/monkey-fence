//! legacy transport 客户端 adapter(T6a,Issue #37;T12 迁入 mf-kernel)。
//!
//! Bridge A 期间 GPUI 继续走 in-process `LegacyKernelClient`(可切换,
//! Gate T6 后默认 IPC;#48 接线)。本模块提供 `mf.legacy-transport.v1`
//! 的行协议客户端:Hello 注册后逐请求 dispatch/snapshot/events/
//! attach/shutdown,响应原样解析——与 in-process 对同一 fixture 给出
//! 相同结果(transport 不改变语义,只差管道)。

use std::io::{BufRead, BufReader, Write};

use anyhow::{Context, Result};

pub use crate::legacy_transport::{LegacyRequest, LegacyResponse, LEGACY_TRANSPORT_PROTOCOL};

/// 行协议客户端(任意双工流;Named Pipe/UDS/内存)。
pub struct LegacyTransportClient<R: std::io::Read, W: std::io::Write> {
    reader: BufReader<R>,
    writer: W,
}

impl<R: std::io::Read, W: std::io::Write> LegacyTransportClient<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    /// 发送请求并读取一行响应。
    pub fn request(&mut self, request: &LegacyRequest) -> Result<LegacyResponse> {
        let line = serde_json::to_string(request)?;
        self.writer
            .write_all(line.as_bytes())
            .and_then(|_| self.writer.write_all(b"\n"))
            .and_then(|_| self.writer.flush())
            .context("transport 写入失败")?;
        let mut response_line = String::new();
        let bytes = self
            .reader
            .read_line(&mut response_line)
            .context("transport 读取失败(对端关闭?)")?;
        anyhow::ensure!(bytes > 0, "transport 对端关闭");
        Ok(serde_json::from_str(response_line.trim())?)
    }

    /// 连接注册(协议版本 + 身份/epoch)。
    pub fn hello(
        &mut self,
        client_id: &str,
        principal: &str,
        controller_lease_epoch: u64,
    ) -> Result<LegacyResponse> {
        self.request(&LegacyRequest::Hello {
            protocol: LEGACY_TRANSPORT_PROTOCOL.into(),
            client_id: client_id.into(),
            principal: principal.into(),
            controller_lease_epoch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::CoreKernel;
    use crate::legacy_transport::LegacyTransportSession;
    use std::sync::Arc;

    struct NoKernel;
    impl CoreKernel for NoKernel {
        fn dispatch(
            &self,
            _request: crate::kernel::KernelCommandRequest,
        ) -> Result<crate::kernel::KernelOutcome, crate::kernel::KernelProblem> {
            Err(crate::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn snapshot(
            &self,
            _query: crate::projection::SnapshotQuery,
        ) -> Result<crate::projection::SnapshotEnvelope, crate::kernel::KernelProblem> {
            Err(crate::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn subscribe_events(
            &self,
            _cursor: crate::projection::EventCursor,
        ) -> Result<crate::projection::EventSubscription, crate::kernel::KernelProblem> {
            Err(crate::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn attach_terminal(
            &self,
            _session: crate::handles::SessionHandle,
            _attach: crate::kernel::TerminalAttach,
        ) -> Result<crate::kernel::TerminalChannel, crate::kernel::KernelProblem> {
            Err(crate::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
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
        ) -> Result<u64, crate::kernel::KernelProblem> {
            Err(crate::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn controller_epoch(&self) -> u64 {
            0
        }
    }

    struct ReadHalf {
        incoming: std::sync::mpsc::Receiver<Vec<u8>>,
        buffer: Vec<u8>,
    }

    struct WriteHalf {
        outgoing: std::sync::mpsc::Sender<Vec<u8>>,
    }

    impl std::io::Read for ReadHalf {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.buffer.is_empty() {
                match self.incoming.recv() {
                    Ok(chunk) => self.buffer = chunk,
                    Err(_) => return Ok(0),
                }
            }
            let n = buf.len().min(self.buffer.len());
            buf[..n].copy_from_slice(&self.buffer[..n]);
            self.buffer.drain(..n);
            Ok(n)
        }
    }

    impl std::io::Write for WriteHalf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.outgoing
                .send(buf.to_vec())
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "对端关闭"))?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn duplex_pair() -> ((ReadHalf, WriteHalf), (ReadHalf, WriteHalf)) {
        let (a_to_b, b_rx) = std::sync::mpsc::channel();
        let (b_to_a, a_rx) = std::sync::mpsc::channel();
        (
            (
                ReadHalf {
                    incoming: a_rx,
                    buffer: Vec::new(),
                },
                WriteHalf { outgoing: a_to_b },
            ),
            (
                ReadHalf {
                    incoming: b_rx,
                    buffer: Vec::new(),
                },
                WriteHalf { outgoing: b_to_a },
            ),
        )
    }

    #[test]
    fn client_and_server_round_trip_over_memory_pipe() {
        let ((client_read, client_write), (server_read, server_write)) = duplex_pair();
        let mut session = LegacyTransportSession::new(Arc::new(NoKernel));
        // 服务端线程:行循环
        let server = std::thread::spawn(move || {
            crate::legacy_transport::serve_connection(&mut session, server_read, server_write)
        });
        let mut client = LegacyTransportClient::new(client_read, client_write);
        // Hello → accepted
        match client.hello("cl_g", "user", 1).unwrap() {
            LegacyResponse::HelloAccepted => {}
            other => panic!("hello 应接受:{other:?}"),
        }
        // shutdown → 关闭
        match client.request(&LegacyRequest::Shutdown).unwrap() {
            LegacyResponse::ShuttingDown => {}
            other => panic!("shutdown:{other:?}"),
        }
        drop(client);
        server.join().unwrap().unwrap();
    }
}
