//! mfctl 命名管道服务端:`\\.\pipe\monkeyfence-mfctl-<pid>`。
//!
//! NDJSON 协议:
//! 请求  {"id":N,"token":"...","method":"step.complete|step.fail|pipeline.propose|agent.state","params":{...}}
//! 响应  {"id":N,"ok":true,"result":"..."} / {"id":N,"ok":false,"error":"..."}

use mf_agent::model::{AgentState, RunView, SettleOutcome, Settlement};
use mf_agent::orchestrator::Orchestrator;
use mf_agent::pipeline::PipelineDraft;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub fn pipe_name_for_current_process() -> String {
    format!(r"\\.\pipe\monkeyfence-mfctl-{}", std::process::id())
}

pub struct PipeServer {
    shutdown: Arc<AtomicBool>,
}

impl PipeServer {
    /// 启动管道服务线程。令牌全局唯一,跨所有打开项目的 Orchestrator 路由。
    pub fn start(orchestrators: Arc<Mutex<Vec<Arc<Orchestrator>>>>) -> anyhow::Result<PipeServer> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let name = pipe_name_for_current_process();
        let flag = shutdown.clone();
        std::thread::Builder::new()
            .name("mfctl-pipe".into())
            .spawn(move || run_server(&name, &orchestrators, &flag))?;
        Ok(PipeServer { shutdown })
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

fn run_server(
    name: &str,
    orchestrators: &Arc<Mutex<Vec<Arc<Orchestrator>>>>,
    shutdown: &Arc<AtomicBool>,
) {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe,
    };

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x00080000;
    const PIPE_TYPE_BYTE: u32 = 0x00000000;
    const PIPE_READMODE_BYTE: u32 = 0x00000000;
    const PIPE_WAIT: u32 = 0x00000000;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    // 第一个实例带 FIRST_PIPE_INSTANCE:防止同机进程抢注同名管道截获令牌
    let mut first = true;

    while !shutdown.load(Ordering::SeqCst) {
        let open_mode = if first {
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            PIPE_ACCESS_DUPLEX
        };
        let pipe = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null_mut(),
            )
        };
        first = false;
        if pipe == INVALID_HANDLE_VALUE {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }
        // 客户端可能在 ConnectNamedPipe 之前已连接(ERROR_PIPE_CONNECTED 是成功态)
        let connected = unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) };
        if connected == 0 {
            let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            const ERROR_PIPE_CONNECTED: u32 = 535;
            if err != ERROR_PIPE_CONNECTED {
                unsafe { CloseHandle(pipe) };
                continue;
            }
        }
        // 每连接一个线程:一个卡住的客户端不阻塞其他 Agent 的结算
        // (HANDLE 是裸指针,以 usize 跨线程传递后在目标线程还原)
        let orch = orchestrators.clone();
        let pipe_raw = pipe as usize;
        std::thread::Builder::new()
            .name("mfctl-conn".into())
            .spawn(move || {
                let handle = pipe_raw as windows_sys::Win32::Foundation::HANDLE;
                serve_connection(handle, &orch)
            })
            .ok();
    }
}

/// 处理单个客户端连接:读一行请求 → 处理 → 写响应 → 断开。
fn serve_connection(
    pipe: windows_sys::Win32::Foundation::HANDLE,
    orchestrators: &Arc<Mutex<Vec<Arc<Orchestrator>>>>,
) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;
    let response = match read_line(pipe) {
        Some(request) => handle_request(&request, orchestrators),
        None => String::new(),
    };
    if !response.is_empty() {
        let mut out = response.as_bytes().to_vec();
        out.push(b'\n');
        let mut written = 0u32;
        unsafe {
            let ok = windows_sys::Win32::Storage::FileSystem::WriteFile(
                pipe,
                out.as_ptr(),
                out.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            );
            if ok == 0 || (written as usize) != out.len() {
                log::warn!("mfctl 响应写入不完整({written}/{})", out.len());
            }
            windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(pipe);
        }
    }
    unsafe {
        DisconnectNamedPipe(pipe);
        CloseHandle(pipe)
    };
}

fn read_line(pipe: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let mut read = 0u32;
    loop {
        let ok = unsafe {
            ReadFile(
                pipe,
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            return if buf.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&buf).to_string())
            };
        }
        buf.extend_from_slice(&chunk[..read as usize]);
        if buf.contains(&b'\n') {
            let line = String::from_utf8_lossy(&buf).to_string();
            return Some(line.lines().next().unwrap_or("").to_string());
        }
        if buf.len() > 1024 * 1024 {
            return None;
        }
    }
}

fn handle_request(line: &str, orchestrators: &Arc<Mutex<Vec<Arc<Orchestrator>>>>) -> String {
    let req: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => return error_response(0, &format!("请求解析失败: {e}")),
    };
    let id = req.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let token = req.get("token").and_then(|t| t.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));
    match dispatch(method, token, &params, orchestrators) {
        Ok(result) => json!({ "id": id, "ok": true, "result": result }).to_string(),
        Err(e) => error_response(id, &e),
    }
}

fn error_response(id: i64, msg: &str) -> String {
    json!({ "id": id, "ok": false, "error": msg }).to_string()
}

fn find_by_token(
    orchestrators: &[Arc<Orchestrator>],
    token: &str,
) -> Option<(Arc<Orchestrator>, RunView)> {
    for orch in orchestrators {
        if let Ok(Some(run)) = orch.store.run_by_token(token) {
            return Some((orch.clone(), run));
        }
    }
    None
}

fn dispatch(
    method: &str,
    token: &str,
    params: &Value,
    orchestrators: &Arc<Mutex<Vec<Arc<Orchestrator>>>>,
) -> std::result::Result<String, String> {
    if token.is_empty() {
        return Err("缺少能力令牌(环境变量 MF_RUN_TOKEN)".into());
    }
    let orch_list = orchestrators.lock().clone();
    match method {
        "step.complete" | "step.fail" => {
            let (orch, run) = find_by_token(&orch_list, token).ok_or("能力令牌无效")?;
            let settlement = if method == "step.complete" {
                Settlement::Complete {
                    summary: params
                        .get("summary")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    // 结构化输出(mfctl --output-json):进入 Handoff.output,
                    // 下游按 ${nodes.<key>.output.<path>} 精确引用
                    output: params.get("output").cloned().unwrap_or_default(),
                }
            } else {
                Settlement::Fail {
                    reason: params
                        .get("reason")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                }
            };
            match orch.settle_by_token(token, settlement) {
                Ok(SettleOutcome::Applied) => Ok(format!("Step 已结算(run #{})", run.id)),
                Ok(SettleOutcome::AlreadyApplied) => Ok("幂等:该结算此前已提交".into()),
                Err(e) => Err(e.to_string()),
            }
        }
        "agent.state" => {
            let state = params
                .get("state")
                .and_then(|s| s.as_str())
                .ok_or("缺少 state 参数")?;
            let state = AgentState::parse(state).ok_or_else(|| format!("未知状态: {state}"))?;
            let (orch, run) = find_by_token(&orch_list, token).ok_or("能力令牌无效")?;
            // 一次性令牌:已结算的 run 不再接受状态上报
            if run.outcome.is_some() {
                return Err("令牌所属运行已结算".into());
            }
            orch.handle_agent_state_report(run.id, state);
            Ok(format!("状态已上报: {state}"))
        }
        "pipeline.propose" => {
            let (orch, run) = find_by_token(&orch_list, token).ok_or("能力令牌无效")?;
            let draft: PipelineDraft =
                serde_json::from_value(params.get("draft").cloned().unwrap_or(json!({})))
                    .map_err(|e| format!("PipelineDraft 解析失败: {e}"))?;
            orch.planner_propose(run.task_id, &draft)
                .map_err(|e| format!("提案失败: {e:#}"))?;
            Ok(format!("草案已提交任务 #{}(等待用户确认)", run.task_id))
        }
        other => Err(format!("未知方法: {other}")),
    }
}
