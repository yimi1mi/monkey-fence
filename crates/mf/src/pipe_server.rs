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
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};

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

// ---------- T0c:mfctl 管道契约(Issue #14) ----------
//
// 冻结 Named Pipe 服务端的 wire 行为:请求/响应 NDJSON 形状、
// Settlement 幂等与冲突、令牌路由(空/无效/跨项目)与 agent.state。
// 真实 PipeServer + 与 mfctl request_over_pipe 同形的最小客户端;
// run 由 no-op 宿主的 Orchestrator 提供(真进程链由 agent_workflow_
// e2e_tests 的 Node 链测试覆盖;本模块聚焦 wire 契约,保持自足,
// 供 crates/mfctl/tests 以 #[path] 复用时独立编译)。

#[cfg(test)]
pub(crate) mod contract_tests {
    use super::*;
    use crossbeam_channel::Sender;
    use mf_agent::runtime::{LaunchSpec, RuntimeEvent, RuntimeHost, WorkflowLaunchSpec};
    use mf_agent::workflow::{ProjectWorkflowDraft, WorkflowNodeDraft, WorkflowTemplateVersion};
    use std::sync::OnceLock;

    /// no-op 宿主:只为让 Orchestrator 创建持有 capability_token 的
    /// 真实 run 行(不拉进程)。
    struct MockHost;
    impl RuntimeHost for MockHost {
        fn launch(&self, _spec: LaunchSpec, _events: Sender<(i64, RuntimeEvent)>) {}
        fn launch_workflow(
            &self,
            _spec: WorkflowLaunchSpec,
            _events: Sender<(i64, RuntimeEvent)>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn launch_ad_hoc(&self, _spec: mf_agent::runtime::AdHocLaunchSpec) -> anyhow::Result<()> {
            Ok(())
        }
        fn send_prompt(&self, _project: &str, _run_id: i64, _session_id: i64, _text: &str) {}
        fn stop_run(&self, _project: &str, _run_id: i64) -> anyhow::Result<()> {
            Ok(())
        }
        fn kill_session(&self, _project: &str, _session_id: i64) {}
        fn kill_ad_hoc(&self, _project: &str, _display_session_id: i64) {}
        fn answer_question(&self, _project: &str, _run_id: i64, _answer: &str) {}
    }

    /// 进程内唯一的 PipeServer(管道名含本进程 pid,
    /// FIRST_PIPE_INSTANCE 要求全进程单实例)。pub(crate) 暴露给
    /// crates/mfctl 的集成测试复用(#[path] include 本文件时,
    /// 同一二进制内必须共享同一服务端实例)。
    pub(crate) fn shared_pipe_server() -> Arc<Mutex<Vec<Arc<Orchestrator>>>> {
        static SERVER: OnceLock<Arc<Mutex<Vec<Arc<Orchestrator>>>>> = OnceLock::new();
        SERVER
            .get_or_init(|| {
                let list: Arc<Mutex<Vec<Arc<Orchestrator>>>> = Arc::new(Mutex::new(Vec::new()));
                PipeServer::start(list.clone()).expect("PipeServer 启动失败");
                list
            })
            .clone()
    }

    /// 服务端串行锁:同一服务端的 orchestrator 注册表被并行测试
    /// 动态增删,持有期间测试独占使用。
    pub(crate) fn serial_guard() -> parking_lot::MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL.lock()
    }

    struct PipeServerHandle {
        orchestrators: Arc<Mutex<Vec<Arc<Orchestrator>>>>,
        _serial: parking_lot::MutexGuard<'static, ()>,
    }

    fn with_pipe_server(test: impl FnOnce(&PipeServerHandle)) {
        let orchestrators = shared_pipe_server();
        let handle = PipeServerHandle {
            orchestrators,
            _serial: serial_guard(),
        };
        test(&handle);
    }

    /// 注册一个真实 orchestrator(项目 Store + no-op 宿主的工作流 run)。
    /// guard 持有项目临时目录(数据库生命周期),Drop 时从服务端注册表
    /// 移除并停止调度线程。
    pub(crate) struct RegisteredRun {
        pub(crate) orch: Arc<Orchestrator>,
        pub(crate) run: mf_agent::model::RunView,
        pub(crate) task_id: i64,
        orchestrators: Option<Arc<Mutex<Vec<Arc<Orchestrator>>>>>,
        _dir: tempfile::TempDir,
    }

    impl Drop for RegisteredRun {
        fn drop(&mut self) {
            if let Some(list) = self.orchestrators.take() {
                list.lock().retain(|o| !Arc::ptr_eq(o, &self.orch));
            }
            self.orch.stop();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while Arc::strong_count(&self.orch) > 1 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }

    impl std::ops::Deref for RegisteredRun {
        type Target = mf_agent::model::RunView;
        fn deref(&self) -> &Self::Target {
            &self.run
        }
    }

    pub(crate) fn registered_run_in(
        orchestrators: Option<Arc<Mutex<Vec<Arc<Orchestrator>>>>>,
    ) -> RegisteredRun {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog: Arc<mf_agent::CatalogStore> =
            mf_agent::CatalogStore::memory().expect("catalog");
        let instance = catalog
            .create_agent_instance(mf_agent::AgentInstanceDraft {
                name: "pipe-worker".into(),
                agent_type: "generic-command".into(),
                scope: mf_agent::InstanceScope::User,
                project_key: None,
                enabled: true,
                run_mode: mf_agent::RunMode::OneShot,
                executable: "cmd".into(),
                argv: vec![],
                env: vec![],
                config: serde_json::json!({}),
                execution_contract: serde_json::json!({
                    "input": "argv",
                    "completion": "process-exit"
                }),
                sealed_secret_ids: vec![],
            })
            .expect("创建实例");
        let orch = Orchestrator::start_with(
            mf_agent::Store::open(&dir.path().join("workflow-v1.db")).expect("打开 Store"),
            dir.path().to_path_buf(),
            mf_agent::Config::default(),
            Arc::new(MockHost),
            Arc::new(parking_lot::RwLock::new(
                mf_agent::orchestrator::ProfileCatalog::default(),
            )),
            mf_agent::orchestrator::GlobalLimiter::new(4),
            pipe_name_for_current_process(),
            Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider),
            mf_agent::orchestrator::WorkflowKernel {
                catalog,
                pins: None,
                instance_resolver: None,
            },
        )
        .expect("启动 Orchestrator");
        let record = orch
            .store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: "wf-pipe".into(),
                name: "管道契约".into(),
                nodes: vec![WorkflowNodeDraft {
                    key: "n1".into(),
                    title: "节点 n1".into(),
                    instructions: "执行".into(),
                    agent_instance_id: instance.id.clone(),
                    deps: vec![],
                }],
                allow_unsafe_parallel: false,
            })
            .expect("保存工作流");
        let version = WorkflowTemplateVersion {
            version_id: 0,
            template_key: format!("project-workflow/{}", record.key),
            version: 1,
            nodes: record.nodes.clone(),
            created_at: String::new(),
        };
        let task = orch.create_task("管道契约任务", "目标").expect("创建任务");
        let mut pins = std::collections::HashMap::new();
        pins.insert(
            "generic-command".to_string(),
            mf_agent::workflow::PluginSourcePin {
                full_id: "builtin.core".into(),
                version: "1.2.3".into(),
                content_hash: String::new(),
                contribution_id: String::new(),
            },
        );
        orch.assign_workflow(task.id, &version, &pins, false)
            .expect("assign");
        orch.confirm_and_run(task.id).expect("run");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let run = loop {
            let run = orch
                .store
                .list_runs_of_task(task.id)
                .expect("列 run")
                .into_iter()
                .max_by_key(|r| r.id);
            if let Some(run) = run {
                break run;
            }
            assert!(std::time::Instant::now() < deadline, "run 未在时限内创建");
            std::thread::sleep(std::time::Duration::from_millis(30));
        };
        if let Some(list) = &orchestrators {
            list.lock().push(orch.clone());
        }
        RegisteredRun {
            orch,
            run,
            task_id: task.id,
            orchestrators,
            _dir: dir,
        }
    }

    /// 与 mfctl 客户端同形的最小 NDJSON 管道请求。
    fn pipe_request(
        pipe_name: &str,
        token: &str,
        method: &str,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        let pipe_name = pipe_name.to_string();
        let token = token.to_string();
        let method = method.to_string();
        let params = params.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("mfctl-contract-client".into())
            .spawn(move || {
                let result = pipe_request_blocking(&pipe_name, &token, &method, &params);
                let _ = tx.send(result);
            })
            .expect("启动管道契约客户端失败");
        rx.recv_timeout(std::time::Duration::from_secs(12))
            .expect("管道请求超过 12s(阻塞 I/O 已与测试主线程隔离)")
    }

    fn pipe_request_blocking(
        pipe_name: &str,
        token: &str,
        method: &str,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, ReadFile, WriteFile, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
        };
        let wide: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let handle = loop {
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                break handle;
            }
            assert!(
                std::time::Instant::now() <= deadline,
                "连接测试管道失败(服务端未就绪)"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let outcome = (|| -> Option<serde_json::Value> {
            let req = json!({
                "id": 1,
                "token": token,
                "method": method,
                "params": params,
            })
            .to_string();
            let mut out = req.into_bytes();
            out.push(b'\n');
            let mut written = 0u32;
            unsafe {
                if WriteFile(
                    handle,
                    out.as_ptr(),
                    out.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                ) == 0
                    || written as usize != out.len()
                {
                    return None;
                }
            }
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if std::time::Instant::now() > deadline {
                    return None;
                }
                let mut read = 0u32;
                let ok = unsafe {
                    ReadFile(
                        handle,
                        chunk.as_mut_ptr(),
                        chunk.len() as u32,
                        &mut read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || read == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..read as usize]);
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    return serde_json::from_slice(&buf[..pos]).ok();
                }
            }
        })();
        unsafe { CloseHandle(handle) };
        outcome.expect("管道请求/响应失败")
    }

    fn wait_run_outcome(orch: &Arc<Orchestrator>, run_id: i64) -> Option<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(r)) = orch.store.run_view(run_id) {
                if r.outcome.is_some() {
                    return r.outcome;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        None
    }

    fn assert_response_has_no_token(response: &serde_json::Value, token: &str) {
        assert!(
            !response.to_string().contains(token),
            "管道响应不得回显真实 capability token"
        );
    }

    /// 契约:step.complete 结算真实 run;重复结算幂等;冲突结算拒绝;
    /// 响应 JSON 形状(ok/result|error)且不回显令牌。
    #[test]
    fn mfctl_wire_step_complete_settles_and_is_idempotent() {
        with_pipe_server(|server| {
            let registered = registered_run_in(Some(server.orchestrators.clone()));
            let run = &registered.run;
            let token = &run.capability_token;
            let pipe = pipe_name_for_current_process();

            let resp = pipe_request(
                &pipe,
                token,
                "step.complete",
                &json!({ "summary": "完成-中文摘要" }),
            );
            assert_response_has_no_token(&resp, token);
            assert_eq!(resp["ok"], json!(true), "step.complete 必须成功: {resp}");
            assert!(
                resp["result"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Step 已结算"),
                "成功文案: {resp}"
            );
            assert_eq!(
                wait_run_outcome(&registered.orch, run.id).as_deref(),
                Some("complete"),
                "run 必须被结算为 complete"
            );

            // 幂等:同结算重复提交返回相同终态
            let again = pipe_request(
                &pipe,
                token,
                "step.complete",
                &json!({ "summary": "重复提交" }),
            );
            assert_response_has_no_token(&again, token);
            assert_eq!(again["ok"], json!(true), "重复同向结算必须幂等: {again}");
            assert!(
                again["result"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("幂等"),
                "幂等文案: {again}"
            );

            // 冲突:已 complete 后提交 fail 拒绝
            let conflict =
                pipe_request(&pipe, token, "step.fail", &json!({ "reason": "反向结算" }));
            assert_response_has_no_token(&conflict, token);
            assert_eq!(conflict["ok"], json!(false), "冲突结算必须拒绝: {conflict}");
            assert!(
                conflict["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("冲突"),
                "冲突错误信息: {conflict}"
            );

            // 响应不携带令牌原文
            let raw = resp.to_string() + &again.to_string() + &conflict.to_string();
            assert!(!raw.contains(token), "响应不得回显能力令牌");
            // RegisteredRun::drop 从服务端注册表移除本测试的 orchestrator
        });
    }

    /// 契约:令牌路由 —— 空令牌与未知令牌拒绝;A 项目令牌只结算 A 的
    /// run,绝不影响 B 项目(结算目标由令牌唯一决定)。
    #[test]
    fn mfctl_wire_routes_token_to_its_own_run_only() {
        with_pipe_server(|server| {
            let pipe = pipe_name_for_current_process();
            let registered_a = registered_run_in(Some(server.orchestrators.clone()));
            let registered_b = registered_run_in(Some(server.orchestrators.clone()));
            let (orch_a, run_a) = (&registered_a.orch, &registered_a.run);
            let (orch_b, run_b) = (&registered_b.orch, &registered_b.run);

            // 空令牌
            let empty = pipe_request(&pipe, "", "step.complete", &json!({ "summary": "s" }));
            assert_eq!(empty["ok"], json!(false));
            assert!(
                empty["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("缺少能力令牌"),
                "空令牌错误: {empty}"
            );
            // 未知令牌
            let unknown = pipe_request(
                &pipe,
                "mft-definitely-not-a-real-token",
                "step.complete",
                &json!({ "summary": "s" }),
            );
            assert_eq!(unknown["ok"], json!(false));
            assert!(
                unknown["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("能力令牌无效"),
                "未知令牌错误: {unknown}"
            );

            // A 令牌结算 → 只动 A 的 run;B 的 run 保持未结算
            let resp = pipe_request(
                &pipe,
                &run_a.capability_token,
                "step.complete",
                &json!({ "summary": "A 完成" }),
            );
            assert_response_has_no_token(&resp, &run_a.capability_token);
            assert_eq!(resp["ok"], json!(true), "A 令牌结算 A run: {resp}");
            assert_eq!(
                wait_run_outcome(&orch_a, run_a.id).as_deref(),
                Some("complete")
            );
            assert!(
                orch_b
                    .store
                    .run_view(run_b.id)
                    .unwrap()
                    .is_some_and(|r| r.outcome.is_none()),
                "A 的令牌结算不得影响 B 的 run(跨项目令牌不可用)"
            );
        });
    }

    /// 契约:agent.state 上报工作状态;结算后一次性令牌不再接受状态上报。
    #[test]
    fn mfctl_wire_agent_state_then_rejects_after_settlement() {
        with_pipe_server(|server| {
            let pipe = pipe_name_for_current_process();
            let registered = registered_run_in(Some(server.orchestrators.clone()));
            let run = &registered.run;
            let token = &run.capability_token;

            let state = pipe_request(&pipe, token, "agent.state", &json!({ "state": "working" }));
            assert_response_has_no_token(&state, token);
            assert_eq!(state["ok"], json!(true), "状态上报: {state}");

            let settle = pipe_request(
                &pipe,
                token,
                "step.complete",
                &json!({ "summary": "先结算" }),
            );
            assert_response_has_no_token(&settle, token);
            assert_eq!(settle["ok"], json!(true));

            let late = pipe_request(&pipe, token, "agent.state", &json!({ "state": "done" }));
            assert_response_has_no_token(&late, token);
            assert_eq!(late["ok"], json!(false), "结算后状态上报必须拒绝: {late}");
            assert!(
                late["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("已结算"),
                "结算后错误: {late}"
            );
        });
    }
}
