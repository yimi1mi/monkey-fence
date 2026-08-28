//! 版本化 NDJSON worker 协议(协议版本 1)。
//!
//! 请求/响应均为单行 JSON(NDJSON);封套显式携带 `protocol` 版本,
//! 不匹配即拒绝。诊断文本入库前按敏感 key 脱敏。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const WORKER_PROTOCOL_VERSION: i64 = 1;
/// stderr / 诊断日志缓冲上限(行)。
pub const STDERR_LOG_LIMIT: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub protocol: i64,
    pub id: i64,
    pub method: String,
    /// 一次性能力令牌:仅对当前 Agent Run 有效,worker 不得持久化。
    #[serde(default)]
    pub capability_token: String,
    #[serde(default)]
    pub params: Value,
}

impl WorkerRequest {
    pub fn new(id: i64, method: &str, capability_token: &str, params: Value) -> Self {
        WorkerRequest {
            protocol: WORKER_PROTOCOL_VERSION,
            id,
            method: method.to_string(),
            capability_token: capability_token.to_string(),
            params,
        }
    }

    pub fn to_line(&self) -> Result<String> {
        let mut line = serde_json::to_string(self).context("序列化 worker 请求失败")?;
        line.push('\n');
        Ok(line)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResponse {
    pub protocol: i64,
    pub id: i64,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<String>,
}

impl WorkerResponse {
    /// 按期望协议版本解析;协议不匹配直接拒绝。
    pub fn parse_for(expected_protocol: i64, line: &str) -> Result<WorkerResponse> {
        let resp: WorkerResponse =
            serde_json::from_str(line.trim()).context("worker 响应不是合法 JSON")?;
        if resp.protocol != expected_protocol {
            bail!(
                "worker 协议版本不匹配: 期望 {expected_protocol},收到 {}",
                resp.protocol
            );
        }
        Ok(resp)
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }

    pub fn to_line(&self) -> Result<String> {
        let mut line = serde_json::to_string(self).context("序列化 worker 响应失败")?;
        line.push('\n');
        Ok(line)
    }
}

/// 校验响应与请求匹配(协议版本 + 请求 id)。
pub fn ensure_matches(expected_protocol: i64, expected_id: i64, resp: &WorkerResponse) -> Result<()> {
    if resp.protocol != expected_protocol {
        bail!(
            "worker 协议版本不匹配: 期望 {expected_protocol},收到 {}",
            resp.protocol
        );
    }
    if resp.id != expected_id {
        bail!("worker 响应 id 不匹配: 期望 {expected_id},收到 {}", resp.id);
    }
    Ok(())
}

/// worker 健康状况(heartbeat 响应)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHealth {
    pub alive: bool,
}

/// 敏感 key 判定:key 归一化(小写、`-`/空格视为 `_`)后
/// 包含 token / secret / password / api_key。
fn is_sensitive_key(key: &str) -> bool {
    let low = key.to_lowercase().replace(['-', ' ', '.'], "_");
    low.contains("token") || low.contains("secret") || low.contains("password") || low.contains("api_key")
}

/// 递归脱敏 JSON 值:命中敏感 key 的值替换为 `[redacted]`。
pub fn redact_sensitive(value: &Value) -> Value {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    if is_sensitive_key(k) {
                        Value::String("[redacted]".into())
                    } else {
                        redact_sensitive(v)
                    },
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into(),
        Value::Array(items) => items.iter().map(redact_sensitive).collect(),
        other => other.clone(),
    }
}

/// 诊断文本入库前脱敏:JSON 行按键脱敏;普通文本按 `key=value` / `key: value` 形式脱敏。
pub fn redact_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            return redact_sensitive(&v).to_string();
        }
    }
    redact_key_value_text(text)
}

/// 普通 `key=value` / `key: value` 文本脱敏。
/// 值的边界是下一个空白、逗号、分号或引号;含空白的裸值只遮蔽第一段(已知限制,
/// 结构化数据请走 JSON 分支)。非敏感键的值不整体吞掉,
/// 避免值里内嵌的 `token=...` 逃过脱敏。
fn redact_key_value_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let Some(sep) = rest.find(|c| c == ':' || c == '=') else {
            out.push_str(rest);
            break;
        };
        // 分隔符左侧最近的“词”(字母数字/-/_,UTF-8 安全)
        let left = &rest[..sep];
        let word_start = left
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '-'))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let word = &left[word_start..];
        let sensitive = is_sensitive_key(word);
        if sensitive {
            // 值起点:跳过分隔符后的空白;值终点:下一个空白/逗号/分号/引号
            let after = &rest[sep + 1..];
            let v_skip = after.len() - after.trim_start().len();
            let value_part = &after[v_skip..];
            let v_end = value_part
                .find(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '"')
                .unwrap_or(value_part.len());
            out.push_str(&rest[..sep + 1 + v_skip]);
            out.push_str("[redacted]");
            let consumed = (sep + 1 + v_skip + v_end).min(rest.len());
            rest = &rest[consumed..];
        } else {
            // 只越过分隔符;值留给后续扫描(其中可能内嵌敏感 k=v)
            out.push_str(&rest[..sep + 1]);
            rest = &rest[sep + 1..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_key_match_rules() {
        assert!(is_sensitive_key("capability_token"));
        assert!(is_sensitive_key("api_key"));
        assert!(is_sensitive_key("USER_PASSWORD"));
        assert!(is_sensitive_key("client-secret"));
        assert!(is_sensitive_key("tokens_page"), "包含 token 子串即视为敏感");
        assert!(!is_sensitive_key("mode"));
        assert!(!is_sensitive_key("username"));
    }

    #[test]
    fn plain_text_without_pairs_unchanged() {
        assert_eq!(redact_text("plain log line"), "plain log line");
        assert_eq!(redact_text(""), "");
    }
}
