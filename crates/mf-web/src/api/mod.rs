//! Web API v1 wire DTO(T7b,Issue #39;spec §7.1/§7.3/§7.4/§7.5)。
//!
//! 跨语言 wire 契约:opaque handle(拒绝 rowid/PID/path/command)、
//! 字符串化 u64(JS 安全)、封闭命令族枚举、canonical digest 幂等与
//! write-only Secret 的 digest 脱敏。handler 副作用属 #41/#42。

pub mod commands;
pub mod events;
pub mod snapshot;

/// opaque handle 前缀(§7.1):`wf_/run_/step_/sess_/inst_/op_/proj_` +
/// UUIDv7 风格。浏览器不能提交任意 path/PID/argv。
pub mod handle {
    pub const PREFIXES: &[&str] = &["wf_", "run_", "step_", "sess_", "inst_", "op_", "proj_"];

    /// 校验 opaque handle 形态(前缀 + 32 hex)。
    pub fn is_valid(handle: &str) -> bool {
        let Some(prefix_len) = PREFIXES
            .iter()
            .find(|p| handle.starts_with(**p))
            .map(|p| p.len())
        else {
            return false;
        };
        let body = &handle[prefix_len..];
        body.len() == 32 && body.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// 校验并返回 handle(非法 → Err,统一映射 resource_not_found)。
    pub fn parse(handle: &str) -> Result<&str, crate::problem::ProblemCode> {
        if is_valid(handle) {
            Ok(handle)
        } else {
            // 存在性差异统一 404:非法 handle 与不存在的资源不可区分
            Err(crate::problem::ProblemCode::ResourceNotFound)
        }
    }
}

/// 字符串化 u64 序列化助手(JS Number 安全上限之上必须字符串)。
pub mod u64_str {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::handle;

    #[test]
    fn opaque_handles_are_prefix_plus_hex() {
        assert!(handle::is_valid("wf_0123456789abcdef0123456789abcdef"));
        assert!(handle::is_valid("sess_0123456789abcdef0123456789abcdef"));
        // 拒绝:rowid、PID、任意路径、命令、裸 UUID、短 body
        for bad in [
            "123",
            "42",
            "C:/Windows/system32/cmd.exe",
            "/usr/bin/env",
            "npm install",
            "0123456789abcdef0123456789abcdef",
            "wf_short",
            "wf_0123456789ABCDEF0123456789abcdeg",
        ] {
            assert!(!handle::is_valid(bad), "拒绝非 opaque handle:{bad}");
            assert_eq!(
                handle::parse(bad),
                Err(crate::problem::ProblemCode::ResourceNotFound)
            );
        }
    }
}
