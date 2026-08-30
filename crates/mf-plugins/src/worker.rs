//! 插件后台 worker:独立进程 + 版本化 NDJSON 请求协议(stdio,协议版本 1)。
//!
//! 权限模型:worker 是否允许运行由插件的 enabled/授权状态决定(宿主把门);
//! 权限只约束 MonkeyFence 宿主接口 —— worker 进程本身仍以当前用户权限运行。
//! 诊断文本(stderr、未匹配的 stdout 行)入库前按敏感 key 脱敏,上限 500 行。
//!
//! F11:**总 deadline 覆盖 stdin 写入与 stdout 读取全程** —— 写入在
//! 独立线程执行(worker 停读且管道缓冲写满时 `write_all` 会无限阻塞),
//! 超时先终止整棵 worker 进程树(管道断裂解除写阻塞)再返回错误;
//! 终止/整树清空/reap 全部有界且检查结果(Windows Job Object /
//! Unix PGID,见 `proc_tree`)。

use crate::proc_tree::{kill_tree_bounded, ProcTreeGuard};
use crate::worker_protocol::{
    ensure_matches, redact_text, WorkerHealth, WorkerRequest, WorkerResponse, STDERR_LOG_LIMIT,
    WORKER_PROTOCOL_VERSION,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 单次请求的默认超时(I9)。写入与读取共享同一个总预算(F11)。
pub const WORKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// 整树终止/清空的有界等待(F11)。
const TREE_KILL_TIMEOUT: Duration = Duration::from_secs(10);

/// reader 线程 → request 的行消息(EOF 单独报告)。
enum WorkerLine {
    Data(String),
    Eof,
}

pub struct WorkerClient {
    child: parking_lot::Mutex<Child>,
    child_pid: u32,
    /// stdin 移出期间为 None(F11:写入线程持有;返回后放回)。
    stdin: parking_lot::Mutex<Option<std::process::ChildStdin>>,
    /// stdout 读取线程的输出通道:阻塞读完全在后台线程,
    /// request 用 `recv_timeout` 实现真超时(I9:同步 read_line 的
    /// deadline 只能在两次读之间检查,worker 活着不换行时永远挂住)。
    lines: std::sync::mpsc::Receiver<WorkerLine>,
    request_timeout: parking_lot::Mutex<Duration>,
    next_id: AtomicI64,
    logs: Arc<parking_lot::Mutex<Vec<String>>>,
    capability_token: parking_lot::Mutex<String>,
    /// 进程树守卫(Windows Job Object / Unix PGID;F11)。
    tree: Option<ProcTreeGuard>,
}

impl WorkerClient {
    /// 启动 worker 进程。命令相对插件根目录或绝对路径。
    pub fn start(
        exe: &std::path::Path,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> Result<WorkerClient> {
        let exe = if exe.exists() {
            exe.to_path_buf()
        } else if let Some(found) = crate::builtin::detect_on_path(&exe.to_string_lossy()) {
            found
        } else {
            bail!("worker 可执行文件不存在: {}", exe.display())
        };
        let mut cmd = Command::new(&exe);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        // Unix:独立进程组(杀树按 -pgid;Windows 用 Job Object)
        #[cfg(not(windows))]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 worker 失败: {}", exe.display()))?;
        let child_pid = child.id();
        // F11:立即挂进程树守卫(Windows Job Object KILL_ON_JOB_CLOSE)
        let tree = ProcTreeGuard::attach(&mut child)
            .map_err(|e| {
                let _ = child.kill();
                let _ = child.wait();
                e
            })
            .ok();
        let stdin = child.stdin.take().context("worker stdin 不可用")?;
        let stdout = child.stdout.take().context("worker stdout 不可用")?;
        // stderr → 日志缓冲(脱敏 + 有界)
        let logs = Arc::new(parking_lot::Mutex::new(Vec::new()));
        if let Some(stderr) = child.stderr.take() {
            let logs = logs.clone();
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    push_bounded(&logs, redact_text(&line));
                }
            });
        }
        // stdout → 读取线程 → 通道(request 侧 recv_timeout 实现真超时)
        let (tx, rx) = std::sync::mpsc::channel::<WorkerLine>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(WorkerLine::Data(l)).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(WorkerLine::Eof);
        });
        Ok(WorkerClient {
            child: parking_lot::Mutex::new(child),
            child_pid,
            stdin: parking_lot::Mutex::new(Some(stdin)),
            lines: rx,
            request_timeout: parking_lot::Mutex::new(WORKER_REQUEST_TIMEOUT),
            next_id: AtomicI64::new(1),
            logs,
            capability_token: parking_lot::Mutex::new(String::new()),
            tree,
        })
    }

    /// 单次请求超时(测试可调短;生产默认 60s)。
    pub fn set_request_timeout(&self, timeout: Duration) {
        *self.request_timeout.lock() = timeout;
    }

    /// 设置一次性能力令牌(仅对当前 Agent Run 有效)。
    pub fn set_capability_token(&self, token: &str) {
        *self.capability_token.lock() = token.to_string();
    }

    /// NDJSON 版本化请求(协议同前)。
    /// F11:**总 deadline 覆盖 stdin 写入 + stdout 读取**:
    /// - 写入在独立线程(worker 停读、管道写满时 `write_all` 无限阻塞)——
    ///   由同一 deadline 看守,超时先杀整棵进程树(写线程随管道断裂
    ///   解除并退出),再返回错误(写入可取消);
    /// - 读取沿用 reader 线程 + `recv_timeout`;
    /// - 任一超时都杀 worker **整棵进程树**后返回错误(绝不留挂死 worker)。
    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = WorkerRequest::new(id, method, &self.capability_token.lock(), params);
        let line = req.to_line()?;
        let timeout = *self.request_timeout.lock();
        let deadline = std::time::Instant::now() + timeout;

        // ---- 写入(独立线程 + 总 deadline 看守;可取消:超时杀树) ----
        let (write_tx, write_rx) = std::sync::mpsc::channel::<
            std::result::Result<std::process::ChildStdin, (std::process::ChildStdin, anyhow::Error)>,
        >();
        let stdin_taken = self.stdin.lock().take();
        let Some(stdin) = stdin_taken else {
            bail!("worker stdin 已不可用(此前超时已终止)");
        };
        let line_for_write = line.clone();
        let writer = std::thread::Builder::new()
            .name("mf-worker-write".into())
            .spawn(move || {
                let mut stdin = stdin;
                let result = (|| -> Result<()> {
                    stdin
                        .write_all(line_for_write.as_bytes())
                        .and_then(|_| stdin.flush())?;
                    Ok(())
                })();
                match result {
                    Ok(()) => {
                        let _ = write_tx.send(Ok(stdin));
                    }
                    Err(e) => {
                        let _ = write_tx.send(Err((stdin, e)));
                    }
                }
            })
            .context("启动 worker 写入线程失败")?;
        match write_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Ok(stdin)) => {
                *self.stdin.lock() = Some(stdin);
            }
            Ok(Err((_stdin, e))) => {
                // 写失败(进程退出/管道断):如实上抛
                return Err(e).context("向 worker 写入失败(进程已退出?)");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.kill();
                let _ = writer.join();
                bail!(
                    "worker 写入超时({method},上限 {timeout:?}):已终止 worker 进程树\
                     (写入已取消)"
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = writer.join();
                bail!("worker 写入线程已结束(进程已退出?)");
            }
        }

        // ---- 读取(同一 deadline 的剩余预算) ----
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                self.kill();
                bail!("worker 响应超时({method},上限 {timeout:?}):已终止 worker 进程树");
            }
            let line = match self.lines.recv_timeout(remaining) {
                Ok(WorkerLine::Data(l)) => l,
                Ok(WorkerLine::Eof) => bail!("worker 已退出"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    self.kill();
                    bail!("worker 响应超时({method},上限 {timeout:?}):已终止 worker 进程树");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("worker 读取通道已关闭(进程已退出?)")
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let resp = match WorkerResponse::parse_for(WORKER_PROTOCOL_VERSION, line) {
                Ok(r) => r,
                Err(e) => {
                    // 合法 JSON 但协议不匹配必须立刻暴露,不允许静默降级
                    if serde_json::from_str::<Value>(line).is_ok() {
                        bail!("worker 协议不兼容: {e}");
                    }
                    push_bounded(&self.logs, redact_text(line));
                    continue;
                }
            };
            if let Err(e) = ensure_matches(WORKER_PROTOCOL_VERSION, id, &resp) {
                bail!("worker 响应与请求不匹配: {e}");
            }
            if resp.is_ok() {
                return Ok(resp.result);
            }
            bail!("worker 错误: {}", resp.error.as_deref().unwrap_or("未知"));
        }
    }

    /// 心跳:探测 worker 存活(方法 `heartbeat`)。
    pub fn heartbeat(&mut self) -> Result<WorkerHealth> {
        self.request("heartbeat", json!({}))?;
        Ok(WorkerHealth { alive: true })
    }

    pub fn logs(&self) -> Vec<String> {
        self.logs.lock().clone()
    }

    /// 杀 worker **整棵进程树**(Windows Job Object;Unix 按 -pgid
    /// SIGKILL),再 reap 直接子进程 —— 全部有界且检查结果(F11)。
    pub fn kill(&self) {
        let mut child = self.child.lock();
        if let Err(e) = kill_tree_bounded(&mut child, self.tree.as_ref(), TREE_KILL_TIMEOUT) {
            log::warn!("终止 worker 进程树(pid {})未完全成功: {e:#}", self.child_pid);
        }
        // stdin 归还写入线程/丢弃:进程已死,写端关闭即 EOF
        *self.stdin.lock() = None;
    }

    pub fn is_alive(&self) -> bool {
        matches!(self.child.lock().try_wait(), Ok(None))
    }
}

fn push_bounded(logs: &Arc<parking_lot::Mutex<Vec<String>>>, line: String) {
    let mut l = logs.lock();
    if l.len() >= STDERR_LOG_LIMIT {
        l.remove(0);
    }
    l.push(line);
}

impl Drop for WorkerClient {
    fn drop(&mut self) {
        WorkerClient::kill(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndjson_roundtrip_with_cmd_worker() {
        if !cfg!(windows) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // 用 PowerShell 模拟协议版本 1 的 NDJSON worker:读一行、回一个匹配响应
        let script = r#"
while ($line = [Console]::In.ReadLine()) {
  if ($null -eq $line) { break }
  $req = $line | ConvertFrom-Json
  if ($req.method -eq 'heartbeat') {
    $result = @{ alive = $true }
  } else {
    $result = @{ echo = $req.params.msg }
  }
  $resp = @{ protocol = 1; id = $req.id; result = $result } | ConvertTo-Json -Compress
  [Console]::Out.WriteLine($resp)
}
"#;
        let ps1 = tmp.path().join("worker.ps1");
        std::fs::write(&ps1, script).unwrap();
        let args = vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            ps1.to_string_lossy().to_string(),
        ];
        let mut client =
            WorkerClient::start(&std::path::PathBuf::from("powershell.exe"), &args, None)
                .expect("powershell 可用");
        client.set_capability_token("mft_test");
        let result = client
            .request("echo", json!({ "msg": "hello" }))
            .expect("NDJSON 往返");
        assert_eq!(result["echo"], "hello");
        let health = client.heartbeat().expect("心跳");
        assert!(health.alive);
        client.kill();
    }

    /// I9:worker 活着但永不输出(不换行)→ request 必须在超时内返回
    /// Err 并杀掉 worker 进程树(同步 read_line 的假超时测不出来)。
    #[test]
    fn hung_worker_times_out_returns_error_and_kills_tree() {
        if !cfg!(windows) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // 读到 hang 请求后进入长眠:进程活着、stdout 无换行输出
        let script = r#"
while ($line = [Console]::In.ReadLine()) {
  if ($null -eq $line) { break }
  $req = $line | ConvertFrom-Json
  if ($req.method -eq 'hang') {
    Start-Sleep -Seconds 600
    exit 0
  }
  $resp = @{ protocol = 1; id = $req.id; result = @{ ok = $true } } | ConvertTo-Json -Compress
  [Console]::Out.WriteLine($resp)
}
"#;
        let ps1 = tmp.path().join("hung-worker.ps1");
        std::fs::write(&ps1, script).unwrap();
        let args = vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            ps1.to_string_lossy().to_string(),
        ];
        let client = WorkerClient::start(&std::path::PathBuf::from("powershell.exe"), &args, None)
            .expect("powershell 可用");
        let client = Arc::new(parking_lot::Mutex::new(client));
        client
            .lock()
            .set_request_timeout(Duration::from_millis(800));
        // 前置:正常请求可用
        client.lock().request("ping", json!({})).expect("前置往返");

        let c2 = client.clone();
        let (tx, rx) = std::sync::mpsc::channel::<Result<Value>>();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(c2.lock().request("hang", json!({})));
        });
        // request 必须在超时 + 余量内返回(同步 read_line 会永远挂住)
        match rx.recv_timeout(Duration::from_secs(6)) {
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("超时") && msg.contains("终止"),
                    "超时错误必须明示已终止 worker: {msg}"
                );
            }
            Ok(Ok(v)) => panic!("hang 请求不得成功: {v}"),
            Err(_) => panic!("hung worker 的 request 必须超时返回,而不是无限阻塞"),
        }
        worker.join().unwrap();
        assert!(
            !client.lock().is_alive(),
            "超时后 worker 进程树必须已被终止"
        );
        // 超时杀树后 client 不可再用(通道已关)
        assert!(client.lock().request("ping", json!({})).is_err());
    }

    /// F11:worker 从不读 stdin —— 大请求写满管道后 `write_all` 无限阻塞;
    /// 总 deadline 必须覆盖写入:超时杀树、写入取消、返回明确错误。
    #[test]
    fn blocked_stdin_write_is_covered_by_total_deadline() {
        if !cfg!(windows) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // 不读 stdin、不输出:PowerShell 只睡眠
        let script = "Start-Sleep -Seconds 600";
        let ps1 = tmp.path().join("blocked-stdin-worker.ps1");
        std::fs::write(&ps1, script).unwrap();
        let args = vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            ps1.to_string_lossy().to_string(),
        ];
        let client = Arc::new(parking_lot::Mutex::new(
            WorkerClient::start(&std::path::PathBuf::from("powershell.exe"), &args, None)
                .expect("powershell 可用"),
        ));
        client.lock().set_request_timeout(Duration::from_millis(900));
        // 2MB 参数:远超匿名管道缓冲(64KB),write_all 必然阻塞
        let big = "x".repeat(2 * 1024 * 1024);
        let (tx, rx) = std::sync::mpsc::channel::<Result<Value>>();
        let c2 = client.clone();
        let handle = std::thread::spawn(move || {
            let _ = tx.send(c2.lock().request("echo", json!({ "blob": big })));
        });
        match rx.recv_timeout(Duration::from_secs(8)) {
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("写入超时") && msg.contains("取消"),
                    "写入超时错误必须明示写入已取消: {msg}"
                );
            }
            Ok(Ok(v)) => panic!("阻塞 stdin 的请求不得成功: {v}"),
            Err(_) => panic!("阻塞的 stdin 写入必须被总 deadline 取消,而不是无限阻塞"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn missing_executable_rejected() {
        assert!(WorkerClient::start(
            std::path::Path::new("Z:/definitely/not/here.exe"),
            &[],
            None
        )
        .is_err());
    }
}
