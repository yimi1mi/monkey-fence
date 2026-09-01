//! Catalog v2 独立 schema/future guard/约束契约。

use mf_agent::catalog_store::{CatalogStore, CatalogV2Store, CATALOG_V2_REQUIRED_TABLES};
use mf_agent::migration::{error_code, MigrationError, StoreKind};
use mf_agent::schema::{CATALOG_SCHEMA_V1, CATALOG_V2_SCHEMA_V1, CATALOG_V2_SCHEMA_VERSION};
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::path::Path;

#[test]
fn fresh_catalog_v2_has_required_schema_and_constraints() {
    let store = CatalogV2Store::memory().unwrap();
    assert_eq!(store.schema_version().unwrap(), CATALOG_V2_SCHEMA_VERSION);
    let tables: BTreeSet<String> = store.table_names().unwrap().into_iter().collect();
    for table in CATALOG_V2_REQUIRED_TABLES {
        assert!(tables.contains(*table), "缺少 Catalog v2 表 `{table}`");
    }
    assert!(
        !tables.contains("sealed_secrets"),
        "v2 不得保存 Secret ciphertext"
    );
    assert!(
        !tables.contains("plugin_packages"),
        "安装收据与插件 pin 必须分表"
    );

    store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO cli_installations
                 (installation_handle, agent_type_id, executable_path, canonical_path,
                  source, scope, health, detected_at)
                 VALUES ('i1', 'codex', 'C:/bin/codex.exe', 'C:/bin/codex.exe',
                         'external', 'user', 'healthy', '2026-01-01')",
                [],
            )?;
            let duplicate = conn.execute(
                "INSERT INTO cli_installations
                 (installation_handle, agent_type_id, executable_path, canonical_path,
                  source, scope, health, detected_at)
                 VALUES ('i2', 'codex', 'C:/other/codex.exe', 'C:/bin/codex.exe',
                         'external', 'user', 'healthy', '2026-01-01')",
                [],
            );
            assert!(duplicate.is_err(), "canonical executable path 必须唯一");

            conn.execute(
                "INSERT INTO installation_receipts
                 (receipt_handle, agent_type_id, operation, source, scope,
                  requesting_principal, target_owner, provenance_json,
                  content_digest, created_at)
                 VALUES ('r1', 'codex', 'install', 'managed', 'user',
                         'p', 'p', '{}', 'digest', '2026-01-01')",
                [],
            )?;
            let update = conn.execute(
                "UPDATE installation_receipts SET content_digest = 'changed'
                 WHERE receipt_handle = 'r1'",
                [],
            );
            let delete = conn.execute(
                "DELETE FROM installation_receipts WHERE receipt_handle = 'r1'",
                [],
            );
            let replace = conn.execute(
                "INSERT OR REPLACE INTO installation_receipts
                 (receipt_handle, agent_type_id, operation, source, scope,
                  requesting_principal, target_owner, provenance_json,
                  content_digest, created_at)
                 VALUES ('r1', 'claude', 'repair', 'managed', 'user',
                         'p', 'p', '{}', 'replacement', '2026-01-02')",
                [],
            );
            assert!(
                update.is_err() && delete.is_err() && replace.is_err(),
                "installation receipt 必须拒绝 UPDATE/DELETE/REPLACE"
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn fresh_profile_without_v1_opens_empty_v2() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".monkeyfence");
    let v2 = dir.join("catalog-v2.db");
    let absent_v1 = dir.join("catalog-v1.db");
    let (store, report) = CatalogV2Store::open_migrating_v1(&v2, &absent_v1).unwrap();
    assert!(report.is_none(), "fresh profile 不应伪造 v1 导入报告");
    assert_eq!(store.schema_version().unwrap(), CATALOG_V2_SCHEMA_VERSION);
    store
        .with_conn(|conn| {
            let marker_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM migration_marker", [], |r| r.get(0))?;
            assert_eq!(marker_count, 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn catalog_v2_future_version_fails_closed_without_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog-v2.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE sentinel(x TEXT); INSERT INTO sentinel VALUES ('keep'); PRAGMA user_version = 2;")
            .unwrap();
    }
    let before = std::fs::read(&db).unwrap();
    let err = match CatalogV2Store::open(&db) {
        Ok(_) => panic!("future Catalog v2 必须拒绝"),
        Err(err) => err,
    };
    assert_eq!(error_code(&err), Some("schema_future_version"));
    assert!(matches!(
        err.downcast_ref::<MigrationError>(),
        Some(MigrationError::FutureVersion {
            store: StoreKind::Catalog,
            found: 2,
            known: CATALOG_V2_SCHEMA_VERSION,
        })
    ));
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert!(!db.with_file_name("catalog-v2.db-wal").exists());
    assert!(!db.with_file_name("catalog-v2.db-shm").exists());
}

#[test]
fn catalog_v1_file_cannot_be_mistaken_for_v2() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(CATALOG_SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let before = std::fs::read(&db).unwrap();
    let err = match CatalogV2Store::open(&db) {
        Ok(_) => panic!("v1 文件不能当作 v2 打开"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("catalog_v2_schema_mismatch"));
    assert_eq!(std::fs::read(&db).unwrap(), before);
}

#[test]
fn catalog_v2_file_cannot_be_mistaken_for_v1_or_repaired() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog-v2.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(CATALOG_V2_SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let before = std::fs::read(&db).unwrap();
    let err = match CatalogStore::open(&db) {
        Ok(_) => panic!("v2 文件不能当作 v1 打开或 repair"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("catalog_schema_kind_mismatch"));
    assert_eq!(std::fs::read(&db).unwrap(), before);
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    for forbidden in ["sealed_secrets", "plugin_packages"] {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [forbidden],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!exists, "反向 guard 前不得向 v2 添加 `{forbidden}`");
    }
    assert!(!db.with_file_name("catalog-v2.db-wal").exists());
    assert!(!db.with_file_name("catalog-v2.db-shm").exists());
}

#[test]
fn catalog_v2_database_and_owned_directory_are_current_user_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".monkeyfence");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("catalog-v2.db");
    let store = CatalogV2Store::open(&db).unwrap();
    store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO projection_outbox(event_json) VALUES ('{}')",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    assert_current_user_only(&dir);
    assert_current_user_only(&db);
    for suffix in ["-wal", "-shm"] {
        let sidecar = Path::new(&format!("{}{}", db.display(), suffix)).to_path_buf();
        if sidecar.exists() {
            assert_current_user_only(&sidecar);
        }
    }
}

#[cfg(unix)]
fn assert_current_user_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let expected = if path.is_dir() { 0o700 } else { 0o600 };
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        expected,
        "{} 权限过宽",
        path.display()
    );
}

#[cfg(windows)]
fn assert_current_user_only(path: &Path) {
    let sddl = dacl_sddl(path);
    assert!(
        sddl.contains(";;;OW)"),
        "{} 未只授权 owner:{sddl}",
        path.display()
    );
    for broad in [";;;WD)", ";;;AU)", ";;;BU)", ";;;BG)"] {
        assert!(
            !sddl.contains(broad),
            "{} 授权了宽泛主体 {broad}:{sddl}",
            path.display()
        );
    }
}

#[cfg(not(any(unix, windows)))]
fn assert_current_user_only(_path: &Path) {}

#[cfg(windows)]
fn dacl_sddl(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetFileSecurityW, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut needed = 0u32;
    unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    assert!(needed > 0);
    let mut descriptor = vec![0u8; needed as usize];
    let ok = unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            DACL_SECURITY_INFORMATION,
            descriptor.as_mut_ptr() as PSECURITY_DESCRIPTOR,
            needed,
            &mut needed,
        )
    };
    assert_ne!(
        ok,
        0,
        "GetFileSecurityW:{:?}",
        std::io::Error::last_os_error()
    );
    let mut sddl_ptr = std::ptr::null_mut();
    let mut length = 0u32;
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.as_mut_ptr() as PSECURITY_DESCRIPTOR,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut sddl_ptr,
            &mut length,
        )
    };
    assert_ne!(ok, 0);
    let result =
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sddl_ptr, length as usize) });
    unsafe {
        LocalFree(sddl_ptr.cast());
    }
    result
}
