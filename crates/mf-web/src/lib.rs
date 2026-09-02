//! mf-web:WebGateway 外壳(T7a,Issue #38;canonical spec §6)。
//!
//! 交付可独立安全测试的 Gateway 判定层与 axum 骨架:仅 loopback 随机
//! 端口、Host/Origin 严格校验(拒绝 public origin/DNS rebinding/LNA
//! 权限作为认证)、一次性 bootstrap nonce(URL fragment 携带,URL/query
//! 永不出现凭据)→ HttpOnly session + 内存 CSRF/client id、CSP/COOP/
//! CORP 等安全头、内容哈希命名的内嵌 assets(无 CDN)。领域路由、用户
//! 可达 bootstrap 与 Workbench 属后续 ticket;Core 重启使全部 Web auth
//! 状态失效。

pub mod api;
pub mod assets;
pub mod auth;
pub mod gateway;
pub mod headers;
pub mod limits;
pub mod problem;
