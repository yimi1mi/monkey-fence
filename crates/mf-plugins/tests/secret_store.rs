//! Secret Store 契约:AES-256-GCM 加密、密文/日志/Debug 全链路脱敏、
//! 租约 drop 即 zeroize(设计 §6.4 / §8)。
//!
//! 测试只注入确定性密钥([7u8; 32] 等),绝不访问真实 OS keyring;
//! keyring 主密钥路径由 `BuiltinSecretStore::open` 在运行时使用。

use mf_agent::catalog_store::CatalogStore;
use mf_agent::secrets::{Redacted, SecretLease, SecretStore};
use mf_plugins::builtin_secret_store::{
    authorize_run_secrets, revoke_run_secrets, BuiltinSecretStore, InMemorySecretStore,
    RunSecretGrant,
};

/// 测试内解封一律先为令牌授权(Runtime 编译 LaunchPlan 的等价动作)。
fn grant(token: &str, ids: &[&str]) {
    authorize_run_secrets(token, ids);
}

const SECRET: &[u8] = b"secret-value";

#[test]
fn ciphertext_and_debug_never_contain_secret() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", b"secret-value").unwrap();
    assert!(!store.ciphertext(&id).contains("secret-value"));
    assert!(!format!("{:?}", store.describe(&id).unwrap()).contains("secret-value"));
}

#[test]
fn seal_unseal_roundtrip() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", SECRET).unwrap();
    grant("tok", &[&id]);
    let lease = store.unseal_for_run("tok", &id).unwrap();
    assert_eq!(lease.as_slice(), SECRET);
    assert_eq!(lease.id(), id.as_str());
}

#[test]
fn wrong_key_cannot_unseal() {
    let catalog = CatalogStore::memory().unwrap();
    let a = BuiltinSecretStore::with_master_key(catalog.clone(), [1u8; 32]).unwrap();
    let id = a.seal("api-key", SECRET).unwrap();
    // 同一目录库、不同主密钥:AES-GCM 认证必须失败
    let b = BuiltinSecretStore::with_master_key(catalog, [2u8; 32]).unwrap();
    grant("tok", &[&id]);
    assert!(b.unseal_for_run("tok", &id).is_err());
}

#[test]
fn tampered_ciphertext_is_rejected() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", SECRET).unwrap();
    assert!(store.tamper(&id));
    grant("tok", &[&id]);
    assert!(store.unseal_for_run("tok", &id).is_err());
}

#[test]
fn delete_removes_secret() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", SECRET).unwrap();
    grant("tok", &[&id]);
    assert!(store.delete(&id).unwrap());
    assert!(store.unseal_for_run("tok", &id).is_err());
    assert!(store.describe(&id).is_err());
    // 幂等:再删返回 false
    assert!(!store.delete(&id).unwrap());
}

#[test]
fn unknown_secret_errors() {
    let store = InMemorySecretStore::new([7u8; 32]);
    assert!(store.unseal_for_run("tok", "nope").is_err());
    assert!(store.describe("nope").is_err());
}

#[test]
fn lease_and_redacted_never_leak_in_debug() {
    let lease = SecretLease::new("sec_x", SECRET.to_vec());
    let debug = format!("{lease:?}");
    assert!(
        !debug.contains("secret-value"),
        "lease Debug 泄露明文: {debug}"
    );
    assert!(
        debug.contains("SecretLease"),
        "lease Debug 应可识别: {debug}"
    );

    let redacted = Redacted::new("secret-value".to_string());
    assert_eq!(format!("{redacted:?}"), "<redacted>");
    assert_eq!(format!("{redacted}"), "<redacted>");
    // 内部仍可读(启动进程需要真实值)
    assert_eq!(redacted.get(), "secret-value");
}

#[test]
fn builtin_store_persists_ciphertext_in_catalog() {
    let catalog = CatalogStore::memory().unwrap();
    let store = BuiltinSecretStore::with_master_key(catalog.clone(), [9u8; 32]).unwrap();
    let id = store.seal("api-key", SECRET).unwrap();
    // 目录库里只有密文,没有明文
    let raw: String = catalog
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT LOWER(HEX(ciphertext)) FROM sealed_secrets WHERE secret_key = ?1",
                rusqlite::params!["api-key"],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(!raw.contains("secret-value"), "目录库出现明文: {raw}");
    assert!(!raw.is_empty());

    // 重开同一目录库(同一主密钥)→ 仍可解封
    let reopened = BuiltinSecretStore::with_master_key(catalog, [9u8; 32]).unwrap();
    grant("tok", &[&id]);
    let lease = reopened.unseal_for_run("tok", &id).unwrap();
    assert_eq!(lease.as_slice(), SECRET);

    assert!(reopened.delete(&id).unwrap());
    assert!(reopened.unseal_for_run("tok", &id).is_err());
}

#[test]
fn describe_reports_metadata_without_plaintext() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", SECRET).unwrap();
    let d = store.describe(&id).unwrap();
    assert_eq!(d.id, id);
    assert_eq!(d.name, "api-key");
    // byte_len 是密文负载长度(明文 + 16 字节 GCM 认证标签),不暴露明文本身
    assert_eq!(d.byte_len, SECRET.len() + 16);
    let text = format!("{d:?}");
    assert!(!text.contains("secret-value"));
    assert!(text.contains("api-key"));
}

#[test]
fn same_name_reseal_replaces() {
    let catalog = CatalogStore::memory().unwrap();
    let store = BuiltinSecretStore::with_master_key(catalog, [3u8; 32]).unwrap();
    let first = store.seal("api-key", b"old").unwrap();
    store.delete(&first).unwrap();
    let second = store.seal("api-key", b"new").unwrap();
    grant("tok", &[&second]);
    let lease = store.unseal_for_run("tok", &second).unwrap();
    assert_eq!(lease.as_slice(), b"new");
}

// ---------- run token 授权(无凭据/未授权/撤销后一律拒绝)----------

#[test]
fn unseal_requires_token_authorization() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", SECRET).unwrap();
    // 空令牌拒绝
    assert!(store.unseal_for_run("", &id).is_err());
    // 未授权令牌拒绝
    assert!(store.unseal_for_run("run-token-a", &id).is_err());
    // 授权后放行
    authorize_run_secrets("run-token-a", &[&id]);
    let lease = store.unseal_for_run("run-token-a", &id).unwrap();
    assert_eq!(lease.as_slice(), SECRET);
    // 令牌只对自己授权的 Secret 有效
    let other = store.seal("api-key-2", b"other").unwrap();
    assert!(store.unseal_for_run("run-token-a", &other).is_err());
    // 撤销后拒绝(spawn 完成)
    revoke_run_secrets("run-token-a");
    assert!(store.unseal_for_run("run-token-a", &id).is_err());
}

// ---------- RunSecretGrant RAII(I11) ----------

#[test]
fn run_secret_grant_authorizes_during_scope_and_revokes_on_drop() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", SECRET).unwrap();
    {
        let _grant = RunSecretGrant::authorize("raii-tok", &[&id]);
        // 守卫存活期间:授权有效
        assert!(store.unseal_for_run("raii-tok", &id).is_ok());
    }
    // Drop 后:授权撤销(无凭据解封拒绝)
    assert!(
        store.unseal_for_run("raii-tok", &id).is_err(),
        "守卫 Drop 后授权必须撤销"
    );
}

#[test]
fn run_secret_grant_revokes_on_panic_unwind() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", SECRET).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _grant = RunSecretGrant::authorize("panic-tok", &[&id]);
        assert!(store.unseal_for_run("panic-tok", &id).is_ok());
        panic!("模拟解封过程中的 panic");
    }));
    assert!(result.is_err(), "前置:闭包确实 panic");
    // panic/unwind 后:授权仍被回收(不留长期有效凭据)
    assert!(
        store.unseal_for_run("panic-tok", &id).is_err(),
        "panic unwind 后授权必须被 Drop 回收"
    );
}

#[test]
fn run_secret_grant_revokes_on_early_error_return() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", SECRET).unwrap();
    fn work(store: &InMemorySecretStore, id: &str) -> Result<(), String> {
        let _grant = RunSecretGrant::authorize("err-tok", &[id]);
        store
            .unseal_for_run("err-tok", id)
            .map_err(|e| format!("{e:#}"))?;
        Err("解封后的后续步骤失败".into()) // 错误提前返回
    }
    assert!(work(&store, &id).is_err());
    assert!(
        store.unseal_for_run("err-tok", &id).is_err(),
        "错误提前返回后授权必须撤销(不得依赖手工配对)"
    );
}
