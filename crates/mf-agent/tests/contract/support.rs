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

// ---------------------------------------------------------------------------
// T1b(Issue #17)v6→v7 迁移契约 fixture 与 schema 精确断言助手
// ---------------------------------------------------------------------------

/// `PRAGMA table_info` 的完整列元组:(cid, name, type, notnull, dflt, pk)。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMeta {
    pub name: String,
    pub col_type: String,
    pub notnull: bool,
    pub dflt: Option<String>,
    pub pk: bool,
}

/// 精确读取列定义(不做 substring 判断)。
pub fn columns_of(path: &Path, table: &str) -> Vec<ColumnMeta> {
    let conn = read_only(path);
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let cols = stmt
        .query_map([], |r| {
            Ok(ColumnMeta {
                name: r.get(1)?,
                col_type: r.get(2)?,
                notnull: r.get::<_, i64>(3)? != 0,
                dflt: r.get(4)?,
                pk: r.get::<_, i64>(5)? != 0,
            })
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert!(!cols.is_empty(), "表 {table} 必须存在");
    cols
}

pub fn column_of(path: &Path, table: &str, column: &str) -> ColumnMeta {
    columns_of(path, table)
        .into_iter()
        .find(|c| c.name == column)
        .unwrap_or_else(|| panic!("表 {table} 缺列 {column}"))
}

/// 非部分唯一索引覆盖的列集合(`PRAGMA index_list` + `index_info`)。
pub fn unique_index_sets(path: &Path, table: &str) -> Vec<Vec<String>> {
    let conn = read_only(path);
    let mut stmt = conn
        .prepare(&format!("PRAGMA index_list({table})"))
        .unwrap();
    let indexes: Vec<(String, bool, bool)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(1)?,
                r.get::<_, i64>(2)? != 0,
                r.get::<_, i64>(4)? != 0,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    drop(stmt);
    indexes
        .into_iter()
        .filter(|(_, unique, partial)| *unique && !*partial)
        .map(|(name, _, _)| {
            let mut info = conn.prepare(&format!("PRAGMA index_info({name})")).unwrap();
            let cols: Vec<String> = info
                .query_map([], |r| r.get(2))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            cols
        })
        .collect()
}

/// 断言 `{table}` 上存在覆盖恰好 `cols` 列的唯一索引。
pub fn assert_unique_index(path: &Path, table: &str, cols: &[&str]) {
    let wanted: Vec<String> = cols.iter().map(|c| c.to_string()).collect();
    let sets = unique_index_sets(path, table);
    assert!(
        sets.iter().any(|set| *set == wanted),
        "{table} 必须有唯一索引覆盖 {cols:?},实际: {sets:?}"
    );
}

/// `PRAGMA foreign_key_list` 行:(from 列, 引用表, 引用列, on_delete)。
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKeyMeta {
    pub from_column: String,
    pub ref_table: String,
    pub ref_column: String,
    pub on_delete: String,
}

pub fn foreign_keys_of(path: &Path, table: &str) -> Vec<ForeignKeyMeta> {
    let conn = read_only(path);
    let mut stmt = conn
        .prepare(&format!("PRAGMA foreign_key_list({table})"))
        .unwrap();
    stmt.query_map([], |r| {
        Ok(ForeignKeyMeta {
            from_column: r.get(3)?,
            ref_table: r.get(2)?,
            ref_column: r.get(4)?,
            on_delete: r.get(6)?,
        })
    })
    .unwrap()
    .collect::<std::result::Result<Vec<_>, _>>()
    .unwrap()
}

/// 解析为真实 UUIDv7(版本号 4 bit 必须是 7)。
pub fn is_uuid_v7(handle: &str) -> bool {
    uuid::Uuid::parse_str(handle)
        .map(|u| u.get_version_num() == 7)
        .unwrap_or(false)
}

/// v6 项目库 fixture:经生产链(`upgrade_project(.., 6)`)构建真实 v6 schema,
/// 再注入小规模业务行 + 项目工作流。工作流图以 `graph_json` 原文写入,
/// 便于构造多 workflow / 多依赖 / 损坏图等场景。
pub fn build_v6_project_db(db: &Path, workflows: &[(&str, &str, &str)]) {
    let mut conn = Connection::open(db).unwrap();
    mf_agent::schema::upgrade_project(&mut conn, 6).unwrap();
    conn.execute_batch(
        "INSERT INTO agent_tasks
             (id, title, goal, status, active_revision, created_at, updated_at)
         VALUES (1, 'v6任务', 'g', 'running', NULL, '2024-01-01', '2024-01-01');
         INSERT INTO pipeline_revisions
             (id, task_id, revision, status, created_at)
         VALUES (1, 1, 1, 'draft', '2024-01-01');
         INSERT INTO steps
             (id, revision_id, task_id, step_key, title, agent_profile, status,
              created_at, updated_at)
         VALUES (1, 1, 1, 's1', 'S1', 'p', 'pending', '2024-01-01', '2024-01-01');
         INSERT INTO agent_sessions
             (id, session_key, runtime, agent_profile, title, status,
              created_at, updated_at)
         VALUES (1, 'k', 'pty', 'p', '会话', 'dead', '2024-01-01', '2024-01-01');
         INSERT INTO agent_runs
             (id, task_id, step_id, revision_id, session_id, status,
              capability_token, started_at)
         VALUES (1, 1, 1, 1, 1, 'succeeded', 'mft_v6_1', '2024-01-01');
         INSERT INTO ad_hoc_sessions
             (id, task_id, title, status, snapshot_json, created_at)
         VALUES (1, 1, '离散', 'dead', '{}', '2024-01-01');",
    )
    .unwrap();
    for (key, name, graph_json) in workflows {
        conn.execute(
            "INSERT INTO project_workflows
                 (workflow_key, name, graph_json, allow_unsafe_parallel,
                  content_digest, created_at, updated_at)
             VALUES (?1, ?2, ?3, 0, 'digest-v6', '2024-01-01', '2024-01-01')",
            rusqlite::params![key, name, graph_json],
        )
        .unwrap();
    }
    drop(conn);
}

/// 由 `WorkflowNodeDraft` 序列化出的 graph_json(与生产写入口径一致)。
pub fn graph_json(nodes: &[(&str, &[&str])]) -> String {
    use mf_agent::workflow::WorkflowNodeDraft;
    let drafts: Vec<WorkflowNodeDraft> = nodes
        .iter()
        .map(|(key, deps)| WorkflowNodeDraft {
            key: key.to_string(),
            title: format!("节点 {key}"),
            instructions: "固定指令".to_string(),
            agent_instance_id: "fixture-inst".to_string(),
            deps: deps.iter().map(|d| d.to_string()).collect(),
        })
        .collect();
    serde_json::to_string(&drafts).unwrap()
}

/// 全部业务表的规范化快照(键按表名排序,行按 rowid):
/// 故障注入后与注入前逐值比较,证明源业务数据不动。
pub fn business_snapshot(path: &Path) -> serde_json::Value {
    let tables = [
        "agent_tasks",
        "pipeline_revisions",
        "steps",
        "step_deps",
        "agent_sessions",
        "agent_runs",
        "events",
        "step_questions",
        "ad_hoc_sessions",
        "task_workflows",
        "handoffs",
        "execution_leases",
        "pending_merges",
        "join_deferrals",
        "merge_batches",
        "project_workflows",
    ];
    let conn = read_only(path);
    let mut out = serde_json::Map::new();
    for table in tables {
        // v1 残缺库可能缺表:快照对缺表记 null(两侧一致即可)
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                [table],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        if !exists {
            out.insert(table.to_string(), serde_json::Value::Null);
            continue;
        }
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let col_count = stmt.column_count();
        let rows: Vec<serde_json::Value> = stmt
            .query_map([], move |r| {
                let mut row = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    let value = match r.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                        rusqlite::types::ValueRef::Integer(v) => serde_json::json!(v),
                        rusqlite::types::ValueRef::Real(v) => serde_json::json!(v),
                        rusqlite::types::ValueRef::Text(v) => {
                            serde_json::json!(String::from_utf8_lossy(v))
                        }
                        rusqlite::types::ValueRef::Blob(v) => {
                            serde_json::json!(format!("blob:{}", v.len()))
                        }
                    };
                    row.push(value);
                }
                Ok(serde_json::Value::Array(row))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        out.insert(table.to_string(), serde_json::Value::Array(rows));
    }
    serde_json::Value::Object(out)
}

/// 全部持久 handle 与 revision 的稳定性快照:
/// `{表: [public_handle 排序列表]}` + identity/presentation/position 行。
pub fn handle_snapshot(path: &Path) -> serde_json::Value {
    let conn = read_only(path);
    let mut out = serde_json::Map::new();
    for table in [
        "agent_tasks",
        "pipeline_revisions",
        "steps",
        "agent_sessions",
        "agent_runs",
        "ad_hoc_sessions",
        "project_workflows",
    ] {
        let mut stmt = conn
            .prepare(&format!("SELECT public_handle FROM {table} ORDER BY rowid"))
            .unwrap();
        let handles: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        out.insert(
            table.to_string(),
            serde_json::Value::Array(handles.into_iter().map(Into::into).collect()),
        );
    }
    for (key, sql) in [
        ("workflow_node_identity", "SELECT workflow_handle, node_key, node_handle FROM workflow_node_identity ORDER BY workflow_handle, node_key"),
        ("workflow_edge_identity", "SELECT workflow_handle, upstream_node_key, downstream_node_key, edge_handle FROM workflow_edge_identity ORDER BY workflow_handle, upstream_node_key, downstream_node_key"),
        ("project_meta", "SELECT CAST(id AS TEXT), CAST(workflow_collection_revision AS TEXT) FROM project_meta ORDER BY id"),
    ] {
        let mut stmt = conn.prepare(sql).unwrap();
        let col_count = stmt.column_count();
        let rows: Vec<serde_json::Value> = stmt
            .query_map([], move |r| {
                let mut row = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    row.push(serde_json::json!(r.get::<_, String>(i)?));
                }
                Ok(serde_json::Value::Array(row))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        out.insert(key.to_string(), serde_json::Value::Array(rows));
    }
    serde_json::Value::Object(out)
}

/// 收集库内全部持久 handle(含 node/edge identity)用于全局唯一性断言。
pub fn all_persistent_handles(path: &Path) -> Vec<String> {
    let conn = read_only(path);
    let mut handles = Vec::new();
    for table in [
        "agent_tasks",
        "pipeline_revisions",
        "steps",
        "agent_sessions",
        "agent_runs",
        "ad_hoc_sessions",
        "project_workflows",
    ] {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT public_handle FROM {table} WHERE public_handle <> ''"
            ))
            .unwrap();
        handles.extend(
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap(),
        );
    }
    for sql in [
        "SELECT node_handle FROM workflow_node_identity",
        "SELECT edge_handle FROM workflow_edge_identity",
    ] {
        let mut stmt = conn.prepare(sql).unwrap();
        handles.extend(
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap(),
        );
    }
    handles
}
