use crate::types::*;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// SQLite 持久层:任务 DAG / 派发 / 邮箱 / 问题
/// 单写者模型:所有写操作经由 Mutex<Connection>
pub struct Db {
    conn: Mutex<Connection>,
}

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
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
);
"#;

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ---------- runs ----------

    pub fn create_run(&self, objective: &str) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO runs (objective, status, created_at) VALUES (?1, 'active', ?2)",
            params![objective, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn finish_run(&self, run_id: i64, status: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE runs SET status = ?2 WHERE id = ?1",
            params![run_id, status],
        )?;
        Ok(())
    }

    pub fn run_active(&self, run_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let s: Option<String> = conn
            .query_row(
                "SELECT status FROM runs WHERE id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(s.as_deref() == Some("active"))
    }

    /// 最近一次 run(优先活跃的)
    pub fn latest_run(&self) -> Result<Option<RunView>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT id, objective, status FROM runs ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok(RunView {
                        id: r.get(0)?,
                        objective: r.get(1)?,
                        status: r.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// run 是否所有任务终结且至少有一个完成
    pub fn run_converged(&self, run_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let unfinished: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE run_id = ?1 AND status IN ('pending','ready','dispatched')",
            params![run_id],
            |r| r.get(0),
        )?;
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )?;
        Ok(total > 0 && unfinished == 0)
    }

    // ---------- tasks ----------

    /// 创建任务;deps 全部 completed 时直接 ready
    pub fn create_task(
        &self,
        run_id: i64,
        parent_id: Option<i64>,
        spec: &str,
        deps: &[i64],
    ) -> Result<TaskView> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let deps_json = serde_json::to_string(deps)?;
        let mut all_done = true;
        for &d in deps {
            let s: String = tx.query_row(
                "SELECT status FROM tasks WHERE id = ?1",
                params![d],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "missing".into());
            if s != "completed" {
                all_done = false;
            }
        }
        let status = if all_done { "ready" } else { "pending" };
        tx.execute(
            "INSERT INTO tasks (run_id, parent_id, spec, status, deps, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![run_id, parent_id, spec, status, deps_json, now()],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        self.task_by_id_inner(&mut conn, id)
    }

    fn task_by_id_inner(&self, conn: &mut Connection, id: i64) -> Result<TaskView> {
        let conn = &mut *conn;
        conn.query_row(
            "SELECT id, run_id, parent_id, spec, status, deps, result, failure_count
             FROM tasks WHERE id = ?1",
            params![id],
            |r| {
                let deps_json: String = r.get(5)?;
                Ok(TaskView {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    parent_id: r.get(2)?,
                    spec: r.get(3)?,
                    status: TaskStatus::parse(&r.get::<_, String>(4)?).unwrap_or(TaskStatus::Pending),
                    deps: serde_json::from_str(&deps_json).unwrap_or_default(),
                    result: r.get(6)?,
                    failure_count: r.get(7)?,
                })
            },
        )
        .with_context(|| format!("load task {id}"))
    }

    pub fn task_view(&self, id: i64) -> Result<TaskView> {
        let mut conn = self.conn.lock();
        self.task_by_id_inner(&mut conn, id)
    }

    pub fn tasks_of_run(&self, run_id: i64) -> Result<Vec<TaskView>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, run_id, parent_id, spec, status, deps, result, failure_count
             FROM tasks WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                let deps_json: String = r.get(5)?;
                Ok(TaskView {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    parent_id: r.get(2)?,
                    spec: r.get(3)?,
                    status: TaskStatus::parse(&r.get::<_, String>(4)?).unwrap_or(TaskStatus::Pending),
                    deps: serde_json::from_str(&deps_json).unwrap_or_default(),
                    result: r.get(6)?,
                    failure_count: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 原子认领一个 ready 任务(无活跃派发)→ (task, dispatch_id)
    pub fn claim_next_ready(&self, worker: &str) -> Result<Option<(TaskView, i64)>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let candidate: Option<i64> = tx
            .query_row(
                "SELECT t.id FROM tasks t
                 WHERE t.status = 'ready'
                   AND NOT EXISTS (SELECT 1 FROM dispatches d WHERE d.task_id = t.id AND d.status = 'dispatched')
                 ORDER BY t.id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let Some(task_id) = candidate else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "UPDATE tasks SET status = 'dispatched', updated_at = ?2 WHERE id = ?1",
            params![task_id, now()],
        )?;
        tx.execute(
            "INSERT INTO dispatches (task_id, status, worker, started_at) VALUES (?1, 'dispatched', ?2, ?3)",
            params![task_id, worker, now()],
        )?;
        let dispatch_id = tx.last_insert_rowid();
        tx.commit()?;
        let view = self.task_by_id_inner(&mut conn, task_id)?;
        Ok(Some((view, dispatch_id)))
    }

    /// 任务完成:结算派发、提升依赖者、返回状态变化了的任务(用于发事件)
    pub fn complete_task(&self, task_id: i64, dispatch_id: i64, result: &str) -> Result<Vec<TaskView>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE dispatches SET status = 'completed', ended_at = ?3 WHERE id = ?1 AND task_id = ?2",
            params![dispatch_id, task_id, now()],
        )?;
        tx.execute(
            "UPDATE tasks SET status = 'completed', result = ?2, updated_at = ?3 WHERE id = ?1",
            params![task_id, result, now()],
        )?;
        let changed = Self::promote_dependents_tx(&tx, task_id)?;
        tx.commit()?;
        Ok(changed)
    }

    /// 任务失败:计数累计,超阈值熔断
    pub fn fail_task(&self, task_id: i64, dispatch_id: i64, reason: &str, max_failures: i32) -> Result<Vec<TaskView>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE dispatches SET status = 'failed', ended_at = ?3 WHERE id = ?1 AND task_id = ?2",
            params![dispatch_id, task_id, now()],
        )?;
        let failure_count: i32 = tx.query_row(
            "SELECT failure_count FROM tasks WHERE id = ?1",
            params![task_id],
            |r| r.get(0),
        )?;
        let new_count = failure_count + 1;
        let new_status = if new_count >= max_failures { "blocked" } else { "ready" };
        tx.execute(
            "UPDATE tasks SET status = ?2, failure_count = ?3, updated_at = ?4 WHERE id = ?1",
            params![task_id, new_status, new_count, now()],
        )?;
        let _ = reason;
        let changed = Self::promote_dependents_tx(&tx, task_id)?;
        tx.commit()?;
        Ok(changed)
    }

    /// 手动重置 blocked 任务
    pub fn unblock_task(&self, task_id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE tasks SET status = 'ready', failure_count = 0, updated_at = ?2 WHERE id = ?1 AND status = 'blocked'",
            params![task_id, now()],
        )?;
        Ok(())
    }

    fn promote_dependents_tx(tx: &rusqlite::Transaction, completed_id: i64) -> Result<Vec<TaskView>> {
        // 找到依赖 completed_id 的 pending 任务
        let mut stmt = tx.prepare("SELECT id, deps FROM tasks WHERE status = 'pending'")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut promoted = Vec::new();
        for (id, deps_json) in rows {
            let deps: Vec<i64> = serde_json::from_str(&deps_json).unwrap_or_default();
            if !deps.contains(&completed_id) {
                continue;
            }
            let mut all_done = true;
            for d in &deps {
                let s: Option<String> = tx
                    .query_row("SELECT status FROM tasks WHERE id = ?1", params![d], |r| r.get(0))
                    .optional()?;
                if s.as_deref() != Some("completed") {
                    all_done = false;
                    break;
                }
            }
            if all_done {
                tx.execute(
                    "UPDATE tasks SET status = 'ready', updated_at = ?2 WHERE id = ?1",
                    params![id, now()],
                )?;
                let view = tx
                    .query_row(
                        "SELECT id, run_id, parent_id, spec, status, deps, result, failure_count FROM tasks WHERE id = ?1",
                        params![id],
                        |r| {
                            let deps_json: String = r.get(5)?;
                            Ok(TaskView {
                                id: r.get(0)?,
                                run_id: r.get(1)?,
                                parent_id: r.get(2)?,
                                spec: r.get(3)?,
                                status: TaskStatus::parse(&r.get::<_, String>(4)?).unwrap_or(TaskStatus::Pending),
                                deps: serde_json::from_str(&deps_json).unwrap_or_default(),
                                result: r.get(6)?,
                                failure_count: r.get(7)?,
                            })
                        },
                    )
                    .ok();
                if let Some(v) = view {
                    promoted.push(v);
                }
            }
        }
        Ok(promoted)
    }

    // ---------- messages ----------

    pub fn push_message(
        &self,
        run_id: i64,
        from: &str,
        to: &str,
        kind: &str,
        body: &str,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO messages (run_id, from_handle, to_handle, kind, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![run_id, from, to, kind, body, now()],
        )?;
        Ok(())
    }

    // ---------- questions ----------

    pub fn ask_question(&self, run_id: i64, task_id: Option<i64>, question: &str) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO questions (run_id, task_id, question, status, created_at)
             VALUES (?1, ?2, ?3, 'open', ?4)",
            params![run_id, task_id, question, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 阻塞等待问题被回答(worker 轮询用)
    pub fn wait_answer(&self, question_id: i64, timeout: std::time::Duration) -> Result<Option<String>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            {
                let conn = self.conn.lock();
                let row: Option<(String, Option<String>)> = conn
                    .query_row(
                        "SELECT status, answer FROM questions WHERE id = ?1",
                        params![question_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((status, answer)) = row {
                    if status == "answered" {
                        return Ok(answer);
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }

    pub fn answer_question(&self, question_id: i64, answer: &str) -> Result<Option<QuestionView>> {
        let conn = self.conn.lock();
        let updated = conn.execute(
            "UPDATE questions SET status = 'answered', answer = ?2 WHERE id = ?1 AND status = 'open'",
            params![question_id, answer],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        let view = Self::question_view_conn(&conn, question_id)?;
        Ok(view)
    }

    pub fn question_view(&self, question_id: i64) -> Result<Option<QuestionView>> {
        let conn = self.conn.lock();
        Self::question_view_conn(&conn, question_id)
    }

    fn question_view_conn(conn: &Connection, question_id: i64) -> Result<Option<QuestionView>> {
        let view = conn
            .query_row(
                "SELECT id, run_id, task_id, question, answer FROM questions WHERE id = ?1",
                params![question_id],
                |r| {
                    Ok(QuestionView {
                        id: r.get(0)?,
                        run_id: r.get(1)?,
                        task_id: r.get(2)?,
                        question: r.get(3)?,
                        answer: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(view)
    }

    pub fn open_questions(&self, run_id: i64) -> Result<Vec<QuestionView>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT id, run_id, task_id, question, answer FROM questions WHERE run_id = ?1 AND status = 'open'")?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(QuestionView {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    task_id: r.get(2)?,
                    question: r.get(3)?,
                    answer: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lifecycle() {
        let db = Db::memory().unwrap();
        let run = db.create_run("test run").unwrap();
        let t1 = db.create_task(run, None, "task one", &[]).unwrap();
        assert_eq!(t1.status, TaskStatus::Ready);
        let t2 = db.create_task(run, None, "task two depends one", &[t1.id]).unwrap();
        assert_eq!(t2.status, TaskStatus::Pending);

        // 认领 t1
        let (claimed, d1) = db.claim_next_ready("worker-1").unwrap().unwrap();
        assert_eq!(claimed.id, t1.id);
        // 无其他 ready
        assert!(db.claim_next_ready("worker-1").unwrap().is_none());

        // 完成 t1 → t2 应被提升
        let changed = db.complete_task(t1.id, d1, "done-1").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].id, t2.id);
        assert_eq!(changed[0].status, TaskStatus::Ready);
        assert!(db.run_converged(run).unwrap() == false);

        let (_, d2) = db.claim_next_ready("worker-2").unwrap().unwrap();
        db.complete_task(t2.id, d2, "done-2").unwrap();
        assert!(db.run_converged(run).unwrap());
    }

    #[test]
    fn failure_circuit_breaker() {
        let db = Db::memory().unwrap();
        let run = db.create_run("r").unwrap();
        let t = db.create_task(run, None, "hard task", &[]).unwrap();
        for i in 0..2 {
            let (_, d) = db.claim_next_ready("w").unwrap().unwrap();
            db.fail_task(t.id, d, "boom", 3).unwrap();
            let _ = i;
        }
        assert_eq!(db.task_view(t.id).unwrap().status, TaskStatus::Ready);
        let (_, d) = db.claim_next_ready("w").unwrap().unwrap();
        db.fail_task(t.id, d, "boom", 3).unwrap();
        assert_eq!(db.task_view(t.id).unwrap().status, TaskStatus::Blocked);
        db.unblock_task(t.id).unwrap();
        assert_eq!(db.task_view(t.id).unwrap().status, TaskStatus::Ready);
    }

    #[test]
    fn question_roundtrip() {
        let db = std::sync::Arc::new(Db::memory().unwrap());
        let run = db.create_run("r").unwrap();
        let q = db.ask_question(run, None, "继续吗?").unwrap();
        let db2 = db.clone();
        let answered = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            db2.answer_question(q, "继续").unwrap().unwrap();
        });
        let ans = db.wait_answer(q, std::time::Duration::from_secs(3)).unwrap();
        assert_eq!(ans.as_deref(), Some("继续"));
        answered.join().unwrap();
    }
}
