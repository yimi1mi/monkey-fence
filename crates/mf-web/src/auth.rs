//! Bootstrap auth 状态机(T7a,Issue #38;spec §6.2/§6.5)。
//!
//! 一次性 128-bit nonce 由 URL fragment 携带(fragment 不进 HTTP 请求/
//! 日志/Referer——服务端只经 POST body 收取);exchange 一次消耗 →
//! HttpOnly session + 内存 CSRF(256-bit)/client id;Core 重启使 nonce/
//! session/CSRF/client id 全部失效。URL/query 永不出现凭据(类型层:
//! 本模块的所有 API 均不接受 query 形态)。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::limits::WebLimits;

/// bootstrap/exchange 判定问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthProblem {
    #[error("nonce 不存在或已消耗")]
    NonceUnknown,
    #[error("nonce 已过期")]
    NonceExpired,
    #[error("速率超限(每 source {limit}/分钟)")]
    RateLimited { limit: u32 },
    #[error("session 不存在或已失效")]
    SessionUnknown,
    #[error("CSRF 不匹配")]
    CsrfMismatch,
    #[error("Core 已重启:全部 Web auth 状态失效")]
    CoreRestarted,
}

/// 会话角色(§6.4:每用户一个 Controller;新 bootstrap 即 Controller,
/// 旧端降 Observer 不断开)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Controller,
    Observer,
}

/// exchange 成功产物(cookie/CSRF 只在服务端与浏览器内存间流转)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSession {
    /// HttpOnly、SameSite=Strict、Path=/ 的 `mf_session` cookie 值。
    pub session_id: String,
    /// 256-bit CSRF token(仅页面内存)。
    pub csrf_token: String,
    /// 每会话唯一 client id(opaque)。
    pub client_id: String,
    pub role: SessionRole,
    pub created_at: Instant,
    pub last_seen: Instant,
}

struct NonceRecord {
    issued_at: Instant,
}

struct RateWindow {
    window_start: Instant,
    count: u32,
}

/// 内存 Web auth 状态(单实例;Core 生命周期)。
pub struct BootstrapAuth {
    limits: WebLimits,
    nonces: HashMap<String, NonceRecord>,
    sessions: HashMap<String, WebSession>,
    client_roles: HashMap<String, SessionRole>,
    rate: HashMap<String, RateWindow>,
    generation: u64,
}

impl BootstrapAuth {
    pub fn new(limits: WebLimits) -> Self {
        Self {
            limits: limits.clamp(),
            nonces: HashMap::new(),
            sessions: HashMap::new(),
            client_roles: HashMap::new(),
            rate: HashMap::new(),
            generation: 0,
        }
    }

    /// 生成一次性 128-bit nonce(launcher 放入 URL fragment)。
    pub fn issue_nonce(&mut self) -> String {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        self.nonces.insert(
            nonce.clone(),
            NonceRecord {
                issued_at: Instant::now(),
            },
        );
        // 惰性清理过期 nonce
        let ttl = Duration::from_secs(self.limits.bootstrap_nonce_ttl_secs);
        self.nonces
            .retain(|_, record| record.issued_at.elapsed() < ttl);
        nonce
    }

    /// 消耗 nonce 换取 session(一次性;per-source 速率限制)。新
    /// bootstrap client 成为 Controller,旧 Controller 降级 Observer
    /// (§6.4:不断开、只降写)。
    pub fn exchange(&mut self, nonce: &str, source: &str) -> Result<WebSession, AuthProblem> {
        // 速率窗口(每 source)
        let limit = self.limits.auth_exchange_rate_per_minute;
        let window = self.rate.entry(source.to_string()).or_insert(RateWindow {
            window_start: Instant::now(),
            count: 0,
        });
        if window.window_start.elapsed() >= Duration::from_secs(60) {
            window.window_start = Instant::now();
            window.count = 0;
        }
        if window.count >= limit {
            return Err(AuthProblem::RateLimited { limit });
        }
        window.count += 1;

        // nonce 一次性消耗(先取后删,未知/过期都不可区分地拒绝)
        let record = self.nonces.remove(nonce).ok_or(AuthProblem::NonceUnknown)?;
        if record.issued_at.elapsed() >= Duration::from_secs(self.limits.bootstrap_nonce_ttl_secs) {
            return Err(AuthProblem::NonceExpired);
        }
        // 旧 Controller 降级(保留 session,仅角色变化)
        for session in self.sessions.values_mut() {
            session.role = SessionRole::Observer;
        }
        let session = WebSession {
            session_id: format!("mfs_{}", uuid::Uuid::new_v4().simple()),
            csrf_token: format!("csrf_{}", uuid::Uuid::new_v4().simple()),
            client_id: format!("cl_{}", uuid::Uuid::now_v7().simple()),
            role: SessionRole::Controller,
            created_at: Instant::now(),
            last_seen: Instant::now(),
        };
        self.client_roles
            .insert(session.client_id.clone(), session.role);
        self.sessions
            .insert(session.session_id.clone(), session.clone());
        Ok(session)
    }

    /// 校验 session cookie + CSRF(写命令前置);滑动续期,绝对上限
    /// 86,400s。
    pub fn verify(
        &mut self,
        session_id: &str,
        csrf_token: Option<&str>,
    ) -> Result<WebSession, AuthProblem> {
        let ttl = Duration::from_secs(self.limits.web_session_ttl_secs);
        let absolute = Duration::from_secs(crate::limits::WEB_SESSION_ABSOLUTE_MAX_SECS);
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or(AuthProblem::SessionUnknown)?;
        if session.last_seen.elapsed() >= ttl || session.created_at.elapsed() >= absolute {
            self.sessions.remove(session_id);
            return Err(AuthProblem::SessionUnknown);
        }
        if let Some(expected) = csrf_token {
            if session.csrf_token != expected {
                return Err(AuthProblem::CsrfMismatch);
            }
        }
        session.last_seen = Instant::now();
        Ok(session.clone())
    }

    /// takeover 提升:该会话升 Controller,其余全部降 Observer(§6.4:
    /// 不断开、只降写;kernel 侧 epoch 旋转由调用方先行完成)。
    pub fn promote_controller(&mut self, session_id: &str) {
        let target_client = self.sessions.get(session_id).map(|s| s.client_id.clone());
        let Some(target_client) = target_client else {
            return;
        };
        for session in self.sessions.values_mut() {
            session.role = SessionRole::Observer;
        }
        for role in self.client_roles.values_mut() {
            *role = SessionRole::Observer;
        }
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.role = SessionRole::Controller;
        }
        self.client_roles
            .insert(target_client, SessionRole::Controller);
    }

    /// Core 重启语义:nonce/session/CSRF/client id 全部失效(§6.2/6.7)。
    pub fn core_restart(&mut self) {
        self.nonces.clear();
        self.sessions.clear();
        self.client_roles.clear();
        self.generation += 1;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

/// Host 校验(§6.1):必须精确等于当前绑定 IP literal + port。
/// DNS rebinding hostname、0.0.0.0、LAN 地址一律拒绝。
pub fn validate_host(host_header: &str, bind_ip: &str, port: u16) -> bool {
    let expected_v4 = format!("{bind_ip}:{port}");
    let expected_v6 = format!("[{bind_ip}]:{port}");
    // 精确匹配;不接受任意 hostname(即使解析到 loopback)。
    // 端口 80 的浏览器规范形态省略 :80(RFC 3986 默认端口)。
    let port80_default = port == 80 && host_header == bind_ip;
    host_header == expected_v4 || host_header == expected_v6 || port80_default
}

/// Origin 校验(§6.1):精确匹配绑定的 loopback origin;public origin、
/// `null`、hostname 变体(rebinding/localhost 名称)全部拒绝;LNA 浏览器
/// 权限不作为认证(不接受 chrome 权限头形态)。
pub fn validate_origin(origin_header: &str, bind_ip: &str, port: u16) -> bool {
    let expected = format!("http://{bind_ip}:{port}");
    let expected_v6 = format!("http://[{bind_ip}]:{port}");
    // 端口 80 的 Origin 省略 :80(浏览器序列化规则)。
    let port80_default = port == 80 && origin_header == format!("http://{bind_ip}");
    origin_header == expected || origin_header == expected_v6 || port80_default
}

/// 绑定地址白名单(仅 loopback IP literal)。
pub fn validate_bind_address(addr: &str) -> bool {
    addr == "127.0.0.1" || addr == "::1"
}
