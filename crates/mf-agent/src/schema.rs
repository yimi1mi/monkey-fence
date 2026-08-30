//! 全新存储 schema(v1):与旧 MonkeyFence 持久化完全断开。
//!
//! - 项目库:`<project>/.mf-agent/workflow-v1.db`(Task/Revision/Step/Session/Run/事件)
//! - 目录库:`~/.monkeyfence/catalog-v1.db`(Agent Instance、工作流模板、Secret、插件包)
//!
//! 旧库文件(`orchestration.db` 等)既不读取也不删除;版本号写入 `user_version` pragma。

use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub const PROJECT_SCHEMA_VERSION: i64 = 4;
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

/// 项目库按 `user_version` 链式迁移到 `target`(每步单事务;
/// `user_version = 0` 视为全新库,从 v1 DDL 起完整应用)。
/// 版本高于程序支持时拒绝打开(禁止隐式降级)。
pub fn upgrade_project(conn: &mut Connection, target: i64) -> Result<()> {
    let current = schema_version_of(conn)?;
    anyhow::ensure!(
        current <= target,
        "数据库 schema 版本 v{current} 高于程序支持的 v{target}:请升级程序后再打开"
    );
    if current == target {
        return Ok(());
    }
    let tx = conn.transaction()?;
    if current < 1 {
        tx.execute_batch(PROJECT_SCHEMA_V1)?;
    }
    if current < 2 {
        tx.execute_batch(PROJECT_SCHEMA_V2_DELTA)?;
        backfill_early_dev_columns(&tx)?;
    }
    if current < 3 {
        backfill_digest_columns(&tx)?;
    }
    if current < 4 {
        backfill_merge_batch_columns(&tx)?;
    }
    tx.pragma_update(None, "user_version", target)?;
    tx.commit()?;
    Ok(())
}

/// 早期开发库(v1 期间缺列/缺表的库,user_version 已是 1)的幂等补齐:
/// 升级到 v2 时统一执行,PRAGMA 探测后补列,已补齐的库为 no-op。
/// 表不存在的残缺库跳过对应补列(后续 DDL 不再创建该表,查询时如实报错)。
fn backfill_early_dev_columns(conn: &Connection) -> Result<()> {
    let has_column = |table: &str, column: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let has = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .any(|c| c.map(|c| c == column).unwrap_or(false));
        drop(stmt);
        if !has {
            // 区分"表存在但缺列"与"表不存在":后者无法 ALTER,跳过
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let table_exists = stmt.exists([])?;
            drop(stmt);
            if !table_exists {
                return Ok(true); // 视为无需补列
            }
        }
        Ok(has)
    };
    if !has_column("steps", "auto_retry")? {
        conn.execute(
            "ALTER TABLE steps ADD COLUMN auto_retry INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column("ad_hoc_sessions", "display_session_id")? {
        conn.execute(
            "ALTER TABLE ad_hoc_sessions ADD COLUMN display_session_id INTEGER",
            [],
        )?;
    }
    if !has_column("task_workflows", "allow_unsafe_parallel")? {
        conn.execute(
            "ALTER TABLE task_workflows ADD COLUMN allow_unsafe_parallel INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column("pipeline_revisions", "snapshot_json")? {
        conn.execute(
            "ALTER TABLE pipeline_revisions ADD COLUMN snapshot_json TEXT",
            [],
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS join_deferrals (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id INTEGER NOT NULL REFERENCES agent_tasks(id),
            join_step_key TEXT NOT NULL,
            lease_key TEXT NOT NULL,
            lease_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(task_id, join_step_key, lease_key)
        );
        CREATE INDEX IF NOT EXISTS idx_join_deferrals_task ON join_deferrals(task_id);",
    )?;
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

/// v2 增量(C1):join 批(或单租约汇合)的持久状态机。
/// `status`:ready → merging → merged / needs_you;
/// 领取是 `UPDATE ... WHERE status = 'ready'` 条件更新(CAS),
/// 并发 complete 同一批只有一个线程能推进到 merging。
/// `transaction_id` 唯一标识领取者,结论回写按它 CAS。
pub const PROJECT_SCHEMA_V2_DELTA: &str = "
CREATE TABLE IF NOT EXISTS merge_batches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES agent_tasks(id),
    join_step_key TEXT NOT NULL,
    revision_id INTEGER NOT NULL,
    lease_keys_json TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'ready',
    transaction_id TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(task_id, join_step_key, revision_id)
);
CREATE INDEX IF NOT EXISTS idx_merge_batches_task ON merge_batches(task_id);
";

/// v3 增量(I12):内容身份摘要列(规范化节点 + unsafe 开关的
/// SHA-256)。`assign/confirm` 只按 digest 等值复用 Revision,
/// 禁止 saved_at/created_at 时间戳比较(时钟回拨/同秒不同内容)。
fn backfill_digest_columns(conn: &Connection) -> Result<()> {
    let has_column = |table: &str, column: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut has = false;
        let mut table_exists = false;
        for c in stmt.query_map([], |r| r.get::<_, String>(1))? {
            table_exists = true;
            if c.map(|c| c == column).unwrap_or(false) {
                has = true;
            }
        }
        drop(stmt);
        if !table_exists {
            return Ok(true); // 残缺库无此表:跳过(无从补列)
        }
        Ok(has)
    };
    if !has_column("task_workflows", "content_digest")? {
        conn.execute(
            "ALTER TABLE task_workflows ADD COLUMN content_digest TEXT",
            [],
        )?;
    }
    if !has_column("pipeline_revisions", "content_digest")? {
        conn.execute(
            "ALTER TABLE pipeline_revisions ADD COLUMN content_digest TEXT",
            [],
        )?;
    }
    Ok(())
}

/// v4 增量(F1/F2):merge_batches 的权威租约集 digest(领取 CAS 绑定)
/// 与 needs_user 冲突投影(启动恢复重建 pending 行用)。
fn backfill_merge_batch_columns(conn: &Connection) -> Result<()> {
    let has_column = |table: &str, column: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut has = false;
        let mut table_exists = false;
        for c in stmt.query_map([], |r| r.get::<_, String>(1))? {
            table_exists = true;
            if c.map(|c| c == column).unwrap_or(false) {
                has = true;
            }
        }
        drop(stmt);
        if !table_exists {
            return Ok(true); // 残缺库无此表:跳过(无从补列)
        }
        Ok(has)
    };
    if !has_column("merge_batches", "lease_digest")? {
        conn.execute(
            "ALTER TABLE merge_batches ADD COLUMN lease_digest TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !has_column("merge_batches", "conflicts_json")? {
        conn.execute(
            "ALTER TABLE merge_batches ADD COLUMN conflicts_json TEXT NOT NULL DEFAULT '[]'",
            [],
        )?;
    }
    Ok(())
}

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
