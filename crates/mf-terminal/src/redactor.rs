//! 统一 launch 脱敏入口(T3a,Issue #29;canonical spec §8.8)。
//!
//! 修复的泄漏窗口:legacy 工作流路径先从 `plan.secret_env` 构造
//! redactor、**之后**才注入 `MF_RUN_TOKEN` 等 capability 环境值——
//! CLI echo token 时不会被脱敏。统一入口要求全部 Secret 租约与
//! capability 值在进程启动前进入同一个 `StreamingRedactor`,此后
//! `raw PTY → redactor → Screen/transcript/fan-out` 只此一条管线。
//!
//! 本模块只做组装;跨块/重叠/尾部匹配语义由 `mf_agent::secrets::
//! StreamingRedactor` 提供并被其测试覆盖。T3b 的 journal/seq 接在
//! redactor 之后,不得绕开。

use std::sync::Arc;

use mf_agent::secrets::{SecretLease, StreamingRedactor};

/// capability 值的租约 ID(诊断用途;值本身即 token 明文,租约 drop
/// 时由 Zeroizing 擦除)。
pub const CAPABILITY_LEASE_ID: &str = "mf-run-token";

/// 组装 launch 期 redactor:Secret 租约 + 可选 capability token
/// (`MF_RUN_TOKEN`)全部在同一入口进入脱敏器。
///
/// - 空 token(或 None)不产生额外租约(离散/Preview 会话无结算令牌);
/// - token 包装为一次性 zeroizing 租约,不改变调用方持有形态。
pub fn launch_redactor(
    secret_leases: Vec<Arc<SecretLease>>,
    capability_token: Option<&str>,
) -> StreamingRedactor {
    let mut leases = secret_leases;
    if let Some(token) = capability_token {
        let bytes = token.as_bytes();
        if !bytes.is_empty() {
            leases.push(Arc::new(SecretLease::new(
                CAPABILITY_LEASE_ID,
                bytes.to_vec(),
            )));
        }
    }
    StreamingRedactor::from_leases(leases)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact_all(redactor: &mut StreamingRedactor, chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend_from_slice(&redactor.redact_chunk(chunk));
        }
        out.extend_from_slice(&redactor.finish());
        out
    }

    #[test]
    fn capability_token_is_redacted_like_secrets() {
        let secret = Arc::new(SecretLease::new("api-key", b"sk-secret-123".to_vec()));
        let mut redactor = launch_redactor(vec![secret], Some("tok_1234567890abcdef"));
        let output = redact_all(
            &mut redactor,
            &[b"key=sk-secret-123 tok=tok_1234567890abcdef\r\n"],
        );
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("sk-secret-123"), "Secret 不得出现:{text}");
        assert!(
            !text.contains("tok_1234567890abcdef"),
            "token 不得出现:{text}"
        );
        assert!(text.contains("***"), "命中应以 *** 替换:{text}");
    }

    #[test]
    fn token_split_across_chunks_is_redacted() {
        let mut redactor = launch_redactor(Vec::new(), Some("tok-cross-chunk"));
        let output = redact_all(&mut redactor, &[b"echo tok-cr", b"oss-chu", b"nk end\r\n"]);
        let text = String::from_utf8(output).unwrap();
        assert!(
            !text.contains("tok-cross-chunk"),
            "跨块 token 不得出现:{text}"
        );
    }

    #[test]
    fn empty_or_missing_token_is_noop() {
        assert!(launch_redactor(Vec::new(), None).is_noop());
        assert!(launch_redactor(Vec::new(), Some("")).is_noop());
        assert!(!launch_redactor(Vec::new(), Some("tok")).is_noop());
    }

    #[test]
    fn overlapping_token_occurrences_all_redacted() {
        let mut redactor = launch_redactor(Vec::new(), Some("abab"));
        let output = redact_all(&mut redactor, &[b"abababab"]);
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("abab"), "重叠出现全部覆盖:{text}");
    }
}
