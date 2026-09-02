//! mfctl:MonkeyFence 辅助程序。
//!
//! Agent 在其 shell 中调用,能力令牌与管道名从环境变量读取:
//! - `MF_RUN_TOKEN`:一次性能力令牌(仅对当前 Agent Run 有效)
//! - `MF_PIPE`:MonkeyFence 命名管道
//!
//! 命令:
//!   mfctl step complete --summary "..." [--output-json '{"report_path":"..."}'] [--command-id <uuidv7>]
//!   mfctl step fail --reason "..."
//!   mfctl agent-state <working|waiting|blocked|done>
//!   mfctl pipeline propose --file draft.json
//!
//! `--command-id`(可选,UUIDv7)是幂等键:同 id 重试返回原结果;
//!
//! 也可显式传参:--token <T> --pipe <NAME>。协议为 NDJSON 单请求/单响应。

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&args) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("mfctl: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn run(args: &[String]) -> Result<String> {
    let mut token = std::env::var("MF_RUN_TOKEN").unwrap_or_default();
    let mut pipe = std::env::var("MF_PIPE").unwrap_or_default();
    let mut positional: Vec<&String> = Vec::new();
    let mut flag_key: Option<&str> = None;
    let mut flags: Vec<(&str, String)> = Vec::new();
    for a in args {
        if let Some(k) = flag_key.take() {
            flags.push((k, a.clone()));
            continue;
        }
        if a == "--token" {
            flag_key = Some("token");
        } else if a == "--pipe" {
            flag_key = Some("pipe");
        } else if a == "--summary" {
            flag_key = Some("summary");
        } else if a == "--reason" {
            flag_key = Some("reason");
        } else if a == "--file" {
            flag_key = Some("file");
        } else if a == "--output-json" {
            flag_key = Some("output-json");
        } else if a == "--command-id" {
            flag_key = Some("command-id");
        } else if let Some(v) = a.strip_prefix("--token=") {
            flags.push(("token", v.to_string()));
        } else if let Some(v) = a.strip_prefix("--pipe=") {
            flags.push(("pipe", v.to_string()));
        } else if let Some(v) = a.strip_prefix("--summary=") {
            flags.push(("summary", v.to_string()));
        } else if let Some(v) = a.strip_prefix("--reason=") {
            flags.push(("reason", v.to_string()));
        } else if let Some(v) = a.strip_prefix("--file=") {
            flags.push(("file", v.to_string()));
        } else if let Some(v) = a.strip_prefix("--output-json=") {
            flags.push(("output-json", v.to_string()));
        } else if let Some(v) = a.strip_prefix("--command-id=") {
            flags.push(("command-id", v.to_string()));
        } else {
            positional.push(a);
        }
    }
    let flag = |k: &str| flags.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    if let Some(t) = flag("token") {
        token = t;
    }
    if let Some(p) = flag("pipe") {
        pipe = p;
    }
    if pipe.is_empty() {
        bail!("找不到 MonkeyFence 管道(环境变量 MF_PIPE 未设置;MonkeyFence 未运行?)");
    }
    if token.is_empty() {
        bail!("缺少能力令牌(环境变量 MF_RUN_TOKEN 未设置)");
    }

    let (method, params) = match positional.as_slice() {
        [cmd, sub] if cmd.as_str() == "step" && sub.as_str() == "complete" => {
            // 结构化输出(--output-json 或 --output-json=<json>):下游
            // 经 ${nodes.<key>.output.<path>} 精确引用(如 output.report_path)
            let mut params = json!({ "summary": flag("summary").unwrap_or_default() });
            if let Some(text) = flag("output-json") {
                let output: Value =
                    serde_json::from_str(&text).with_context(|| "--output-json 必须是合法 JSON")?;
                params["output"] = output;
            }
            ("step.complete", params)
        }
        [cmd, sub] if cmd.as_str() == "step" && sub.as_str() == "fail" => (
            "step.fail",
            json!({ "reason": flag("reason").unwrap_or_default() }),
        ),
        [cmd, state] if cmd.as_str() == "agent-state" => ("agent.state", json!({ "state": state })),
        [cmd, sub] if cmd.as_str() == "pipeline" && sub.as_str() == "propose" => {
            let file = flag("file")
                .context("pipeline propose 需要 --file <draft.json>(或用 - 从 stdin 读)")?;
            let text = if file == "-" {
                let mut buf = String::new();
                std::io::stdin()
                    .lock()
                    .read_to_string(&mut buf)
                    .context("读取 stdin 失败")?;
                buf
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("读取 {file} 失败"))?
            };
            let draft: Value =
                serde_json::from_str(&text).context("PipelineDraft JSON 解析失败")?;
            ("pipeline.propose", json!({ "draft": draft }))
        }
        _ => {
            bail!(
                "用法:\n  mfctl step complete --summary \"...\"\n  mfctl step fail --reason \"...\"\n  mfctl agent-state <working|waiting|blocked|done>\n  mfctl pipeline propose --file draft.json"
            )
        }
    };

    let response = request_over_pipe(
        &pipe,
        &token,
        method,
        &params,
        flag("command-id").as_deref(),
    )?;
    if response
        .get("ok")
        .and_then(|o| o.as_bool())
        .unwrap_or(false)
    {
        Ok(response
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("ok")
            .to_string())
    } else {
        bail!(
            "MonkeyFence 拒绝: {}",
            response
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("未知错误")
        )
    }
}

/// Windows 命名管道客户端:单请求/单响应 NDJSON。
/// `command_id` 为可选幂等键(UUIDv7):同 id + 同请求内容由服务端
/// 返回原结果;省略时每次都是新命令,幂等由 Settlement 语义保证。
fn request_over_pipe(
    pipe_name: &str,
    token: &str,
    method: &str,
    params: &Value,
    command_id: Option<&str>,
) -> Result<Value> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    let wide: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
    // 服务端逐实例处理请求;客户端可能撞上实例未就绪的窗口(ERROR_FILE_NOT_FOUND/BUSY)→ 重试
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let handle = loop {
        let handle = unsafe {
            windows_sys::Win32::Storage::FileSystem::CreateFileW(
                wide.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE,
                0,
                std::ptr::null_mut(),
                windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            break handle;
        }
        if std::time::Instant::now() > deadline {
            bail!("连接管道 {pipe_name} 失败(MonkeyFence 未运行?)");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut request = json!({ "id": 1, "token": token, "method": method, "params": params });
    if let Some(command_id) = command_id {
        request["command_id"] = json!(command_id);
    }
    let req = request.to_string();
    let outcome = (|| -> Result<Value> {
        let mut out = req.as_bytes().to_vec();
        out.push(b'\n');
        let mut written = 0u32;
        unsafe {
            if windows_sys::Win32::Storage::FileSystem::WriteFile(
                handle,
                out.as_ptr(),
                out.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            ) == 0
            {
                bail!("写入管道失败");
            }
            windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(handle);
        }
        // 读一行响应
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if std::time::Instant::now() > deadline {
                bail!("等待响应超时");
            }
            let mut read = 0u32;
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::ReadFile(
                    handle,
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                bail!("读取响应失败(管道关闭)");
            }
            buf.extend_from_slice(&chunk[..read as usize]);
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..pos]).to_string();
                return serde_json::from_str(&line).context("响应解析失败");
            }
        }
    })();
    unsafe { CloseHandle(handle) };
    outcome
}
