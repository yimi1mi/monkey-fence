//! Workbench 服务器(T11 发布形态;用户验收入口)。
//!
//! 同一 loopback 端口上服务:静态资源(构建产物目录,路径穿越拒绝)、
//! `/auth/exchange`(一次性 nonce → HttpOnly session + CSRF;同时经
//! kernel `grant_controller` 把 web session 立为真 Controller,响应
//! 返回真实 lease epoch)、`/api/v1/snapshots/workspace`(session 认证
//! 后经 kernel 权威快照)、`/api/v1/commands`(Controller 写路径,经
//! kernel_bridge 全链 dispatch)、`/api/v1/controller/takeover`
//! (Observer 显式接管,kernel epoch CAS)。凭据永不进 URL/query;
//! nonce 由启动方签发并放进 fragment。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Router;
use base64::Engine as _;
use parking_lot::Mutex;

use crate::api::commands::{CommandEnvelope, CommandOutcomeWire};
use crate::api::kernel_bridge::{dispatch_via_kernel, snapshot_to_wire};
use crate::auth::{validate_host, validate_origin, BootstrapAuth, SessionRole, WebSession};
use crate::headers::security_headers;
use crate::limits::WebLimits;
use crate::problem::{Problem, ProblemCode, Retry};
use mf_kernel::kernel::CoreKernel;

struct WorkbenchState {
    bind_ip: String,
    port: u16,
    auth: Mutex<BootstrapAuth>,
    dist_root: PathBuf,
    kernel: Arc<dyn CoreKernel>,
    /// 本机验收模式(MF_WEB_ACCEPTANCE=1 显式开启):允许页面在 nonce
    /// 被浏览器预加载消耗后请求重签。生产 bundle 不设置该变量,
    /// 路由保持 404(一次性 nonce 语义不变)。
    acceptance: bool,
    /// 项目挂载后的执行面装配钩(#75:Orchestrator + ports;None =
    /// 仅数据面——快照/命令可见但运行不可执行)。
    on_project_attached: Option<ProjectAttachHook>,
    /// 终端宿主(#87 MFT1 输入面;None = 输入面禁用,只读输出仍可用)。
    terminal_host: Option<Arc<dyn mf_terminal::TerminalHost>>,
}

/// 挂载钩:参数为 project handle 与项目根目录(Store 由钩子幂等重开)。
pub type ProjectAttachHook = Arc<dyn Fn(&str, &Path) -> Result<(), String> + Send + Sync>;

/// 绑定并启动 workbench 服务(后台线程持有运行时;进程退出即止)。
/// 返回带一次性 nonce fragment 的入口 URL(浏览器直接打开)。
pub fn serve_workbench(
    kernel: Arc<dyn CoreKernel>,
    dist_root: impl Into<PathBuf>,
    port: u16,
) -> anyhow::Result<String> {
    serve_workbench_with_hook(kernel, dist_root, port, None)
}

/// 同 [`serve_workbench`],并在每次项目挂载成功后调用执行面装配钩。
pub fn serve_workbench_with_hook(
    kernel: Arc<dyn CoreKernel>,
    dist_root: impl Into<PathBuf>,
    port: u16,
    on_project_attached: Option<ProjectAttachHook>,
) -> anyhow::Result<String> {
    serve_workbench_full(kernel, dist_root, port, on_project_attached, None)
}

/// 全量装配(含终端宿主:#87 MFT1 输入面)。
pub fn serve_workbench_full(
    kernel: Arc<dyn CoreKernel>,
    dist_root: impl Into<PathBuf>,
    port: u16,
    on_project_attached: Option<ProjectAttachHook>,
    terminal_host: Option<Arc<dyn mf_terminal::TerminalHost>>,
) -> anyhow::Result<String> {
    let dist_root = dist_root.into();
    anyhow::ensure!(
        dist_root.join("index.html").is_file(),
        "dist 目录缺少 index.html:{}",
        dist_root.display()
    );
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    let bound_port = listener.local_addr()?.port();
    // tokio from_std 要求 non-blocking;否则注册后请求挂起
    listener.set_nonblocking(true)?;
    let mut auth = BootstrapAuth::new(WebLimits::default());
    let nonce = auth.issue_nonce();
    let acceptance = std::env::var("MF_WEB_ACCEPTANCE").ok().as_deref() == Some("1");
    let state = Arc::new(WorkbenchState {
        bind_ip: "127.0.0.1".into(),
        port: bound_port,
        auth: Mutex::new(auth),
        dist_root,
        kernel,
        acceptance,
        on_project_attached,
        terminal_host,
    });
    let router = Router::new()
        .route("/", get(index))
        .route("/auth/exchange", post(auth_exchange))
        .route("/auth/session", get(auth_session))
        .route("/api/v1/snapshots/workspace", get(workspace_snapshot))
        .route(
            "/api/v1/snapshots/workflow-run/{project}/{run}",
            get(workflow_run_snapshot),
        )
        .route(
            "/api/v1/snapshots/workflow/{project}/{workflow}",
            get(workflow_snapshot),
        )
        .route("/api/v1/terminal/{session}/output", get(terminal_output))
        .route("/api/v1/fs/file", get(fs_file))
        .route("/api/v1/vcs/status", get(vcs_status))
        .route("/api/v1/cli/detect", get(cli_detect))
        .route("/api/v1/catalog/instances", get(catalog_instances))
        .route("/api/v1/cli/recipes", get(cli_recipes))
        .route("/api/v1/cli/install", post(cli_install_route))
        .route("/api/v1/terminal/ws", get(terminal_ws))
        .route("/api/v1/commands", post(submit_command))
        .route("/api/v1/controller/takeover", post(controller_takeover))
        .route("/api/v1/projects", post(attach_project_route))
        .route("/api/v1/projects/{handle}", delete(detach_project_route))
        .route("/api/v1/fs/roots", get(fs_roots))
        .route("/api/v1/fs/dirs", get(fs_dirs))
        .route("/api/v1/events", get(events_ws))
        .route("/acceptance/new-nonce", post(acceptance_new_nonce))
        .route("/assets/{*path}", get(asset))
        .with_state(state);
    std::thread::Builder::new()
        .name("mf-workbench-web".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("listener upgrade");
                axum::serve(listener, router)
                    .await
                    .expect("workbench serve");
            });
        })?;
    let url = if bound_port == 80 {
        format!("http://127.0.0.1/#nonce={nonce}")
    } else {
        format!("http://127.0.0.1:{bound_port}/#nonce={nonce}")
    };
    Ok(url)
}

fn respond(
    status: StatusCode,
    headers: Vec<(axum::http::HeaderName, String)>,
    body: Vec<u8>,
) -> axum::response::Response {
    let mut response = axum::response::Response::builder().status(status);
    for (name, value) in headers {
        response = response.header(name, value);
    }
    response.body(axum::body::Body::from(body)).unwrap()
}

fn header_name(name: &'static str) -> axum::http::HeaderName {
    // from_static 要求全小写;安全头名称是混合大小写,经 try_from 规范化
    axum::http::HeaderName::try_from(name).expect("合法 header 名")
}

fn security(state: &WorkbenchState) -> Vec<(axum::http::HeaderName, String)> {
    security_headers(&state.bind_ip, state.port, false)
        .into_iter()
        .map(|(n, v)| (header_name(n), v))
        .collect()
}

fn host_origin_ok(state: &WorkbenchState, headers: &HeaderMap, require_origin: bool) -> bool {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !validate_host(host, &state.bind_ip, state.port) {
        return false;
    }
    if require_origin {
        let origin = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        return validate_origin(origin, &state.bind_ip, state.port);
    }
    true
}

fn session_of(state: &WorkbenchState, headers: &HeaderMap) -> Option<String> {
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    cookie
        .split(';')
        .find_map(|part| {
            let part = part.trim();
            part.strip_prefix("mf_session=").map(str::to_string)
        })
        .filter(|_| host_origin_ok(state, headers, false))
        .filter(|session| state.auth.lock().verify(session, None).is_ok())
}

/// web session 的 principal(kernel lease 授予与 dispatch 复验逐字一致)。
fn web_principal(client_id: &str) -> String {
    format!("web:{client_id}")
}

fn problem_response(problem: &Problem) -> axum::response::Response {
    let status =
        StatusCode::from_u16(problem.code.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
    let body = serde_json::to_vec(problem).unwrap_or_default();
    respond(status, Vec::new(), body)
}

/// 写路径授权:Origin 精确校验 + session cookie + CSRF + X-Client-Id
/// 一致性;`require_controller` 时 Observer 拒绝(controller_required)。
fn authorize(
    state: &WorkbenchState,
    headers: &HeaderMap,
    csrf: Option<&str>,
    require_controller: bool,
) -> Result<WebSession, Problem> {
    if !host_origin_ok(state, headers, true) {
        return Err(Problem::new(
            ProblemCode::OriginRejected,
            "Origin/Host 校验失败(loopback 精确匹配)",
            Some(Retry::Never),
        ));
    }
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let session_id = cookie
        .split(';')
        .find_map(|part| part.trim().strip_prefix("mf_session=").map(str::to_string))
        .ok_or_else(|| {
            Problem::new(ProblemCode::Unauthenticated, "缺少 mf_session cookie", None)
        })?;
    let session = state.auth.lock().verify(&session_id, csrf).map_err(|p| {
        let code = if matches!(p, crate::auth::AuthProblem::CsrfMismatch) {
            ProblemCode::CsrfRejected
        } else {
            ProblemCode::Unauthenticated
        };
        Problem::new(code, p.to_string(), Some(Retry::AfterReauth))
    })?;
    if let Some(claimed) = headers.get("x-client-id").and_then(|v| v.to_str().ok()) {
        if claimed != session.client_id {
            return Err(Problem::new(
                ProblemCode::Unauthenticated,
                "X-Client-Id 与 session 不符",
                Some(Retry::AfterReauth),
            ));
        }
    }
    if require_controller && session.role != SessionRole::Controller {
        return Err(Problem::new(
            ProblemCode::ControllerRequired,
            "Observer 禁写:请先接管为 Controller",
            Some(Retry::AfterReauth),
        ));
    }
    Ok(session)
}

/// exchange 后授予 kernel controller(返回新 lease epoch)。
fn grant_web_controller(kernel: &dyn CoreKernel, client_id: &str) -> Result<u64, Problem> {
    kernel
        .grant_controller(client_id, &web_principal(client_id))
        .map_err(|p| {
            Problem::new(
                ProblemCode::InternalError,
                format!("controller 授予失败:{p}"),
                None,
            )
        })
}

/// takeover 核心:kernel epoch CAS → 授予 → BootstrapAuth 角色提升。
fn takeover_core(
    state: &WorkbenchState,
    session: &WebSession,
    observed_epoch: u64,
) -> Result<u64, Problem> {
    let current = state.kernel.controller_epoch();
    if observed_epoch != current {
        let mut problem = Problem::new(
            ProblemCode::ControllerLeaseExpired,
            format!("观察 epoch({observed_epoch})已过期:当前 {current}(CAS 失败)"),
            Some(Retry::AfterReauth),
        );
        problem.current = Some(serde_json::json!({ "controller_epoch": current.to_string() }));
        return Err(problem);
    }
    let epoch = grant_web_controller(state.kernel.as_ref(), &session.client_id)?;
    state.auth.lock().promote_controller(&session.session_id);
    Ok(epoch)
}

/// 命令核心:envelope 身份一致性 → kernel_bridge 全链 dispatch。
fn command_core(
    state: &WorkbenchState,
    session: &WebSession,
    envelope: &CommandEnvelope,
) -> Result<CommandOutcomeWire, Problem> {
    if envelope.client_id != session.client_id {
        return Err(Problem::new(
            ProblemCode::InvalidEnvelope,
            "envelope client_id 与 session 不符",
            Some(Retry::Never),
        ));
    }
    dispatch_via_kernel(
        state.kernel.as_ref(),
        envelope,
        &web_principal(&session.client_id),
    )
}

fn content_type_of(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn read_dist(state: &WorkbenchState, rel: &str) -> Option<Vec<u8>> {
    // 路径穿越拒绝:规范化后必须仍在 dist 根内
    let target = state.dist_root.join(rel);
    let canonical = target.canonicalize().ok()?;
    let root = state.dist_root.canonicalize().ok()?;
    if !canonical.starts_with(&root) {
        return None;
    }
    std::fs::read(canonical).ok()
}

async fn index(State(state): State<Arc<WorkbenchState>>) -> impl IntoResponse {
    match read_dist(&state, "index.html") {
        Some(bytes) => {
            let mut headers = security(&state);
            headers.push((
                header_name("content-type"),
                "text/html; charset=utf-8".into(),
            ));
            respond(StatusCode::OK, headers, bytes)
        }
        None => respond(StatusCode::NOT_FOUND, security(&state), Vec::new()),
    }
}

async fn asset(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    if !host_origin_ok(&state, &headers, false) {
        return respond(StatusCode::FORBIDDEN, Vec::new(), Vec::new());
    }
    match read_dist(&state, &format!("assets/{path}")) {
        Some(bytes) => {
            let mut parts = security(&state);
            parts.push((
                header_name("content-type"),
                content_type_of(Path::new(&path)).into(),
            ));
            respond(StatusCode::OK, parts, bytes)
        }
        None => respond(StatusCode::NOT_FOUND, Vec::new(), Vec::new()),
    }
}

#[derive(serde::Deserialize)]
struct ExchangeRequest {
    nonce: String,
}

async fn auth_exchange(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<ExchangeRequest>,
) -> impl IntoResponse {
    if !host_origin_ok(&state, &headers, true) {
        return respond(StatusCode::FORBIDDEN, Vec::new(), Vec::new());
    }
    let source = format!("127.0.0.1:{}", state.port);
    let mut auth = state.auth.lock();
    match auth.exchange(&request.nonce, &source) {
        Ok(session) => {
            // 新 bootstrap 即 Controller:kernel lease 旋转使旧 web
            // controller 的 dispatch 立即失效(§6.4;与角色降级一致)。
            let epoch = match grant_web_controller(state.kernel.as_ref(), &session.client_id) {
                Ok(epoch) => epoch,
                Err(problem) => return problem_response(&problem),
            };
            let body = serde_json::json!({
                "schema": "mf.auth-bootstrap.v1",
                "client_id": session.client_id,
                "csrf_token": session.csrf_token,
                "controller": { "role": "controller", "lease_epoch": epoch.to_string() },
                "api_versions": crate::problem::API_VERSIONS,
                "ws_subprotocols": crate::problem::WS_SUBPROTOCOLS,
                "core_build": env!("CARGO_PKG_VERSION"),
            });
            let mut headers = security(&state);
            headers.push((
                axum::http::header::SET_COOKIE,
                format!(
                    "mf_session={}; HttpOnly; SameSite=Strict; Path=/",
                    session.session_id
                ),
            ));
            respond(StatusCode::OK, headers, body.to_string().into_bytes())
        }
        Err(problem) => {
            let body = serde_json::json!({
                "schema": "mf.problem.v1",
                "code": "auth_failed",
                "detail": problem.to_string(),
            });
            let status = if matches!(problem, crate::auth::AuthProblem::RateLimited { .. }) {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::UNAUTHORIZED
            };
            respond(status, Vec::new(), body.to_string().into_bytes())
        }
    }
}

async fn workspace_snapshot(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(_session) = session_of(&state, &headers) else {
        let body = serde_json::json!({
            "schema": "mf.problem.v1",
            "code": "unauthenticated",
            "detail": "需要已认证 session",
        });
        return respond(
            StatusCode::UNAUTHORIZED,
            Vec::new(),
            body.to_string().into_bytes(),
        );
    };
    match state
        .kernel
        .snapshot(mf_kernel::projection::SnapshotQuery::Workspace)
    {
        Ok(envelope) => {
            let wire = snapshot_to_wire(envelope);
            let mut headers = security(&state);
            headers.push((header_name("content-type"), "application/json".into()));
            respond(
                StatusCode::OK,
                headers,
                serde_json::to_vec(&wire).unwrap_or_default(),
            )
        }
        Err(problem) => {
            let body = serde_json::json!({
                "schema": "mf.problem.v1",
                "code": "internal_error",
                "detail": problem.to_string(),
            });
            respond(
                StatusCode::INTERNAL_SERVER_ERROR,
                Vec::new(),
                body.to_string().into_bytes(),
            )
        }
    }
}

/// 验收模式重签(默认 404;Host/Origin 校验同其它写路径)。
async fn acceptance_new_nonce(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !state.acceptance || !host_origin_ok(&state, &headers, true) {
        return respond(StatusCode::NOT_FOUND, Vec::new(), Vec::new());
    }
    let nonce = state.auth.lock().issue_nonce();
    let body = serde_json::json!({ "nonce": nonce });
    respond(StatusCode::OK, Vec::new(), body.to_string().into_bytes())
}

/// `POST /api/v1/commands`:Controller 写路径(kernel_bridge 全链)。
async fn submit_command(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    payload: Result<axum::Json<CommandEnvelope>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let csrf = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // 先授权后解析:未认证请求不暴露 envelope 校验细节
    let session = match authorize(&state, &headers, csrf.as_deref(), true) {
        Ok(session) => session,
        Err(problem) => return problem_response(&problem),
    };
    let envelope = match payload {
        Ok(axum::Json(envelope)) => envelope,
        Err(rejection) => {
            return problem_response(&Problem::new(
                ProblemCode::InvalidEnvelope,
                rejection.body_text(),
                Some(Retry::Never),
            ))
        }
    };
    match command_core(&state, &session, &envelope) {
        Ok(outcome) => {
            let status = match &outcome {
                CommandOutcomeWire::Accepted { .. } => StatusCode::ACCEPTED,
                CommandOutcomeWire::Applied { .. } => StatusCode::OK,
            };
            let mut headers = security(&state);
            headers.push((header_name("content-type"), "application/json".into()));
            respond(
                status,
                headers,
                serde_json::to_vec(&outcome).unwrap_or_default(),
            )
        }
        Err(problem) => {
            eprintln!(
                "[mf-workbench] command {} failed: {} ({})",
                envelope.command_type.as_str(),
                problem.message,
                serde_json::to_string(&problem.code).unwrap_or_default()
            );
            problem_response(&problem)
        }
    }
}

/// `GET /api/v1/snapshots/workflow-run/{project}/{run}`:单次运行的权威
/// 详情(steps/questions/agent_runs/sessions;#74 响应链的数据面)。
async fn workflow_run_snapshot(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::extract::Path((project, run)): axum::extract::Path<(String, String)>,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "需要已认证 session",
            None,
        ));
    }
    let Ok(project) = mf_kernel::handles::ProjectStoreHandle::parse(&project) else {
        return problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            "project handle 非法",
            Some(Retry::Never),
        ));
    };
    let Ok(run) =
        mf_kernel::handles::WorkflowRunHandle::parse(run.strip_prefix("run_").unwrap_or(&run))
    else {
        return problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            "workflow run handle 非法",
            Some(Retry::Never),
        ));
    };
    match state
        .kernel
        .snapshot(mf_kernel::projection::SnapshotQuery::WorkflowRun {
            project,
            workflow_run: run,
        }) {
        Ok(envelope) => {
            let wire = snapshot_to_wire(envelope);
            let mut headers = security(&state);
            headers.push((header_name("content-type"), "application/json".into()));
            respond(
                StatusCode::OK,
                headers,
                serde_json::to_vec(&wire).unwrap_or_default(),
            )
        }
        Err(problem) => problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            problem.to_string(),
            Some(Retry::Never),
        )),
    }
}

/// `GET /api/v1/terminal/ws?epoch=N`(WS;子协议 mf-terminal.v1):
/// MFT1 完整输入面(#87)——attach/hello/replay、writer lease、
/// binary 输入帧(lease 复验 + 真实写 PTY)、增量输出、resize、exit。
async fn terminal_ws(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "需要已认证 session",
            None,
        ))
        .into_response();
    }
    let Some(host) = state.terminal_host.clone() else {
        return problem_response(&Problem::new(
            ProblemCode::ServiceUnavailable,
            "终端宿主未装配(输入面不可用;只读输出走 HTTP 端点)",
            Some(Retry::Never),
        ))
        .into_response();
    };
    let epoch: u64 = query.get("epoch").and_then(|v| v.parse().ok()).unwrap_or(0);
    ws.protocols(["mf-terminal.v1"])
        .on_upgrade(move |socket| async move {
            terminal_pump(socket, host, epoch).await;
        })
        .into_response()
}

/// MFT1 会话泵:Text=ClientControl、Binary=输入帧;100ms tick 驱动
/// 增量输出与 exit。
async fn terminal_pump(
    mut socket: axum::extract::ws::WebSocket,
    host: Arc<dyn mf_terminal::TerminalHost>,
    epoch: u64,
) {
    use crate::ws::terminal::{
        ClientControl, ControlOutcome, InputOutcome, ServerControl, TerminalWsSession,
    };
    use axum::extract::ws::{CloseFrame, Message, WebSocket};
    use mf_terminal::TerminalSessionRef;

    let mut session = TerminalWsSession::new();
    let mut session_handle: Option<String> = None;
    let mut last_seq: u64 = 0;

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(control) = serde_json::from_str::<ClientControl>(&text) else {
                            let _ = socket.send(Message::Text(
                                serde_json::to_string(&ServerControl::Problem {
                                    code: "invalid_envelope".into(),
                                    detail: "控制帧解析失败".into(),
                                }).unwrap_or_default().into(),
                            )).await;
                            continue;
                        };
                        if session_handle.is_none() {
                            if let ClientControl::Attach { session_handle: handle, .. } = &control {
                                session_handle = Some(handle.clone());
                            }
                        }
                        let Some(handle) = session_handle.clone() else {
                            let _ = socket.send(Message::Text(
                                serde_json::to_string(&ServerControl::Problem {
                                    code: "invalid_envelope".into(),
                                    detail: "首帧必须是 attach".into(),
                                }).unwrap_or_default().into(),
                            )).await;
                            return;
                        };
                        match session.control(host.as_ref(), &handle, control, epoch) {
                            ControlOutcome::Attached(hello, frames) => {
                                if let Ok(text) = serde_json::to_string(&hello) {
                                    let _ = socket.send(Message::Text(text.into())).await;
                                }
                                for frame in frames {
                                    if let Some(seq) = decode_output_seq(&frame) {
                                        last_seq = last_seq.max(seq);
                                    }
                                    let _ = socket.send(Message::Binary(frame.into())).await;
                                }
                            }
                            ControlOutcome::Continued(optional) => {
                                if let Some(control) = optional {
                                    if let Ok(text) = serde_json::to_string(&control) {
                                        let _ = socket.send(Message::Text(text.into())).await;
                                    }
                                }
                            }
                            ControlOutcome::Close { close_code, problem } => {
                                if let Ok(text) = serde_json::to_string(&ServerControl::Problem {
                                    code: "terminal_close".into(),
                                    detail: problem.message,
                                }) {
                                    let _ = socket.send(Message::Text(text.into())).await;
                                }
                                let _ = socket.send(Message::Close(Some(CloseFrame {
                                    code: close_code,
                                    reason: "mf-terminal".into(),
                                }))).await;
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Binary(frame))) => {
                        let Some(handle) = session_handle.clone() else { return };
                        match session.binary_input(host.as_ref(), &frame, epoch, &handle) {
                            InputOutcome::Acked { input_seq, ack_id } => {
                                if let Ok(text) = serde_json::to_string(&ServerControl::InputAck {
                                    input_seq: input_seq.to_string(),
                                    ack_id: ack_id.to_string(),
                                }) {
                                    let _ = socket.send(Message::Text(text.into())).await;
                                }
                            }
                            InputOutcome::OutOfOrder { expected_seq } => {
                                if let Ok(text) = serde_json::to_string(&ServerControl::OutOfOrder {
                                    expected_input_seq: expected_seq.to_string(),
                                }) {
                                    let _ = socket.send(Message::Text(text.into())).await;
                                }
                            }
                            InputOutcome::Rejected { problem, close } => {
                                if let Ok(text) = serde_json::to_string(&ServerControl::Problem {
                                    code: "input_rejected".into(),
                                    detail: problem.message,
                                }) {
                                    let _ = socket.send(Message::Text(text.into())).await;
                                }
                                if close { return; }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        session.connection_closed();
                        return;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                let Some(handle) = session_handle.clone() else { continue };
                let reference = TerminalSessionRef::new(handle.clone());
                if session.is_attached() {
                    if let Ok(chunks) = host.replay_output(&reference, last_seq) {
                        for chunk in chunks {
                            if let Ok(frame) =
                                mf_terminal::channel::encode_output_frame(chunk.seq, &chunk.bytes)
                            {
                                last_seq = last_seq.max(chunk.seq);
                                if socket.send(Message::Binary(frame.into())).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    if let Some(problem) = session.poll_output() {
                        if let Ok(text) = serde_json::to_string(&ServerControl::Problem {
                            code: "rate_limited".into(),
                            detail: problem.message,
                        }) {
                            let _ = socket.send(Message::Text(text.into())).await;
                        }
                        return;
                    }
                    if let Some(exit) = session.poll_exit(host.as_ref(), &handle) {
                        if let Ok(text) = serde_json::to_string(&exit) {
                            let _ = socket.send(Message::Text(text.into())).await;
                        }
                        let _ = socket.send(Message::Close(Some(CloseFrame {
                            code: 1000,
                            reason: "mf-terminal-exit".into(),
                        }))).await;
                        return;
                    }
                }
            }
        }
    }
}

/// 从输出帧解 seq(MFT1 头:kind@4, seq@8 大端)。
fn decode_output_seq(frame: &[u8]) -> Option<u64> {
    if frame.len() < 16 {
        return None;
    }
    Some(u64::from_be_bytes(frame[8..16].try_into().ok()?))
}

/// `GET /api/v1/terminal/{session}/output?after=N`:只读终端输出增量
/// (#77 v1;MFT1 writer 输入面待完整 WS 票)。frames 为 [seq, base64]。
async fn terminal_output(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::extract::Path(session): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "需要已认证 session",
            None,
        ));
    }
    let after: u64 = query.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
    let handle = mf_kernel::handles::SessionHandle::try_from(
        session
            .strip_prefix("sess_")
            .unwrap_or(&session)
            .to_string(),
    );
    let Ok(handle) = handle else {
        return problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            "session handle 非法",
            Some(Retry::Never),
        ));
    };
    let attach = mf_kernel::kernel::TerminalAttach { after_seq: after };
    match state.kernel.attach_terminal(handle, attach) {
        Ok(channel) => {
            let facts = channel.output_facts().ok();
            let alive = channel.is_alive();
            let frames = channel
                .replay_output(after)
                .unwrap_or_default()
                .into_iter()
                .map(|chunk| {
                    (
                        chunk.seq,
                        base64::engine::general_purpose::STANDARD.encode(chunk.bytes.as_ref()),
                    )
                })
                .collect::<Vec<_>>();
            let body = serde_json::json!({
                "schema": "mf.terminal-output.v1",
                "alive": alive,
                "last_seq": facts.as_ref().map(|f| f.last_seq.to_string()),
                "frames": frames,
            });
            let mut headers = security(&state);
            headers.push((header_name("content-type"), "application/json".into()));
            respond(
                StatusCode::OK,
                headers,
                serde_json::to_vec(&body).unwrap_or_default(),
            )
        }
        Err(problem) => problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            format!("终端附着失败:{problem}"),
            Some(Retry::Never),
        )),
    }
}

/// `GET /api/v1/fs/file?path=…`:只读文本文件内容(≤256KB;#80)。
async fn fs_file(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "需要已认证 session",
            None,
        ));
    }
    let Some(path) = query.get("path") else {
        return problem_response(&Problem::new(
            ProblemCode::InvalidEnvelope,
            "缺少 path",
            Some(Retry::Never),
        ));
    };
    let target = std::path::PathBuf::from(path);
    match std::fs::metadata(&target) {
        Ok(meta) if meta.is_file() && meta.len() <= 256 * 1024 => {}
        Ok(_) => {
            return problem_response(&Problem::new(
                ProblemCode::ValidationFailed,
                "仅支持 ≤256KB 的文件",
                Some(Retry::Never),
            ))
        }
        Err(error) => {
            return problem_response(&Problem::new(
                ProblemCode::ResourceNotFound,
                format!("读取失败:{error}"),
                Some(Retry::Never),
            ))
        }
    }
    match std::fs::read_to_string(&target) {
        Ok(content) => {
            let body = serde_json::json!({
                "schema": "mf.fs-file.v1",
                "path": path,
                "content": content,
            });
            let mut headers = security(&state);
            headers.push((header_name("content-type"), "application/json".into()));
            respond(
                StatusCode::OK,
                headers,
                serde_json::to_vec(&body).unwrap_or_default(),
            )
        }
        Err(error) => problem_response(&Problem::new(
            ProblemCode::ValidationFailed,
            format!("非 UTF-8 文本或读取失败:{error}"),
            Some(Retry::Never),
        )),
    }
}

/// `GET /api/v1/vcs/status?root=…`:git 分支与工作区状态(#81;只读)。
async fn vcs_status(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "需要已认证 session",
            None,
        ));
    }
    let Some(root) = query.get("root") else {
        return problem_response(&Problem::new(
            ProblemCode::InvalidEnvelope,
            "缺少 root",
            Some(Retry::Never),
        ));
    };
    if !mf_vcs::git::Git::is_repo(root) {
        let body = serde_json::json!({ "schema": "mf.vcs-status.v1", "repo": false });
        let mut headers = security(&state);
        headers.push((header_name("content-type"), "application/json".into()));
        return respond(
            StatusCode::OK,
            headers,
            serde_json::to_vec(&body).unwrap_or_default(),
        );
    }
    let repo = match mf_vcs::git::Git::open(root) {
        Ok(repo) => repo,
        Err(error) => {
            return problem_response(&Problem::new(
                ProblemCode::ServiceUnavailable,
                format!("仓库打开失败:{error}"),
                Some(Retry::Never),
            ))
        }
    };
    let branch = repo.branch().unwrap_or_else(|_| "HEAD".into());
    let entries = repo
        .status()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            serde_json::json!({
                "path": entry.path.to_string_lossy(),
                "status": entry.status.code(),
            })
        })
        .collect::<Vec<_>>();
    let body = serde_json::json!({
        "schema": "mf.vcs-status.v1",
        "repo": true,
        "branch": branch,
        "entries": entries,
    });
    let mut headers = security(&state);
    headers.push((header_name("content-type"), "application/json".into()));
    respond(
        StatusCode::OK,
        headers,
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

/// `GET /api/v1/cli/detect`(#90):PATH 扫描常见 agent CLI + catalog
/// 安装记录只读合并。安装/维护写面待 cli.* 内核接管(#87)。
async fn cli_detect(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "需要已认证 session",
            None,
        ));
    }
    const KNOWN: &[&str] = &[
        "codex",
        "claude",
        "gemini",
        "qwen",
        "opencode",
        "crush",
        "copilot",
        "aider",
        "cursor-agent",
        "goose",
        "amazonq",
    ];
    let path_env = std::env::var("PATH").unwrap_or_default();
    let mut detected: Vec<serde_json::Value> = Vec::new();
    for name in KNOWN {
        let hit = std::env::split_paths(&path_env).find_map(|dir| {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                Some(exe.to_string_lossy().into_owned())
            } else {
                let plain = dir.join(name);
                if plain.is_file() {
                    Some(plain.to_string_lossy().into_owned())
                } else {
                    None
                }
            }
        });
        if let Some(executable) = hit {
            detected.push(serde_json::json!({
                "agent_type_id": name,
                "executable": executable,
                "source": "path",
            }));
        }
    }
    let body = serde_json::json!({
        "schema": "mf.cli-detect.v1",
        "detected": detected,
        "maintenance": "unavailable",
        "maintenance_reason": "cli.* 命令族待 CoreKernel 接管(#87);检测面只读",
    });
    let mut headers = security(&state);
    headers.push((header_name("content-type"), "application/json".into()));
    respond(
        StatusCode::OK,
        headers,
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

/// `GET /api/v1/catalog/instances`(#87):真实 catalog 只读实例列表
/// (READ_ONLY 打开,绝不写入)。写面(注册/编辑)经 launcher/CLI。
async fn catalog_instances(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "需要已认证 session",
            None,
        ));
    }
    let catalog = match mf_agent::CatalogStore::open_read_only(&mf_agent::catalog_db_path()) {
        Ok(catalog) => catalog,
        Err(error) => {
            return problem_response(&Problem::new(
                ProblemCode::ServiceUnavailable,
                format!("catalog 只读打开失败:{error:#}"),
                Some(Retry::Never),
            ))
        }
    };
    let instances = catalog
        .list_agent_instances(None)
        .unwrap_or_default()
        .into_iter()
        .map(|instance| {
            serde_json::json!({
                "id": instance.id,
                "name": instance.name,
                "agent_type": instance.agent_type,
                "enabled": instance.enabled,
                "version": instance.current_version,
            })
        })
        .collect::<Vec<_>>();
    let body = serde_json::json!({
        "schema": "mf.catalog-instances.v1",
        "instances": instances,
        "writable": false,
        "write_note": "实例写面经 launcher/CLI(web 只读)",
    });
    let mut headers = security(&state);
    headers.push((header_name("content-type"), "application/json".into()));
    respond(
        StatusCode::OK,
        headers,
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

/// `GET /api/v1/cli/recipes`(#93):内置安装 recipe + 包管理器探测。
async fn cli_recipes(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "需要已认证 session",
            None,
        ));
    }
    let manager = crate::cli_install::detect_package_manager();
    let recipes = crate::cli_install::RECIPES
        .iter()
        .map(|recipe| {
            serde_json::json!({
                "agent_type": recipe.agent_type,
                "package": recipe.package,
                "display": recipe.display,
                "manager": manager.as_ref().map(|(m, _)| m),
            })
        })
        .collect::<Vec<_>>();
    let body = serde_json::json!({
        "schema": "mf.cli-recipes.v1",
        "recipes": recipes,
        "package_manager": manager.as_ref().map(|(m, _)| m),
        "install_available": manager.is_some(),
    });
    let mut headers = security(&state);
    headers.push((header_name("content-type"), "application/json".into()));
    respond(
        StatusCode::OK,
        headers,
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

#[derive(serde::Deserialize)]
struct CliInstallRequest {
    agent_type: String,
}

/// `POST /api/v1/cli/install`(#93):Controller-only;PlanFreezer 冻结
/// → execute_plan(生产 OsExecutorEnv)→ receipt。catalog 恒零写入;
/// 安装幂等(包管理器语义),检测以 PATH 事实为准。
async fn cli_install_route(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    payload: Result<axum::Json<CliInstallRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let csrf = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Err(problem) = authorize(&state, &headers, csrf.as_deref(), true) {
        return problem_response(&problem).into_response();
    }
    let request = match payload {
        Ok(axum::Json(request)) => request,
        Err(rejection) => {
            return problem_response(&Problem::new(
                ProblemCode::InvalidEnvelope,
                rejection.body_text(),
                Some(Retry::Never),
            ))
            .into_response()
        }
    };
    let Some(recipe) = crate::cli_install::RECIPES
        .iter()
        .find(|r| r.agent_type == request.agent_type)
    else {
        return problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            format!("未知 agent CLI:{}", request.agent_type),
            Some(Retry::Never),
        ))
        .into_response();
    };
    let Some((manager, base_argv)) = crate::cli_install::detect_package_manager() else {
        return problem_response(&Problem::new(
            ProblemCode::ServiceUnavailable,
            "无可用包管理器(npm/winget 均未检测到)",
            Some(Retry::Never),
        ))
        .into_response();
    };
    // 构造冻结计划(package-manager 类):
    // exact_package = 可执行名(executor 以它为 program), argv = 完整参数
    let program = if manager == "npm" {
        if cfg!(windows) { "npm.cmd" } else { "npm" }.to_string()
    } else {
        manager.to_string()
    };
    let mut argv = base_argv.clone();
    argv.push(recipe.package.to_string());
    let preview = mf_installer::plan::InstallPreview {
        agent_type_id: recipe.agent_type.to_string(),
        installer_id: format!("{manager}-global"),
        kind: "package-manager".into(),
        exact_package: program,
        exact_version: "latest".into(),
        argv,
        download: None,
        catalog_revision: 0,
    };
    let mut freezer = mf_installer::plan::PlanFreezer::new(30);
    let ticket = match freezer.freeze(preview, 0) {
        Ok(ticket) => ticket,
        Err(problem) => {
            return problem_response(&Problem::new(
                ProblemCode::ValidationFailed,
                format!("安装计划冻结失败:{problem:?}"),
                Some(Retry::Never),
            ))
            .into_response()
        }
    };
    let plan = match freezer.redeem(&ticket) {
        Ok(plan) => plan.clone(),
        Err(problem) => {
            return problem_response(&Problem::new(
                ProblemCode::ValidationFailed,
                format!("安装票据兑换失败:{problem:?}"),
                Some(Retry::Never),
            ))
            .into_response()
        }
    };
    let staging =
        std::env::temp_dir().join(format!("mf-install-{}", uuid::Uuid::now_v7().simple()));
    // package-manager 全局安装的可执行落点:npm prefix -g(Windows 的
    // prefix 即全局 bin,含 <agent_type>.cmd shim)。探测它 = 真实事实。
    let prefix_output = std::process::Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
        .args(["prefix", "-g"])
        .output();
    let target = match prefix_output {
        Ok(output) if output.status.success() => {
            let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if cfg!(windows) {
                std::path::PathBuf::from(prefix).join(format!("{}.cmd", recipe.agent_type))
            } else {
                std::path::PathBuf::from(prefix)
                    .join("bin")
                    .join(recipe.agent_type)
            }
        }
        _ => staging.join("marker"),
    };
    let mut env = crate::cli_install::OsExecutorEnv;
    let outcome = mf_installer::executor::execute_plan(
        &mut env,
        &plan,
        &staging,
        &target,
        &["--version".to_string()],
        &mf_installer::executor::DownloadPolicy::default(),
        &|| false,
    );
    let body = match outcome {
        mf_installer::executor::ExecuteOutcome::Installed(receipt) => serde_json::json!({
            "schema": "mf.cli-install-result.v1",
            "outcome": "installed",
            "agent_type": recipe.agent_type,
            "package": recipe.package,
            "version": receipt.actual_version,
        }),
        mf_installer::executor::ExecuteOutcome::Failed { phase, reason } => serde_json::json!({
            "schema": "mf.cli-install-result.v1",
            "outcome": "failed",
            "agent_type": recipe.agent_type,
            "phase": format!("{phase:?}"),
            "reason": reason,
        }),
        other => serde_json::json!({
            "schema": "mf.cli-install-result.v1",
            "outcome": "other",
            "detail": format!("{other:?}"),
        }),
    };
    let mut headers = security(&state);
    headers.push((header_name("content-type"), "application/json".into()));
    respond(
        StatusCode::OK,
        headers,
        serde_json::to_vec(&body).unwrap_or_default(),
    )
    .into_response()
}

#[derive(serde::Deserialize)]
struct TakeoverRequest {
    last_observed_epoch: String,
}

/// `GET /api/v1/snapshots/workflow/{project}/{workflow}`:单工作流权威
/// 详情(nodes/edges/双轴 revision;#76 编辑器数据面)。
async fn workflow_snapshot(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::extract::Path((project, workflow)): axum::extract::Path<(String, String)>,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "需要已认证 session",
            None,
        ));
    }
    let Ok(project) = mf_kernel::handles::ProjectStoreHandle::parse(&project) else {
        return problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            "project handle 非法",
            Some(Retry::Never),
        ));
    };
    let Ok(workflow) = mf_kernel::handles::WorkflowHandle::parse(
        workflow.strip_prefix("wf_").unwrap_or(&workflow),
    ) else {
        return problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            "workflow handle 非法",
            Some(Retry::Never),
        ));
    };
    match state
        .kernel
        .snapshot(mf_kernel::projection::SnapshotQuery::Workflow { project, workflow })
    {
        Ok(envelope) => {
            let wire = snapshot_to_wire(envelope);
            let mut headers = security(&state);
            headers.push((header_name("content-type"), "application/json".into()));
            respond(
                StatusCode::OK,
                headers,
                serde_json::to_vec(&wire).unwrap_or_default(),
            )
        }
        Err(problem) => problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            problem.to_string(),
            Some(Retry::Never),
        )),
    }
}

// ---------------------------------------------------------------------------
// 目录浏览(#73):添加项目的浏览选择面。只读、仅目录名;已认证会话
// 可用(挂载仍需 Controller)。
// ---------------------------------------------------------------------------

/// 单个目录的可见性过滤:隐藏/系统目录不进入浏览面。
fn browsable_dir_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    #[cfg(unix)]
    {
        !name.starts_with('.')
    }
    #[cfg(windows)]
    {
        !(name.starts_with('$')
            || name.starts_with('.')
            || name.eq_ignore_ascii_case("System Volume Information")
            || name.eq_ignore_ascii_case("Config.Msi")
            || name.eq_ignore_ascii_case("Recovery")
            || name.eq_ignore_ascii_case("PerfLogs"))
    }
}

#[derive(serde::Serialize)]
struct FsEntryWire {
    path: String,
    name: String,
}

#[derive(serde::Serialize)]
struct FsDirsWire {
    path: String,
    parent: Option<String>,
    dirs: Vec<FsEntryWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn parent_of(path: &std::path::Path) -> Option<String> {
    path.parent()
        .filter(|p| p.as_os_str() != std::ffi::OsStr::new(""))
        .map(|p| p.to_string_lossy().into_owned())
}

/// `GET /api/v1/fs/dirs?path=…`:列子目录(仅目录名,上限 500;
/// 无权限/不存在 → 200 + error 字段,UI 原地提示)。
async fn fs_dirs(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "目录浏览需要已认证 session",
            None,
        ))
        .into_response();
    }
    let raw = query.get("path").cloned().unwrap_or_default();
    if raw.trim().is_empty() {
        return problem_response(&Problem::new(
            ProblemCode::InvalidEnvelope,
            "缺少 path 查询参数",
            Some(Retry::Never),
        ))
        .into_response();
    }
    let path = std::path::PathBuf::from(&raw);
    let mut dirs = Vec::new();
    let mut error = None;
    match std::fs::read_dir(&path) {
        Ok(entries) => {
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if !browsable_dir_name(&name) {
                    continue;
                }
                dirs.push(FsEntryWire {
                    path: entry.path().to_string_lossy().into_owned(),
                    name,
                });
                if dirs.len() >= 500 {
                    break;
                }
            }
            dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        }
        Err(err) => {
            error = Some(format!("无法读取目录:{err}"));
        }
    }
    let wire = FsDirsWire {
        path: path.to_string_lossy().into_owned(),
        parent: parent_of(&path),
        dirs,
        error,
    };
    let mut headers = security(&state);
    headers.push((header_name("content-type"), "application/json".into()));
    respond(
        StatusCode::OK,
        headers,
        serde_json::to_vec(&wire).unwrap_or_default(),
    )
    .into_response()
}

/// `GET /api/v1/fs/roots`:浏览起点(盘符 + 用户主目录)。
async fn fs_roots(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "目录浏览需要已认证 session",
            None,
        ))
        .into_response();
    }
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        roots.push(FsEntryWire {
            name: "主目录".into(),
            path: home.clone(),
        });
        let desktop = std::path::PathBuf::from(&home).join("Desktop");
        if desktop.is_dir() {
            roots.push(FsEntryWire {
                name: "桌面".into(),
                path: desktop.to_string_lossy().into_owned(),
            });
        }
    }
    #[cfg(windows)]
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:\\", letter as char);
        if std::path::Path::new(&drive).is_dir() {
            roots.push(FsEntryWire {
                name: format!("{} 盘", letter as char),
                path: drive,
            });
        }
    }
    #[cfg(unix)]
    roots.push(FsEntryWire {
        name: "根目录".into(),
        path: "/".into(),
    });
    let mut headers = security(&state);
    headers.push((header_name("content-type"), "application/json".into()));
    respond(
        StatusCode::OK,
        headers,
        serde_json::to_vec(&serde_json::json!({ "roots": roots })).unwrap_or_default(),
    )
    .into_response()
}

/// `GET /auth/session`:刷新续用探测——服务端权威回报当前角色与
/// lease epoch(存储的 bootstrap 会过期:其它会话接管/重换后本会话
/// 已降 Observer,客户端不得沿用旧角色)。
async fn auth_session(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(session_id) = session_of(&state, &headers) else {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "会话不存在或已失效",
            Some(Retry::AfterReauth),
        ));
    };
    let session = match state.auth.lock().verify(&session_id, None) {
        Ok(session) => session,
        Err(problem) => {
            return problem_response(&Problem::new(
                ProblemCode::Unauthenticated,
                problem.to_string(),
                Some(Retry::AfterReauth),
            ))
        }
    };
    let role = if session.role == SessionRole::Controller {
        "controller"
    } else {
        "observer"
    };
    let body = serde_json::json!({
        "schema": "mf.auth-bootstrap.v1",
        "client_id": session.client_id,
        "csrf_token": session.csrf_token,
        "controller": {
            "role": role,
            "lease_epoch": state.kernel.controller_epoch().to_string(),
        },
    });
    let mut headers = security(&state);
    headers.push((header_name("content-type"), "application/json".into()));
    respond(StatusCode::OK, headers, body.to_string().into_bytes())
}

/// `GET /api/v1/events`(WS;子协议 mf-workflow.v1):session 认证 +
/// Origin 校验 → 首帧 Resume → hello → 100ms poll 驱动事件 fan-out;
/// epoch 旋转/gap/慢客户端 → problem 帧 + close 4409(客户端全量
/// resync)。凭据不进 URL(cookie 自动携带)。
async fn events_ws(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> impl IntoResponse {
    if !host_origin_ok(&state, &headers, true) {
        return problem_response(&Problem::new(
            ProblemCode::OriginRejected,
            "Origin/Host 校验失败(loopback 精确匹配)",
            Some(Retry::Never),
        ))
        .into_response();
    }
    if session_of(&state, &headers).is_none() {
        return problem_response(&Problem::new(
            ProblemCode::Unauthenticated,
            "事件流需要已认证 session",
            None,
        ))
        .into_response();
    }
    // 子协议:未带默认 mf-workflow.v1;带了就必须精确
    let requested: Vec<String> = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_string))
        .collect();
    if !requested.is_empty() && !requested.iter().any(|p| p == "mf-workflow.v1") {
        return problem_response(&Problem::new(
            ProblemCode::UnsupportedWsSubprotocol,
            "事件流仅支持 mf-workflow.v1",
            Some(Retry::Never),
        ))
        .into_response();
    }
    let kernel = state.kernel.clone();
    ws.protocols(["mf-workflow.v1"])
        .on_upgrade(move |socket| async move {
            events_pump(socket, kernel).await;
        })
        .into_response()
}

async fn events_pump(mut socket: axum::extract::ws::WebSocket, kernel: Arc<dyn CoreKernel>) {
    use crate::ws::events::{EventsSession, PollOutcome};

    // 首帧必须是 Resume(cursor 恢复)
    let control = loop {
        match socket.recv().await {
            Some(Ok(axum::extract::ws::Message::Text(text))) => {
                match serde_json::from_str::<crate::ws::events::EventsControl>(&text) {
                    Ok(control) => break control,
                    Err(error) => {
                        let problem = Problem::new(
                            ProblemCode::InvalidEnvelope,
                            format!("首帧必须是 Resume 控制帧:{error}"),
                            Some(Retry::Never),
                        );
                        let _ = socket
                            .send(axum::extract::ws::Message::Text(
                                serde_json::to_string(&problem).unwrap_or_default().into(),
                            ))
                            .await;
                        close_ws(&mut socket, crate::problem::close_code::INVALID_ENVELOPE).await;
                        return;
                    }
                }
            }
            Some(Ok(_)) => continue,
            _ => return,
        }
    };
    let mut session = match EventsSession::resume(kernel.as_ref(), &control) {
        Ok(session) => session,
        Err(problem) => {
            let _ = socket
                .send(axum::extract::ws::Message::Text(
                    serde_json::to_string(&problem).unwrap_or_default().into(),
                ))
                .await;
            close_ws(
                &mut socket,
                crate::problem::close_code::RESYNC_OR_HISTORY_GAP,
            )
            .await;
            return;
        }
    };
    // hello(resume 回执;resync_required 时客户端自行拉全量快照)
    if let Ok(hello) = serde_json::to_string(session.hello()) {
        if socket
            .send(axum::extract::ws::Message::Text(hello.into()))
            .await
            .is_err()
        {
            return;
        }
    }
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = tick.tick() => match session.poll() {
                PollOutcome::Events(events) => {
                    for event in events {
                        let wire = crate::api::events::EventEnvelope::from(event);
                        let Ok(text) = serde_json::to_string(&wire) else { continue };
                        if socket
                            .send(axum::extract::ws::Message::Text(text.into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                PollOutcome::Closed { close_code, problem } => {
                    let _ = socket
                        .send(axum::extract::ws::Message::Text(
                            serde_json::to_string(&problem).unwrap_or_default().into(),
                        ))
                        .await;
                    close_ws(&mut socket, close_code).await;
                    return;
                }
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(axum::extract::ws::Message::Close(_))) | None => return,
                Some(Ok(_)) => {}
                Some(Err(_)) => return,
            },
        }
    }
}

async fn close_ws(socket: &mut axum::extract::ws::WebSocket, code: u16) {
    use axum::extract::ws::{CloseFrame, Message};
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: "mf.events".into(),
        })))
        .await;
}

/// `POST /api/v1/controller/takeover`:Observer 显式接管(kernel CAS)。
async fn controller_takeover(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    payload: Result<axum::Json<TakeoverRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let csrf = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let session = match authorize(&state, &headers, csrf.as_deref(), false) {
        Ok(session) => session,
        Err(problem) => return problem_response(&problem),
    };
    let request = match payload {
        Ok(axum::Json(request)) => request,
        Err(rejection) => {
            return problem_response(&Problem::new(
                ProblemCode::InvalidEnvelope,
                rejection.body_text(),
                Some(Retry::Never),
            ))
        }
    };
    let Ok(observed) = request.last_observed_epoch.parse::<u64>() else {
        return problem_response(&Problem::new(
            ProblemCode::InvalidEnvelope,
            "last_observed_epoch 必须是 u64 字符串",
            Some(Retry::Never),
        ));
    };
    match takeover_core(&state, &session, observed) {
        Ok(epoch) => {
            let body = serde_json::json!({
                "schema": "mf.auth-bootstrap.v1",
                "client_id": session.client_id,
                "csrf_token": session.csrf_token,
                "controller": { "role": "controller", "lease_epoch": epoch.to_string() },
            });
            let mut headers = security(&state);
            headers.push((header_name("content-type"), "application/json".into()));
            respond(StatusCode::OK, headers, body.to_string().into_bytes())
        }
        Err(problem) => problem_response(&problem),
    }
}

#[derive(serde::Deserialize)]
struct AttachProjectRequest {
    path: String,
}

/// `POST /api/v1/projects`:挂载项目目录(多项目入口;Controller-only)。
/// 根目录必须已存在且为目录;同一目录重挂由 service registry 幂等
/// (返回既有 handle)。
async fn attach_project_route(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    payload: Result<axum::Json<AttachProjectRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let csrf = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Err(problem) = authorize(&state, &headers, csrf.as_deref(), true) {
        return problem_response(&problem).into_response();
    }
    let request = match payload {
        Ok(axum::Json(request)) => request,
        Err(rejection) => {
            return problem_response(&Problem::new(
                ProblemCode::InvalidEnvelope,
                rejection.body_text(),
                Some(Retry::Never),
            ))
        }
    };
    let root = std::path::PathBuf::from(request.path.trim_end_matches(['/', '\\']));
    if !root.is_dir() {
        return problem_response(&Problem::new(
            ProblemCode::ValidationFailed,
            format!("项目目录不存在或不是目录:{}", root.display()),
            Some(Retry::Never),
        ));
    }
    match state.kernel.attach_project(&root) {
        Ok(handle) => {
            // 执行面装配(#75):失败不回滚数据面挂载(快照可见),
            // 错误进入响应供 UI 提示。
            let execution_error = state
                .on_project_attached
                .as_ref()
                .and_then(|hook| hook(&handle, &root).err());
            let body = serde_json::json!({
                "schema": "mf.project-attach.v1",
                "project": handle,
                "display_name": root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Project"),
                "execution": if execution_error.is_some() { "unavailable" } else { "ready" },
                "execution_error": execution_error,
            });
            let mut headers = security(&state);
            headers.push((header_name("content-type"), "application/json".into()));
            respond(StatusCode::CREATED, headers, body.to_string().into_bytes()).into_response()
        }
        Err(problem) => problem_response(&Problem::new(
            ProblemCode::ServiceUnavailable,
            format!("项目挂载失败:{problem}"),
            Some(Retry::Never),
        ))
        .into_response(),
    }
}

/// `DELETE /api/v1/projects/{handle}`:卸载项目(Controller-only)。
async fn detach_project_route(
    State(state): State<Arc<WorkbenchState>>,
    headers: HeaderMap,
    axum::extract::Path(handle): axum::extract::Path<String>,
) -> impl IntoResponse {
    let csrf = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Err(problem) = authorize(&state, &headers, csrf.as_deref(), true) {
        return problem_response(&problem);
    }
    match state.kernel.detach_project(&handle) {
        Ok(()) => {
            let body = serde_json::json!({ "schema": "mf.project-detach.v1", "project": handle });
            let mut headers = security(&state);
            headers.push((header_name("content-type"), "application/json".into()));
            respond(StatusCode::OK, headers, body.to_string().into_bytes())
        }
        Err(problem) => problem_response(&Problem::new(
            ProblemCode::ResourceNotFound,
            format!("项目卸载失败:{problem}"),
            Some(Retry::Never),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use mf_kernel::kernel::{KernelCommandRequest, KernelOutcome, KernelProblem};
    use mf_kernel::projection::{EventCursor, SnapshotEnvelope, SnapshotQuery};
    use mf_kernel::shutdown::{ShutdownAssessment, ShutdownIntent};
    use mf_terminal::TerminalChannel;
    use parking_lot::Mutex as PlMutex;
    use std::sync::Mutex as StdMutex;

    /// 裁决面真实的 fake kernel:grant/epoch/controller 复验照 kernel
    /// 语义实现;dispatch 成功返回固定 Applied。
    struct FakeKernel {
        lease: StdMutex<(u64, String, String)>,
    }

    impl FakeKernel {
        fn new() -> Self {
            Self {
                lease: StdMutex::new((0, String::new(), String::new())),
            }
        }
    }

    impl CoreKernel for FakeKernel {
        fn dispatch(&self, request: KernelCommandRequest) -> Result<KernelOutcome, KernelProblem> {
            let guard = self.lease.lock().unwrap();
            let (epoch, client, principal) = &*guard;
            let ok = request.controller_epoch() == *epoch
                && request.client_id().as_str() == client.as_str()
                && request.principal().as_str() == principal.as_str();
            drop(guard);
            if !ok {
                return Err(KernelProblem::ControllerLeaseExpired);
            }
            Ok(KernelOutcome::Applied {
                revisions: mf_kernel::projection::RevisionVector {
                    semantic_revision: 1,
                    presentation_revision: 1,
                },
                replayed: false,
            })
        }
        fn snapshot(&self, _query: SnapshotQuery) -> Result<SnapshotEnvelope, KernelProblem> {
            Err(KernelProblem::ServiceUnavailable("fake".into()))
        }
        fn subscribe_events(
            &self,
            _cursor: EventCursor,
        ) -> Result<mf_kernel::projection::EventSubscription, KernelProblem> {
            Err(KernelProblem::ServiceUnavailable("fake".into()))
        }
        fn attach_terminal(
            &self,
            _session: mf_kernel::handles::SessionHandle,
            _attach: mf_kernel::kernel::TerminalAttach,
        ) -> Result<TerminalChannel, KernelProblem> {
            Err(KernelProblem::ServiceUnavailable("fake".into()))
        }
        fn shutdown(&self, _intent: ShutdownIntent) -> ShutdownAssessment {
            ShutdownAssessment::default()
        }
        fn grant_controller(&self, client_id: &str, principal: &str) -> Result<u64, KernelProblem> {
            let mut guard = self.lease.lock().unwrap();
            guard.0 += 1;
            guard.1 = client_id.to_string();
            guard.2 = principal.to_string();
            Ok(guard.0)
        }
        fn controller_epoch(&self) -> u64 {
            self.lease.lock().unwrap().0
        }
        fn attach_project(&self, root: &std::path::Path) -> Result<String, KernelProblem> {
            // 测试面:目录必须存在;handle 由路径派生保持稳定
            if !root.is_dir() {
                return Err(KernelProblem::ServiceUnavailable(format!(
                    "目录不存在:{}",
                    root.display()
                )));
            }
            Ok(format!(
                "proj_{}",
                root.display().to_string().len().to_string()
            ))
        }
        fn detach_project(&self, _project_handle: &str) -> Result<(), KernelProblem> {
            Ok(())
        }
    }

    fn test_state(kernel: Arc<dyn CoreKernel>) -> Arc<WorkbenchState> {
        Arc::new(WorkbenchState {
            bind_ip: "127.0.0.1".into(),
            port: 80,
            auth: PlMutex::new(BootstrapAuth::new(WebLimits::default())),
            dist_root: PathBuf::from("."),
            kernel,
            acceptance: true,
            on_project_attached: None,
            terminal_host: None,
        })
    }

    fn exchange_with_grant(state: &WorkbenchState) -> WebSession {
        let nonce = state.auth.lock().issue_nonce();
        let session = state.auth.lock().exchange(&nonce, "127.0.0.1:80").unwrap();
        grant_web_controller(state.kernel.as_ref(), &session.client_id).unwrap();
        session
    }

    fn auth_headers(session: &WebSession, csrf: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            HeaderValue::from_static("127.0.0.1"),
        );
        headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1"),
        );
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_str(&format!("mf_session={}", session.session_id)).unwrap(),
        );
        if let Some(csrf) = csrf {
            headers.insert("x-csrf-token", HeaderValue::from_str(csrf).unwrap());
        }
        headers
    }

    #[test]
    fn exchange_grant_makes_kernel_controller_and_rotates_epoch() {
        let state = test_state(Arc::new(FakeKernel::new()));
        let first = exchange_with_grant(&state);
        assert_eq!(state.kernel.controller_epoch(), 1);
        // 第二次 bootstrap:epoch 旋转,旧 controller 失效,角色降 Observer
        let second = exchange_with_grant(&state);
        assert_eq!(state.kernel.controller_epoch(), 2);
        let demoted = state.auth.lock().verify(&first.session_id, None).unwrap();
        assert_eq!(demoted.role, SessionRole::Observer);
        let current = state.auth.lock().verify(&second.session_id, None).unwrap();
        assert_eq!(current.role, SessionRole::Controller);
    }

    #[test]
    fn authorize_rejects_observer_and_bad_csrf() {
        let state = test_state(Arc::new(FakeKernel::new()));
        let first = exchange_with_grant(&state);
        let _second = exchange_with_grant(&state);

        // 旧会话已降 Observer:写路径拒绝
        let problem = authorize(
            &state,
            &auth_headers(&first, Some(&first.csrf_token)),
            Some(&first.csrf_token),
            true,
        )
        .unwrap_err();
        assert_eq!(problem.code, ProblemCode::ControllerRequired);

        // Controller 会话但 CSRF 错误 → csrf_rejected
        let problem = authorize(
            &state,
            &auth_headers(&_second, Some("wrong")),
            Some("wrong"),
            true,
        )
        .unwrap_err();
        assert_eq!(problem.code, ProblemCode::CsrfRejected);
    }

    #[test]
    fn command_core_rejects_stale_epoch_and_identity_mismatch() {
        let state = test_state(Arc::new(FakeKernel::new()));
        let session = exchange_with_grant(&state);
        let project = mf_kernel::handles::ProjectStoreHandle::generate();
        // 合法 UUIDv7(裸形态;wire 前缀 wf_ 由 translate strip)
        let workflow = "018f1e2d-3c4b-7a69-8e8f-9a0b1c2d3e4f";
        let envelope = CommandEnvelope::new(
            &mf_kernel::handles::CommandId::new().as_str(),
            &session.client_id,
            1,
            crate::api::commands::AggregateRef {
                kind: "project".into(),
                handle: project.as_str().to_string(),
            },
            crate::api::commands::CommandType::WorkflowRename,
            serde_json::json!({
                "workflow_handle": format!("wf_{workflow}"),
                "name": "改名",
            }),
        );
        // 期望轴在 expected 上:构造完整 envelope
        let mut envelope = envelope;
        envelope.expected = vec![crate::api::commands::ExpectedRevision {
            aggregate: crate::api::commands::AggregateRef {
                kind: "project_workflow".into(),
                handle: format!("wf_{workflow}"),
            },
            presentation_revision: Some("1".into()),
            semantic_revision: None,
        }];
        // kernel epoch=1,client/principal 一致 → Applied
        assert!(command_core(&state, &session, &envelope).is_ok());

        // 陈旧 epoch → kernel ControllerLeaseExpired(controller_lease_expired)
        exchange_with_grant(&state);
        let problem = command_core(&state, &session, &envelope).unwrap_err();
        assert_eq!(problem.code, ProblemCode::ControllerLeaseExpired);

        // envelope client_id 与 session 不符 → invalid_envelope
        let mut forged = envelope.clone();
        forged.client_id = "cl_other".into();
        let problem = command_core(&state, &session, &forged).unwrap_err();
        assert_eq!(problem.code, ProblemCode::InvalidEnvelope);
    }

    #[test]
    fn project_attach_requires_controller_and_existing_directory() {
        let state = test_state(Arc::new(FakeKernel::new()));
        let controller = exchange_with_grant(&state);
        let observer_session = {
            // 第二次 bootstrap 使 controller 降 Observer
            let nonce = state.auth.lock().issue_nonce();
            let _new = state.auth.lock().exchange(&nonce, "127.0.0.1:80").unwrap();
            controller
        };
        let demoted = state
            .auth
            .lock()
            .verify(&observer_session.session_id, None)
            .unwrap();
        assert_eq!(demoted.role, SessionRole::Observer);

        // Observer 挂载 → controller_required
        let problem = authorize(
            &state,
            &auth_headers(&observer_session, Some(&observer_session.csrf_token)),
            Some(&observer_session.csrf_token),
            true,
        )
        .unwrap_err();
        assert_eq!(problem.code, ProblemCode::ControllerRequired);

        // 目录不存在 → fake kernel 拒绝(ServiceUnavailable 映射)
        let missing = std::path::Path::new("Z:/definitely/not/here");
        let problem = state.kernel.attach_project(missing).unwrap_err();
        assert!(matches!(problem, KernelProblem::ServiceUnavailable(_)));

        // 存在的目录 → handle 返回(fake 以路径长度派生)
        let tmp = tempfile::tempdir().unwrap();
        let handle = state.kernel.attach_project(tmp.path()).unwrap();
        assert!(handle.starts_with("proj_"));
        state.kernel.detach_project(&handle).unwrap();
    }

    #[test]
    fn fs_browsing_filters_hidden_and_reports_parent() {
        // 过滤规则:隐藏/系统目录不进入浏览面
        assert!(!browsable_dir_name(""));
        assert!(!browsable_dir_name(".hidden"));
        assert!(!browsable_dir_name("$RECYCLE.BIN"));
        assert!(!browsable_dir_name("System Volume Information"));
        assert!(browsable_dir_name("workspace"));
        assert!(browsable_dir_name("我的项目"));

        // parent 语义:根/盘符无 parent,子目录有
        assert_eq!(parent_of(std::path::Path::new("C:\\")), None);
        let parent = parent_of(std::path::Path::new("C:\\Users\\dev"));
        assert_eq!(parent.as_deref(), Some("C:\\Users"));
    }

    #[test]
    fn fs_dirs_listing_only_contains_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join("file.txt"), "x").unwrap();
        // 直接复用 handler 的核心读取逻辑(read_dir + 过滤)
        let mut names = Vec::new();
        for entry in std::fs::read_dir(tmp.path()).unwrap().flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if browsable_dir_name(&name) {
                names.push(name);
            }
        }
        assert_eq!(names, vec!["alpha".to_string()], "文件与隐藏目录被过滤");
    }

    #[test]
    fn takeover_cas_grants_new_epoch_and_promotes_role() {
        let state = test_state(Arc::new(FakeKernel::new()));
        let first = exchange_with_grant(&state);
        let second = exchange_with_grant(&state); // epoch=2,first 降 Observer

        // 陈旧观察 → 409 语义(controller_lease_expired + current)
        let problem = takeover_core(&state, &first, 1).unwrap_err();
        assert_eq!(problem.code, ProblemCode::ControllerLeaseExpired);
        assert!(problem.current.is_some());

        // 正确 CAS → epoch=3,first 会话升 Controller,second 降 Observer
        let epoch = takeover_core(&state, &first, 2).unwrap();
        assert_eq!(epoch, 3);
        assert_eq!(state.kernel.controller_epoch(), 3);
        let promoted = state.auth.lock().verify(&first.session_id, None).unwrap();
        assert_eq!(promoted.role, SessionRole::Controller);
        let demoted = state.auth.lock().verify(&second.session_id, None).unwrap();
        assert_eq!(demoted.role, SessionRole::Observer);
    }
}
