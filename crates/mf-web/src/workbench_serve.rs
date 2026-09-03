//! Workbench 服务器(T11 发布形态;用户验收入口)。
//!
//! 同一 loopback 端口上服务:静态资源(构建产物目录,路径穿越拒绝)、
//! `/auth/exchange`(一次性 nonce → HttpOnly session + CSRF)、
//! `/api/v1/snapshots/workspace`(session 认证后经 kernel 权威快照)。
//! 凭据永不进 URL/query;nonce 由启动方签发并放进 fragment。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use parking_lot::Mutex;

use crate::api::kernel_bridge::snapshot_to_wire;
use crate::auth::{validate_host, validate_origin, BootstrapAuth};
use crate::headers::security_headers;
use crate::limits::WebLimits;
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
        .route("/api/v1/snapshots/workspace", get(workspace_snapshot))
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
            let body = serde_json::json!({
                "schema": "mf.auth-bootstrap.v1",
                "client_id": session.client_id,
                "csrf_token": session.csrf_token,
                "controller": { "role": "controller", "lease_epoch": "1" },
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
