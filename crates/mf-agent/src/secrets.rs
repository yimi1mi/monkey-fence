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
