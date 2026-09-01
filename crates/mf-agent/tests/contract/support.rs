//! 契约测试共享 fixture 助手:全部基于 tempfile 独立数据库,
//! 不解析真实用户目录,不触碰 `~/.monkeyfence` 或真实 Project 目录。

use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

pub fn sha256_file(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(fs::read(path).unwrap());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 独立只读连接:任何校验都不经由生产写路径。
pub fn read_only(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

pub fn user_version_of(path: &Path) -> i64 {
    read_only(path)
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

pub fn journal_mode_of(path: &Path) -> String {
    read_only(path)
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap()
}

pub fn integrity_check_of(path: &Path) -> String {
    read_only(path)
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap()
}

/// schema 对象规范化快照(`type:name → DDL`):证明「DDL 未执行」
/// 不依赖脆弱的文件字节比较。
pub fn schema_objects_of(path: &Path) -> Vec<(String, String)> {
    let conn = read_only(path);
    let mut stmt = conn
        .prepare(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            r.get::<_, String>(2)?,
        ))
    })
    .unwrap()
    .collect::<std::result::Result<Vec<_>, _>>()
    .unwrap()
}

pub fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// 与 fresh_schema 相同口径的最小 v1 项目库 fixture:
/// 核心表(agent_tasks + pipeline_revisions,后者被 TaskView 子查询引用)
/// + 业务行 + `user_version = 1`(迁移起点)。
pub fn build_legacy_v1_db(db: &Path, titles: &[&str]) {
    let conn = Connection::open(db).unwrap();
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
            snapshot_json TEXT, created_at TEXT NOT NULL,
            UNIQUE(task_id, revision));
         PRAGMA user_version = 1;",
    )
    .unwrap();
    for title in titles {
        conn.execute(
            "INSERT INTO agent_tasks
                (title, goal, status, active_revision, created_at, updated_at)
             VALUES (?1, '旧目标', 'running', NULL,
                     '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![title],
        )
        .unwrap();
    }
}

/// 取出目录中唯一 manifest 及其解析值(备份 db 与 manifest 应恰为一对)。
pub fn sole_manifest(dir: &Path) -> (PathBuf, serde_json::Value) {
    let artifacts = mf_agent::migration::published_artifact_dirs(dir).unwrap();
    assert_eq!(
        artifacts.len(),
        1,
        "必须恰好一个完整 artifact:{artifacts:?}"
    );
    let manifest = artifacts[0].join("manifest.json");
    let raw = fs::read_to_string(&manifest).unwrap();
    let value = serde_json::from_str(&raw).unwrap();
    (manifest, value)
}

pub fn json_string_values(value: &serde_json::Value) -> Vec<&str> {
    fn visit<'a>(value: &'a serde_json::Value, output: &mut Vec<&'a str>) {
        match value {
            serde_json::Value::String(value) => output.push(value),
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, output);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values() {
                    visit(value, output);
                }
            }
            _ => {}
        }
    }
    let mut output = Vec::new();
    visit(value, &mut output);
    output
}
