//! T1a 契约(Issue #16):统一 schema future guard。
//!
//! 任一 Store 的 `user_version` 高于程序已知版本时,打开必须 fail-closed
//! 并返回稳定的 `schema_future_version` 错误码;拒绝路径不执行任何 DDL、
//! 不改写 WAL/journal 模式、不留下备份或 manifest、数据库文件字节不变。
//! 全部使用 tempfile fixture,不触碰真实用户目录。

use crate::support::{
    build_legacy_v1_db, dir_entries, journal_mode_of, schema_objects_of, sha256_file,
    user_version_of,
};
use mf_agent::catalog_store::CatalogStore;
use mf_agent::migration::{self, error_code, MigrationError, StoreKind};
use mf_agent::schema::{CATALOG_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION};
use mf_agent::store::Store;
use rusqlite::Connection;

/// Project 库 `user_version = 7`(> 6):打开拒绝、错误码稳定、库不动。
#[test]
fn project_future_version_fails_closed_without_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_tasks (id INTEGER PRIMARY KEY, title TEXT NOT NULL);
             INSERT INTO agent_tasks (id, title) VALUES (1, 'future-marker');
             PRAGMA user_version = 7;",
        )
        .unwrap();
    }
    let hash_before = sha256_file(&db);
    let schema_before = schema_objects_of(&db);
    assert_eq!(
        journal_mode_of(&db),
        "delete",
        "fixture 起点必须是默认 journal 模式(最敏感的改写场景)"
    );

    let err = match Store::open(&db) {
        Ok(_) => panic!("v7 项目库必须 fail-closed"),
        Err(err) => err,
    };
    assert_eq!(
        error_code(&err),
        Some(migration::CODE_SCHEMA_FUTURE_VERSION),
        "必须返回稳定错误码而非脆弱 substring: {err:#}"
    );
    match err.downcast_ref::<MigrationError>() {
        Some(MigrationError::FutureVersion {
            store,
            found,
            known,
        }) => {
            assert_eq!(*store, StoreKind::Project);
            assert_eq!(*found, 7);
            assert_eq!(*known, PROJECT_SCHEMA_VERSION);
        }
        other => panic!("必须是 FutureVersion 判别值: {other:?}"),
    }

    assert_eq!(
        sha256_file(&db),
        hash_before,
        "拒绝路径不得改写数据库文件字节"
    );
    assert_eq!(user_version_of(&db), 7, "user_version 不变");
    assert_eq!(
        journal_mode_of(&db),
        "delete",
        "拒绝路径不得改写 WAL/journal 模式"
    );
    assert_eq!(
        schema_objects_of(&db),
        schema_before,
        "拒绝路径不得执行任何 DDL"
    );
    assert!(
        !migration::backup_dir_for(&db).exists(),
        "拒绝路径不得留下备份目录/manifest"
    );
    assert_eq!(
        dir_entries(tmp.path()),
        vec!["workflow-v1.db".to_string()],
        "目录内不得出现任何新伴随文件"
    );
}

/// Catalog 库 `user_version = 2`(> 1):打开拒绝,v1 DDL 一个字节都不执行
/// (现状缺陷:`initialize_schema` 会无条件回写 user_version=1)。
#[test]
fn catalog_future_version_fails_closed_without_v1_ddl() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE marker (id INTEGER PRIMARY KEY, note TEXT NOT NULL);
             INSERT INTO marker (id, note) VALUES (1, 'v2-data');
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }
    let hash_before = sha256_file(&db);
    let schema_before = schema_objects_of(&db);

    let err = match CatalogStore::open(&db) {
        Ok(_) => panic!("v2 目录库必须 fail-closed"),
        Err(err) => err,
    };
    assert_eq!(
        error_code(&err),
        Some(migration::CODE_SCHEMA_FUTURE_VERSION),
        "错误码: {err:#}"
    );
    match err.downcast_ref::<MigrationError>() {
        Some(MigrationError::FutureVersion {
            store,
            found,
            known,
        }) => {
            assert_eq!(*store, StoreKind::Catalog);
            assert_eq!(*found, 2);
            assert_eq!(*known, CATALOG_SCHEMA_VERSION);
        }
        other => panic!("必须是 FutureVersion 判别值: {other:?}"),
    }

    assert_eq!(user_version_of(&db), 2, "不得回写 user_version");
    assert_eq!(sha256_file(&db), hash_before, "数据库文件字节不变");
    assert_eq!(
        schema_objects_of(&db),
        schema_before,
        "不得执行 catalog v1 DDL(agent_instances 等表不得被创建)"
    );
    assert!(
        !schema_before
            .iter()
            .any(|(name, _)| name.contains("agent_instances")),
        "fixture 本身不含 v1 表: {schema_before:?}"
    );
    let note: String = crate::support::read_only(&db)
        .query_row("SELECT note FROM marker WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(note, "v2-data", "业务数据不动");
    assert_eq!(journal_mode_of(&db), "delete", "journal 模式不变");
    assert!(!migration::backup_dir_for(&db).exists(), "不得留下备份产物");
    assert_eq!(dir_entries(tmp.path()), vec!["catalog-v1.db".to_string()]);
}

/// 当前版本(Project v6 / Catalog v1)正常打开,不产生任何多余备份:
/// 只有确实需要 schema 升级才触发 backup。
#[test]
fn current_version_opens_without_backup_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let project_db = tmp.path().join("workflow-v1.db");
    let store = Store::open(&project_db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert!(
        !migration::backup_dir_for(&project_db).exists(),
        "全新库初始化(user_version=0)不是 schema 升级,不得备份"
    );

    let catalog_db = tmp.path().join("catalog-v1.db");
    let catalog = CatalogStore::open(&catalog_db).unwrap();
    assert_eq!(catalog.schema_version().unwrap(), CATALOG_SCHEMA_VERSION);
    assert!(
        !migration::backup_dir_for(&catalog_db).exists(),
        "v1 目录库无升级,不得备份"
    );
}

/// 初次 guard 之后若另一个 writer 提交 future schema,等待 writer lock 的旧
/// 进程必须在锁内重读并拒绝,不能继续 DDL/把版本降回 target。
#[test]
fn writer_lock_recheck_rejects_concurrent_future_migration() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_legacy_v1_db(&db, &["并发 future"]);

    let future_writer = Connection::open(&db).unwrap();
    future_writer
        .execute_batch("BEGIN IMMEDIATE; PRAGMA user_version = 99;")
        .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let open_path = db.clone();
    let opener = std::thread::spawn(move || {
        let result = Store::open(&open_path).map(|_| ());
        tx.send(result).unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        matches!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
        "旧进程应阻塞在 BEGIN IMMEDIATE,等待竞争迁移提交"
    );

    future_writer.execute_batch("COMMIT").unwrap();
    let error = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    opener.join().unwrap();
    assert_eq!(
        error_code(&error),
        Some(migration::CODE_SCHEMA_FUTURE_VERSION)
    );
    assert_eq!(user_version_of(&db), 99, "future 版本不得被降回 v6");
    assert!(!migration::backup_dir_for(&db).exists());
    let tables = schema_objects_of(&db);
    assert!(
        !tables
            .iter()
            .any(|(name, _)| name.contains("merge_batches")),
        "旧进程不得执行 v2+ DDL"
    );
}

#[test]
fn catalog_writer_lock_recheck_rejects_concurrent_future_version() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog-v1.db");
    drop(CatalogStore::open(&db).unwrap());

    let future_writer = Connection::open(&db).unwrap();
    future_writer
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    future_writer
        .execute_batch("BEGIN IMMEDIATE; PRAGMA user_version = 2;")
        .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let open_path = db.clone();
    let opener = std::thread::spawn(move || {
        let result = CatalogStore::open(&open_path).map(|_| ());
        tx.send(result).unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(matches!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));

    future_writer.execute_batch("COMMIT").unwrap();
    let error = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    opener.join().unwrap();
    assert_eq!(
        error_code(&error),
        Some(migration::CODE_SCHEMA_FUTURE_VERSION)
    );
    assert_eq!(user_version_of(&db), 2);
    assert!(!migration::backup_dir_for(&db).exists());
    let table_names: Vec<String> = schema_objects_of(&db)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(table_names
        .iter()
        .any(|name| name == "table:agent_instances"));
}
