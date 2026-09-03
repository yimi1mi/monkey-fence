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
use axum::routing::{get, post};
use axum::Router;
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
}

/// 绑定并启动 workbench 服务(后台线程持有运行时;进程退出即止)。
/// 返回带一次性 nonce fragment 的入口 URL(浏览器直接打开)。
pub fn serve_workbench(
    kernel: Arc<dyn CoreKernel>,
    dist_root: impl Into<PathBuf>,
    port: u16,
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
    });
    let router = Router::new()
        .route("/", get(index))
        .route("/auth/exchange", post(auth_exchange))
        .route("/auth/session", get(auth_session))
        .route("/api/v1/snapshots/workspace", get(workspace_snapshot))
        .route("/api/v1/commands", post(submit_command))
        .route("/api/v1/controller/takeover", post(controller_takeover))
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
        Err(problem) => problem_response(&problem),
    }
}

#[derive(serde::Deserialize)]
struct TakeoverRequest {
    last_observed_epoch: String,
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
    }

    fn test_state(kernel: Arc<dyn CoreKernel>) -> Arc<WorkbenchState> {
        Arc::new(WorkbenchState {
            bind_ip: "127.0.0.1".into(),
            port: 80,
            auth: PlMutex::new(BootstrapAuth::new(WebLimits::default())),
            dist_root: PathBuf::from("."),
            kernel,
            acceptance: true,
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
