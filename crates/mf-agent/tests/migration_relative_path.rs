//! Issue #16:裸相对数据库路径也必须完成 backup→migration,目录 durability
//! 不得把空 parent 传给平台 flush。独立 test binary,切换 cwd 不影响其他测试。

use mf_agent::migration::{backup_dir_for, published_artifact_dirs};
use mf_agent::schema::PROJECT_SCHEMA_VERSION;
use mf_agent::Store;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

struct CwdGuard(PathBuf);

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("恢复测试 cwd");
    }
}

#[test]
fn relative_project_db_path_publishes_durable_backup_then_migrates() {
    let temp = tempfile::tempdir().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    let _cwd = CwdGuard(previous);

    let db = Path::new("workflow-v1.db");
    let connection = Connection::open(db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE agent_tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL,
                goal TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'draft',
                active_revision INTEGER, paused INTEGER NOT NULL DEFAULT 0,
                unread INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL, archived_at TEXT);
             CREATE TABLE pipeline_revisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT, task_id INTEGER NOT NULL,
                revision INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'draft',
                snapshot_json TEXT, created_at TEXT NOT NULL,
                UNIQUE(task_id, revision));
             PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    let store = Store::open(db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    drop(store);
    let artifacts = published_artifact_dirs(&backup_dir_for(db)).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].join("COMPLETE").is_file());
}
