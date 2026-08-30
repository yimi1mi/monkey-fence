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
    /// 全部 Secret 的脱敏描述(升序;无枚举能力的实现返回空)。
    fn list(&self) -> Result<Vec<SecretDescription>> {
        Ok(Vec::new())
    }
}

/// 流式 Secret 脱敏器:CLI 输出进入 screen/output_tail 前逐块脱敏。
///
/// 明文只以 zeroizing 租约的共享引用存在(`from_leases` 不复制副本);
/// 匹配支持跨 read chunk 的部分前缀;流结束(`finish`)立即释放
/// 明文表 —— 不随会话长期驻留。缓冲与 Secret 表 drop 时 zeroize。
pub struct StreamingRedactor {
    /// zeroizing 租约的共享引用 —— 明文唯一形态在租约里,
    /// 这里绝不复制副本(最后一个引用 drop 即擦除)。
    secrets: Vec<std::sync::Arc<SecretLease>>,
    max_len: usize,
    /// 跨块未决前缀(可能是明文/Secret 前缀):Zeroizing ——
    /// 每次被新前缀替换、redactor drop 时都原地清零。
    carry: Zeroizing<Vec<u8>>,
}

impl StreamingRedactor {
    /// 用 Secret 明文构造(测试与遗留路径;生产应使用 `from_leases`)。
    pub fn new(secret_values: Vec<Vec<u8>>) -> StreamingRedactor {
        Self::from_leases(
            secret_values
                .into_iter()
                .filter(|value| !value.is_empty())
                .map(|value| std::sync::Arc::new(SecretLease::new("redactor", value)))
                .collect(),
        )
    }

    /// 从 zeroizing 租约构造(Runtime 启动期):共享 Arc 引用,
    /// 不 `.to_vec()` 复制明文 —— 明文随最后一个租约引用 drop 擦除。
    pub fn from_leases(leases: Vec<std::sync::Arc<SecretLease>>) -> StreamingRedactor {
        let secrets: Vec<std::sync::Arc<SecretLease>> = leases
            .into_iter()
            .filter(|lease| !lease.is_empty())
            .collect();
        let max_len = secrets.iter().map(|lease| lease.len()).max().unwrap_or(0);
        StreamingRedactor {
            secrets,
            max_len,
            carry: Zeroizing::new(Vec::new()),
        }
    }

    pub fn is_noop(&self) -> bool {
        self.secrets.is_empty()
    }

    fn match_at(&self, data: &[u8], pos: usize) -> Option<usize> {
        self.secrets
            .iter()
            .find(|lease| data[pos..].starts_with(lease.as_slice()))
            .map(|lease| lease.len())
    }

    /// 脱敏一个输出块。最后 `max_len - 1` 个原始字节可能是不完整前缀,
    /// 留在 carry 中等待下一块;函数返回可安全进入 screen/日志的前缀。
    /// carry+chunk 的合并缓冲(`combined`)也是明文形态:Zeroizing,
    /// 函数返回时未保留部分原地清零后释放。
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
        // 尾部换成新的 carry;前缀部分随 combined(Zeroizing)drop 清零
        self.carry = Zeroizing::new(combined.split_off(i));
        out
    }

    /// 流结束:冲出 carry(carry 不可能包含完整 Secret,仍再扫一遍兜底),
    /// 并立即释放明文表 —— 进程退出后不再长期持有任何明文。
    pub fn finish(&mut self) -> Vec<u8> {
        let carry = std::mem::take(&mut self.carry);
        if self.secrets.is_empty() {
            return carry.to_vec();
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
        self.secrets.clear(); // 归还共享引用(最后一个引用 drop 即擦除明文)
        out
    }

    /// carry 缓冲视图(测试:验证擦除例程作用于真实驻留缓冲)。
    #[cfg(test)]
    pub(crate) fn carry_parts(&self) -> (*const u8, usize) {
        (self.carry.as_ptr(), self.carry.len())
    }

    /// 与 Drop 走的同一清零例程(测试在持有分配时验证清零;
    /// 释放后内存可能被分配器合法复用,"释放后仍为零"不是可测不变量)。
    #[cfg(test)]
    pub(crate) fn zeroize_scrub_routine(&mut self) {
        use zeroize::Zeroize;
        self.carry.zeroize();
    }
}

/// spawn 环境块中 Secret 值的一次性明文形态(I7):
/// - 构造时借用 LaunchPlan 的 `secret_env` 租约(不复制 Arc),
///   把每个值解成 Zeroizing 的 UTF-8 缓冲 —— 不是随计划/会话长期
///   存活的普通 String/OsString 副本;
/// - `iter()` 在 spawn 现场借用给 CommandBuilder;
/// - drop 时逐值擦除(Zeroizing),spawn 结束明文零残留。
pub struct SecretEnvBlock {
    entries: Vec<(String, Zeroizing<String>)>,
}

impl SecretEnvBlock {
    /// 从 LaunchPlan 的 secret_env 租约构造(值必须是 UTF-8)。
    pub fn from_leases(
        secret_env: &[(String, std::sync::Arc<SecretLease>)],
    ) -> anyhow::Result<SecretEnvBlock> {
        let mut entries = Vec::with_capacity(secret_env.len());
        for (key, lease) in secret_env {
            let value = std::str::from_utf8(lease.as_slice()).map_err(|_| {
                anyhow::anyhow!(
                    "Secret `{}` 不是有效 UTF-8,无法注入环境变量 {key}",
                    lease.id()
                )
            })?;
            entries.push((key.clone(), Zeroizing::new(value.to_string())));
        }
        Ok(SecretEnvBlock { entries })
    }

    /// 环境条目视图(spawn 现场使用;不得长期持有值引用)。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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

#[cfg(test)]
mod carry_bound_tests {
    use super::*;

    #[test]
    fn redactor_carry_never_holds_ordinary_plaintext_long_term() {
        // 大量普通明文流过:carry 缓冲必须保持有界(≤ 最长 Secret - 1),
        // 不会积累输出明文;Secret 表外的字节全部即时放行。
        let mut redactor = StreamingRedactor::new(vec![b"tok".to_vec()]);
        let chunk = vec![b'x'; 4096];
        let mut total = 0usize;
        for _ in 0..64 {
            total += redactor.redact_chunk(&chunk).len();
        }
        // 只有最后 max_len-1 = 2 字节可能滞留 carry
        assert_eq!(total, 64 * 4096 - 2, "普通明文不得长期滞留 carry");
        // 流结束全部冲出
        let rest = redactor.finish();
        assert_eq!(rest, b"xx".to_vec());
    }

    #[test]
    fn carry_flush_never_releases_a_full_secret() {
        // Secret 被块边界切开:carry 持有的只能是未确认前缀,
        // finish 冲出时完整 Secret 仍必须被替换。
        let mut redactor = StreamingRedactor::new(vec![b"secret-key".to_vec()]);
        let a = redactor.redact_chunk(b"abc secret-");
        let b = redactor.redact_chunk(b"key tail");
        let c = redactor.finish();
        let text = String::from_utf8_lossy(&[a, b, c].concat()).into_owned();
        assert!(
            !text.contains("secret-key"),
            "finish 泄露完整 Secret: {text}"
        );
        assert!(text.contains("***"), "完整 Secret 应被替换: {text}");
        assert!(text.ends_with(" tail"), "普通明文应放行: {text}");
    }
}

#[cfg(test)]
mod carry_scrub_tests {
    use super::*;

    /// F7:carry 驻留缓冲的清零例程(Drop 走同一例程):
    /// 跨块未决前缀(可能是 Secret 前缀/明文)在持有分配时必须能被
    /// 原地清零;释放后内存可能被分配器合法复用,"释放后仍为零"
    /// 不是可测不变量。
    #[test]
    fn carry_plaintext_is_scrubbed_by_zeroize_routine() {
        let mut redactor = StreamingRedactor::new(vec![b"tok-scrub".to_vec()]);
        // 尾部留下未决前缀(Secret 前缀 + 普通明文混杂)
        redactor.redact_chunk(b"plain tok-scr");
        let (ptr, len) = redactor.carry_parts();
        assert!(len > 0, "前置:carry 持有未决前缀");
        let has_plaintext = unsafe { std::slice::from_raw_parts(ptr, len) }
            .windows(4)
            .any(|w| w == b"crub" || w == b"tok-" || w == b"n to");
        assert!(has_plaintext, "前置:carry 里确实有明文残留");
        redactor.zeroize_scrub_routine();
        let zeroized = unsafe { std::slice::from_raw_parts(ptr, len) }
            .iter()
            .all(|b| *b == 0);
        assert!(zeroized, "清零例程必须原地清空 carry 缓冲");
        // 正常 drop 路径冒烟(不应 panic/泄漏)
        drop(redactor);
    }

    /// 跨块回显仍然正确(carry 换 Zeroizing 后的行为回归)。
    #[test]
    fn redaction_unchanged_after_carry_hardening() {
        let mut redactor = StreamingRedactor::new(vec![b"tok-multi".to_vec()]);
        let a = redactor.redact_chunk(b"x tok-mul");
        let b = redactor.redact_chunk(b"ti y tok-multi z");
        let c = redactor.finish();
        let text = String::from_utf8_lossy(&[a, b, c].concat()).into_owned();
        assert!(!text.contains("tok-multi"), "{text}");
        assert_eq!(text.matches("***").count(), 2, "{text}");
        assert!(
            text.contains('x') && text.contains(" y ") && text.contains(" z"),
            "{text}"
        );
    }
}

#[cfg(test)]
mod lease_redactor_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn from_leases_shares_lease_references_without_copying_plaintext() {
        // 构造不得复制明文副本:只共享 zeroizing 租约的 Arc 引用
        // (StrongCount 证明);redactor 生命周期结束即归还引用。
        let lease = Arc::new(SecretLease::new("sec", b"tok-42".to_vec()));
        let mut redactor = StreamingRedactor::from_leases(vec![lease.clone()]);
        assert_eq!(
            Arc::strong_count(&lease),
            2,
            "from_leases 不得 .to_vec() 复制整份 secret(应共享租约引用)"
        );
        // 共享引用的脱敏同样有效
        let a = redactor.redact_chunk(b"tok-42");
        let b = redactor.finish();
        let text = String::from_utf8_lossy(&[a, b].concat()).into_owned();
        assert!(text.contains("***"), "{text}");
        assert!(!text.contains("tok-42"), "{text}");
        drop(redactor);
        assert_eq!(
            Arc::strong_count(&lease),
            1,
            "redactor 结束后必须归还租约引用(不再持有明文)"
        );
    }

    #[test]
    fn secret_env_block_borrows_leases_and_reports_values() {
        let lease = Arc::new(SecretLease::new("sec", b"env-value".to_vec()));
        let block =
            super::SecretEnvBlock::from_leases(&[("MY_KEY".into(), lease.clone())]).unwrap();
        assert_eq!(Arc::strong_count(&lease), 1, "不得复制租约 Arc");
        let pairs: Vec<(&str, &str)> = block.iter().collect();
        assert_eq!(pairs, vec![("MY_KEY", "env-value")]);
        assert_eq!(block.len(), 1);
        // 非 UTF-8 secret 不能进环境块(拒绝而不是替换/丢弃)
        let bad = Arc::new(SecretLease::new("bad", vec![0xff, 0xfe]));
        assert!(super::SecretEnvBlock::from_leases(&[("K".into(), bad)]).is_err());
    }

    #[test]
    fn from_leases_redacts_and_finish_releases_plaintext() {
        let lease = Arc::new(SecretLease::new("sec", b"tok-42".to_vec()));
        let mut redactor = StreamingRedactor::from_leases(vec![lease.clone()]);
        let a = redactor.redact_chunk(b"echo tok-42 ");
        let b = redactor.finish();
        let text = String::from_utf8_lossy(&[a, b].concat()).into_owned();
        assert!(text.contains("***"), "租约明文必须被替换: {text}");
        assert!(!text.contains("tok-42"), "不得泄露: {text}");
        // 流结束后明文表已释放:后续块直接放行(is_noop)
        assert!(redactor.is_noop(), "finish 后不得再持有明文表");
        assert_eq!(redactor.redact_chunk(b"tok-42"), b"tok-42".to_vec());
    }
}
