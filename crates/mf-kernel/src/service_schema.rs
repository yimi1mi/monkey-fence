//! service-v1.db 的 schema 常量、DDL 与 future guard(canonical spec §3.4)。
//!
//! service-v1 是跨项目协调状态库(`~/.monkeyfence/service-v1.db`):Project
//! Registry、command intent、Operation、audit、Root 状态、durable feature
//! 注册表与迁移 marker。T1d 只建立 schema 与 session.json 导入;
//! command_intent/operation/audit 的写路径属后续 ticket(§4/§10),本库
//! 当前无人写入。
//!
//! future guard 与 `crates/mf-agent/src/migration.rs` 同一契约:打开时
//! `user_version > SERVICE_SCHEMA_VERSION` → 在任何 DDL/pragma 改写之前
//! fail-closed(稳定错误码 `schema_future_version`,spec §7.5),拒绝路径
//! 不改文件字节、不留 sidecar。service 库是新版本链(无旧库可迁),
//! `user_version = 0` 视为全新库初始化,不触发备份。

use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// service 库 schema 版本(新库新版本链,从 v1 起)。
pub const SERVICE_SCHEMA_VERSION: i64 = 4;

/// 稳定错误码:与主规格 §7.5 problem code `schema_future_version` 对齐。
pub const CODE_SCHEMA_FUTURE_VERSION: &str = "schema_future_version";

/// service 库打开失败的稳定判别错误(不止靠脆弱 substring)。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServiceSchemaError {
    /// user_version 高于程序已知版本:在任何 DDL 之前 fail-closed。
    #[error(
        "schema_future_version:service 库 user_version v{found} 高于程序支持的 v{known},拒绝打开"
    )]
    FutureVersion { found: i64, known: i64 },
}

impl ServiceSchemaError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::FutureVersion { .. } => CODE_SCHEMA_FUTURE_VERSION,
        }
    }
}

/// 集中判别:anyhow 错误链中的 [`ServiceSchemaError`] → 稳定错误码。
pub fn error_code(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<ServiceSchemaError>()
        .map(|error| error.code())
}

/// future guard:读 user_version,高于 `known` 立即拒绝。
/// 必须在任何 DDL 与 journal 模式等改写性操作之前调用。
pub fn guard_future_version(conn: &Connection, known: i64) -> Result<()> {
    let found = schema_version_of(conn)?;
    if found > known {
        return Err(ServiceSchemaError::FutureVersion { found, known }.into());
    }
    Ok(())
}

/// 读取 `user_version`。
pub fn schema_version_of(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?)
}

/// 列出库中用户表名(按名称排序;不含 `sqlite_%` 内部表)。
pub fn table_names_of(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names)
}

/// service 库路径:`~/.monkeyfence/service-v1.db`。
/// 测试与嵌入式场景可用 `MF_SERVICE_DB` 重定向,避免触碰用户真实目录
/// (与 `MF_CATALOG_DB` 同一约定)。
pub fn service_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("MF_SERVICE_DB") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".monkeyfence")
        .join("service-v1.db")
}

/// service 库 v1 DDL(§3.4 全部 8 张表;幂等 IF NOT EXISTS)。
///
/// 约束取值来源:project_registry.status / command_intent.state /
/// root_state.mode 由 §3.4 列出;command_intent 的 `revoked` 与
/// root_state 的 `enabling/disabling`、operation 的完整状态集分别来自
/// §4.2 与 §10.1 的状态机(同一 canonical spec 的权威枚举)。
/// `audit` 是 append-only(§3.4):行一经写入不改写;retention 删除由
/// §4.6 GC 负责,因此不加阻断 DELETE 的触发器。
pub const SERVICE_SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS meta (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    instance_id TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    owner_epoch INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS project_registry (
    project_handle TEXT PRIMARY KEY,
    public_id TEXT NOT NULL UNIQUE,
    canonical_root TEXT NOT NULL UNIQUE,
    display_path TEXT NOT NULL,
    registered_at TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('registered', 'missing'))
);
CREATE TABLE IF NOT EXISTS command_intent (
    command_id TEXT PRIMARY KEY,
    semantic_digest TEXT NOT NULL,
    target_store TEXT NOT NULL,
    aggregate TEXT NOT NULL,
    principal TEXT NOT NULL,
    client_id TEXT NOT NULL,
    controller_epoch INTEGER NOT NULL,
    root_epoch INTEGER,
    state TEXT NOT NULL CHECK(state IN ('reserved', 'applied', 'failed', 'cancelled', 'revoked')),
    created_at TEXT NOT NULL,
    resolved_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_command_intent_state ON command_intent(state);
CREATE TABLE IF NOT EXISTS operation (
    operation_handle TEXT PRIMARY KEY,
    command_id TEXT NOT NULL REFERENCES command_intent(command_id),
    kind TEXT NOT NULL,
    state TEXT NOT NULL
        CHECK(state IN ('accepted', 'running', 'completed', 'compensating', 'needs_you', 'reconciling')),
    saga_state TEXT NOT NULL DEFAULT '',
    progress_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_operation_command ON operation(command_id);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    summary_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_created_at ON audit(created_at);
CREATE TRIGGER IF NOT EXISTS audit_immutable_update
BEFORE UPDATE ON audit
BEGIN SELECT RAISE(ABORT, 'audit_immutable'); END;
CREATE TABLE IF NOT EXISTS root_state (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    mode TEXT NOT NULL DEFAULT 'off' CHECK(mode IN ('off', 'enabling', 'on', 'disabling')),
    root_epoch INTEGER NOT NULL DEFAULT 0,
    enabled_at TEXT
);
CREATE TABLE IF NOT EXISTS durable_feature (
    feature TEXT PRIMARY KEY,
    min_reader_version TEXT NOT NULL,
    writer_enabled_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS migration_marker (
    name TEXT PRIMARY KEY,
    payload_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL
);
";

/// T1f 增量：保留 terminal command 的稳定 problem code，使相同
/// command id + digest 跨重启仍返回原错误，而不是 generic internal。
pub const SERVICE_SCHEMA_V2_DELTA: &str = "
ALTER TABLE command_intent ADD COLUMN problem_code TEXT;
";

/// T1g 增量(Issue #22,§4/§4.2):Operation saga 的幂等 step receipt
/// 协调表。每个 step 在 accept 时冻结身份(target store/aggregate/
/// semantic_digest/expected/compensates)并以 `pending` 落盘;执行只由
/// durable target receipt 推进(`succeeded`),重启 reconcile 只终结
/// (`revoked`/`failed`),绝不重放业务写。行随所属 operation 级联清理
/// (GC 只删终态 operation,§4.6)。`operation` 表(§3.4 固定列)不动:
/// 终态 problem 记录在 `progress_json` 的稳定 DTO 内。
pub const SERVICE_SCHEMA_V3_DELTA: &str = "
CREATE TABLE IF NOT EXISTS operation_step (
    operation_handle TEXT NOT NULL
        REFERENCES operation(operation_handle) ON DELETE CASCADE,
    step_index INTEGER NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('forward', 'compensate')),
    step_id TEXT NOT NULL UNIQUE,
    target_store TEXT NOT NULL,
    aggregate TEXT NOT NULL,
    semantic_digest TEXT NOT NULL,
    expected_json TEXT NOT NULL DEFAULT '[]',
    compensates INTEGER,
    state TEXT NOT NULL
        CHECK(state IN ('pending', 'succeeded', 'failed', 'revoked')),
    result_json TEXT NOT NULL DEFAULT '{}',
    problem_code TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(operation_handle, step_index)
);
CREATE INDEX IF NOT EXISTS idx_operation_step_state
    ON operation_step(operation_handle, state);
";

/// RunControl capability authority/index。令牌只以 keyed HMAC 落盘；
/// `project_handle` / `agent_run_handle` 均是 opaque handle。冲突映射不
/// 猜测 winner，而是持久进入 `quarantined`，供 resolve 稳定返回多命中。
pub const SERVICE_SCHEMA_V4_DELTA: &str = "
CREATE TABLE IF NOT EXISTS run_capability (
    token_hmac TEXT NOT NULL PRIMARY KEY,
    project_handle TEXT NOT NULL
        REFERENCES project_registry(project_handle),
    agent_run_handle TEXT NOT NULL,
    state TEXT NOT NULL
        CHECK(state IN ('active', 'settled', 'revoked', 'quarantined')),
    issued_at TEXT NOT NULL,
    revoked_at TEXT,
    UNIQUE(project_handle, agent_run_handle)
);
CREATE INDEX IF NOT EXISTS idx_run_capability_project_state
    ON run_capability(project_handle, state);
CREATE INDEX IF NOT EXISTS idx_run_capability_agent_run
    ON run_capability(agent_run_handle);
";

/// 与 DDL 配套的 singleton 种子行(初始化事务内与 DDL 同事务执行)。
/// `meta.instance_id` 是该 service 库的持久实例身份(建库时生成一次,
/// 重开不变);`root_state` 建库即 `mode=off`(§3.4:Core 启动强制 off)。
pub(crate) fn seed_singletons(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO meta (id, instance_id, schema_version, owner_epoch)
         VALUES (1, ?1, ?2, 0)",
        rusqlite::params![uuid::Uuid::now_v7().to_string(), SERVICE_SCHEMA_VERSION],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO root_state (id, mode, root_epoch, enabled_at)
         VALUES (1, 'off', 0, NULL)",
        [],
    )?;
    Ok(())
}

/// 当前版本(v4)精确指纹:V1 全量 + V2/V3/V4 delta。
pub(crate) fn service_schema_ready(conn: &Connection) -> Result<bool> {
    service_schema_version_ready(conn, SERVICE_SCHEMA_VERSION)
}

pub(crate) fn service_schema_v1_ready(conn: &Connection) -> Result<bool> {
    service_schema_version_ready(conn, 1)
}

/// 既有 v2 库升级前的完整性校验指纹。
pub(crate) fn service_schema_v2_ready(conn: &Connection) -> Result<bool> {
    service_schema_version_ready(conn, 2)
}

/// 既有 v3 库升级前的完整性校验指纹。
pub(crate) fn service_schema_v3_ready(conn: &Connection) -> Result<bool> {
    service_schema_version_ready(conn, 3)
}

/// 指定历史版本的完整 DDL 指纹(逐版本链式应用;同为该 user_version 的
/// 其他/残缺 SQLite 文件不得被误当成 service 库,静默接受)。
fn service_schema_version_ready(conn: &Connection, version: i64) -> Result<bool> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SERVICE_SCHEMA_V1)?;
    if version >= 2 {
        expected.execute_batch(SERVICE_SCHEMA_V2_DELTA)?;
    }
    if version >= 3 {
        expected.execute_batch(SERVICE_SCHEMA_V3_DELTA)?;
    }
    if version >= 4 {
        expected.execute_batch(SERVICE_SCHEMA_V4_DELTA)?;
    }
    Ok(schema_fingerprint(conn)? == schema_fingerprint(&expected)?)
}

type SchemaObject = (String, String, String, String);

/// 比较 SQLite 保存的完整 DDL，而不是只比较名字/列集合：PK、UNIQUE、
/// FK、CHECK、NOT NULL、索引列与 trigger body 都属于 v1 指纹。
fn schema_fingerprint(conn: &Connection) -> Result<Vec<SchemaObject>> {
    let mut stmt = conn.prepare(
        "SELECT type, name, tbl_name, sql
         FROM sqlite_master
         WHERE type IN ('table', 'index', 'trigger')
           AND name NOT LIKE 'sqlite_%'
           AND sql IS NOT NULL
         ORDER BY type, name",
    )?;
    let objects = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(objects)
}

pub(crate) fn validate_singletons(conn: &Connection) -> Result<()> {
    let meta: (i64, String, i64) = conn.query_row(
        "SELECT id, instance_id, schema_version FROM meta",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    anyhow::ensure!(
        meta.0 == 1 && !meta.1.is_empty() && meta.2 == SERVICE_SCHEMA_VERSION,
        "service_schema_mismatch:meta singleton 损坏"
    );
    let meta_count: i64 = conn.query_row("SELECT COUNT(*) FROM meta", [], |row| row.get(0))?;
    let root_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM root_state WHERE id=1", [], |row| {
            row.get(0)
        })?;
    anyhow::ensure!(
        meta_count == 1 && root_count == 1,
        "service_schema_mismatch:singleton 行缺失或重复"
    );
    Ok(())
}

/// service 库所在目录是否为专用 `.monkeyfence`(ACL 收紧目标)。
pub(crate) fn is_service_home(parent: &Path) -> bool {
    matches!(
        parent.file_name().and_then(|name| name.to_str()),
        Some(".monkeyfence")
    )
}
