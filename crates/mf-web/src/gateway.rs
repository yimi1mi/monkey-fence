//! Gateway axum 骨架(T7a,Issue #38;spec §6.1/§6.2)。
//!
//! 仅 loopback IP literal 随机端口;每请求 Host 精确校验(防 DNS
//! rebinding);/auth/exchange 经 POST body 收 nonce(fragment 由首屏
//! JS 读取,URL/query 无凭据);安全头 middleware;hash assets 内嵌。
//! 领域路由(snapshot/commands/events WS)在 #41/#42 接入。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;

use crate::assets::AssetRegistry;
use crate::auth::{validate_host, validate_origin, AuthProblem, BootstrapAuth};
use crate::headers::security_headers;
use crate::limits::WebLimits;

pub struct GatewayState {
    pub bind_ip: String,
    pub port: u16,
    pub auth: Mutex<BootstrapAuth>,
    pub assets: AssetRegistry,
}

/// 已绑定信息(loopback IP literal + 随机端口)。
pub struct BoundGateway {
    pub addr: SocketAddr,
    pub router: Router,
}

/// 绑定 loopback 随机端口并构建 Router(仅 127.0.0.1/::1;§6.1)。
pub async fn bind_gateway(bind_ip: &str, assets: AssetRegistry) -> anyhow::Result<BoundGateway> {
    anyhow::ensure!(
        crate::auth::validate_bind_address(bind_ip),
        "仅允许绑定 127.0.0.1/::1(拒绝 LAN/0.0.0.0):{bind_ip}"
    );
    let listener = tokio::net::TcpListener::bind(format!("{bind_ip}:0")).await?;
    let port = listener.local_addr()?.port();
    let state = Arc::new(GatewayState {
        bind_ip: bind_ip.to_string(),
        port,
        auth: Mutex::new(BootstrapAuth::new(WebLimits::default())),
        assets,
    });
    let router = build_router(state.clone());
    // serve 由调用方驱动(测试用 axum serve;#41 接入正式生命周期)
    Ok(BoundGateway {
        addr: listener.local_addr()?,
        router,
    })
}

fn build_router(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/auth/exchange", post(auth_exchange))
        .route("/assets/{name}", get(asset))
        .with_state(state)
}

/// 统一响应构造:状态 + (HeaderName, value) 对 + body。
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

fn text_header(name: &'static str) -> axum::http::HeaderName {
    axum::http::HeaderName::from_static(name)
}

async fn index(State(state): State<Arc<GatewayState>>) -> impl IntoResponse {
    let mut headers: Vec<(axum::http::HeaderName, String)> =
        security_headers(&state.bind_ip, state.port, false)
            .into_iter()
            .map(|(n, v)| (text_header(n), v))
            .collect();
    headers.push((
        axum::http::header::LOCATION,
        format!("/assets/{}", state.assets.index_hash_name()),
    ));
    respond(StatusCode::SEE_OTHER, headers, Vec::new())
}

/// Host/Origin 双校验 middleware 语义(在 handler 前统一判定)。
pub fn request_host_origin_ok(
    state: &GatewayState,
    headers: &HeaderMap,
    require_origin: bool,
) -> Result<(), StatusCode> {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !validate_host(host, &state.bind_ip, state.port) {
        // 403:防 DNS rebinding(域名/0.0.0.0/LAN 全拒)
        return Err(StatusCode::FORBIDDEN);
    }
    if require_origin {
        let origin = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !validate_origin(origin, &state.bind_ip, state.port) {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct ExchangeRequest {
    nonce: String,
}

async fn auth_exchange(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<ExchangeRequest>,
) -> impl IntoResponse {
    if let Err(status) = request_host_origin_ok(&state, &headers, true) {
        return respond(status, Vec::new(), Vec::new());
    }
    // source = 绑定地址(单用户 loopback;速率按来源整体限制)
    let source = format!("{}:{}", state.bind_ip, state.port);
    let mut auth = state.auth.lock().unwrap();
    match auth.exchange(&request.nonce, &source) {
        Ok(session) => {
            let body = serde_json::json!({
                "schema": "mf.auth-bootstrap.v1",
                "client_id": session.client_id,
                "csrf_token": session.csrf_token,
                "controller": { "role": "controller" },
                "api_versions": ["v1"],
                "ws_subprotocols": ["mf-workflow.v1", "mf-terminal.v1"],
                "core_build": env!("CARGO_PKG_VERSION"),
            });
            let mut parts: Vec<(axum::http::HeaderName, String)> =
                security_headers(&state.bind_ip, state.port, false)
                    .into_iter()
                    .map(|(n, v)| (text_header(n), v))
                    .collect();
            parts.push((
                axum::http::header::SET_COOKIE,
                format!(
                    "mf_session={}; HttpOnly; SameSite=Strict; Path=/",
                    session.session_id
                ),
            ));
            respond(StatusCode::OK, parts, body.to_string().into_bytes())
        }
        Err(problem) => auth_problem_response(problem),
    }
}

fn auth_problem_response(problem: AuthProblem) -> axum::response::Response {
    let status = match &problem {
        AuthProblem::NonceUnknown | AuthProblem::NonceExpired => StatusCode::UNAUTHORIZED,
        AuthProblem::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        AuthProblem::SessionUnknown | AuthProblem::CsrfMismatch => StatusCode::UNAUTHORIZED,
        AuthProblem::CoreRestarted => StatusCode::UNAUTHORIZED,
    };
    let body = serde_json::json!({
        "schema": "mf.problem.v1",
        "code": "auth_failed",
        "detail": problem.to_string(),
    });
    respond(status, Vec::new(), body.to_string().into_bytes())
}

async fn asset(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(status) = request_host_origin_ok(&state, &headers, false) {
        return respond(status, Vec::new(), Vec::new());
    }
    match state.assets.by_hash_name(&name) {
        Some(asset) => {
            let mut parts: Vec<(axum::http::HeaderName, String)> =
                security_headers(&state.bind_ip, state.port, true)
                    .into_iter()
                    .map(|(n, v)| (text_header(n), v))
                    .collect();
            parts.push((
                axum::http::header::CONTENT_TYPE,
                asset.content_type.to_string(),
            ));
            respond(StatusCode::OK, parts, asset.bytes.to_vec())
        }
        None => respond(StatusCode::NOT_FOUND, Vec::new(), Vec::new()),
    }
}
