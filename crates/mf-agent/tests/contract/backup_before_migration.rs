//! T1a 契约(Issue #16):schema 升级前的 SQLite Backup API 前置屏障。
//!
//! - 实际 schema 升级(0 < user_version < target)必须先产出一致备份 +
//!   manifest,覆盖 WAL 中已提交未 checkpoint 的数据;
//! - 备份失败则迁移函数/DDL 不执行,user_version 与 schema 不变;
//! - 备份/manifest 配对精确(hash/size/version/store-kind),manifest 不含
//!   Secret/capability 明文或环境路径;
//! - 全部使用 tempfile fixture 与独立只读连接校验。

use crate::support::{
    build_legacy_v1_db, dir_entries, integrity_check_of, json_string_values, read_only,
    sha256_file, sole_manifest, user_version_of,
};
use mf_agent::migration::{self, error_code, StoreKind};
use mf_agent::schema::{CATALOG_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION};
use mf_agent::store::Store;
use mf_agent::CatalogStore;
use rusqlite::Connection;

/// 打开中的 WAL 库:已提交未 checkpoint 的数据必须进入 Backup API 备份;
/// 备份可独立只读打开,integrity_check=ok 且与 source 逻辑视图一致。
#[test]
fn wal_committed_uncheckpointed_data_is_captured_by_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    let source = Connection::open(&db).unwrap();
    source.pragma_update(None, "journal_mode", "WAL").unwrap();
    source.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    source
        .execute_batch(
            "CREATE TABLE wal_probe (id INTEGER PRIMARY KEY, payload TEXT NOT NULL);
             INSERT INTO wal_probe (id, payload)
                 VALUES (1, 'committed-in-wal'), (2, 'second-row');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    let wal_path = db.with_file_name(format!("{}-wal", db.file_name().unwrap().to_string_lossy()));
    assert!(
        wal_path.exists() && std::fs::metadata(&wal_path).unwrap().len() > 0,
        "前提:已提交数据仍在 WAL 中未 checkpoint"
    );
    let mut stmt = source
        .prepare("SELECT id, payload FROM wal_probe ORDER BY id")
        .unwrap();
    let source_rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    drop(stmt);

    let backup_dir = migration::backup_dir_for(&db);
    let artifact =
        migration::create_verified_backup(&source, StoreKind::Project, PROJECT_SCHEMA_VERSION)
            .expect("Backup API 必须在库打开中且 WAL 未 checkpoint 时成功");

    // 独立只读打开备份:完整、版本正确、逻辑视图一致
    assert_eq!(integrity_check_of(&artifact.db_path), "ok");
    assert_eq!(user_version_of(&artifact.db_path), 1);
    let backup_conn = read_only(&artifact.db_path);
    let mut stmt = backup_conn
        .prepare("SELECT id, payload FROM wal_probe ORDER BY id")
        .unwrap();
    let backup_rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        backup_rows, source_rows,
        "备份必须与 source 逻辑视图一致(WAL 已提交数据不丢失)"
    );

    // manifest 与备份文件精确配对
    let (_, manifest) = sole_manifest(&backup_dir);
    assert_eq!(manifest["schema"], "mf.backup.manifest.v1");
    assert_eq!(manifest["store_kind"], "project");
    assert_eq!(manifest["source_file"], "workflow-v1.db");
    assert_eq!(manifest["from_version"], 1);
    assert_eq!(manifest["to_version"], PROJECT_SCHEMA_VERSION);
    assert_eq!(manifest["complete"], true);
    assert!(artifact.commit_marker_path.is_file());
    assert_eq!(
        manifest["backup_file"],
        artifact
            .db_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(manifest["sha256"], sha256_file(&artifact.db_path));
    assert_eq!(
        manifest["size_bytes"].as_u64(),
        Some(std::fs::metadata(&artifact.db_path).unwrap().len())
    );
}

/// 真实 Store v1→v6 屏障也必须从 WAL 逻辑视图备份,而不是只在直接调用
/// Backup helper 时成立。
#[test]
fn wal_legacy_store_upgrade_backs_up_uncheckpointed_business_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_legacy_v1_db(&db, &["主文件行"]);
    let holder = Connection::open(&db).unwrap();
    holder.pragma_update(None, "journal_mode", "WAL").unwrap();
    holder.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    holder
        .execute(
            "INSERT INTO agent_tasks
                (title, goal, status, active_revision, created_at, updated_at)
             VALUES ('WAL 行', '', 'draft', NULL, '2024-01-01', '2024-01-01')",
            [],
        )
        .unwrap();
    let wal = db.with_file_name("workflow-v1.db-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    let backup_dir = migration::backup_dir_for(&db);
    let (manifest_path, manifest) = sole_manifest(&backup_dir);
    let backup = manifest_path
        .parent()
        .unwrap()
        .join(manifest["backup_file"].as_str().unwrap());
    let titles: Vec<String> = {
        let connection = read_only(&backup);
        let mut statement = connection
            .prepare("SELECT title FROM agent_tasks ORDER BY id")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        rows
    };
    assert_eq!(titles, vec!["主文件行", "WAL 行"]);
    assert_eq!(user_version_of(&backup), 1);
}

/// v1 → v6 真实升级:恰好发布一对备份+manifest;备份是迁移前快照;
/// 再次打开(已在 v6)不新增备份。
#[test]
fn real_upgrade_publishes_one_paired_backup_and_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_legacy_v1_db(&db, &["迁移前任务"]);

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert_eq!(
        store.list_tasks(false).unwrap().len(),
        1,
        "迁移后业务数据保留"
    );

    let backup_dir = migration::backup_dir_for(&db);
    let entries = migration::published_artifact_dirs(&backup_dir).unwrap();
    assert_eq!(entries.len(), 1, "恰好一个完整 artifact: {entries:?}");

    let (manifest_path, manifest) = sole_manifest(&backup_dir);
    let backup_file = manifest["backup_file"].as_str().unwrap();
    let backup_db = manifest_path.parent().unwrap().join(backup_file);
    assert!(backup_db.is_file(), "manifest 必须与备份文件配对");
    assert_eq!(
        manifest_path.file_name().unwrap().to_string_lossy(),
        "manifest.json"
    );

    // 备份是升级前快照:版本 1、完整、业务数据在
    assert_eq!(user_version_of(&backup_db), 1);
    assert_eq!(integrity_check_of(&backup_db), "ok");
    let title: String = read_only(&backup_db)
        .query_row("SELECT title FROM agent_tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "迁移前任务");
    assert_eq!(manifest["store_kind"], "project");
    assert_eq!(manifest["source_file"], "workflow-v1.db");
    assert_eq!(manifest["from_version"], 1);
    assert_eq!(manifest["to_version"], PROJECT_SCHEMA_VERSION);
    assert_eq!(manifest["complete"], true);
    assert_eq!(manifest["sha256"], sha256_file(&backup_db));
    assert_eq!(
        manifest["size_bytes"].as_u64(),
        Some(std::fs::metadata(&backup_db).unwrap().len())
    );

    // 已在当前版本:重复打开不得新增备份
    drop(store);
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert_eq!(
        migration::published_artifact_dirs(&backup_dir)
            .unwrap()
            .len(),
        1,
        "无 schema 升级不得再触发备份"
    );
}

/// user_version 已是 v1 但缺表/列的 Catalog 仍是 schema repair:必须先
/// 备份残缺旧库,再执行 CREATE/ALTER。
#[test]
fn catalog_v1_repair_is_backed_up_before_any_ddl() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE legacy_marker(id INTEGER PRIMARY KEY, note TEXT NOT NULL);
             INSERT INTO legacy_marker VALUES(1, 'before-repair');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }

    let catalog = CatalogStore::open(&db).unwrap();
    assert_eq!(catalog.schema_version().unwrap(), CATALOG_SCHEMA_VERSION);
    assert!(catalog
        .table_names()
        .unwrap()
        .iter()
        .any(|name| name == "agent_instances"));

    let backup_dir = migration::backup_dir_for(&db);
    let (manifest_path, manifest) = sole_manifest(&backup_dir);
    assert_eq!(manifest["store_kind"], "catalog");
    assert_eq!(manifest["from_version"], 1);
    assert_eq!(manifest["to_version"], 1);
    let backup = manifest_path
        .parent()
        .unwrap()
        .join(manifest["backup_file"].as_str().unwrap());
    let marker: String = read_only(&backup)
        .query_row("SELECT note FROM legacy_marker WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(marker, "before-repair");
    let repaired_table_in_backup: i64 = read_only(&backup)
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_instances'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(repaired_table_in_backup, 0, "备份必须是 repair 前快照");
}

/// 备份失败(备份目录无法创建的文件系统故障注入):迁移与 DDL 不执行,
/// user_version 与 schema 对象不变,失败产物不冒充成功。
#[test]
fn backup_failure_blocks_migration_and_schema_stays_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_legacy_v1_db(&db, &["阻止迁移"]);
    let schema_before = crate::support::schema_objects_of(&db);
    let hash_before = sha256_file(&db);
    let journal_before = crate::support::journal_mode_of(&db);

    // 故障注入:在备份目录路径上放置普通文件,目录创建必然失败。
    // 不依赖机器权限/杀进程,跨平台可移植。
    let backup_dir = migration::backup_dir_for(&db);
    std::fs::write(&backup_dir, b"not-a-directory").unwrap();

    let err = match Store::open(&db) {
        Ok(_) => panic!("备份失败必须阻止迁移与打开"),
        Err(err) => err,
    };
    assert_eq!(
        error_code(&err),
        Some(migration::CODE_SCHEMA_BACKUP_FAILED),
        "稳定错误码: {err:#}"
    );

    assert_eq!(user_version_of(&db), 1, "user_version 不变");
    assert_eq!(sha256_file(&db), hash_before, "源库文件字节不变");
    assert_eq!(
        crate::support::journal_mode_of(&db),
        journal_before,
        "备份失败前不得持久改写 journal_mode"
    );
    assert!(!db.with_file_name("workflow-v1.db-wal").exists());
    assert!(!db.with_file_name("workflow-v1.db-shm").exists());
    assert_eq!(
        crate::support::schema_objects_of(&db),
        schema_before,
        "迁移 DDL 未执行:merge_batches 等 v2+ 表不得出现(schema hash 不变)"
    );
    let title: String = read_only(&db)
        .query_row("SELECT title FROM agent_tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "阻止迁移", "业务数据不动");
    let manifest_files: Vec<String> = dir_entries(tmp.path())
        .into_iter()
        .filter(|n| n.contains("manifest"))
        .collect();
    assert!(
        manifest_files.is_empty(),
        "失败路径不得发布任何 manifest: {manifest_files:?}"
    );
}

/// manifest 只保留恢复元数据:不含 Secret/能力令牌明文,不含环境绝对路径。
#[test]
fn manifest_carries_no_secret_or_environment_material() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_legacy_v1_db(
        &db,
        &["MF_RUN_TOKEN=mft_canary_7f31", "sk-canary-apikey-2f90"],
    );
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);

    let backup_dir = migration::backup_dir_for(&db);
    let (manifest_path, manifest) = sole_manifest(&backup_dir);
    let manifest_raw = std::fs::read_to_string(&manifest_path).unwrap();
    let manifest_strings = json_string_values(&manifest);
    for forbidden in [
        "canary",
        "MF_RUN_TOKEN",
        "sk-",
        tmp.path().to_string_lossy().as_ref(),
    ] {
        assert!(
            !manifest_strings
                .iter()
                .any(|value| value.contains(forbidden)),
            "manifest 不得包含敏感/环境材料 `{forbidden}`: {manifest_raw}"
        );
    }
}

/// Windows 文件占用:另一连接保持库文件打开时,升级 + 备份 + 发布仍完成。
#[test]
fn upgrade_succeeds_while_another_connection_keeps_the_database_open() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_legacy_v1_db(&db, &["占用中升级"]);
    // 模拟并发占用:另一连接完成一次读后保持打开(无未决锁)
    let holder = Connection::open(&db).unwrap();
    let count: i64 = holder
        .query_row("SELECT COUNT(*) FROM agent_tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    let backup_dir = migration::backup_dir_for(&db);
    assert_eq!(
        migration::published_artifact_dirs(&backup_dir)
            .unwrap()
            .len(),
        1,
        "占用不阻止备份发布(恰一个 complete artifact)"
    );
    let (_, manifest) = sole_manifest(&backup_dir);
    assert_eq!(manifest["complete"], true);
}

/// Backup 完成到 migration commit 之间持有 writer reservation:并发业务写
/// 必须等待,不能晚于备份却早于 DDL。
#[test]
fn concurrent_business_write_cannot_land_between_backup_and_migration() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_legacy_v1_db(&db, &["线性化"]);
    let mut connection = Connection::open(&db).unwrap();
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();

    let receiver = std::cell::RefCell::new(None);
    let handle = std::cell::RefCell::new(None);
    migration::upgrade_with_barrier(&mut connection, StoreKind::Project, 2, &|tx, _, _| {
        let writer_path = db.clone();
        let (send, receive) = std::sync::mpsc::channel();
        *receiver.borrow_mut() = Some(receive);
        *handle.borrow_mut() = Some(std::thread::spawn(move || {
            let writer = Connection::open(writer_path).unwrap();
            writer
                .busy_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let result = writer.execute(
                "INSERT INTO agent_tasks
                    (title, goal, status, active_revision, created_at, updated_at)
                 VALUES ('并发写', '', 'draft', NULL, '2024-01-01', '2024-01-01')",
                [],
            );
            send.send(result).unwrap();
        }));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(matches!(
            receiver.borrow().as_ref().unwrap().try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        tx.execute_batch("CREATE TABLE v2_linearized(id INTEGER PRIMARY KEY)")?;
        Ok(())
    })
    .unwrap();

    let write_result = receiver
        .borrow()
        .as_ref()
        .unwrap()
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(write_result.unwrap(), 1);
    handle.borrow_mut().take().unwrap().join().unwrap();
    assert_eq!(user_version_of(&db), 2);
}
