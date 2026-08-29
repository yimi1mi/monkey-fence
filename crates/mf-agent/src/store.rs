//! v1 持久化层:Agent 工作区 schema(全新命名空间,不迁移旧数据)。
//!
//! 每项目一个 `<project>/.mf-agent/workflow-v1.db`。
//! 表:tasks(agent_tasks)/ pipeline_revisions / steps / step_deps /
//! agent_sessions / agent_runs / events / step_questions。
//! 旧库(`orchestration.db`)既不读取也不删除;schema 版本记录在 `user_version`。

use crate::model::*;
use crate::pipeline::{PipelineDraft, SessionPolicy};
use crate::schema::{
    initialize_schema, schema_version_of, table_names_of, PROJECT_SCHEMA_V1, PROJECT_SCHEMA_VERSION,
};
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

pub struct Store {
    conn: Mutex<Connection>,
}

/// 离散会话的"终态"集合(用于决定是否写 ended_at)。
fn ad_hoc_status_terminal(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Done | SessionStatus::Dead | SessionStatus::Hidden
    )
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
        initialize_schema(&conn, PROJECT_SCHEMA_V1, PROJECT_SCHEMA_VERSION)
            .context("初始化项目库 v1 schema 失败")?;
        // 早期开发库的 steps/pipeline_revisions 缺列(CREATE IF NOT EXISTS 不补列)
        {
            let mut stmt = conn.prepare("PRAGMA table_info(steps)")?;
            let has_auto = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .any(|c| c.map(|c| c == "auto_retry").unwrap_or(false));
            drop(stmt);
            if !has_auto {
                conn.execute(
                    "ALTER TABLE steps ADD COLUMN auto_retry INTEGER NOT NULL DEFAULT 0",
                    [],
                )
                .context("补齐 steps.auto_retry 列失败")?;
            }
        }
        {
            let mut stmt = conn.prepare("PRAGMA table_info(ad_hoc_sessions)")?;
            let has_display = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .any(|c| c.map(|c| c == "display_session_id").unwrap_or(false));
            drop(stmt);
            if !has_display {
                conn.execute(
                    "ALTER TABLE ad_hoc_sessions ADD COLUMN display_session_id INTEGER",
                    [],
                )
                .context("补齐 ad_hoc_sessions.display_session_id 列失败")?;
            }
        }
        {
            let mut stmt = conn.prepare("PRAGMA table_info(pipeline_revisions)")?;
            let has_snapshot = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .any(|c| c.map(|c| c == "snapshot_json").unwrap_or(false));
            drop(stmt);
            if !has_snapshot {
                conn.execute(
                    "ALTER TABLE pipeline_revisions ADD COLUMN snapshot_json TEXT",
                    [],
                )
                .context("补齐 pipeline_revisions.snapshot_json 列失败")?;
            }
        }
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.with_conn(schema_version_of)
    }

    pub fn table_names(&self) -> Result<Vec<String>> {
        self.with_conn(table_names_of)
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

    /// 工作流快照 → Step 投影(与快照落盘同一事务):
    /// 节点键→step_key、标题/说明原样、agent_profile=冻结实例的 Agent Type、
    /// 会话策略固定 fresh(每个节点独立会话),依赖按节点键接线。
    fn project_workflow_steps_tx(
        c: &Connection,
        task_id: i64,
        rev_id: i64,
        snapshot: &crate::workflow::WorkflowSnapshot,
    ) -> Result<()> {
        let ts = now();
        let mut key_to_id = HashMap::new();
        for node in &snapshot.nodes {
            c.execute(
                "INSERT INTO steps (revision_id, task_id, step_key, title, instructions,
                    agent_profile, session_policy, status, attempts, result, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'fresh', 'pending', 0, NULL, ?7, ?7)",
                params![
                    rev_id,
                    task_id,
                    node.key,
                    node.title,
                    node.instructions,
                    node.instance.agent_type,
                    ts,
                ],
            )?;
            key_to_id.insert(node.key.clone(), c.last_insert_rowid());
        }
        for node in &snapshot.nodes {
            if let Some(sid) = key_to_id.get(&node.key) {
                for dep in &node.deps {
                    if let Some(did) = key_to_id.get(dep) {
                        c.execute(
                            "INSERT OR IGNORE INTO step_deps (step_id, dep_step_id) VALUES (?1, ?2)",
                            params![sid, did],
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

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
                    session_policy, status, attempts, auto_retry, result, started_at, ended_at
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
                    auto_retry: r.get::<_, i64>(10)? as i32,
                    result: r.get(11)?,
                    started_at: r.get(12)?,
                    ended_at: r.get(13)?,
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

    /// 配置节点自动重试上限(0 = 手动重试;设计 §9.6)。
    pub fn set_step_auto_retry(&self, step_id: i64, limit: i32) -> Result<Option<StepView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE steps SET auto_retry = ?2, updated_at = ?3 WHERE id = ?1",
                params![step_id, limit, now()],
            )?;
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

    /// 孤儿 step 修复:非终态但没有任何活动 run 的 step(崩溃窗口遗留)→ 标记失败;
    /// awaiting-outcome 不算孤儿(重启恢复的合法等待确认状态),
    /// 对应任务进入 needs-you,避免永久卡死。
    pub fn repair_orphan_steps(&self) -> Result<Vec<i64>> {
        let ts = now();
        self.with_tx(|tx| {
            let mut stmt = tx.prepare(
                "SELECT s.id, s.task_id FROM steps s
                 WHERE s.status IN ('running','needs-input')
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
            // interrupted(重启后未知状态)允许人工结算(设计 §13)
            if !matches!(
                run.status,
                RunStatus::Running | RunStatus::AwaitingOutcome | RunStatus::Interrupted
            ) {
                return Err(SettleError::RunNotActive(run.status)).map_err(anyhow::Error::from);
            }
            let ts = now();
            // 条件更新:outcome 仍为空才写入;0 行受影响 = 并发已结算 → 重读判定幂等/冲突
            let applied = tx.execute(
                "UPDATE agent_runs SET status = ?2, outcome = ?3, outcome_payload = ?4, ended_at = ?5
                 WHERE id = ?1 AND outcome IS NULL
                   AND status IN ('running','awaiting-outcome','interrupted')",
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
            // 成功结算与 Handoff 落库同一事务:下游解锁的前提是两者都已持久化
            if settlement.kind_str() == "complete" {
                // 完整 Handoff:摘要 + 结构化 output(下游精确引用)+ 日志引用
                let handoff = crate::handoff::Handoff {
                    status: "complete".into(),
                    summary: settlement.payload().to_string(),
                    output: match &settlement {
                        Settlement::Complete { output, .. } => output.clone(),
                        Settlement::Fail { .. } => serde_json::Value::Null,
                    },
                    raw_log_ref: Some(format!("agent-run:{}", run.id)),
                    ..Default::default()
                };
                tx.execute(
                    "INSERT INTO handoffs (task_id, step_id, run_id, handoff_json, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        run.task_id,
                        step_id,
                        run.id,
                        serde_json::to_string(&handoff)?,
                        ts
                    ],
                )?;
            }
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
        self.recover_interrupted_with(&|_session_id| false)
    }

    /// 重启恢复(设计 §13):
    /// - 宿主确认存活的会话 → run/step/task 状态原样保留(重连);
    /// - awaiting-outcome(已退出未结算)→ 原样保留,等待人工确认;
    /// - 其余(进程状态未知)→ run = interrupted、step = awaiting-outcome
    ///   (未知不是失败)、task = needs-you;执行租约保持 held。
    /// 返回受影响(未重连)的 run 列表。
    pub fn recover_interrupted_with(
        &self,
        session_alive: &dyn Fn(i64) -> bool,
    ) -> Result<Vec<RunView>> {
        let ts = now();
        self.with_tx(|tx| {
            let rows: Vec<(i64, Option<i64>, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, session_id, status FROM agent_runs
                     WHERE status IN ('running','awaiting-outcome')",
                )?;
                let ids = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<std::result::Result<_, _>>()?;
                drop(stmt);
                ids
            };
            if rows.is_empty() {
                return Ok(Vec::new());
            }
            let mut interrupted_ids = Vec::new();
            let mut reattached_sessions: Vec<i64> = Vec::new();
            for (id, session_id, status) in &rows {
                let reattach = match session_id {
                    Some(sid) if status == "running" => session_alive(*sid),
                    _ => false,
                };
                if reattach {
                    // 宿主确认存活:重连,状态原样;其会话进程仍在运行,
                    // 不得被下方批量恢复标死
                    if let Some(sid) = session_id {
                        reattached_sessions.push(*sid);
                    }
                    continue;
                }
                if status == "awaiting-outcome" {
                    // 已退出未结算:等待人工确认,状态原样,任务提示
                    tx.execute(
                        "UPDATE agent_tasks SET status = 'needs-you', unread = 1, updated_at = ?2
                         WHERE id = (SELECT task_id FROM agent_runs WHERE id = ?1)
                           AND status NOT IN ('succeeded','failed','cancelled','archived')",
                        params![id, ts],
                    )?;
                    continue;
                }
                // 进程状态未知:interrupted + awaiting-outcome(不判失败)
                interrupted_ids.push(*id);
                tx.execute(
                    "UPDATE agent_runs SET status = 'interrupted', ended_at = ?2 WHERE id = ?1",
                    params![id, ts],
                )?;
                tx.execute(
                    "UPDATE steps SET status = 'awaiting-outcome', updated_at = ?2
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
            // 只有真正活动的会话进程才随崩溃消失;done/idle 保留,
            // reattached(宿主确认存活)的会话排除在批量标死之外
            {
                let mut stmt = tx.prepare(
                    "SELECT id FROM agent_sessions
                     WHERE status IN ('starting','working','waiting','blocked')",
                )?;
                let active: Vec<i64> = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<std::result::Result<Vec<i64>, _>>()?;
                drop(stmt);
                for sid in active {
                    if reattached_sessions.contains(&sid) {
                        continue;
                    }
                    tx.execute(
                        "UPDATE agent_sessions SET status = 'dead', updated_at = ?2 WHERE id = ?1",
                        params![sid, ts],
                    )?;
                }
            }
            let mut out = Vec::new();
            for id in &interrupted_ids {
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

    // ---------- 离散 CLI 会话 ----------

    /// 行 → 视图(快照 JSON 解析失败时返回数据库错误,不 panic)。
    fn ad_hoc_view_row(r: &rusqlite::Row) -> rusqlite::Result<AdHocSessionView> {
        let snapshot_json: String = r.get(4)?;
        Ok(AdHocSessionView {
            id: r.get(0)?,
            task_id: r.get(1)?,
            title: r.get(2)?,
            status: SessionStatus::parse(&r.get::<_, String>(3)?).unwrap_or(SessionStatus::Idle),
            display_session_id: r.get(6)?,
            snapshot: serde_json::from_str(&snapshot_json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("ad_hoc 快照损坏: {e}"),
                    )),
                )
            })?,
            handoff: r.get(5)?,
            created_at: r.get(7)?,
            launched_at: r.get(8)?,
            ended_at: r.get(9)?,
        })
    }

    const AD_HOC_COLS: &'static str = "id, task_id, title, status, snapshot_json, handoff_json, display_session_id, created_at, launched_at, ended_at";

    /// 插入离散会话行(状态 starting;启动后由 mark_ad_hoc_launched 推进)。
    pub fn insert_ad_hoc_session(
        &self,
        task_id: i64,
        snapshot: &crate::agent_instance::AgentInstanceSnapshot,
    ) -> Result<AdHocSessionView> {
        self.with_conn(|c| {
            let ts = now();
            c.execute(
                "INSERT INTO ad_hoc_sessions (task_id, title, status, snapshot_json, created_at)
                 VALUES (?1, ?2, 'starting', ?3, ?4)",
                params![task_id, snapshot.name, serde_json::to_string(snapshot)?, ts],
            )?;
            Self::ad_hoc_view_by_id(c, c.last_insert_rowid())?
                .ok_or_else(|| anyhow::anyhow!("ad_hoc 插入后读取失败"))
        })
    }

    fn ad_hoc_view_by_id(c: &Connection, id: i64) -> Result<Option<AdHocSessionView>> {
        c.query_row(
            &format!(
                "SELECT {} FROM ad_hoc_sessions WHERE id = ?1",
                Self::AD_HOC_COLS
            ),
            params![id],
            Self::ad_hoc_view_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn ad_hoc_session_view(&self, id: i64) -> Result<Option<AdHocSessionView>> {
        self.with_conn(|c| Self::ad_hoc_view_by_id(c, id))
    }

    pub fn list_ad_hoc_sessions(&self, task_id: i64) -> Result<Vec<AdHocSessionView>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {} FROM ad_hoc_sessions WHERE task_id = ?1 ORDER BY id",
                Self::AD_HOC_COLS
            ))?;
            let rows = stmt
                .query_map(params![task_id], Self::ad_hoc_view_row)?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
    }

    /// 绑定展示会话行(agent_sessions)。
    pub fn attach_display_session(
        &self,
        ad_hoc_id: i64,
        display_session_id: i64,
    ) -> Result<Option<AdHocSessionView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE ad_hoc_sessions SET display_session_id = ?2 WHERE id = ?1",
                params![ad_hoc_id, display_session_id],
            )?;
            Self::ad_hoc_view_by_id(c, ad_hoc_id)
        })
    }

    /// 启动成功:仅 starting 行推进为 working(快速退出已终结的行不得覆盖)。
    pub fn mark_ad_hoc_launched(&self, id: i64) -> Result<Option<AdHocSessionView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE ad_hoc_sessions
                 SET status = 'working', launched_at = ?2
                 WHERE id = ?1 AND status = 'starting'",
                params![id, now()],
            )?;
            Self::ad_hoc_view_by_id(c, id)
        })
    }

    pub fn set_ad_hoc_status(
        &self,
        id: i64,
        status: SessionStatus,
    ) -> Result<Option<AdHocSessionView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE ad_hoc_sessions
                 SET status = ?2,
                     ended_at = CASE WHEN ?3 = 1 THEN ?4 ELSE ended_at END
                 WHERE id = ?1",
                params![
                    id,
                    status.as_str(),
                    ad_hoc_status_terminal(status) as i64,
                    now()
                ],
            )?;
            Self::ad_hoc_view_by_id(c, id)
        })
    }

    /// 用户显式提交 Handoff:记录 JSON 并终结会话。
    pub fn submit_ad_hoc_handoff(
        &self,
        id: i64,
        handoff_json: &str,
    ) -> Result<Option<AdHocSessionView>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE ad_hoc_sessions
                 SET handoff_json = ?2, status = 'done', ended_at = ?3
                 WHERE id = ?1",
                params![id, handoff_json, now()],
            )?;
            Self::ad_hoc_view_by_id(c, id)
        })
    }

    // ---------- Workflow 快照与 Handoff ----------

    /// 保存序列化工作流快照为新 Revision:同一事务内完成快照落盘与
    /// Step 投影(节点键/标题/说明/依赖;agent_profile 投影为冻结实例的
    /// Agent Type),失败不留半截 Revision。
    pub fn create_workflow_revision(
        &self,
        task_id: i64,
        snapshot: &crate::workflow::WorkflowSnapshot,
    ) -> Result<RevisionView> {
        self.with_tx(|c| {
            let ts = now();
            let next: i64 = c.query_row(
                "SELECT COALESCE(MAX(revision), 0) + 1 FROM pipeline_revisions WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )?;
            c.execute(
                "INSERT INTO pipeline_revisions (task_id, revision, status, snapshot_json, created_at)
                 VALUES (?1, ?2, 'draft', ?3, ?4)",
                params![task_id, next, serde_json::to_string(snapshot)?, ts],
            )?;
            let rev_id = c.last_insert_rowid();
            Self::project_workflow_steps_tx(c, task_id, rev_id, snapshot)?;
            Ok(RevisionView {
                id: rev_id,
                task_id,
                revision: next,
                status: RevisionStatus::Draft,
                created_at: ts,
            })
        })
    }

    /// 删除尚未激活的 draft Revision(pin 失败回滚用;连带删除投影 Step)。
    /// 非 draft(已激活/已废弃)拒绝删除。
    pub fn delete_draft_revision(&self, revision_id: i64) -> Result<()> {
        self.with_tx(|c| {
            let status: Option<String> = c
                .query_row(
                    "SELECT status FROM pipeline_revisions WHERE id = ?1",
                    params![revision_id],
                    |r| r.get(0),
                )
                .optional()?;
            match status.as_deref() {
                None => Ok(()), // 已不存在:幂等
                Some("draft") => {
                    c.execute(
                        "DELETE FROM step_deps WHERE step_id IN
                           (SELECT id FROM steps WHERE revision_id = ?1)",
                        params![revision_id],
                    )?;
                    c.execute(
                        "DELETE FROM steps WHERE revision_id = ?1",
                        params![revision_id],
                    )?;
                    c.execute(
                        "DELETE FROM pipeline_revisions WHERE id = ?1",
                        params![revision_id],
                    )?;
                    Ok(())
                }
                Some(other) => anyhow::bail!("拒绝删除非 draft 状态的 Revision({other})"),
            }
        })
    }

    /// 任务全部 Revision 行 id(升序;pin 释放按键遍历)。
    pub fn list_revision_ids(&self, task_id: i64) -> Result<Vec<i64>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT id FROM pipeline_revisions WHERE task_id = ?1 ORDER BY id")?;
            let ids = stmt
                .query_map(params![task_id], |r| r.get(0))?
                .collect::<std::result::Result<Vec<i64>, _>>()?;
            Ok(ids)
        })
    }

    /// Revision 归属的任务 id。
    pub fn task_of_revision(&self, revision_id: i64) -> Result<Option<i64>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT task_id FROM pipeline_revisions WHERE id = ?1",
                params![revision_id],
                |r| r.get(0),
            )
            .optional()?)
        })
    }

    /// 读取 Revision 冻结的工作流快照(旧 PipelineDraft revision 为 None)。
    pub fn revision_snapshot(
        &self,
        revision_id: i64,
    ) -> Result<Option<crate::workflow::WorkflowSnapshot>> {
        self.with_conn(|c| {
            // 外层 Option:行是否存在;内层 Option:列是否为 NULL(旧 Draft Revision)
            let json: Option<Option<String>> = c
                .query_row(
                    "SELECT snapshot_json FROM pipeline_revisions WHERE id = ?1",
                    params![revision_id],
                    |r| r.get(0),
                )
                .optional()?;
            match json.flatten() {
                Some(json) => {
                    Ok(Some(serde_json::from_str(&json).with_context(|| {
                        format!("Revision {revision_id} 快照损坏")
                    })?))
                }
                None => Ok(None),
            }
        })
    }

    // ---------- 任务本地工作流(项目 Store,按 project+task 键) ----------

    /// 保存任务本地工作流草稿(同键覆盖;跨项目同 task id 互不影响)。
    pub fn save_task_workflow(
        &self,
        project_key: &str,
        task_id: i64,
        draft: &crate::workflow::WorkflowTemplateDraft,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO task_workflows (project_key, task_id, graph_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(project_key, task_id) DO UPDATE SET
                    graph_json = excluded.graph_json, updated_at = excluded.updated_at",
                params![
                    project_key,
                    task_id,
                    serde_json::to_string(&draft.nodes)?,
                    now()
                ],
            )?;
            Ok(())
        })
    }

    /// 读取任务本地工作流草稿(无草稿为 None)。
    pub fn load_task_workflow(
        &self,
        project_key: &str,
        task_id: i64,
    ) -> Result<Option<crate::workflow::WorkflowTemplateDraft>> {
        self.with_conn(|c| {
            let json: Option<String> = c
                .query_row(
                    "SELECT graph_json FROM task_workflows
                     WHERE project_key = ?1 AND task_id = ?2",
                    params![project_key, task_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(json) = json else {
                return Ok(None);
            };
            let nodes: Vec<crate::workflow::WorkflowNodeDraft> = serde_json::from_str(&json)
                .with_context(|| format!("任务 {task_id} 工作流草稿损坏"))?;
            Ok(Some(crate::workflow::WorkflowTemplateDraft {
                key: format!("task-{task_id}"),
                name: format!("任务 {task_id} 工作流"),
                task_local: true,
                nodes,
            }))
        })
    }

    // ---------- Execution Lease ----------

    /// 派发前持久化租约(崩溃后仍可审计/清理)。
    pub fn insert_execution_lease(
        &self,
        lease: &crate::execution_directory::ExecutionLease,
        run_id: Option<i64>,
        step_id: i64,
        task_id: i64,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO execution_leases
                    (lease_key, run_id, step_id, task_id, provider, path, isolated, metadata_json, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'held', ?9)
                 ON CONFLICT(lease_key) DO UPDATE SET
                    run_id = excluded.run_id, status = 'held', released_at = NULL",
                params![
                    lease.id,
                    run_id,
                    step_id,
                    task_id,
                    lease.provider,
                    lease.path.to_string_lossy(),
                    lease.isolated as i64,
                    lease.metadata.to_string(),
                    now()
                ],
            )?;
            Ok(())
        })
    }

    /// run 当前持有的租约(重启恢复后释放用)。
    pub fn held_lease_of_run(&self, run_id: i64) -> Result<Option<ExecutionLeaseRow>> {
        self.held_lease_where("run_id = ?1", params![run_id])
    }

    /// 按租约键查持有中的租约(待决汇合重启后按 lease id 释放用)。
    pub fn held_lease_by_key(&self, lease_key: &str) -> Result<Option<ExecutionLeaseRow>> {
        self.held_lease_where("lease_key = ?1", params![lease_key])
    }

    fn held_lease_where(
        &self,
        condition: &str,
        params: impl rusqlite::Params,
    ) -> Result<Option<ExecutionLeaseRow>> {
        self.with_conn(|c| {
            c.query_row(
                &format!(
                    "SELECT lease_key, run_id, step_id, task_id, provider, path, isolated,
                            metadata_json, status, created_at, released_at
                     FROM execution_leases WHERE {condition} AND status = 'held'
                     ORDER BY id DESC LIMIT 1"
                ),
                params,
                |r| {
                    Ok(ExecutionLeaseRow {
                        lease_key: r.get(0)?,
                        run_id: r.get(1)?,
                        step_id: r.get(2)?,
                        task_id: r.get(3)?,
                        provider: r.get(4)?,
                        path: r.get(5)?,
                        isolated: r.get::<_, i64>(6)? != 0,
                        metadata_json: r.get(7)?,
                        status: r.get(8)?,
                        created_at: r.get(9)?,
                        released_at: r.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
        })
    }

    /// 释放租约(终态结算/取消后)。
    pub fn release_execution_lease(&self, lease_key: &str) -> Result<bool> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE execution_leases SET status = 'released', released_at = ?2
                 WHERE lease_key = ?1 AND status = 'held'",
                params![lease_key, now()],
            )?;
            Ok(n == 1)
        })
    }

    /// 任务的租约列表(升序)。
    pub fn list_execution_leases(&self, task_id: i64) -> Result<Vec<ExecutionLeaseRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT lease_key, run_id, step_id, task_id, provider, path, isolated,
                        metadata_json, status, created_at, released_at
                 FROM execution_leases WHERE task_id = ?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map(params![task_id], |r| {
                    Ok(ExecutionLeaseRow {
                        lease_key: r.get(0)?,
                        run_id: r.get(1)?,
                        step_id: r.get(2)?,
                        task_id: r.get(3)?,
                        provider: r.get(4)?,
                        path: r.get(5)?,
                        isolated: r.get::<_, i64>(6)? != 0,
                        metadata_json: r.get(7)?,
                        status: r.get(8)?,
                        created_at: r.get(9)?,
                        released_at: r.get(10)?,
                    })
                })?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
    }

    /// 记录一条待决汇合(汇合冲突 → needs-you 时写入)。
    pub fn insert_pending_merge(
        &self,
        task_id: i64,
        lease: &crate::execution_directory::ExecutionLease,
        conflicts: &[String],
    ) -> Result<i64> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO pending_merges (task_id, lease_id, lease_json, conflicts_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    task_id,
                    lease.id,
                    serde_json::to_string(lease)?,
                    serde_json::to_string(conflicts)?,
                    now()
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// 待决汇合列表;`task_id = None` 返回全部任务(重启恢复用)。
    pub fn list_pending_merges(&self, task_id: Option<i64>) -> Result<Vec<PendingMergeRow>> {
        self.with_conn(|c| {
            let sql = match task_id {
                Some(_) => {
                    "SELECT id, task_id, lease_json, conflicts_json FROM pending_merges
                     WHERE task_id = ?1 ORDER BY id"
                }
                None => {
                    "SELECT id, task_id, lease_json, conflicts_json FROM pending_merges
                         ORDER BY id"
                }
            };
            let mut stmt = c.prepare(sql)?;
            let map_row =
                |r: &rusqlite::Row<'_>| -> std::result::Result<PendingMergeRow, rusqlite::Error> {
                    let lease_json: String = r.get(2)?;
                    let conflicts_json: String = r.get(3)?;
                    let id: i64 = r.get(0)?;
                    let lease: crate::execution_directory::ExecutionLease =
                        serde_json::from_str(&lease_json).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                lease_json.len(),
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                    Ok(PendingMergeRow {
                        id,
                        task_id: r.get(1)?,
                        lease,
                        conflicts: serde_json::from_str(&conflicts_json).unwrap_or_default(),
                    })
                };
            let rows = match task_id {
                Some(id) => stmt
                    .query_map(params![id], map_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                None => stmt
                    .query_map([], map_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            };
            Ok(rows)
        })
    }

    /// 更新待决汇合的冲突列表(重试后仍冲突时刷新)。
    pub fn update_pending_merge_conflicts(&self, id: i64, conflicts: &[String]) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE pending_merges SET conflicts_json = ?2 WHERE id = ?1",
                params![id, serde_json::to_string(conflicts)?],
            )?;
            Ok(())
        })
    }

    /// 清空任务的待决汇合(全部解决/取消);返回删除行数。
    pub fn clear_pending_merges(&self, task_id: i64) -> Result<usize> {
        self.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM pending_merges WHERE task_id = ?1",
                params![task_id],
            )?;
            Ok(n)
        })
    }

    /// 记录 Handoff(与结算同事务的写入由 Run Coordinator 组合)。
    /// 返回 handoff 行 id。
    pub fn insert_handoff(
        &self,
        task_id: i64,
        step_id: Option<i64>,
        run_id: Option<i64>,
        handoff: &crate::handoff::Handoff,
    ) -> Result<i64> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO handoffs (task_id, step_id, run_id, handoff_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    task_id,
                    step_id,
                    run_id,
                    serde_json::to_string(handoff)?,
                    now()
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// 任务的 Handoff 列表(升序):(行 id, Handoff)。
    pub fn list_handoffs(&self, task_id: i64) -> Result<Vec<(i64, crate::handoff::Handoff)>> {
        Ok(self
            .list_handoff_rows(task_id)?
            .into_iter()
            .map(|row| (row.id, row.handoff))
            .collect())
    }

    /// 任务的 Handoff 行(含 step/run 归属;工作流变量替换按 step 定位)。
    pub fn list_handoff_rows(&self, task_id: i64) -> Result<Vec<HandoffRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, step_id, run_id, handoff_json FROM handoffs
                 WHERE task_id = ?1 ORDER BY id",
            )?;
            let rows: Vec<(i64, Option<i64>, Option<i64>, String)> = stmt
                .query_map(params![task_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            rows.into_iter()
                .map(|(id, step_id, run_id, json)| {
                    let handoff: crate::handoff::Handoff = serde_json::from_str(&json)
                        .with_context(|| format!("handoff 行 {id} 损坏"))?;
                    Ok(HandoffRow {
                        id,
                        step_id,
                        run_id,
                        handoff,
                    })
                })
                .collect()
        })
    }
}
