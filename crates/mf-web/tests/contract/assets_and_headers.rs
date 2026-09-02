//! T7a 契约(Issue #38):内嵌 assets(hash 命名/无 CDN)与 gateway
//! 请求级安全头/Host/Origin 强制。

use mf_web::assets::{AssetRegistry, EmbeddedAsset};
use mf_web::auth::BootstrapAuth;
use mf_web::gateway::request_host_origin_ok;
use mf_web::headers::security_headers;
use mf_web::limits::{WebLimits, CSRF_ENTROPY_BITS, WEB_SESSION_ABSOLUTE_MAX_SECS};
use std::sync::{Arc, Mutex};

fn test_assets() -> AssetRegistry {
    AssetRegistry::new(vec![
        EmbeddedAsset {
            name: "index.html",
            content_type: "text/html; charset=utf-8",
            bytes: b"<html><script src=\"./app.js\"></script></html>",
        },
        EmbeddedAsset {
            name: "app.js",
            content_type: "text/javascript",
            bytes: b"console.log('embedded');",
        },
    ])
}

#[test]
fn assets_are_hash_named_and_indexed() {
    let registry = test_assets();
    let index = registry.index_hash_name();
    assert!(index.starts_with("index."), "内容哈希命名:{index}");
    assert!(index.ends_with(".html"));
    assert!(registry.by_hash_name(&index).is_some());
    assert!(
        registry.by_hash_name("index.html").is_none(),
        "原名不可寻址"
    );
    // 同内容稳定哈希
    let again = test_assets();
    assert_eq!(again.index_hash_name(), index);
}

#[test]
fn assets_audit_rejects_external_references() {
    assert!(test_assets().audit_no_external_references().is_ok());
    let cdn = AssetRegistry::new(vec![EmbeddedAsset {
        name: "evil.js",
        content_type: "text/javascript",
        bytes: b"fetch('https://cdn.example.com/lib.js')",
    }]);
    assert!(cdn.audit_no_external_references().is_err());
}

#[tokio::test]
async fn gateway_binds_loopback_only() {
    // LAN 地址拒绝
    assert!(mf_web::gateway::bind_gateway("0.0.0.0", test_assets())
        .await
        .is_err());
    assert!(mf_web::gateway::bind_gateway("192.168.1.5", test_assets())
        .await
        .is_err());
    // loopback 绑定成功且端口随机
    let bound = mf_web::gateway::bind_gateway("127.0.0.1", test_assets())
        .await
        .unwrap();
    assert_eq!(bound.addr.ip().to_string(), "127.0.0.1");
    let second = mf_web::gateway::bind_gateway("127.0.0.1", test_assets())
        .await
        .unwrap();
    assert_ne!(bound.addr.port(), second.addr.port(), "随机端口");
}

#[tokio::test]
async fn request_level_host_origin_enforcement() {
    let bound = mf_web::gateway::bind_gateway("127.0.0.1", test_assets())
        .await
        .unwrap();
    let port = bound.addr.port();
    let state = mf_web_test_state(port);
    let mut headers = axum::http::HeaderMap::new();
    // 正确 Host,无 Origin(asset 类只查 Host)
    headers.insert(
        axum::http::header::HOST,
        axum::http::HeaderValue::from_str(&format!("127.0.0.1:{port}")).unwrap(),
    );
    assert!(request_host_origin_ok(&state, &headers, false).is_ok());
    // 写路径要求精确 Origin:缺失拒绝
    assert_eq!(
        request_host_origin_ok(&state, &headers, true).unwrap_err(),
        axum::http::StatusCode::FORBIDDEN
    );
    // public Origin 拒绝
    headers.insert(
        axum::http::header::ORIGIN,
        axum::http::HeaderValue::from_static("https://evil.example.com"),
    );
    assert_eq!(
        request_host_origin_ok(&state, &headers, true).unwrap_err(),
        axum::http::StatusCode::FORBIDDEN
    );
    // DNS rebinding Host 拒绝
    headers.insert(
        axum::http::header::HOST,
        axum::http::HeaderValue::from_static("evil.example.com"),
    );
    assert_eq!(
        request_host_origin_ok(&state, &headers, false).unwrap_err(),
        axum::http::StatusCode::FORBIDDEN
    );
}

fn mf_web_test_state(port: u16) -> mf_web::gateway::GatewayState {
    mf_web::gateway::GatewayState {
        bind_ip: "127.0.0.1".into(),
        port,
        auth: Mutex::new(BootstrapAuth::new(WebLimits::default())),
        assets: test_assets(),
    }
}

#[test]
fn header_golden_minimum_set_present() {
    let headers = security_headers("127.0.0.1", 8123, false);
    let names: Vec<&str> = headers.iter().map(|(n, _)| *n).collect();
    for expected in [
        "Content-Security-Policy",
        "Cross-Origin-Opener-Policy",
        "Cross-Origin-Resource-Policy",
        "X-Content-Type-Options",
        "Referrer-Policy",
        "Permissions-Policy",
        "Cache-Control",
    ] {
        assert!(names.contains(&expected), "缺安全头:{expected}");
    }
}

#[test]
fn limits_defaults_and_clamp_match_appendix_a3() {
    let defaults = WebLimits::default();
    assert_eq!(defaults.bootstrap_nonce_ttl_secs, 120);
    assert_eq!(defaults.web_session_ttl_secs, 43_200);
    assert_eq!(defaults.auth_exchange_rate_per_minute, 10);
    assert_eq!(CSRF_ENTROPY_BITS, 256, "csrf entropy fixed 256");
    assert_eq!(WEB_SESSION_ABSOLUTE_MAX_SECS, 86_400);
    let clamped = WebLimits {
        bootstrap_nonce_ttl_secs: 1,
        web_session_ttl_secs: 1,
        auth_exchange_rate_per_minute: 1,
    }
    .clamp();
    assert_eq!(clamped.bootstrap_nonce_ttl_secs, 30);
    assert_eq!(clamped.web_session_ttl_secs, 600);
    assert_eq!(clamped.auth_exchange_rate_per_minute, 5);
}

#[test]
fn no_node_runtime_constant() {
    assert!(mf_web::assets::NO_NODE_RUNTIME);
}
