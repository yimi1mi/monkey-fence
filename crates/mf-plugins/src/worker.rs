//! 插件后台 worker:独立进程 + 版本化 NDJSON 请求协议(stdio,协议版本 1)。
//!
//! 权限模型:worker 是否允许运行由插件的 enabled/授权状态决定(宿主把门);
//! 权限只约束 MonkeyFence 宿主接口 —— worker 进程本身仍以当前用户权限运行。
//! 诊断文本(stderr、未匹配的 stdout 行)入库前按敏感 key 脱敏,上限 500 行。

use crate::worker_protocol::{
    ensure_matches, redact_text, WorkerRequest, WorkerResponse, WorkerHealth,
    STDERR_LOG_LIMIT, WORKER_PROTOCOL_VERSION,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct WorkerClient {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: AtomicI64,
    logs: Arc<parking_lot::Mutex<Vec<String>>>,
    capability_token: parking_lot::Mutex<String>,
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
        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 worker 失败: {}", exe.display()))?;
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
        Ok(WorkerClient {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: AtomicI64::new(1),
            logs,
            capability_token: parking_lot::Mutex::new(String::new()),
        })
    }

    /// 设置一次性能力令牌(仅对当前 Agent Run 有效)。
    pub fn set_capability_token(&self, token: &str) {
        *self.capability_token.lock() = token.to_string();
    }

    /// NDJSON 版本化请求:`{"protocol":1,"id":N,"method":"...","capability_token":"...","params":{...}}`
    /// → `{"protocol":1,"id":N,"result":...}` / `{"protocol":1,"id":N,"error":"..."}`。
    /// 协议版本或响应 id 与请求不符 → 拒绝;其余行进入(脱敏后的)诊断日志。
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = WorkerRequest::new(
            id,
            method,
            &self.capability_token.lock(),
            params,
        );
        let line = req.to_line()?;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .context("向 worker 写入失败(进程已退出?)")?;
        // 逐行读,直到拿到协议版本与 id 都匹配的响应
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if std::time::Instant::now() > deadline {
                bail!("worker 响应超时: {method}");
            }
            let mut buf = String::new();
            let n = self
                .reader
                .read_line(&mut buf)
                .context("读取 worker 响应失败(进程已退出?)")?;
            if n == 0 {
                bail!("worker 已退出");
            }
            let line = buf.trim();
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
            bail!(
                "worker 错误: {}",
                resp.error.as_deref().unwrap_or("未知")
            );
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

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
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
        self.kill();
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
