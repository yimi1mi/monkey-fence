//! 插件后台 worker:独立进程 + NDJSON 请求协议(stdio)。
//!
//! 权限模型:worker 是否允许运行由插件的 enabled/授权状态决定(注册表把门);
//! 权限只约束 MonkeyFence 宿主接口 —— worker 进程本身仍以当前用户权限运行。

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
        // stderr → 日志缓冲
        let logs = Arc::new(parking_lot::Mutex::new(Vec::new()));
        if let Some(stderr) = child.stderr.take() {
            let logs = logs.clone();
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    let mut l = logs.lock();
                    if l.len() >= 500 {
                        l.remove(0);
                    }
                    l.push(line);
                }
            });
        }
        Ok(WorkerClient {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: AtomicI64::new(1),
            logs,
        })
    }

    /// NDJSON 请求/响应:{"id":N,"method":"...","params":{...}} → {"id":N,"ok":true,"result":...}
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({ "id": id, "method": method, "params": params });
        let line = serde_json::to_string(&req)?;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .context("向 worker 写入失败(进程已退出?)")?;
        // 逐行读,直到拿到对应 id 的响应
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
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                self.push_log(line);
                continue;
            };
            if v.get("id").and_then(|i| i.as_i64()) == Some(id) {
                if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                bail!(
                    "worker 错误: {}",
                    v.get("error").and_then(|e| e.as_str()).unwrap_or("未知")
                );
            }
            self.push_log(line);
        }
    }

    fn push_log(&self, line: &str) {
        let mut l = self.logs.lock();
        if l.len() >= 500 {
            l.remove(0);
        }
        l.push(line.to_string());
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
        // 用 PowerShell 模拟 NDJSON worker:读一行、回一个 ok 响应
        let script = r#"
while ($line = [Console]::In.ReadLine()) {
  if ($null -eq $line) { break }
  $req = $line | ConvertFrom-Json
  $resp = @{ id = $req.id; ok = $true; result = @{ echo = $req.params.msg } } | ConvertTo-Json -Compress
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
        let result = client
            .request("echo", json!({ "msg": "hello" }))
            .expect("NDJSON 往返");
        assert_eq!(result["echo"], "hello");
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
