//! 全新存储命名空间验收:项目库与目录库相互独立,且不含任何旧 MonkeyFence 表。
//! schema 版本按 user_version 迁移(I15):全新库直接落在当前版本,
//! 旧 v1 库按链升级且数据保留。

use mf_agent::catalog_store::CatalogStore;
use mf_agent::schema::PROJECT_SCHEMA_VERSION;
use mf_agent::store::Store;
use rusqlite::Connection;

#[test]
fn project_schema_starts_at_current_version_without_legacy_tables() {
    let store = Store::memory().unwrap();
    let tables = store.table_names().unwrap();
    assert!(tables.contains(&"agent_tasks".to_string()));
    assert!(!tables.contains(&"runs".to_string()));
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert!(
        tables.contains(&"merge_batches".to_string()),
        "v2 起包含 join 批持久状态表: {tables:?}"
    );
}

#[test]
fn catalog_schema_is_independent() {
    let catalog = CatalogStore::memory().unwrap();
    assert_eq!(catalog.schema_version().unwrap(), 1);
    let tables = catalog.table_names().unwrap();
    assert!(tables.contains(&"plugin_packages".to_string()));
    assert!(tables.contains(&"plugin_pins".to_string()));
    assert!(!tables.contains(&"agent_tasks".to_string()));
}

/// 真实旧 v1 库 fixture:手工构造 v1 DDL + 业务数据 + user_version=1,
/// 打开后必须事务迁移到当前版本且数据完整。
#[test]
fn legacy_v1_database_is_migrated_with_data_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        // 真实 v1 时期的最小可用库:核心表 + 数据 + user_version=1
        conn.execute_batch(
            "CREATE TABLE agent_tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL,
                goal TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'draft',
                active_revision INTEGER, paused INTEGER NOT NULL DEFAULT 0,
                unread INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL, archived_at TEXT);
             CREATE TABLE pipeline_revisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT, task_id INTEGER NOT NULL,
                revision INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'draft',
                snapshot_json TEXT, created_at TEXT NOT NULL, UNIQUE(task_id, revision));
             INSERT INTO agent_tasks (title, goal, status, active_revision, created_at, updated_at)
                 VALUES ('旧任务', '旧目标', 'running', NULL, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let store = Store::open(&db).unwrap();
    assert_eq!(
        store.schema_version().unwrap(),
        PROJECT_SCHEMA_VERSION,
        "旧 v1 库必须按 user_version 链升级到当前版本"
    );
    let tasks = store.list_tasks(false).unwrap();
    assert_eq!(tasks.len(), 1, "迁移不得丢失业务数据");
    assert_eq!(tasks[0].title, "旧任务");
    let tables = store.table_names().unwrap();
    assert!(tables.contains(&"merge_batches".to_string()));
    assert!(tables.contains(&"join_deferrals".to_string()));
    // 迁移后的库可正常写入新状态
    assert!(store
        .claim_merge_batch(1, "j", 1, &["l1".into()], "txn-x")
        .unwrap());
    assert!(!store
        .claim_merge_batch(1, "j", 1, &["l1".into()], "txn-y")
        .unwrap());
    store.complete_merge_batch("txn-x", false).unwrap();
    assert_eq!(store.list_merge_batches(1).unwrap()[0].status, "merged");
}

/// 版本高于程序支持的库必须拒绝打开(禁止隐式降级)。
#[test]
fn future_schema_version_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_tasks (id INTEGER PRIMARY KEY);
             PRAGMA user_version = 99;",
        )
        .unwrap();
    }
    let err = Store::open(&db).err().expect("高版本库必须拒绝打开");
    assert!(
        format!("{err:#}").contains("高于程序支持"),
        "错误必须明示版本不兼容: {err:#}"
    );
}
