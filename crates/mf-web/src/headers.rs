//! 安全响应头(T7a,Issue #38;spec §6.3 发行版最低集)。

/// 按 §6.3 构造全部安全头(hash asset 允许 immutable 缓存,其余
/// no-store)。
pub fn security_headers(
    bind_ip: &str,
    port: u16,
    immutable_asset: bool,
) -> Vec<(&'static str, String)> {
    let ws_v4 = format!("ws://{bind_ip}:{port}");
    let ws_v6 = format!("ws://[{bind_ip}]:{port}");
    let csp = format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
         font-src 'self'; connect-src 'self' {ws_v4} {ws_v6}; object-src 'none'; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    vec![
        ("Content-Security-Policy", csp),
        ("Cross-Origin-Opener-Policy", "same-origin".into()),
        ("Cross-Origin-Resource-Policy", "same-origin".into()),
        ("X-Content-Type-Options", "nosniff".into()),
        ("Referrer-Policy", "no-referrer".into()),
        (
            "Permissions-Policy",
            "camera=(), microphone=(), geolocation=(), payment=()".into(),
        ),
        (
            "Cache-Control",
            if immutable_asset {
                "public, max-age=31536000, immutable".into()
            } else {
                "no-store".into()
            },
        ),
    ]
}

/// nonce fragment 携带的 bootstrap URL(URL fragment 不进服务器;此
/// 函数只给 launcher 构造入口用)。凭据永不进 query string。
pub fn bootstrap_url(bind_ip: &str, port: u16, nonce: &str) -> String {
    if bind_ip.contains(':') {
        format!("http://[{bind_ip}]:{port}/#nonce={nonce}")
    } else {
        format!("http://{bind_ip}:{port}/#nonce={nonce}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_match_spec_minimum_set() {
        let headers = security_headers("127.0.0.1", 8443, false);
        let csp = headers
            .iter()
            .find(|(name, _)| *name == "Content-Security-Policy")
            .unwrap()
            .1
            .clone();
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("script-src 'self'"));
        assert!(csp.contains("connect-src 'self' ws://127.0.0.1:8443"));
        assert!(csp.contains("frame-ancestors 'none'"));
        let cache = headers
            .iter()
            .find(|(name, _)| *name == "Cache-Control")
            .unwrap();
        assert_eq!(cache.1, "no-store");
        let immutable = security_headers("127.0.0.1", 8443, true);
        assert!(immutable
            .iter()
            .any(|(n, v)| *n == "Cache-Control" && v.contains("immutable")));
    }

    #[test]
    fn bootstrap_url_keeps_nonce_in_fragment_only() {
        let url = bootstrap_url("127.0.0.1", 9000, "abc123");
        assert!(url.contains('#'), "nonce 必须在 fragment");
        assert!(!url.contains("?"), "URL 不得携带 query 凭据");
    }
}
