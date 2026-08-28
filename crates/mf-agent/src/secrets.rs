//! Secret Store 契约(设计 §6.4 / §8)。
//!
//! 内核只拥有接口与 zeroizing 租约类型;AES-256-GCM、OS keyring
//! 主密钥与内置实现都在 `mf-plugins::builtin_secret_store`。
//!
//! 明文约束:
//! - `seal` 之后明文只允许在 `unseal_for_run` 返回的 `SecretLease` 中存在;
//! - `SecretLease` drop 时 zeroize(进程内零残留);
//! - 日志、Handoff、错误与默认导出一律通过 `Redacted` / 脱敏描述输出。

use anyhow::Result;
use zeroize::Zeroizing;

/// 解密租约:持有明文的唯一形态,drop 即 zeroize。
pub struct SecretLease {
    secret_id: String,
    plaintext: Zeroizing<Vec<u8>>,
}

impl SecretLease {
    pub fn new(secret_id: impl Into<String>, plaintext: Vec<u8>) -> SecretLease {
        SecretLease {
            secret_id: secret_id.into(),
            plaintext: Zeroizing::new(plaintext),
        }
    }

    /// Secret 稳定 ID(非敏感)。
    pub fn id(&self) -> &str {
        &self.secret_id
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.plaintext
    }

    pub fn len(&self) -> usize {
        self.plaintext.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plaintext.is_empty()
    }
}

// Debug 一律脱敏:租约里是明文,绝不能进日志。
impl std::fmt::Debug for SecretLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SecretLease({}, {} bytes, redacted)",
            self.secret_id,
            self.plaintext.len()
        )
    }
}

/// 永不泄露内值的包装:Debug/Display 只输出 `<redacted>`。
/// 启动进程等需要真实值的调用方用 `get`/`into_inner` 显式取出。
#[derive(Clone)]
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    pub fn new(value: T) -> Redacted<T> {
        Redacted(value)
    }
    pub fn get(&self) -> &T {
        &self.0
    }
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T> std::fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Secret 的脱敏元数据(用于 UI 列表与日志)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretDescription {
    pub id: String,
    pub name: String,
    pub byte_len: usize,
}

/// Secret Store 接口:`seal` / `unseal_for_run` / `delete` / `describe`。
///
/// `unseal_for_run` 以当前 Agent Run 的能力令牌为凭据:
/// 令牌 → Secret 授权映射在 Runtime Host 接线时生效(里程碑 4+),
/// 当前契约要求实现方记录并校验调用上下文,不得提供无凭据的整库导出。
pub trait SecretStore: Send + Sync {
    /// 加密并保存;返回 Secret 稳定 ID。
    fn seal(&self, name: &str, plaintext: &[u8]) -> Result<String>;
    /// 为指定 Agent Run 解封;明文只在返回的租约里存活。
    fn unseal_for_run(&self, run_token: &str, secret_id: &str) -> Result<SecretLease>;
    /// 删除;返回是否真的删除了。
    fn delete(&self, secret_id: &str) -> Result<bool>;
    /// 脱敏描述(不含明文)。
    fn describe(&self, secret_id: &str) -> Result<SecretDescription>;
}

/// 流式 Secret 脱敏器:CLI 输出进入 screen/output_tail 前逐块脱敏。
///
/// 匹配支持跨 read chunk 的部分前缀(可能横跨两次 `read` 的 Secret
/// 也会被整体替换);未确认的原始尾部保留在 carry 缓冲中,
/// 缓冲与内部 Secret 表在 drop 时 zeroize(应用侧不留明文副本)。
pub struct StreamingRedactor {
    secrets: Vec<Zeroizing<Vec<u8>>>,
    max_len: usize,
    carry: Vec<u8>,
}

impl StreamingRedactor {
    /// 用 Secret 明文构造(启动期调用;值来自 zeroizing 租约)。
    pub fn new(secret_values: Vec<Vec<u8>>) -> StreamingRedactor {
        let secrets: Vec<Zeroizing<Vec<u8>>> = secret_values
            .into_iter()
            .filter(|value| !value.is_empty())
            .map(Zeroizing::new)
            .collect();
        let max_len = secrets.iter().map(|s| s.len()).max().unwrap_or(0);
        StreamingRedactor {
            secrets,
            max_len,
            carry: Vec::new(),
        }
    }

    pub fn is_noop(&self) -> bool {
        self.secrets.is_empty()
    }

    fn match_at(&self, data: &[u8], pos: usize) -> Option<usize> {
        self.secrets
            .iter()
            .find(|secret| data[pos..].starts_with(&secret[..]))
            .map(|secret| secret.len())
    }

    /// 脱敏一个输出块。最后 `max_len - 1` 个原始字节可能是不完整前缀,
    /// 留在 carry 中等待下一块;函数返回可安全进入 screen/日志的前缀。
    pub fn redact_chunk(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.secrets.is_empty() {
            return chunk.to_vec();
        }
        let mut combined = std::mem::take(&mut self.carry);
        combined.extend_from_slice(chunk);
        // emit_upto 之后的原始字节可能是未来匹配的前缀,不下发
        let emit_upto = combined.len().saturating_sub(self.max_len - 1);
        let mut out = Vec::with_capacity(combined.len());
        let mut i = 0;
        while i < combined.len() {
            if let Some(len) = self.match_at(&combined, i) {
                out.extend_from_slice(b"***");
                i += len;
            } else if i < emit_upto {
                out.push(combined[i]);
                i += 1;
            } else {
                break;
            }
        }
        self.carry = combined.split_off(i);
        out
    }

    /// 流结束:冲出 carry(carry 不可能包含完整 Secret,仍再扫一遍兜底)。
    pub fn finish(&mut self) -> Vec<u8> {
        let carry = std::mem::take(&mut self.carry);
        if self.secrets.is_empty() {
            return carry;
        }
        let mut out = Vec::with_capacity(carry.len());
        let mut i = 0;
        while i < carry.len() {
            if let Some(len) = self.match_at(&carry, i) {
                out.extend_from_slice(b"***");
                i += len;
            } else {
                out.push(carry[i]);
                i += 1;
            }
        }
        out
    }
}

impl Drop for StreamingRedactor {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.carry.zeroize();
    }
}

#[cfg(test)]
mod streaming_redactor_tests {
    use super::*;

    fn joined(parts: &[Vec<u8>]) -> String {
        String::from_utf8_lossy(&parts.concat()).into_owned()
    }

    #[test]
    fn redacts_secret_split_across_chunks() {
        let mut redactor = StreamingRedactor::new(vec![b"sk-secret-99".to_vec()]);
        let a = redactor.redact_chunk(b"token: sk-");
        let b = redactor.redact_chunk(b"secret-99 done");
        let c = redactor.finish();
        let text = joined(&[a, b, c]);
        assert!(!text.contains("sk-secret-99"), "跨 chunk 泄露: {text}");
        assert!(text.contains("***"), "应替换为占位: {text}");
        assert!(text.contains("done"));
    }

    #[test]
    fn passes_clean_data_through_multiple_secrets() {
        let mut redactor =
            StreamingRedactor::new(vec![b"alpha-key".to_vec(), b"beta-key".to_vec()]);
        let a = redactor.redact_chunk(b"clean alpha");
        let b = redactor.redact_chunk(b"-key and beta-key tail");
        let c = redactor.finish();
        let text = joined(&[a, b, c]);
        assert!(
            !text.contains("alpha-key") && !text.contains("beta-key"),
            "{text}"
        );
        assert!(text.contains("clean") && text.contains(" and ") && text.contains(" tail"));
        // 占位数量 = 两个 Secret 各一次
        assert_eq!(text.matches("***").count(), 2);
    }

    #[test]
    fn partial_prefix_eventually_passes_through() {
        // "sk-" 是 Secret 前缀但后续 chunk 未补全:不得吞字,也不得泄露
        let mut redactor = StreamingRedactor::new(vec![b"sk-secret".to_vec()]);
        let a = redactor.redact_chunk(b"prefix sk-");
        let b = redactor.redact_chunk(b" other");
        let c = redactor.finish();
        let text = joined(&[a, b, c]);
        assert!(text.contains("sk- other"), "前缀最终应放行: {text}");
        assert!(!text.contains("sk-secret"));
    }

    #[test]
    fn noop_without_secrets() {
        let mut redactor = StreamingRedactor::new(vec![]);
        assert!(redactor.is_noop());
        assert_eq!(redactor.redact_chunk(b"plain"), b"plain".to_vec());
        assert_eq!(redactor.finish(), Vec::<u8>::new());
    }

    #[test]
    fn repeated_and_adjacent_secrets_all_redacted() {
        let mut redactor = StreamingRedactor::new(vec![b"tok".to_vec()]);
        let a = redactor.redact_chunk(b"toktok tail");
        let b = redactor.finish();
        let text = joined(&[a, b]);
        assert_eq!(text, "***tok tail".to_string().replace("tok", "***") + "");
        // 直接断言:开头两处相邻都被替换
        assert!(text.starts_with("******"), "{text}");
        assert!(text.ends_with(" tail"));
    }
}
