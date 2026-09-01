//! 全新存储 schema(v1):与旧 MonkeyFence 持久化完全断开。
//!
//! - 项目库:`<project>/.mf-agent/workflow-v1.db`(Task/Revision/Step/Session/Run/事件)
//! - 目录库:`~/.monkeyfence/catalog-v1.db`(Agent Instance、工作流模板、Secret、插件包)
//!
//! 旧库文件(`orchestration.db` 等)既不读取也不删除;版本号写入 `user_version` pragma。

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub const PROJECT_SCHEMA_VERSION: i64 = 7;
pub const CATALOG_SCHEMA_VERSION: i64 = 1;
/// Catalog v2 使用独立文件与独立版本链，不能复用 v1 的 user_version
/// 含义，否则 pre-Bridge 旧程序可能把新库当作 v1 打开。
pub const CATALOG_V2_SCHEMA_VERSION: i64 = 1;

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

/// 新目录库路径：`~/.monkeyfence/catalog-v2.db`。
/// 测试可用 `MF_CATALOG_V2_DB` 重定向，禁止回退到 v1 路径。
pub fn catalog_v2_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("MF_CATALOG_V2_DB") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".monkeyfence")
        .join("catalog-v2.db")
}

/// 应用 DDL 并把版本写入 `user_version`。DDL 必须幂等(IF NOT EXISTS)。
/// 防御性 future guard:高于 `version` 的库禁止经此入口降级
/// (生产打开路径走 `migration::upgrade_with_barrier`)。
pub fn initialize_schema(conn: &Connection, ddl: &str, version: i64) -> Result<()> {
    let current = schema_version_of(conn)?;
    anyhow::ensure!(
        current <= version,
        "数据库 schema 版本 v{current} 高于目标 v{version},拒绝初始化降级"
    );
    conn.execute_batch(ddl)?;
    conn.pragma_update(None, "user_version", version)?;
    Ok(())
}

/// 项目库按 `user_version` 链式迁移到 `target`。
/// T1a:经 `migration::upgrade_with_barrier` 统一执行 future guard 与
/// Backup 前置屏障(0 < user_version < target 的真实升级先备份再迁移);
/// `user_version = 0` 视为全新库,从 v1 DDL 起完整应用,不触发备份。
pub fn upgrade_project(conn: &mut Connection, target: i64) -> Result<()> {
    let metric_key =
        crate::observability::store_metric_key(conn, crate::migration::StoreKind::Project);
    let pending = std::cell::RefCell::new(None);
    crate::migration::upgrade_with_barrier(
        conn,
        crate::migration::StoreKind::Project,
        target,
        &|tx, from, to| {
            *pending.borrow_mut() = apply_project_chain(tx, from, to)?;
            Ok(())
        },
    )?;
    if let Some(stats) = pending.into_inner() {
        let outbox_depth: i64 = conn.query_row(
            "SELECT COUNT(*) FROM projection_outbox WHERE published_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        log::info!(
            "migration_identity_backfill store=project schema_version=7 aggregate_handles={} workflows={} nodes_created={} edges_created={} nodes_removed={} edges_removed={} outbox_depth={}",
            stats.aggregate_handles,
            stats.identity.workflows,
            stats.identity.identity.nodes_created,
            stats.identity.identity.edges_created,
            stats.identity.identity.nodes_deleted,
            stats.identity.identity.edges_deleted,
            outbox_depth
        );
        crate::observability::record_identity_backfill(
            &metric_key,
            stats.aggregate_handles,
            stats.identity.identity.nodes_created,
            stats.identity.identity.edges_created,
        );
    }
    Ok(())
}

/// `(from, to]` 区间的 v1–v6 DDL/回填链(在迁移事务内执行)。
fn apply_project_chain(
    tx: &rusqlite::Transaction,
    from: i64,
    to: i64,
) -> Result<Option<ProjectMigrationStats>> {
    // 每次真实升级都重放幂等地基/repair 探针:早期开发库可能已标 v1–v6
    // 却缺表/列。必须先补齐完整历史 schema,不能把残缺库标成 v7。
    if to >= 1 {
        tx.execute_batch(PROJECT_SCHEMA_V1)?;
    }
    if to >= 2 {
        tx.execute_batch(PROJECT_SCHEMA_V2_DELTA)?;
        backfill_early_dev_columns(tx)?;
    }
    if to >= 3 {
        backfill_digest_columns(tx)?;
    }
    if to >= 4 {
        backfill_merge_batch_columns(tx)?;
    }
    if to >= 5 {
        backfill_merge_owner_columns(tx)?;
    }
    if to >= 6 {
        tx.execute_batch(PROJECT_SCHEMA_V6_DELTA)?;
    }
    if from < 7 && to >= 7 {
        return Ok(Some(backfill_project_v7(tx)?));
    }
    Ok(None)
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
    owner_id TEXT NOT NULL DEFAULT '',
    owner_expires_at TEXT NOT NULL DEFAULT '',
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

/// v5 增量:合并批领取者租期。新实例只能回收已过期 owner 的
/// `merging/resolving`，不能在另一个活跃 Orchestrator 工作时无条件抢占。
fn backfill_merge_owner_columns(conn: &Connection) -> Result<()> {
    let has_column = |column: &str| -> Result<bool> {
        let mut stmt = conn.prepare("PRAGMA table_info(merge_batches)")?;
        let mut exists = false;
        let mut found = false;
        for c in stmt.query_map([], |r| r.get::<_, String>(1))? {
            exists = true;
            if c? == column {
                found = true;
            }
        }
        Ok(!exists || found)
    };
    if !has_column("owner_id")? {
        conn.execute(
            "ALTER TABLE merge_batches ADD COLUMN owner_id TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !has_column("owner_expires_at")? {
        conn.execute(
            "ALTER TABLE merge_batches ADD COLUMN owner_expires_at TEXT NOT NULL DEFAULT ''",
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

/// Catalog v2 新文件的 v1 schema。legacy Agent Instance/Template 使用
/// 已有稳定业务 key 作为关系键；Secret 只迁移引用，绝不复制 ciphertext。
pub const CATALOG_V2_SCHEMA_V1: &str = "
CREATE TABLE agent_type_catalog (
    agent_type_id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    plugin_full_id TEXT,
    manifest_version INTEGER NOT NULL DEFAULT 3,
    capabilities_json TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0, 1)),
    revision INTEGER NOT NULL DEFAULT 1 CHECK(revision >= 1),
    updated_at TEXT NOT NULL
);
CREATE TABLE cli_installations (
    installation_handle TEXT PRIMARY KEY,
    agent_type_id TEXT NOT NULL,
    executable_path TEXT NOT NULL,
    canonical_path TEXT NOT NULL UNIQUE,
    actual_version TEXT,
    source TEXT NOT NULL CHECK(source IN ('external', 'managed')),
    scope TEXT NOT NULL CHECK(scope IN ('user', 'machine')),
    health TEXT NOT NULL CHECK(health IN ('detected', 'healthy', 'unhealthy', 'repair-needed', 'missing')),
    receipt_handle TEXT,
    detected_at TEXT NOT NULL
);
CREATE INDEX idx_cli_installations_agent_type ON cli_installations(agent_type_id);
CREATE TABLE installation_receipts (
    receipt_handle TEXT PRIMARY KEY,
    installation_handle TEXT,
    agent_type_id TEXT NOT NULL,
    operation TEXT NOT NULL CHECK(operation IN ('install', 'update', 'repair', 'uninstall', 'adopt')),
    source TEXT NOT NULL CHECK(source IN ('external', 'managed')),
    scope TEXT NOT NULL CHECK(scope IN ('user', 'machine')),
    requesting_principal TEXT NOT NULL,
    target_owner TEXT NOT NULL,
    provenance_json TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TRIGGER installation_receipts_immutable_update
BEFORE UPDATE ON installation_receipts
BEGIN SELECT RAISE(ABORT, 'installation_receipt_immutable'); END;
CREATE TRIGGER installation_receipts_immutable_delete
BEFORE DELETE ON installation_receipts
BEGIN SELECT RAISE(ABORT, 'installation_receipt_immutable'); END;
CREATE TRIGGER installation_receipts_immutable_reinsert
BEFORE INSERT ON installation_receipts
WHEN EXISTS(SELECT 1 FROM installation_receipts WHERE receipt_handle = NEW.receipt_handle)
BEGIN SELECT RAISE(ABORT, 'installation_receipt_immutable'); END;
CREATE TABLE installation_jobs (
    job_handle TEXT PRIMARY KEY,
    agent_type_id TEXT NOT NULL,
    target_version TEXT,
    state TEXT NOT NULL CHECK(state IN ('planned', 'running', 'succeeded', 'failed', 'cancelled', 'repair-needed')),
    progress_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE provider_profiles (
    profile_handle TEXT PRIMARY KEY,
    provider_type_id TEXT NOT NULL,
    name TEXT NOT NULL,
    base_url TEXT,
    secret_ref TEXT,
    config_json TEXT NOT NULL DEFAULT '{}',
    revision INTEGER NOT NULL DEFAULT 1 CHECK(revision >= 1),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE provider_model_cache (
    profile_handle TEXT NOT NULL,
    model_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    capabilities_json TEXT NOT NULL DEFAULT '{}',
    fetched_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY(profile_handle, model_id)
);
CREATE TABLE agent_instances (
    instance_key TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    agent_type TEXT NOT NULL,
    scope TEXT NOT NULL,
    project_key TEXT,
    current_version INTEGER NOT NULL,
    enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE agent_instance_versions (
    instance_key TEXT NOT NULL REFERENCES agent_instances(instance_key) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    config_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY(instance_key, version)
);
CREATE TABLE workflow_templates (
    template_key TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    current_version INTEGER NOT NULL,
    task_local INTEGER NOT NULL CHECK(task_local IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE workflow_template_versions (
    template_key TEXT NOT NULL REFERENCES workflow_templates(template_key) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    graph_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY(template_key, version)
);
CREATE TABLE secret_refs (
    secret_key TEXT NOT NULL,
    store_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(secret_key, store_id)
);
CREATE TABLE plugin_pins (
    run_key TEXT NOT NULL,
    full_id TEXT NOT NULL,
    version TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY(run_key, full_id, version, content_hash)
);
CREATE INDEX idx_catalog_v2_plugin_pins_hash ON plugin_pins(content_hash);
CREATE TABLE command_receipt (
    command_id TEXT PRIMARY KEY,
    semantic_digest TEXT NOT NULL,
    aggregate_handle TEXT NOT NULL,
    result_revisions TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    finalized_at TEXT
);
CREATE TABLE projection_outbox (
    outbox_id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_json TEXT NOT NULL,
    published_at TEXT
);
CREATE TABLE migration_marker (
    marker TEXT PRIMARY KEY,
    source_schema_version INTEGER NOT NULL,
    source_digest TEXT NOT NULL,
    imported_counts_json TEXT NOT NULL,
    completed_at TEXT NOT NULL
);
";

/// v6 增量(ADR 0004):独立的项目工作流存储。项目工作流是项目内
/// 一级对象(可编辑、可重复运行),与 `task_workflows`(Task 本地
/// 草稿)互不迁移、互不改写;只存当前版本,运行时冻结为 Revision。
pub const PROJECT_SCHEMA_V6_DELTA: &str = "
CREATE TABLE IF NOT EXISTS project_workflows (
    workflow_key TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    graph_json TEXT NOT NULL,
    allow_unsafe_parallel INTEGER NOT NULL DEFAULT 0,
    content_digest TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
";

// ---------------------------------------------------------------------------
// v7 增量(T1b,canonical spec §3.2):expand-only——持久 opaque identity、
// 语义/展示双 revision、T1 本地持久表。
//
// 持久 aggregate 审计清单(逐一决定,不凭模糊规则):
//   加 public_handle:agent_tasks(Task;Workflow Run 由 Task 承载)、
//     pipeline_revisions(Pipeline Revision;已有 per-task `revision` 列,
//     语义保持不变,不另加)、steps、agent_sessions(SessionRegistry 改用
//     持久 handle)、agent_runs、ad_hoc_sessions、project_workflows。
//   另补 revision(DEFAULT 1):agent_tasks、steps、agent_sessions、
//     agent_runs、ad_hoc_sessions。
//   不加 handle:step_deps(纯 join)、events(事件日志)、step_questions
//     (依附 Task/Step/Run 的交互记录)、handoffs(Agent Run 的不可变负载,
//     经所属 run 定位)、execution_leases / pending_merges /
//     join_deferrals / merge_batches(内部编排状态机,自有租约键)、
//     task_workflows(Task 聚合的本地草稿投影,经 task 定位)。
//
// SQLite 限制:ALTER ADD COLUMN 不允许 UNIQUE/PRIMARY KEY;NOT NULL 列
// 必须带非 NULL 默认。因此 handle 列先以 `DEFAULT ''` 加入,同事务回填
// 真实 UUIDv7 后再建唯一索引(空串默认迁移后失效:全部写入都经 Store
// 深模块生成 handle,唯一索引最多再容忍一行 '' 即拒绝)。
// ---------------------------------------------------------------------------

/// v7 新表:project_meta singleton、node/edge identity、presentation、
/// position、command receipt、outbox、terminal transcript。
/// identity/presentation 经 FK 随所属工作流级联清理(显式清理在
/// Store 事务 API 中同事务执行,两者互为兜底)。
pub const PROJECT_SCHEMA_V7_DELTA: &str = "
CREATE TABLE IF NOT EXISTS project_meta (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    workflow_collection_revision INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS workflow_node_identity (
    workflow_handle TEXT NOT NULL
        REFERENCES project_workflows(public_handle) ON DELETE CASCADE,
    node_key TEXT NOT NULL,
    node_handle TEXT NOT NULL UNIQUE,
    UNIQUE(workflow_handle, node_key)
);
CREATE TABLE IF NOT EXISTS workflow_edge_identity (
    workflow_handle TEXT NOT NULL
        REFERENCES project_workflows(public_handle) ON DELETE CASCADE,
    upstream_node_key TEXT NOT NULL,
    downstream_node_key TEXT NOT NULL,
    edge_handle TEXT NOT NULL UNIQUE,
    UNIQUE(workflow_handle, upstream_node_key, downstream_node_key)
);
CREATE TABLE IF NOT EXISTS workflow_presentation (
    workflow_handle TEXT NOT NULL UNIQUE
        REFERENCES project_workflows(public_handle) ON DELETE CASCADE,
    viewport_json TEXT,
    collapse_json TEXT,
    layout_json TEXT
);
CREATE TABLE IF NOT EXISTS node_position (
    node_handle TEXT NOT NULL UNIQUE
        REFERENCES workflow_node_identity(node_handle) ON DELETE CASCADE,
    x REAL NOT NULL,
    y REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS command_receipt (
    command_id TEXT NOT NULL UNIQUE,
    semantic_digest TEXT NOT NULL,
    aggregate_handle TEXT NOT NULL,
    result_revisions TEXT NOT NULL DEFAULT '{}',
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    finalized_at TEXT
);
CREATE TABLE IF NOT EXISTS projection_outbox (
    outbox_id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_json TEXT NOT NULL,
    published_at TEXT
);
CREATE TABLE IF NOT EXISTS terminal_transcript (
    session_handle TEXT PRIMARY KEY NOT NULL,
    terminal_epoch INTEGER NOT NULL,
    final_state TEXT NOT NULL
        CHECK(final_state IN ('live', 'complete', 'crash_incomplete', 'lost')),
    durable_through_seq INTEGER NOT NULL,
    exit_code INTEGER,
    exit_signal INTEGER,
    as_of_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS terminal_transcript_segment (
    session_handle TEXT NOT NULL
        REFERENCES terminal_transcript(session_handle) ON DELETE CASCADE,
    seq_start INTEGER NOT NULL,
    seq_end INTEGER NOT NULL,
    bytes BLOB NOT NULL,
    PRIMARY KEY (session_handle, seq_start),
    CHECK(seq_start >= 1),
    CHECK(seq_end >= seq_start)
);
CREATE INDEX IF NOT EXISTS idx_workflow_node_identity_handle
    ON workflow_node_identity(workflow_handle);
CREATE INDEX IF NOT EXISTS idx_workflow_edge_identity_handle
    ON workflow_edge_identity(workflow_handle);
";

/// v7 handle 列 ALTER(经 has_column 守卫幂等;残缺库缺表跳过)。
const V7_HANDLE_COLUMN_DDLS: &[&str] = &[
    "ALTER TABLE agent_tasks ADD COLUMN public_handle TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE pipeline_revisions ADD COLUMN public_handle TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE steps ADD COLUMN public_handle TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE agent_sessions ADD COLUMN public_handle TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE agent_runs ADD COLUMN public_handle TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE ad_hoc_sessions ADD COLUMN public_handle TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE project_workflows ADD COLUMN public_handle TEXT NOT NULL DEFAULT ''",
];

/// v7 缺 revision 的 aggregate 补列(pipeline_revisions 已有 per-task
/// `revision`,语义不变,不在此列)。
const V7_REVISION_COLUMN_DDLS: &[&str] = &[
    "ALTER TABLE agent_tasks ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
    "ALTER TABLE steps ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
    "ALTER TABLE agent_sessions ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
    "ALTER TABLE agent_runs ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
    "ALTER TABLE ad_hoc_sessions ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
];

/// project_workflows 双 revision 列(语义/展示分离,互不串增)。
const V7_WORKFLOW_REVISION_DDLS: &[&str] = &[
    "ALTER TABLE project_workflows ADD COLUMN semantic_revision INTEGER NOT NULL DEFAULT 1",
    "ALTER TABLE project_workflows ADD COLUMN presentation_revision INTEGER NOT NULL DEFAULT 1",
];

/// handle 唯一索引:在回填消灭全部 '' 之后创建(见上文 SQLite 限制)。
/// 残缺旧库缺表时跳过(`CREATE INDEX` 对缺表报错,与 ALTER 同口径)。
const V7_HANDLE_INDEX_DDLS: &[(&str, &str)] = &[
    (
        "agent_tasks",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_tasks_public_handle
         ON agent_tasks(public_handle)",
    ),
    (
        "pipeline_revisions",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_pipeline_revisions_public_handle
         ON pipeline_revisions(public_handle)",
    ),
    (
        "steps",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_steps_public_handle
         ON steps(public_handle)",
    ),
    (
        "agent_sessions",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_sessions_public_handle
         ON agent_sessions(public_handle)",
    ),
    (
        "agent_runs",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_runs_public_handle
         ON agent_runs(public_handle)",
    ),
    (
        "ad_hoc_sessions",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_ad_hoc_sessions_public_handle
         ON ad_hoc_sessions(public_handle)",
    ),
    (
        "project_workflows",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_project_workflows_public_handle
         ON project_workflows(public_handle)",
    ),
];

pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    Ok(stmt.exists([table])?)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let has = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .any(|c| c.map(|c| c == column).unwrap_or(false));
    Ok(has)
}

/// 从 ALTER DDL 中提取 `(表名, 列名)`(仅用于幂等守卫,不执行)。
fn table_column_of_alter(ddl: &str) -> (&str, &str) {
    let table = ddl
        .split_whitespace()
        .nth(2)
        .unwrap_or_else(|| panic!("非法 ALTER DDL: {ddl}"));
    let column = ddl
        .rsplit_once("ADD COLUMN ")
        .map(|(_, rest)| rest.split_whitespace().next().unwrap_or(""))
        .unwrap_or_else(|| panic!("非法 ALTER DDL: {ddl}"));
    (table, column)
}

fn apply_guarded_alters(conn: &Connection, ddls: &[&str]) -> Result<()> {
    for ddl in ddls {
        let (table, column) = table_column_of_alter(ddl);
        if !table_exists(conn, table)? {
            // 残缺旧库缺表:无从 ALTER,与 v2 早期补列同一容错口径
            continue;
        }
        if !column_exists(conn, table, column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

/// 为 `table` 回填空 handle 行(每行一个新 UUIDv7,永不复用)。
fn backfill_public_handles(conn: &Connection, table: &str) -> Result<usize> {
    if !table_exists(conn, table)? {
        return Ok(0);
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT rowid FROM {table} WHERE public_handle = '' ORDER BY rowid"
    ))?;
    let ids: Vec<i64> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    let count = ids.len();
    for id in ids {
        conn.execute(
            &format!("UPDATE {table} SET public_handle = ?1 WHERE rowid = ?2"),
            rusqlite::params![crate::store::new_public_handle(), id],
        )?;
    }
    Ok(count)
}

/// v6→v7 迁移体(单一 writer-lock 事务内执行;任一步失败整体回滚):
/// ALTER 加列 → 回填聚合/工作流 handle → handle 唯一索引 → 新表 DDL →
/// project_meta singleton → 既有工作流图 node/edge identity 回填。
/// graph_json 解析失败、重复 node key、未知 dependency 一律 fail-closed。
#[derive(Debug, Default)]
struct ProjectMigrationStats {
    aggregate_handles: usize,
    identity: WorkflowIdentityBackfillStats,
}

fn backfill_project_v7(tx: &rusqlite::Transaction) -> Result<ProjectMigrationStats> {
    apply_guarded_alters(tx, V7_HANDLE_COLUMN_DDLS)?;
    apply_guarded_alters(tx, V7_REVISION_COLUMN_DDLS)?;
    apply_guarded_alters(tx, V7_WORKFLOW_REVISION_DDLS)?;
    let mut aggregate_handles = 0usize;
    for table in [
        "agent_tasks",
        "pipeline_revisions",
        "steps",
        "agent_sessions",
        "agent_runs",
        "ad_hoc_sessions",
        "project_workflows",
    ] {
        aggregate_handles += backfill_public_handles(tx, table)?;
    }
    for (table, ddl) in V7_HANDLE_INDEX_DDLS {
        if table_exists(tx, table)? {
            tx.execute(ddl, [])?;
        }
    }
    tx.execute_batch(PROJECT_SCHEMA_V7_DELTA)?;
    tx.execute(
        "INSERT OR IGNORE INTO project_meta (id, workflow_collection_revision) VALUES (1, 1)",
        [],
    )?;
    Ok(ProjectMigrationStats {
        aggregate_handles,
        identity: backfill_workflow_identity(tx)?,
    })
}

/// 为每个现存 project_workflow 回填 node/edge identity。
/// 既有 `(workflow_handle, node_key)` 行保留原 handle(幂等重跑不改);
/// 节点 key 用 `WorkflowNodeDraft.key`,边按 downstream deps 键对。
#[derive(Debug, Default)]
struct WorkflowIdentityBackfillStats {
    workflows: usize,
    identity: crate::store::IdentitySyncStats,
}

fn backfill_workflow_identity(tx: &rusqlite::Transaction) -> Result<WorkflowIdentityBackfillStats> {
    if !table_exists(tx, "project_workflows")? {
        return Ok(WorkflowIdentityBackfillStats::default());
    }
    let mut stmt = tx.prepare(
        "SELECT workflow_key, graph_json, public_handle
         FROM project_workflows ORDER BY workflow_key",
    )?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut stats = WorkflowIdentityBackfillStats::default();
    for (key, graph_json, handle) in rows {
        anyhow::ensure!(
            !handle.is_empty(),
            "工作流 `{key}` 缺少持久 handle,identity 无法回填"
        );
        let nodes = crate::workflow::parse_graph_json(&graph_json)
            .with_context(|| format!("工作流 `{key}` graph_json 损坏,拒绝回填 identity"))?;
        stats.identity += crate::store::sync_workflow_identity_tx_with_stats(tx, &handle, &nodes)
            .with_context(|| format!("工作流 `{key}` identity 回填失败"))?;
        stats.workflows += 1;
    }
    Ok(stats)
}
