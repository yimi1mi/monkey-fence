//! v2 持久化层:正式数据库迁移 + Agent 工作区 schema。
//!
//! 每项目一个 `<project>/.mf-agent/orchestration.db`。
//! 旧表(runs/tasks/dispatches/messages/questions)迁移后保留为只读历史,不再写入。
//! 新模型:tasks / pipeline_revisions / steps / step_deps / agent_sessions /
//! agent_runs / events / step_questions / schema_migrations。

use crate::model::*;
use crate::pipeline::{PipelineDraft, SessionPolicy};
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// 一次性能力令牌:仅对一个 Agent Run 有效。
pub fn gen_capability_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h1 = std::collections::hash_map::RandomState::new().build_hasher();
    let mut h2 = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    h1.write_u64(nanos);
    h1.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h2.write_u64(nanos ^ 0x9e37_79b9_7f4a_7c15);
    h2.write_u64(COUNTER.load(Ordering::Relaxed));
    format!("mft_{:016x}{:016x}", h1.finish(), h2.finish())
}

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub up: fn(&Transaction) -> Result<()>,
}

/// v1:旧引擎 schema(与 `db::SCHEMA` 相同,全部 IF NOT EXISTS)。
fn up_v1(tx: &Transaction) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            objective TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tasks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id INTEGER NOT NULL REFERENCES runs(id),
            parent_id INTEGER,
            spec TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            deps TEXT NOT NULL DEFAULT '[]',
            result TEXT,
            failure_count INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_tasks_run ON tasks(run_id);
        CREATE TABLE IF NOT EXISTS dispatches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id INTEGER NOT NULL REFERENCES tasks(id),
            status TEXT NOT NULL DEFAULT 'dispatched',
            worker TEXT NOT NULL DEFAULT '',
            started_at TEXT NOT NULL,
            ended_at TEXT
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id INTEGER NOT NULL,
            from_handle TEXT NOT NULL,
            to_handle TEXT NOT NULL,
            kind TEXT NOT NULL,
            body TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS questions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id INTEGER NOT NULL,
            task_id INTEGER,
            question TEXT NOT NULL,
            answer TEXT,
            status TEXT NOT NULL DEFAULT 'open',
            created_at TEXT NOT NULL
        );",
    )?;
    Ok(())
}

/// v2 DDL(幂等)。
fn v2_ddl() -> &'static str {
    "CREATE TABLE IF NOT EXISTS schema_migrations (
        version INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        applied_at TEXT NOT NULL
    );
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
    CREATE TABLE IF NOT EXISTS import_markers (
        name TEXT PRIMARY KEY,
        applied_at TEXT NOT NULL
    );"
}

/// v2:新模型 DDL + 旧数据变换(runs→Task,tasks→Step,dispatches→Agent Run)。
fn up_v2(tx: &Transaction) -> Result<()> {
    tx.execute_batch(v2_ddl())?;
    let ts = now();

    // 旧 runs → 新 Task
    let mut run_to_task: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt =
            tx.prepare("SELECT id, objective, status, created_at FROM runs ORDER BY id")?;
        let rows: Vec<(i64, String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        for (id, objective, status, created_at) in rows {
            let new_status = match status.as_str() {
                "active" => "needs-you", // 旧引擎已停,转人工确认
                "done" => "succeeded",
                _ => "failed",
            };
            tx.execute(
                "INSERT INTO agent_tasks (title, goal, status, unread, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5)",
                params![objective, objective, new_status, created_at, ts],
            )?;
            run_to_task.insert(id, tx.last_insert_rowid());
        }
    }

    // 旧 tasks → 新 Step(挂在各自 Task 的 Revision 1)
    let mut old_task_to_step: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, run_id, spec, status, deps, result, failure_count, created_at, updated_at
             FROM tasks ORDER BY id",
        )?;
        let rows: Vec<(
            i64,
            i64,
            String,
            String,
            String,
            Option<String>,
            i32,
            String,
            String,
        )> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        // 按 run 分组建立 revision
        let mut rev_of_task: HashMap<i64, i64> = HashMap::new();
        for (id, run_id, spec, status, _deps, result, failure_count, created_at, _updated) in rows {
            let task_id = match run_to_task.get(&run_id) {
                Some(t) => *t,
                None => continue,
            };
            let rev_id = match rev_of_task.get(&task_id) {
                Some(r) => *r,
                None => {
                    let run_terminal = tx
                        .query_row(
                            "SELECT status FROM runs WHERE id = ?1",
                            params![run_id],
                            |r| r.get::<_, String>(0),
                        )
                        .map(|s| s != "active")
                        .unwrap_or(true);
                    tx.execute(
                        "INSERT INTO pipeline_revisions (task_id, revision, status, created_at)
                         VALUES (?1, 1, ?2, ?3)",
                        params![
                            task_id,
                            if run_terminal { "cancelled" } else { "draft" },
                            ts
                        ],
                    )?;
                    let rid = tx.last_insert_rowid();
                    rev_of_task.insert(task_id, rid);
                    rid
                }
            };
            let new_status = match status.as_str() {
                "completed" => "succeeded",
                "failed" | "blocked" => "failed",
                "dispatched" => "cancelled",
                _ => "pending",
            };
            tx.execute(
                "INSERT INTO steps (revision_id, task_id, step_key, title, instructions,
                    agent_profile, session_policy, status, attempts, result, started_at, ended_at,
                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'legacy-engine', 'fresh', ?6, ?7, ?8,
                    CASE ?9 WHEN 0 THEN NULL ELSE ?10 END, ?10, ?11, ?11)",
                params![
                    rev_id,
                    task_id,
                    format!("legacy-{id}"),
                    spec.chars().take(80).collect::<String>(),
                    spec,
                    new_status,
                    failure_count.max(if new_status == "pending" { 0 } else { 1 }),
                    result,
                    if new_status == "pending" { 0 } else { 1 },
                    ts,
                    created_at,
                ],
            )?;
            old_task_to_step.insert(id, tx.last_insert_rowid());
        }
        // 旧 deps JSON → step_deps
        let mut stmt = tx.prepare("SELECT id, deps FROM tasks WHERE deps != '[]'")?;
        let dep_rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        for (old_id, deps_json) in dep_rows {
            let step_id = match old_task_to_step.get(&old_id) {
                Some(s) => *s,
                None => continue,
            };
            if let Ok(deps) = serde_json::from_str::<Vec<i64>>(&deps_json) {
                for d in deps {
                    if let Some(&dep_step) = old_task_to_step.get(&d) {
                        tx.execute(
                            "INSERT OR IGNORE INTO step_deps (step_id, dep_step_id) VALUES (?1, ?2)",
                            params![step_id, dep_step],
                        )?;
                    }
                }
            }
        }
    }

    // 旧 dispatches → 新 Agent Run
    {
        let mut stmt = tx.prepare(
            "SELECT id, task_id, status, started_at, ended_at FROM dispatches ORDER BY id",
        )?;
        let rows: Vec<(i64, i64, String, String, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        for (id, old_task_id, status, started_at, ended_at) in rows {
            let step_id = match old_task_to_step.get(&old_task_id) {
                Some(s) => *s,
                None => continue,
            };
            let (run_status, end) = match status.as_str() {
                "completed" => ("succeeded", ended_at.clone()),
                "failed" => ("failed", ended_at),
                _ => ("interrupted", Some(ts.clone())), // 未结算:恢复为 interrupted
            };
            tx.execute(
                "INSERT INTO agent_runs (task_id, step_id, revision_id, session_id, status,
                    capability_token, agent_state, outcome, outcome_payload, started_at, ended_at)
                 SELECT ?1, ?2, revision_id, NULL, ?3, ?4, 'done',
                    CASE ?3 WHEN 'succeeded' THEN 'complete' WHEN 'failed' THEN 'fail' ELSE NULL END,
                    result, ?5, ?6
                 FROM steps WHERE id = ?2",
                params![
                    tx.query_row("SELECT task_id FROM steps WHERE id = ?1", params![step_id], |r| {
                        r.get::<_, i64>(0)
                    })?,
                    step_id,
                    run_status,
                    format!("legacy_{id}_{}", gen_capability_token()),
                    started_at,
                    end,
                ],
            )?;
        }
    }

    // 迁移事件留痕
    tx.execute(
        "INSERT INTO events (kind, payload, created_at) VALUES ('db-migrated', ?1, ?2)",
        params![
            format!(
                "runs={},tasks={},dispatches={}",
                run_to_task.len(),
                old_task_to_step.len(),
                old_task_to_step.len()
            ),
            ts
        ],
    )?;
    Ok(())
}

pub fn migrations() -> Vec<Migration> {
    vec![
        Migration {
            version: 1,
            name: "legacy-engine",
            up: up_v1,
        },
        Migration {
            version: 2,
            name: "agent-workspace",
            up: up_v2,
        },
    ]
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Arc<Store>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建数据库目录失败: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库失败: {}", path.display()))?;
        Self::init(conn).map(Arc::new)
    }

    pub fn memory() -> Result<Arc<Store>> {
        Self::init(Connection::open_in_memory()?).map(Arc::new)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Store {
            conn: Mutex::new(conn),
        };
        store.run_migrations()?;
        Ok(store)
    }

    /// 依次应用未执行的迁移;单个迁移失败整体回滚并报错。
    pub fn run_migrations(&self) -> Result<()> {
        self.run_migrations_with(migrations())
    }

    pub fn run_migrations_with(&self, list: Vec<Migration>) -> Result<()> {
        let mut conn = self.conn.lock();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at TEXT NOT NULL)",
        )?;
        for m in list {
            let applied: bool = conn
                .query_row(
                    "SELECT 1 FROM schema_migrations WHERE version = ?1",
                    params![m.version],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if applied {
                continue;
            }
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            (m.up)(&tx).with_context(|| format!("迁移 v{} ({}) 失败", m.version, m.name))?;
            tx.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                params![m.version, m.name, now()],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock();
        f(&conn)
    }

    /// 在写事务中执行(供 Orchestrator 组合多步状态变更)。
    pub fn with_tx<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// 依赖满足的 pending/blocked Step → ready(活动 revision)。
    pub fn promote_ready(&self, revision_id: i64) -> Result<Vec<StepView>> {
        self.with_tx(|c| Self::promote_ready_tx(c, revision_id))
    }

    // ---------- Task ----------

    pub fn create_task(&self, title: &str, goal: &str) -> Result<TaskView> {
        let ts = now();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO agent_tasks (title, goal, status, created_at, updated_at) VALUES (?1, ?2, 'draft', ?3, ?3)",
                params![title, goal, ts],
            )?;
            Self::task_view_by_id(c, c.last_insert_rowid())
                .transpose()
                .unwrap_or_else(|| Err(anyhow::anyhow!("task 插入后读取失败")))
        })
    }

    fn task_view_by_id(c: &Connection, id: i64) -> Result<Option<TaskView>> {
        c.query_row(
            "SELECT t.id, t.title, t.goal, t.status, t.paused, t.unread, t.active_revision,
                    (SELECT COUNT(*) FROM pipeline_revisions r WHERE r.task_id = t.id) AS rev_count,
                    t.created_at, t.updated_at
             FROM agent_tasks t WHERE t.id = ?1",
            params![id],
            |r| {
                Ok(TaskView {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    goal: r.get(2)?,
                    status: TaskStatus::parse(&r.get::<_, String>(3)?).unwrap_or(TaskStatus::Draft),
                    paused: r.get::<_, i64>(4)? != 0,
                    unread: r.get::<_, i64>(5)? != 0,
                    active_revision: r.get(6)?,
                    revision_count: r.get(7)?,
                    created_at: r.get(8)?,
                    updated_at: r.get(9)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn task_view(&self, id: i64) -> Result<Option<TaskView>> {
        self.with_conn(|c| Self::task_view_by_id(c, id))
    }

    pub fn list_tasks(&self, include_archived: bool) -> Result<Vec<TaskView>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT t.id FROM agent_tasks t WHERE (?1 OR t.status != 'archived') ORDER BY t.id DESC",
            )?;
            let ids: Vec<i64> = stmt
                .query_map(params![include_archived], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            Ok(ids.iter().filter_map(|id| Self::task_view_by_id(c, *id).ok().flatten()).collect())
        })
    }

    pub fn set_task_status(&self, id: i64, status: TaskStatus) -> Result<Option<TaskView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE agent_tasks SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, status.as_str(), now()],
            )?;
            Self::task_view_by_id(c, id)
        })
    }

    pub fn set_task_unread(&self, id: i64, unread: bool) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE agent_tasks SET unread = ?2 WHERE id = ?1",
                params![id, unread as i64],
            )?;
            Ok(())
        })
    }

    pub fn set_task_paused(&self, id: i64, paused: bool) -> Result<Option<TaskView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE agent_tasks SET paused = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, paused as i64, now()],
            )?;
            Self::task_view_by_id(c, id)
        })
    }

    pub fn update_task_meta(&self, id: i64, title: &str, goal: &str) -> Result<Option<TaskView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE agent_tasks SET title = ?2, goal = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, title, goal, now()],
            )?;
            Self::task_view_by_id(c, id)
        })
    }

    // ---------- Revision / Step ----------

    fn insert_revision_steps(
        c: &Connection,
        task_id: i64,
        rev_id: i64,
        draft: &PipelineDraft,
        status_of: impl Fn(&str, &SessionPolicy, usize) -> (String, i32, Option<String>),
    ) -> Result<Vec<StepView>> {
        let ts = now();
        let mut key_to_id = HashMap::new();
        for s in &draft.steps {
            let (status, attempts, result) = status_of(&s.key, &s.session_policy, 0);
            c.execute(
                "INSERT INTO steps (revision_id, task_id, step_key, title, instructions,
                    agent_profile, session_policy, status, attempts, result, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                params![
                    rev_id,
                    task_id,
                    s.key,
                    s.title,
                    s.instructions,
                    s.agent_profile,
                    s.session_policy.as_db_str(),
                    status,
                    attempts,
                    result,
                    ts,
                ],
            )?;
            key_to_id.insert(s.key.clone(), c.last_insert_rowid());
        }
        for s in &draft.steps {
            if let Some(sid) = key_to_id.get(&s.key) {
                for d in &s.deps {
                    if let Some(did) = key_to_id.get(d) {
                        c.execute(
                            "INSERT OR IGNORE INTO step_deps (step_id, dep_step_id) VALUES (?1, ?2)",
                            params![sid, did],
                        )?;
                    }
                }
            }
        }
        Self::revision_steps_by_id(c, rev_id)
    }

    /// 保存草案为新的(未激活)Revision。用于初始草案、Planner 草案与暂停后的编辑。
    pub fn create_draft_revision(
        &self,
        task_id: i64,
        draft: &PipelineDraft,
    ) -> Result<RevisionView> {
        self.with_conn(|c| {
            let ts = now();
            let next: i64 = c.query_row(
                "SELECT COALESCE(MAX(revision), 0) + 1 FROM pipeline_revisions WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )?;
            c.execute(
                "INSERT INTO pipeline_revisions (task_id, revision, status, created_at) VALUES (?1, ?2, 'draft', ?3)",
                params![task_id, next, ts],
            )?;
            let rev_id = c.last_insert_rowid();
            Self::insert_revision_steps(c, task_id, rev_id, draft, |_, _, _| {
                ("pending".into(), 0, None)
            })?;
            c.execute(
                "UPDATE agent_tasks SET updated_at = ?2 WHERE id = ?1",
                params![task_id, ts],
            )?;
            Ok(RevisionView { id: rev_id, task_id, revision: next, status: RevisionStatus::Draft, created_at: ts })
        })
    }

    /// 编辑规则(ADR:运行中修改 DAG 必须先暂停,且只允许修改尚未启动的 Step):
    /// 已启动(attempts > 0)的 Step 必须原样带入新 Revision(依赖只允许减少被删除的未启动依赖)。
    pub fn save_edited_revision(
        &self,
        task_id: i64,
        draft: &PipelineDraft,
    ) -> Result<RevisionView> {
        self.with_conn(|c| {
            let ts = now();
            let current = Self::active_revision_by_id(c, task_id)?
                .ok_or_else(|| anyhow::anyhow!("任务没有活动 Revision,请先用 create_draft_revision"))?;
            let current_steps = Self::revision_steps_by_id(c, current.id)?;
            let by_key: HashMap<&str, &StepView> =
                current_steps.iter().map(|s| (s.step_key.as_str(), s)).collect();
            // 校验已启动 Step 不可修改
            for old in &current_steps {
                if old.attempts == 0 {
                    continue;
                }
                let new = match draft.step(&old.step_key) {
                    Some(s) => s,
                    None => anyhow::bail!("已启动的 Step `{}` 不能删除", old.step_key),
                };
                if new.title != old.title
                    || new.instructions != old.instructions
                    || new.agent_profile != old.agent_profile
                    || new.session_policy.as_db_str() != old.session_policy
                {
                    anyhow::bail!("已启动的 Step `{}` 不能修改(仅允许调整其依赖中未启动节点的去留)", old.step_key);
                }
                let old_deps: Vec<&str> = old
                    .deps
                    .iter()
                    .filter_map(|id| current_steps.iter().find(|s| s.id == *id).map(|s| s.step_key.as_str()))
                    .collect();
                for d in &new.deps {
                    if !old_deps.contains(&d.as_str()) {
                        anyhow::bail!("已启动的 Step `{}` 不能新增依赖 `{}`", old.step_key, d);
                    }
                }
            }
            let next: i64 = c.query_row(
                "SELECT COALESCE(MAX(revision), 0) + 1 FROM pipeline_revisions WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )?;
            c.execute(
                "INSERT INTO pipeline_revisions (task_id, revision, status, created_at) VALUES (?1, ?2, 'draft', ?3)",
                params![task_id, next, ts],
            )?;
            let rev_id = c.last_insert_rowid();
            Self::insert_revision_steps(c, task_id, rev_id, draft, |key, _, _| {
                if let Some(old) = by_key.get(key) {
                    (old.status.as_str().into(), old.attempts, old.result.clone())
                } else {
                    ("pending".into(), 0, None)
                }
            })?;
            // 已启动 Step 的时间戳与结果保持
            for old in &current_steps {
                if old.attempts == 0 {
                    continue;
                }
                let new_id: Option<i64> = c
                    .query_row(
                        "SELECT id FROM steps WHERE revision_id = ?1 AND step_key = ?2",
                        params![rev_id, old.step_key],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(nid) = new_id {
                    c.execute(
                        "UPDATE steps SET started_at = ?2, ended_at = ?3, updated_at = ?4 WHERE id = ?1",
                        params![nid, old.started_at, old.ended_at, ts],
                    )?;
                }
            }
            c.execute(
                "UPDATE pipeline_revisions SET status = 'superseded' WHERE id = ?1",
                params![current.id],
            )?;
            c.execute(
                "UPDATE pipeline_revisions SET status = 'active' WHERE id = ?1",
                params![rev_id],
            )?;
            c.execute(
                "UPDATE agent_tasks SET active_revision = ?2, updated_at = ?3 WHERE id = ?1",
                params![task_id, rev_id, ts],
            )?;
            Ok(RevisionView { id: rev_id, task_id, revision: next, status: RevisionStatus::Active, created_at: ts })
        })
    }

    /// 激活当前 draft revision:无依赖(或依赖已满足)的 Step → ready,Task → ready。
    pub fn activate_revision(&self, task_id: i64) -> Result<Option<TaskView>> {
        self.with_conn(|c| {
            let ts = now();
            let rev = c
                .query_row(
                    "SELECT id FROM pipeline_revisions WHERE task_id = ?1 AND status = 'draft' ORDER BY revision DESC LIMIT 1",
                    params![task_id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?;
            let rev_id = match rev {
                Some(r) => r,
                None => {
                    // 没有新草案:重复确认当前活动 revision(幂等)
                    return Self::task_view_by_id(c, task_id);
                }
            };
            c.execute(
                "UPDATE pipeline_revisions SET status = 'superseded' WHERE task_id = ?1 AND status = 'active'",
                params![task_id],
            )?;
            c.execute(
                "UPDATE pipeline_revisions SET status = 'active' WHERE id = ?1",
                params![rev_id],
            )?;
            c.execute(
                "UPDATE agent_tasks SET active_revision = ?2, status = 'ready', updated_at = ?3 WHERE id = ?1",
                params![task_id, rev_id, ts],
            )?;
            Self::promote_ready_tx(c, rev_id)?;
            Self::task_view_by_id(c, task_id)
        })
    }

    fn active_revision_by_id(c: &Connection, task_id: i64) -> Result<Option<RevisionView>> {
        c.query_row(
            "SELECT id, task_id, revision, status, created_at FROM pipeline_revisions
             WHERE task_id = ?1 AND status = 'active'",
            params![task_id],
            |r| {
                Ok(RevisionView {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    revision: r.get(2)?,
                    status: RevisionStatus::parse(&r.get::<_, String>(3)?)
                        .unwrap_or(RevisionStatus::Draft),
                    created_at: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn active_revision(&self, task_id: i64) -> Result<Option<RevisionView>> {
        self.with_conn(|c| Self::active_revision_by_id(c, task_id))
    }

    fn revision_steps_by_id(c: &Connection, revision_id: i64) -> Result<Vec<StepView>> {
        let mut stmt = c.prepare(
            "SELECT id, revision_id, task_id, step_key, title, instructions, agent_profile,
                    session_policy, status, attempts, result, started_at, ended_at
             FROM steps WHERE revision_id = ?1 ORDER BY id",
        )?;
        let mut rows: Vec<StepView> = stmt
            .query_map(params![revision_id], |r| {
                Ok(StepView {
                    id: r.get(0)?,
                    revision_id: r.get(1)?,
                    task_id: r.get(2)?,
                    step_key: r.get(3)?,
                    title: r.get(4)?,
                    instructions: r.get(5)?,
                    agent_profile: r.get(6)?,
                    session_policy: r.get(7)?,
                    status: StepStatus::parse(&r.get::<_, String>(8)?)
                        .unwrap_or(StepStatus::Pending),
                    attempts: r.get(9)?,
                    result: r.get(10)?,
                    started_at: r.get(11)?,
                    ended_at: r.get(12)?,
                    deps: Vec::new(),
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        for s in &mut rows {
            let mut stmt = c.prepare("SELECT dep_step_id FROM step_deps WHERE step_id = ?1")?;
            s.deps = stmt
                .query_map(params![s.id], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<_, _>>()?;
        }
        Ok(rows)
    }

    pub fn revision_steps(&self, revision_id: i64) -> Result<Vec<StepView>> {
        self.with_conn(|c| Self::revision_steps_by_id(c, revision_id))
    }

    /// 当前活动 revision 的 steps;若无活动 revision,取最新 revision。
    pub fn task_steps(&self, task_id: i64) -> Result<Vec<StepView>> {
        self.with_conn(|c| {
            let rev = Self::active_revision_by_id(c, task_id)?.or_else(|| {
                c.query_row(
                    "SELECT id, task_id, revision, status, created_at FROM pipeline_revisions
                     WHERE task_id = ?1 ORDER BY revision DESC LIMIT 1",
                    params![task_id],
                    |r| {
                        Ok(RevisionView {
                            id: r.get(0)?,
                            task_id: r.get(1)?,
                            revision: r.get(2)?,
                            status: RevisionStatus::parse(&r.get::<_, String>(3)?)
                                .unwrap_or(RevisionStatus::Draft),
                            created_at: r.get(4)?,
                        })
                    },
                )
                .optional()
                .unwrap_or(None)
            });
            match rev {
                Some(r) => Self::revision_steps_by_id(c, r.id),
                None => Ok(Vec::new()),
            }
        })
    }

    pub fn step_view(&self, step_id: i64) -> Result<Option<StepView>> {
        self.with_conn(|c| {
            let rev: Option<i64> = c
                .query_row(
                    "SELECT revision_id FROM steps WHERE id = ?1",
                    params![step_id],
                    |r| r.get(0),
                )
                .optional()?;
            match rev {
                Some(r) => Ok(Self::revision_steps_by_id(c, r)?
                    .into_iter()
                    .find(|s| s.id == step_id)),
                None => Ok(None),
            }
        })
    }

    pub fn set_step_status(&self, step_id: i64, status: StepStatus) -> Result<Option<StepView>> {
        self.with_conn(|c| {
            let ts = now();
            c.execute(
                "UPDATE steps SET status = ?2,
                    started_at = CASE WHEN ?3 = 1 AND started_at IS NULL THEN ?4 ELSE started_at END,
                    ended_at = CASE WHEN ?5 = 1 THEN ?4 ELSE ended_at END,
                    updated_at = ?4
                 WHERE id = ?1",
                params![
                    step_id,
                    status.as_str(),
                    status.started() as i64,
                    ts,
                    status.terminal() as i64,
                ],
            )?;
            let rev: Option<i64> = c
                .query_row("SELECT revision_id FROM steps WHERE id = ?1", params![step_id], |r| r.get(0))
                .optional()?;
            match rev {
                Some(r) => Ok(Self::revision_steps_by_id(c, r)?.into_iter().find(|s| s.id == step_id)),
                None => Ok(None),
            }
        })
    }

    pub fn bump_step_attempts(&self, step_id: i64) -> Result<i32> {
        self.with_conn(|c| {
            let ts = now();
            c.execute(
                "UPDATE steps SET attempts = attempts + 1, started_at = COALESCE(started_at, ?2), updated_at = ?2 WHERE id = ?1",
                params![step_id, ts],
            )?;
            c.query_row("SELECT attempts FROM steps WHERE id = ?1", params![step_id], |r| r.get(0))
                .map_err(Into::into)
        })
    }

    /// 依赖全部成功或显式跳过后,pending/blocked Step → ready。返回被提升的 Step。
    pub fn promote_ready_tx(c: &Connection, revision_id: i64) -> Result<Vec<StepView>> {
        let steps = Self::revision_steps_by_id(c, revision_id)?;
        let status_of: HashMap<i64, StepStatus> = steps.iter().map(|s| (s.id, s.status)).collect();
        let mut promoted = Vec::new();
        for s in &steps {
            if !matches!(s.status, StepStatus::Pending | StepStatus::Blocked) {
                continue;
            }
            let ok = s.deps.iter().all(|d| {
                matches!(
                    status_of.get(d),
                    Some(StepStatus::Succeeded) | Some(StepStatus::Skipped)
                )
            });
            if ok {
                c.execute(
                    "UPDATE steps SET status = 'ready', updated_at = ?2 WHERE id = ?1",
                    params![s.id, now()],
                )?;
                promoted.push(s.clone());
            }
        }
        if !promoted.is_empty() {
            for s in &mut promoted {
                s.status = StepStatus::Ready;
            }
        }
        Ok(promoted)
    }

    /// 失败只阻塞后代:failed Step 的全部传递后代中仍 pending/ready 的 → blocked。
    pub fn block_descendants(&self, step_id: i64) -> Result<Vec<StepView>> {
        self.with_conn(|c| {
            let rev: i64 = c.query_row(
                "SELECT revision_id FROM steps WHERE id = ?1",
                params![step_id],
                |r| r.get(0),
            )?;
            let steps = Self::revision_steps_by_id(c, rev)?;
            let by_id: HashMap<i64, &StepView> = steps.iter().map(|s| (s.id, s)).collect();
            let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
            for s in &steps {
                for d in &s.deps {
                    children.entry(*d).or_default().push(s.id);
                }
            }
            let mut blocked_ids = Vec::new();
            let mut stack = vec![step_id];
            let mut seen = std::collections::HashSet::new();
            while let Some(n) = stack.pop() {
                if let Some(kids) = children.get(&n) {
                    for &k in kids {
                        if seen.insert(k) {
                            if let Some(st) = by_id.get(&k) {
                                if matches!(st.status, StepStatus::Pending | StepStatus::Ready) {
                                    blocked_ids.push(k);
                                }
                            }
                            stack.push(k);
                        }
                    }
                }
            }
            let mut out = Vec::new();
            for id in blocked_ids {
                c.execute(
                    "UPDATE steps SET status = 'blocked', updated_at = ?2 WHERE id = ?1",
                    params![id, now()],
                )?;
                if let Some(s) = by_id.get(&id) {
                    let mut s = (*s).clone();
                    s.status = StepStatus::Blocked;
                    out.push(s);
                }
            }
            Ok(out)
        })
    }

    /// 依赖图收敛判定:全部 Step 终结;全 succeeded/skipped → Ok(true)。
    pub fn revision_converged(&self, revision_id: i64) -> Result<Option<bool>> {
        self.with_conn(|c| {
            let steps = Self::revision_steps_by_id(c, revision_id)?;
            if steps.is_empty() {
                return Ok(None);
            }
            if steps.iter().any(|s| !s.status.terminal()) {
                return Ok(None);
            }
            Ok(Some(steps.iter().all(|s| {
                matches!(s.status, StepStatus::Succeeded | StepStatus::Skipped)
            })))
        })
    }

    // ---------- Agent Session ----------

    fn session_view_row(r: &rusqlite::Row) -> rusqlite::Result<SessionView> {
        Ok(SessionView {
            id: r.get(0)?,
            session_key: r.get(1)?,
            runtime: r.get(2)?,
            agent_profile: r.get(3)?,
            title: r.get(4)?,
            status: SessionStatus::parse(&r.get::<_, String>(5)?).unwrap_or(SessionStatus::Idle),
            last_instruction: r.get(6)?,
            last_reply: r.get(7)?,
            unread: r.get::<_, i64>(8)? != 0,
            created_at: r.get(9)?,
            updated_at: r.get(10)?,
        })
    }

    pub fn create_session(
        &self,
        session_key: Option<&str>,
        runtime: &str,
        agent_profile: &str,
        title: &str,
    ) -> Result<SessionView> {
        self.with_conn(|c| {
            let ts = now();
            c.execute(
                "INSERT INTO agent_sessions (session_key, runtime, agent_profile, title, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'starting', ?5, ?5)",
                params![session_key, runtime, agent_profile, title, ts],
            )?;
            Self::session_view_by_id(c, c.last_insert_rowid())
                .transpose()
                .unwrap_or_else(|| Err(anyhow::anyhow!("session 插入后读取失败")))
        })
    }

    fn session_view_by_id(c: &Connection, id: i64) -> Result<Option<SessionView>> {
        c.query_row(
            "SELECT id, session_key, runtime, agent_profile, title, status, last_instruction,
                    last_reply, unread, created_at, updated_at
             FROM agent_sessions WHERE id = ?1",
            params![id],
            Self::session_view_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn session_view(&self, id: i64) -> Result<Option<SessionView>> {
        self.with_conn(|c| Self::session_view_by_id(c, id))
    }

    pub fn find_reusable_session(
        &self,
        session_key: &str,
        agent_profile: &str,
    ) -> Result<Option<SessionView>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT id, session_key, runtime, agent_profile, title, status, last_instruction,
                        last_reply, unread, created_at, updated_at
                 FROM agent_sessions
                 WHERE session_key = ?1 AND agent_profile = ?2 AND status NOT IN ('dead', 'hidden')
                 ORDER BY id DESC LIMIT 1",
                params![session_key, agent_profile],
                Self::session_view_row,
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionView>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, session_key, runtime, agent_profile, title, status, last_instruction,
                        last_reply, unread, created_at, updated_at
                 FROM agent_sessions ORDER BY id DESC LIMIT 500",
            )?;
            let rows = stmt
                .query_map([], Self::session_view_row)?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
    }

    pub fn update_session(
        &self,
        id: i64,
        status: Option<SessionStatus>,
        last_instruction: Option<&str>,
        last_reply: Option<&str>,
    ) -> Result<Option<SessionView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE agent_sessions SET
                    status = COALESCE(?2, status),
                    last_instruction = COALESCE(?3, last_instruction),
                    last_reply = COALESCE(?4, last_reply),
                    updated_at = ?5
                 WHERE id = ?1",
                params![
                    id,
                    status.map(|s| s.as_str()),
                    last_instruction,
                    last_reply,
                    now(),
                ],
            )?;
            Self::session_view_by_id(c, id)
        })
    }

    pub fn set_session_unread(&self, id: i64, unread: bool) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE agent_sessions SET unread = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, unread as i64, now()],
            )?;
            Ok(())
        })
    }

    // ---------- Agent Run ----------

    fn run_view_row(r: &rusqlite::Row) -> rusqlite::Result<RunView> {
        Ok(RunView {
            id: r.get(0)?,
            task_id: r.get(1)?,
            step_id: r.get(2)?,
            revision_id: r.get(3)?,
            session_id: r.get(4)?,
            status: RunStatus::parse(&r.get::<_, String>(5)?).unwrap_or(RunStatus::Running),
            agent_state: AgentState::parse(&r.get::<_, String>(6)?).unwrap_or(AgentState::Working),
            capability_token: r.get(7)?,
            outcome: r.get(8)?,
            outcome_payload: r.get(9)?,
            started_at: r.get(10)?,
            ended_at: r.get(11)?,
        })
    }

    const RUN_COLS: &'static str =
        "id, task_id, step_id, revision_id, session_id, status, agent_state, capability_token, outcome, outcome_payload, started_at, ended_at";

    pub fn create_run(
        &self,
        task_id: i64,
        step_id: i64,
        revision_id: i64,
        session_id: Option<i64>,
    ) -> Result<RunView> {
        self.with_conn(|c| {
            let ts = now();
            c.execute(
                "INSERT INTO agent_runs (task_id, step_id, revision_id, session_id, status,
                    capability_token, agent_state, started_at)
                 VALUES (?1, ?2, ?3, ?4, 'running', ?5, 'working', ?6)",
                params![
                    task_id,
                    step_id,
                    revision_id,
                    session_id,
                    gen_capability_token(),
                    ts
                ],
            )?;
            Self::run_view_by_id(c, c.last_insert_rowid())
                .transpose()
                .unwrap_or_else(|| Err(anyhow::anyhow!("run 插入后读取失败")))
        })
    }

    fn run_view_by_id_tx(tx: &Transaction, id: i64) -> Result<Option<RunView>> {
        tx.query_row(
            &format!("SELECT {} FROM agent_runs WHERE id = ?1", Self::RUN_COLS),
            params![id],
            Self::run_view_row,
        )
        .optional()
        .map_err(Into::into)
    }

    fn run_view_by_id(c: &Connection, id: i64) -> Result<Option<RunView>> {
        c.query_row(
            &format!("SELECT {} FROM agent_runs WHERE id = ?1", Self::RUN_COLS),
            params![id],
            Self::run_view_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn run_view(&self, id: i64) -> Result<Option<RunView>> {
        self.with_conn(|c| Self::run_view_by_id(c, id))
    }

    pub fn run_by_token(&self, token: &str) -> Result<Option<RunView>> {
        self.with_conn(|c| {
            c.query_row(
                &format!(
                    "SELECT {} FROM agent_runs WHERE capability_token = ?1",
                    Self::RUN_COLS
                ),
                params![token],
                Self::run_view_row,
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn set_run_status(&self, id: i64, status: RunStatus) -> Result<Option<RunView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE agent_runs SET status = ?2,
                    ended_at = CASE ?3 WHEN 1 THEN ?4 ELSE ended_at END
                 WHERE id = ?1",
                params![
                    id,
                    status.as_str(),
                    !matches!(status, RunStatus::Running | RunStatus::AwaitingOutcome) as i64,
                    now(),
                ],
            )?;
            Self::run_view_by_id(c, id)
        })
    }

    pub fn set_run_agent_state(&self, id: i64, state: AgentState) -> Result<Option<RunView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE agent_runs SET agent_state = ?2 WHERE id = ?1",
                params![id, state.as_str()],
            )?;
            Self::run_view_by_id(c, id)
        })
    }

    pub fn list_runs_of_step(&self, step_id: i64) -> Result<Vec<RunView>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {} FROM agent_runs WHERE step_id = ?1 ORDER BY id",
                Self::RUN_COLS
            ))?;
            let rows = stmt
                .query_map(params![step_id], Self::run_view_row)?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
    }

    pub fn list_runs_of_task(&self, task_id: i64) -> Result<Vec<RunView>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {} FROM agent_runs WHERE task_id = ?1 ORDER BY id DESC LIMIT 200",
                Self::RUN_COLS
            ))?;
            let rows = stmt
                .query_map(params![task_id], Self::run_view_row)?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
    }

    pub fn running_runs(&self) -> Result<Vec<RunView>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {} FROM agent_runs WHERE status IN ('running', 'awaiting-outcome') ORDER BY id",
                Self::RUN_COLS
            ))?;
            let rows = stmt
                .query_map([], Self::run_view_row)?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
    }

    pub fn active_run_of_step(&self, step_id: i64) -> Result<Option<RunView>> {
        self.with_conn(|c| {
            c.query_row(
                &format!(
                    "{} WHERE step_id = ?1 AND status IN ('running', 'awaiting-outcome') ORDER BY id DESC LIMIT 1",
                    format!("SELECT {} FROM agent_runs", Self::RUN_COLS)
                ),
                params![step_id],
                Self::run_view_row,
            )
            .optional()
            .map_err(Into::into)
        })
    }

    /// 派发原子化:bump attempts + step→running + 建 run 必须同事务,
    /// 防止崩溃窗口产生「running 但无 run」的孤儿 step。
    pub fn dispatch_run(
        &self,
        task_id: i64,
        step_id: i64,
        revision_id: i64,
        session_id: i64,
    ) -> Result<RunView> {
        let ts = now();
        self.with_tx(|tx| {
            tx.execute(
                "UPDATE steps SET attempts = attempts + 1, status = 'running',
                    started_at = COALESCE(started_at, ?2), updated_at = ?2
                 WHERE id = ?1",
                params![step_id, ts],
            )?;
            tx.execute(
                "INSERT INTO agent_runs (task_id, step_id, revision_id, session_id, status,
                    capability_token, agent_state, started_at)
                 VALUES (?1, ?2, ?3, ?4, 'running', ?5, 'working', ?6)",
                params![
                    task_id,
                    step_id,
                    revision_id,
                    session_id,
                    gen_capability_token(),
                    ts
                ],
            )?;
            Self::run_view_by_id_tx(tx, tx.last_insert_rowid())?
                .ok_or_else(|| anyhow::anyhow!("run 插入后读取失败"))
        })
    }

    /// 任务是否有活动 run(直接按状态查询,不受 LIMIT 影响)。
    pub fn task_has_active_runs(&self, task_id: i64) -> Result<bool> {
        self.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM agent_runs
                 WHERE task_id = ?1 AND status IN ('running','awaiting-outcome')",
                params![task_id],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }

    /// 孤儿 step 修复:非终态但没有任何活动 run 的 step(崩溃窗口遗留)→ 标记失败,
    /// 对应任务进入 needs-you,避免永久卡死。
    pub fn repair_orphan_steps(&self) -> Result<Vec<i64>> {
        let ts = now();
        self.with_tx(|tx| {
            let mut stmt = tx.prepare(
                "SELECT s.id, s.task_id FROM steps s
                 WHERE s.status IN ('running','awaiting-outcome','needs-input')
                   AND NOT EXISTS (
                     SELECT 1 FROM agent_runs r
                     WHERE r.step_id = s.id AND r.status IN ('running','awaiting-outcome')
                   )",
            )?;
            let orphans: Vec<(i64, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            for (step_id, task_id) in &orphans {
                tx.execute(
                    "UPDATE steps SET status = 'failed', result = '崩溃窗口修复:step 无活动 run',
                        ended_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![step_id, ts],
                )?;
                tx.execute(
                    "UPDATE agent_tasks SET status = 'needs-you', unread = 1, updated_at = ?2
                     WHERE id = ?1 AND status NOT IN ('succeeded','failed','cancelled','archived')",
                    params![task_id, ts],
                )?;
            }
            Ok(orphans.iter().map(|(s, _)| *s).collect())
        })
    }

    /// 显式结算:能力令牌校验、幂等、冲突拒绝。
    pub fn settle_run_by_token(
        &self,
        token: &str,
        settlement: Settlement,
    ) -> std::result::Result<(RunView, SettleOutcome), SettleError> {
        self.settle_impl(token, None, settlement)
    }

    pub fn settle_run_by_id(
        &self,
        run_id: i64,
        settlement: Settlement,
    ) -> std::result::Result<(RunView, SettleOutcome), SettleError> {
        self.settle_impl("", Some(run_id), settlement)
    }

    fn settle_impl(
        &self,
        token: &str,
        run_id: Option<i64>,
        settlement: Settlement,
    ) -> std::result::Result<(RunView, SettleOutcome), SettleError> {
        // 结算是唯一成功依据:必须整体事务 + 条件更新(防止半应用与并发双写)
        self.with_tx(|tx| -> Result<(RunView, SettleOutcome)> {
            let run = if let Some(id) = run_id {
                Self::run_view_by_id_tx(tx, id)?
            } else {
                tx.query_row(
                    &format!("SELECT {} FROM agent_runs WHERE capability_token = ?1", Self::RUN_COLS),
                    params![token],
                    Self::run_view_row,
                )
                .optional()?
            }
            .ok_or(SettleError::UnknownToken)?;

            if let Some(existing) = &run.outcome {
                if existing == settlement.kind_str() {
                    return Ok((run, SettleOutcome::AlreadyApplied));
                }
                return Err(SettleError::Conflict {
                    existing: existing.clone(),
                    attempted: settlement.kind_str().into(),
                })
                .map_err(anyhow::Error::from);
            }
            if !matches!(run.status, RunStatus::Running | RunStatus::AwaitingOutcome) {
                return Err(SettleError::RunNotActive(run.status)).map_err(anyhow::Error::from);
            }
            let ts = now();
            // 条件更新:outcome 仍为空才写入;0 行受影响 = 并发已结算 → 重读判定幂等/冲突
            let applied = tx.execute(
                "UPDATE agent_runs SET status = ?2, outcome = ?3, outcome_payload = ?4, ended_at = ?5
                 WHERE id = ?1 AND outcome IS NULL AND status IN ('running','awaiting-outcome')",
                params![
                    run.id,
                    settlement.result_status().as_str(),
                    settlement.kind_str(),
                    settlement.payload(),
                    ts,
                ],
            )?;
            if applied == 0 {
                let fresh = Self::run_view_by_id_tx(tx, run.id)?
                    .ok_or(SettleError::UnknownToken)?;
                if fresh.outcome.as_deref() == Some(settlement.kind_str()) {
                    return Ok((fresh, SettleOutcome::AlreadyApplied));
                }
                return Err(SettleError::Conflict {
                    existing: fresh.outcome.unwrap_or_default(),
                    attempted: settlement.kind_str().into(),
                })
                .map_err(anyhow::Error::from);
            }
            // 结算写入「活动 revision」中同 key 的 step(暂停编辑产生新 revision 时,
            // run 记录的 step_id 属于旧 revision,只改旧行会让新 revision 的副本永久卡在 running)
            let step_key: Option<String> = tx
                .query_row(
                    "SELECT step_key FROM steps WHERE id = ?1",
                    params![run.step_id],
                    |r| r.get(0),
                )
                .optional()?;
            let target_step: Option<i64> = match step_key {
                Some(key) => {
                    let active_rev: Option<i64> = tx
                        .query_row(
                            "SELECT active_revision FROM agent_tasks WHERE id = ?1",
                            params![run.task_id],
                            |r| r.get(0),
                        )
                        .optional()?
                        .flatten();
                    match active_rev {
                        Some(rev) => tx
                            .query_row(
                                "SELECT id FROM steps WHERE revision_id = ?1 AND step_key = ?2",
                                params![rev, key],
                                |r| r.get(0),
                            )
                            .optional()?,
                        None => None,
                    }
                }
                None => None,
            };
            let step_id = target_step.unwrap_or(run.step_id);
            tx.execute(
                "UPDATE steps SET status = ?2, result = ?3, ended_at = ?4, updated_at = ?4
                 WHERE id = ?1 AND status NOT IN ('succeeded','failed','skipped','cancelled')",
                params![step_id, settlement.step_status().as_str(), settlement.payload(), ts],
            )?;
            let updated = Self::run_view_by_id_tx(tx, run.id)?
                .ok_or(SettleError::UnknownToken)?;
            Ok((updated, SettleOutcome::Applied))
        })
        .map_err(|e| match e.downcast_ref::<SettleError>() {
            Some(s) => s.clone(),
            None => SettleError::Db(e.to_string()),
        })
    }

    /// 异常退出恢复:未结算 Agent Run → interrupted,对应 Task → needs-you。
    /// 正常启动(非崩溃)时没有 running 记录,本方法为 no-op。
    /// done/idle 会话是合法终态,不判死(否则 reuse 策略重启后无法复用)。
    pub fn recover_interrupted(&self) -> Result<Vec<RunView>> {
        let ts = now();
        self.with_tx(|tx| {
            let affected_ids: Vec<i64> = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM agent_runs WHERE status IN ('running','awaiting-outcome')",
                )?;
                let ids = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<std::result::Result<_, _>>()?;
                drop(stmt);
                ids
            };
            if affected_ids.is_empty() {
                return Ok(Vec::new());
            }
            // 逐个参数化更新,避免字符串拼接 SQL
            for id in &affected_ids {
                tx.execute(
                    "UPDATE agent_runs SET status = 'interrupted', ended_at = ?2 WHERE id = ?1",
                    params![id, ts],
                )?;
                tx.execute(
                    "UPDATE steps SET status = 'failed', ended_at = ?2, updated_at = ?2
                     WHERE id = (SELECT step_id FROM agent_runs WHERE id = ?1)
                       AND status NOT IN ('succeeded','failed','skipped','cancelled')",
                    params![id, ts],
                )?;
                tx.execute(
                    "UPDATE agent_tasks SET status = 'needs-you', unread = 1, updated_at = ?2
                     WHERE id = (SELECT task_id FROM agent_runs WHERE id = ?1)
                       AND status NOT IN ('succeeded','failed','cancelled','archived')",
                    params![id, ts],
                )?;
            }
            // 只有真正活动的会话进程才随崩溃消失;done/idle 保留
            tx.execute(
                "UPDATE agent_sessions SET status = 'dead', updated_at = ?1
                 WHERE status IN ('starting','working','waiting','blocked')",
                params![ts],
            )?;
            let mut out = Vec::new();
            for id in &affected_ids {
                if let Some(r) = Self::run_view_by_id_tx(tx, *id)? {
                    out.push(r);
                }
            }
            Ok(out)
        })
    }

    // ---------- Event / Question / 导入标记 ----------

    pub fn push_event(&self, kind: &str, payload: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO events (kind, payload, created_at) VALUES (?1, ?2, ?3)",
                params![kind, payload, now()],
            )?;
            Ok(())
        })
    }

    pub fn list_events(&self, limit: i64) -> Result<Vec<(i64, String, String, String)>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, kind, payload, created_at FROM events ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
    }

    pub fn ask_question(
        &self,
        task_id: i64,
        step_id: Option<i64>,
        run_id: Option<i64>,
        question: &str,
    ) -> Result<StepQuestionView> {
        self.with_conn(|c| {
            let ts = now();
            c.execute(
                "INSERT INTO step_questions (task_id, step_id, run_id, question, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'open', ?5)",
                params![task_id, step_id, run_id, question, ts],
            )?;
            Self::question_view_by_id(c, c.last_insert_rowid())
                .transpose()
                .unwrap_or_else(|| Err(anyhow::anyhow!("question 插入后读取失败")))
        })
    }

    fn question_view_by_id(c: &Connection, id: i64) -> Result<Option<StepQuestionView>> {
        c.query_row(
            "SELECT id, task_id, step_id, run_id, question, answer, status, created_at
             FROM step_questions WHERE id = ?1",
            params![id],
            |r| {
                Ok(StepQuestionView {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    step_id: r.get(2)?,
                    run_id: r.get(3)?,
                    question: r.get(4)?,
                    answer: r.get(5)?,
                    status: r.get(6)?,
                    created_at: r.get(7)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn answer_question(&self, id: i64, answer: &str) -> Result<Option<StepQuestionView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE step_questions SET answer = ?2, status = 'answered', answered_at = ?3
                 WHERE id = ?1 AND status = 'open'",
                params![id, answer, now()],
            )?;
            Self::question_view_by_id(c, id)
        })
    }

    pub fn open_questions(&self, task_id: Option<i64>) -> Result<Vec<StepQuestionView>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, task_id, step_id, run_id, question, answer, status, created_at
                 FROM step_questions WHERE status = 'open' AND (?1 IS NULL OR task_id = ?1)
                 ORDER BY id",
            )?;
            let rows = stmt
                .query_map(params![task_id], |r| {
                    Ok(StepQuestionView {
                        id: r.get(0)?,
                        task_id: r.get(1)?,
                        step_id: r.get(2)?,
                        run_id: r.get(3)?,
                        question: r.get(4)?,
                        answer: r.get(5)?,
                        status: r.get(6)?,
                        created_at: r.get(7)?,
                    })
                })?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
    }

    pub fn mark_import(&self, name: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO import_markers (name, applied_at) VALUES (?1, ?2)",
                params![name, now()],
            )?;
            Ok(())
        })
    }

    pub fn has_import(&self, name: &str) -> Result<bool> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT 1 FROM import_markers WHERE name = ?1",
                params![name],
                |_| Ok(true),
            )
            .optional()
            .map(|o| o.unwrap_or(false))
            .map_err(Into::into)
        })
    }
}

fn db_err(e: rusqlite::Error) -> SettleError {
    SettleError::Db(e.to_string())
}
#[allow(unused)]
fn _unused_db_err_guard() {
    let _ = db_err;
}
