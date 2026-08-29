//! 全新存储 schema(v1):与旧 MonkeyFence 持久化完全断开。
//!
//! - 项目库:`<project>/.mf-agent/workflow-v1.db`(Task/Revision/Step/Session/Run/事件)
//! - 目录库:`~/.monkeyfence/catalog-v1.db`(Agent Instance、工作流模板、Secret、插件包)
//!
//! 旧库文件(`orchestration.db` 等)既不读取也不删除;版本号写入 `user_version` pragma。

use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub const PROJECT_SCHEMA_VERSION: i64 = 1;
pub const CATALOG_SCHEMA_VERSION: i64 = 1;

/// 项目库路径:`<project>/.mf-agent/workflow-v1.db`。
pub fn project_db_path(project_root: &Path) -> PathBuf {
    project_root.join(".mf-agent").join("workflow-v1.db")
}

/// 目录库路径:`~/.monkeyfence/catalog-v1.db`。
/// 测试与嵌入式场景可用 `MF_CATALOG_DB` 重定向,避免触碰用户真实目录。
pub fn catalog_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("MF_CATALOG_DB") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".monkeyfence")
        .join("catalog-v1.db")
}

/// 应用 DDL 并把版本写入 `user_version`。DDL 必须幂等(IF NOT EXISTS)。
pub fn initialize_schema(conn: &Connection, ddl: &str, version: i64) -> Result<()> {
    conn.execute_batch(ddl)?;
    conn.pragma_update(None, "user_version", version)?;
    Ok(())
}

/// 读取 `user_version`。
pub fn schema_version_of(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?)
}

/// 列出库中用户表名(按名称排序)。
pub fn table_names_of(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names)
}

/// 项目库 v1 DDL:延续现有 Task/Revision/Step/Session/Run 模型;
/// 不含旧引擎表(runs/tasks/dispatches/messages/questions)、
/// 不含 schema_migrations 与 import_markers。
pub const PROJECT_SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS agent_tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    goal TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'draft',
    active_revision INTEGER,
    paused INTEGER NOT NULL DEFAULT 0,
    unread INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    archived_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_agent_tasks_status ON agent_tasks(status);
CREATE TABLE IF NOT EXISTS pipeline_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES agent_tasks(id),
    revision INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft',
    snapshot_json TEXT,
    created_at TEXT NOT NULL,
    UNIQUE(task_id, revision)
);
CREATE TABLE IF NOT EXISTS steps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    revision_id INTEGER NOT NULL REFERENCES pipeline_revisions(id),
    task_id INTEGER NOT NULL REFERENCES agent_tasks(id),
    step_key TEXT NOT NULL,
    title TEXT NOT NULL,
    instructions TEXT NOT NULL DEFAULT '',
    agent_profile TEXT NOT NULL,
    session_policy TEXT NOT NULL DEFAULT 'fresh',
    status TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    auto_retry INTEGER NOT NULL DEFAULT 0,
    result TEXT,
    started_at TEXT,
    ended_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(revision_id, step_key)
);
CREATE INDEX IF NOT EXISTS idx_steps_task ON steps(task_id);
CREATE INDEX IF NOT EXISTS idx_steps_revision ON steps(revision_id);
CREATE TABLE IF NOT EXISTS step_deps (
    step_id INTEGER NOT NULL REFERENCES steps(id),
    dep_step_id INTEGER NOT NULL REFERENCES steps(id),
    PRIMARY KEY (step_id, dep_step_id)
);
CREATE TABLE IF NOT EXISTS agent_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_key TEXT,
    runtime TEXT NOT NULL,
    agent_profile TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'starting',
    last_instruction TEXT,
    last_reply TEXT,
    unread INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_key ON agent_sessions(session_key);
CREATE TABLE IF NOT EXISTS agent_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL,
    step_id INTEGER NOT NULL,
    revision_id INTEGER NOT NULL,
    session_id INTEGER,
    status TEXT NOT NULL DEFAULT 'running',
    capability_token TEXT NOT NULL UNIQUE,
    agent_state TEXT NOT NULL DEFAULT 'working',
    outcome TEXT,
    outcome_payload TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_runs_step ON agent_runs(step_id);
CREATE INDEX IF NOT EXISTS idx_runs_task ON agent_runs(task_id);
CREATE INDEX IF NOT EXISTS idx_runs_session ON agent_runs(session_id);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS step_questions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL,
    step_id INTEGER,
    run_id INTEGER,
    question TEXT NOT NULL,
    answer TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    created_at TEXT NOT NULL,
    answered_at TEXT
);
CREATE TABLE IF NOT EXISTS ad_hoc_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES agent_tasks(id),
    title TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'starting',
    snapshot_json TEXT NOT NULL,
    handoff_json TEXT,
    display_session_id INTEGER,
    created_at TEXT NOT NULL,
    launched_at TEXT,
    ended_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_ad_hoc_task ON ad_hoc_sessions(task_id);
CREATE TABLE IF NOT EXISTS task_workflows (
    project_key TEXT NOT NULL,
    task_id INTEGER NOT NULL,
    graph_json TEXT NOT NULL,
    allow_unsafe_parallel INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (project_key, task_id)
);
CREATE INDEX IF NOT EXISTS idx_task_workflows_task ON task_workflows(task_id);
CREATE TABLE IF NOT EXISTS handoffs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES agent_tasks(id),
    step_id INTEGER,
    run_id INTEGER,
    handoff_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_handoffs_task ON handoffs(task_id);
CREATE INDEX IF NOT EXISTS idx_handoffs_run ON handoffs(run_id);
CREATE TABLE IF NOT EXISTS execution_leases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    lease_key TEXT NOT NULL UNIQUE,
    run_id INTEGER,
    step_id INTEGER NOT NULL,
    task_id INTEGER NOT NULL,
    provider TEXT NOT NULL,
    path TEXT NOT NULL,
    isolated INTEGER NOT NULL DEFAULT 0,
    metadata_json TEXT,
    status TEXT NOT NULL DEFAULT 'held',
    created_at TEXT NOT NULL,
    released_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_execution_leases_task ON execution_leases(task_id);
CREATE INDEX IF NOT EXISTS idx_execution_leases_run ON execution_leases(run_id);
CREATE TABLE IF NOT EXISTS pending_merges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES agent_tasks(id),
    lease_id TEXT NOT NULL,
    lease_json TEXT NOT NULL,
    conflicts_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pending_merges_task ON pending_merges(task_id);
CREATE TABLE IF NOT EXISTS join_deferrals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES agent_tasks(id),
    join_step_key TEXT NOT NULL,
    lease_key TEXT NOT NULL,
    lease_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(task_id, join_step_key, lease_key)
);
CREATE INDEX IF NOT EXISTS idx_join_deferrals_task ON join_deferrals(task_id);
";

/// 目录库 v1 DDL:仅空表地基(字段随后续里程碑补全);
/// Secret 密文与普通配置分表存放。
pub const CATALOG_SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS agent_instances (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    instance_key TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    agent_type TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'user',
    project_key TEXT,
    current_version INTEGER NOT NULL DEFAULT 1,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS agent_instance_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    instance_id INTEGER NOT NULL REFERENCES agent_instances(id),
    version INTEGER NOT NULL,
    config_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(instance_id, version)
);
CREATE TABLE IF NOT EXISTS workflow_templates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    template_key TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    current_version INTEGER NOT NULL DEFAULT 1,
    task_local INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS workflow_template_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    template_id INTEGER NOT NULL REFERENCES workflow_templates(id),
    version INTEGER NOT NULL,
    graph_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(template_id, version)
);
CREATE TABLE IF NOT EXISTS sealed_secrets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    secret_key TEXT NOT NULL,
    store_id TEXT NOT NULL DEFAULT 'default',
    ciphertext BLOB NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(secret_key, store_id)
);
CREATE TABLE IF NOT EXISTS plugin_packages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    full_id TEXT NOT NULL,
    version TEXT NOT NULL,
    content_hash TEXT NOT NULL UNIQUE,
    installed_at TEXT NOT NULL,
    UNIQUE(full_id, version)
);
CREATE TABLE IF NOT EXISTS plugin_pins (
    run_key TEXT NOT NULL,
    full_id TEXT NOT NULL,
    version TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (run_key, full_id, version, content_hash)
);
CREATE INDEX IF NOT EXISTS idx_plugin_pins_hash ON plugin_pins(content_hash);
";
