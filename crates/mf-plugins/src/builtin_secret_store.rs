//! 内置 Secret Store 实现(设计 §6.4 / §8)。
//!
//! - `AesGcmSealer`:AES-256-GCM 加解密核心(nonce || ciphertext)。
//! - `InMemorySecretStore`:注入确定性密钥的内存实现(测试/嵌入场景)。
//! - `BuiltinSecretStore`:主密钥由 OS 凭据管理器保护
//!   (keyring service `MonkeyFence` / account `agent-instance-master-key`),
//!   密文持久化在目录库 `sealed_secrets` 表,与普通配置分表存放。
//!
//! 测试一律走 `with_master_key` 注入确定性密钥,不访问真实 OS keyring。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context as _, Result};
use mf_agent::secrets::{SecretDescription, SecretLease, SecretStore};
use parking_lot::Mutex;
use rand::RngCore;
use std::collections::HashMap;

const NONCE_LEN: usize = 12;
const KEYRING_SERVICE: &str = "MonkeyFence";
const KEYRING_ACCOUNT: &str = "agent-instance-master-key";
const STORE_ID: &str = "default";

/// AES-256-GCM 核心:相同明文每次产生不同密文(随机 nonce)。
struct AesGcmSealer {
    cipher: Aes256Gcm,
}

impl AesGcmSealer {
    fn new(key: [u8; 32]) -> AesGcmSealer {
        AesGcmSealer {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
        }
    }

    /// 输出 nonce || ciphertext(含认证标签)。
    fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("AES-GCM 加密不会失败(密钥/nonce 长度固定)");
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        out
    }

    fn unseal(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() <= NONCE_LEN {
            anyhow::bail!("密文长度非法");
        }
        let (nonce_bytes, ct) = sealed.split_at(NONCE_LEN);
        self.cipher
            .decrypt(Nonce::from_slice(nonce_bytes), Payload { msg: ct, aad })
            .map_err(|_| anyhow::anyhow!("解密失败(密钥不匹配或密文被篡改)"))
    }
}

/// AAD:把 Secret 名称与 store 绑进认证数据,防止跨条目挪用密文。
fn aad_for(name: &str) -> Vec<u8> {
    format!("{STORE_ID}\x00{name}").into_bytes()
}

// ---------------- 内存实现 ----------------

/// 内存 Secret Store(测试与嵌入式;确定性密钥注入)。
/// 外部稳定 ID 即 Secret 名称(与 `sealed_secrets` 的 UNIQUE(secret_key, store_id) 一致)。
pub struct InMemorySecretStore {
    sealer: AesGcmSealer,
    entries: Mutex<HashMap<String, Vec<u8>>>, // name → sealed
}

impl InMemorySecretStore {
    pub fn new(key: [u8; 32]) -> InMemorySecretStore {
        InMemorySecretStore {
            sealer: AesGcmSealer::new(key),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 密文的十六进制形式(测试断言"不含明文"用)。
    pub fn ciphertext(&self, id: &str) -> String {
        let entries = self.entries.lock();
        match entries.get(id) {
            Some(sealed) => sealed.iter().map(|b| format!("{b:02x}")).collect(),
            None => String::new(),
        }
    }

    /// 破坏密文的最后一个字节(认证失败路径测试用)。
    pub fn tamper(&self, id: &str) -> bool {
        let mut entries = self.entries.lock();
        match entries.get_mut(id) {
            Some(sealed) => {
                let last = sealed.len() - 1;
                sealed[last] ^= 0xff;
                true
            }
            None => false,
        }
    }
}

impl SecretStore for InMemorySecretStore {
    fn seal(&self, name: &str, plaintext: &[u8]) -> Result<String> {
        let sealed = self.sealer.seal(plaintext, &aad_for(name));
        self.entries.lock().insert(name.to_string(), sealed);
        Ok(name.to_string())
    }

    fn unseal_for_run(&self, _run_token: &str, secret_id: &str) -> Result<SecretLease> {
        let sealed = {
            let entries = self.entries.lock();
            entries
                .get(secret_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Secret `{secret_id}` 不存在"))?
        };
        // 明文所有权 move 进租约(Zeroizing);实现内部不留副本
        let plaintext = self.sealer.unseal(&sealed, &aad_for(secret_id))?;
        Ok(SecretLease::new(secret_id, plaintext))
    }

    fn delete(&self, secret_id: &str) -> Result<bool> {
        Ok(self.entries.lock().remove(secret_id).is_some())
    }

    fn describe(&self, secret_id: &str) -> Result<SecretDescription> {
        let entries = self.entries.lock();
        let sealed = entries
            .get(secret_id)
            .ok_or_else(|| anyhow::anyhow!("Secret `{secret_id}` 不存在"))?;
        Ok(SecretDescription {
            id: secret_id.to_string(),
            name: secret_id.to_string(),
            byte_len: sealed.len().saturating_sub(NONCE_LEN),
        })
    }
}

// ---------------- 内置实现(目录库持久化) ----------------

/// 内置 Secret Store:OS keyring 保护主密钥,密文入目录库。
pub struct BuiltinSecretStore {
    sealer: AesGcmSealer,
    catalog: std::sync::Arc<mf_agent::CatalogStore>,
}

impl BuiltinSecretStore {
    /// 运行时入口:加载(不存在则生成并保存)keyring 主密钥。
    /// 首次创建经进程内互斥:并行组件同时发现 NoEntry 会各自生成
    /// 不同密钥并互相覆盖,导致先密封的密文永远无法解密。
    pub fn open(catalog: std::sync::Arc<mf_agent::CatalogStore>) -> Result<BuiltinSecretStore> {
        static KEYRING_INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = KEYRING_INIT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
            .context("创建 keyring 条目失败")?;
        let key: [u8; 32] = match entry.get_password() {
            Ok(hex) => parse_key_hex(&hex).context("keyring 主密钥格式非法(长度/十六进制)")?,
            Err(keyring::Error::NoEntry) => {
                let mut key = [0u8; 32];
                rand::thread_rng().fill_bytes(&mut key);
                let hex = key.iter().map(|b| format!("{b:02x}")).collect::<String>();
                entry
                    .set_password(&hex)
                    .context("保存 keyring 主密钥失败")?;
                key
            }
            Err(e) => return Err(anyhow::anyhow!("读取 keyring 主密钥失败: {e}")),
        };
        Self::with_master_key(catalog, key)
    }

    /// 注入确定性主密钥(测试/嵌入式;不触 OS keyring)。
    pub fn with_master_key(
        catalog: std::sync::Arc<mf_agent::CatalogStore>,
        key: [u8; 32],
    ) -> Result<BuiltinSecretStore> {
        Ok(BuiltinSecretStore {
            sealer: AesGcmSealer::new(key),
            catalog,
        })
    }
}

impl SecretStore for BuiltinSecretStore {
    /// 外部稳定 ID 即 Secret 名称(表以 UNIQUE(secret_key, store_id) 为自然键)。
    fn seal(&self, name: &str, plaintext: &[u8]) -> Result<String> {
        let sealed = self.sealer.seal(plaintext, &aad_for(name));
        let n = self.catalog.with_conn(|c| {
            Ok(c.execute(
                "INSERT OR REPLACE INTO sealed_secrets (secret_key, store_id, ciphertext, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![name, STORE_ID, sealed, mf_agent::store::now()],
            )?)
        })?;
        if n != 1 {
            anyhow::bail!("写入 sealed_secrets 失败");
        }
        Ok(name.to_string())
    }

    fn unseal_for_run(&self, _run_token: &str, secret_id: &str) -> Result<SecretLease> {
        let sealed: Vec<u8> = self.catalog.with_conn(|c| {
            use rusqlite::OptionalExtension;
            c.query_row(
                "SELECT ciphertext FROM sealed_secrets WHERE secret_key = ?1 AND store_id = ?2",
                rusqlite::params![secret_id, STORE_ID],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("Secret `{secret_id}` 不存在"))
        })?;
        // 明文所有权 move 进租约(Zeroizing);实现内部不留副本
        let plaintext = self.sealer.unseal(&sealed, &aad_for(secret_id))?;
        Ok(SecretLease::new(secret_id, plaintext))
    }

    fn delete(&self, secret_id: &str) -> Result<bool> {
        let n = self.catalog.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM sealed_secrets WHERE secret_key = ?1 AND store_id = ?2",
                rusqlite::params![secret_id, STORE_ID],
            )?)
        })?;
        Ok(n == 1)
    }

    fn describe(&self, secret_id: &str) -> Result<SecretDescription> {
        let row: Option<Vec<u8>> = self.catalog.with_conn(|c| {
            use rusqlite::OptionalExtension;
            c.query_row(
                "SELECT ciphertext FROM sealed_secrets WHERE secret_key = ?1 AND store_id = ?2",
                rusqlite::params![secret_id, STORE_ID],
                |r| r.get(0),
            )
            .optional()
            .map_err(anyhow::Error::from)
        })?;
        let sealed = row.ok_or_else(|| anyhow::anyhow!("Secret `{secret_id}` 不存在"))?;
        Ok(SecretDescription {
            id: secret_id.to_string(),
            name: secret_id.to_string(),
            byte_len: sealed.len().saturating_sub(NONCE_LEN),
        })
    }

    fn list(&self) -> Result<Vec<SecretDescription>> {
        let rows: Vec<(String, Vec<u8>)> = self.catalog.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT secret_key, ciphertext FROM sealed_secrets
                 WHERE store_id = ?1 ORDER BY secret_key",
            )?;
            let out = stmt
                .query_map(rusqlite::params![STORE_ID], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            Ok(out)
        })?;
        Ok(rows
            .into_iter()
            .map(|(key, sealed)| SecretDescription {
                id: key.clone(),
                name: key,
                byte_len: sealed.len().saturating_sub(NONCE_LEN),
            })
            .collect())
    }
}

fn parse_key_hex(hex: &str) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        anyhow::bail!("主密钥必须是 64 个十六进制字符");
    }
    for (i, chunk) in bytes.chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16);
        let lo = (chunk[1] as char).to_digit(16);
        key[i] = match (hi, lo) {
            (Some(h), Some(l)) => ((h << 4) | l) as u8,
            _ => anyhow::bail!("主密钥含非十六进制字符"),
        };
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hex_roundtrip() {
        let key = [0x12u8; 32];
        let hex = key.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(parse_key_hex(&hex).unwrap(), key);
        assert!(parse_key_hex("xyz").is_err());
        assert!(parse_key_hex("1234").is_err());
    }

    #[test]
    fn same_plaintext_different_ciphertext() {
        let s = AesGcmSealer::new([7u8; 32]);
        let a = s.seal(b"same", &[]);
        let b = s.seal(b"same", &[]);
        assert_ne!(a, b, "随机 nonce 必须使相同明文产生不同密文");
        assert_eq!(s.unseal(&a, &[]).unwrap(), b"same");
        assert_eq!(s.unseal(&b, &[]).unwrap(), b"same");
    }

    #[test]
    fn aad_mismatch_rejected() {
        let s = AesGcmSealer::new([7u8; 32]);
        let sealed = s.seal(b"secret-value", b"name-a");
        assert!(s.unseal(&sealed, b"name-b").is_err());
    }
}
