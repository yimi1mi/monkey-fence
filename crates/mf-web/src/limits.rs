//! Web/auth 数值上限(canonical spec 附录 A3;T7a,Issue #38)。

/// `csrf_entropy_bits` fixed = 256(附录 A3)。
pub const CSRF_ENTROPY_BITS: u32 = 256;

/// Web session 绝对上限 fixed 86,400s(附录 A3)。
pub const WEB_SESSION_ABSOLUTE_MAX_SECS: u64 = 86_400;

/// Web/auth 可配置上限(全部内存态;Core 重启失效)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebLimits {
    /// 一次性 bootstrap nonce 有效期(30–600s)。
    pub bootstrap_nonce_ttl_secs: u64,
    /// session 滑动 TTL(600–86,400s);绝对上限 fixed。
    pub web_session_ttl_secs: u64,
    /// 每 source 的 exchange 速率限制(5–60/min)。
    pub auth_exchange_rate_per_minute: u32,
}

impl Default for WebLimits {
    fn default() -> Self {
        Self {
            bootstrap_nonce_ttl_secs: 120,
            web_session_ttl_secs: 43_200,
            auth_exchange_rate_per_minute: 10,
        }
    }
}

impl WebLimits {
    /// 越界钳制到附录 A3 允许范围。
    pub fn clamp(&self) -> Self {
        Self {
            bootstrap_nonce_ttl_secs: self.bootstrap_nonce_ttl_secs.clamp(30, 600),
            web_session_ttl_secs: self
                .web_session_ttl_secs
                .clamp(600, WEB_SESSION_ABSOLUTE_MAX_SECS),
            auth_exchange_rate_per_minute: self.auth_exchange_rate_per_minute.clamp(5, 60),
        }
    }
}
