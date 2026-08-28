//! 版本化 NDJSON worker 协议验收:协议版本拒绝不匹配、请求/响应封套、敏感值脱敏。

use mf_plugins::worker_protocol::{
    ensure_matches, redact_sensitive, redact_text, WorkerRequest, WorkerResponse,
    WORKER_PROTOCOL_VERSION,
};
use serde_json::json;

#[test]
fn rejects_response_from_another_protocol() {
    let response = r#"{"protocol":2,"id":1,"result":{}}"#;
    assert!(WorkerResponse::parse_for(1, response).is_err());
}

#[test]
fn request_response_roundtrip() {
    let req = WorkerRequest::new(7, "echo", "mft_token", json!({ "msg": "hi" }));
    assert_eq!(req.protocol, WORKER_PROTOCOL_VERSION);
    assert_eq!(req.capability_token, "mft_token");
    let line = req.to_line().unwrap();
    let parsed: WorkerRequest = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(parsed.id, 7);
    assert_eq!(parsed.method, "echo");

    let resp = WorkerResponse::parse_for(
        WORKER_PROTOCOL_VERSION,
        r#"{"protocol":1,"id":7,"result":{"ok":true}}"#,
    )
    .unwrap();
    assert_eq!(resp.id, 7);
    assert!(resp.is_ok());
    assert!(ensure_matches(WORKER_PROTOCOL_VERSION, 7, &resp).is_ok());
    // id 不匹配拒绝
    assert!(ensure_matches(WORKER_PROTOCOL_VERSION, 8, &resp).is_err());
    // 错误响应可解析且 is_ok() 为 false
    let err = WorkerResponse::parse_for(
        WORKER_PROTOCOL_VERSION,
        r#"{"protocol":1,"id":7,"error":"boom"}"#,
    )
    .unwrap();
    assert!(!err.is_ok());
    // 缺字段的行拒绝
    assert!(WorkerResponse::parse_for(WORKER_PROTOCOL_VERSION, "not json").is_err());
}

#[test]
fn redacts_sensitive_values_by_key() {
    let value = json!({
        "api_key": "sk-123",
        "nested": { "auth_token": "abc", "keep": 1 },
        "list": [ { "password": "p" } ],
        "mode": "fast"
    });
    let red = redact_sensitive(&value);
    assert_eq!(red["api_key"], "[redacted]");
    assert_eq!(red["nested"]["auth_token"], "[redacted]");
    assert_eq!(red["nested"]["keep"], 1);
    assert_eq!(red["list"][0]["password"], "[redacted]");
    assert_eq!(red["mode"], "fast");
    // 大小写不敏感
    let v2 = json!({ "Api-Key": "x", "MY_SECRET": "y" });
    let r2 = redact_sensitive(&v2);
    assert_eq!(r2["Api-Key"], "[redacted]");
    assert_eq!(r2["MY_SECRET"], "[redacted]");
}

#[test]
fn redacts_plain_text_diagnostics() {
    // JSON 行按键脱敏
    let json_line = r#"{"token":"mft_secret","level":"info"}"#;
    assert!(redact_text(json_line).contains("[redacted]"));
    assert!(!redact_text(json_line).contains("mft_secret"));
    // key=value 文本行脱敏
    let kv = "worker failed: token=mft_secret password=hunter2 mode=fast";
    let out = redact_text(kv);
    assert!(!out.contains("mft_secret"));
    assert!(!out.contains("hunter2"));
    assert!(out.contains("mode=fast"));
    assert!(out.contains("[redacted]"));
}
