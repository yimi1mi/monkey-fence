//! 全新存储命名空间验收:项目库与目录库相互独立,且不含任何旧 MonkeyFence 表。
//! schema 版本按 user_version 迁移(I15):全新库直接落在当前版本,
//! 旧 v1 库按链升级且数据保留。

use mf_agent::catalog_store::CatalogStore;
use mf_agent::migration;
use mf_agent::schema::PROJECT_SCHEMA_VERSION;
use mf_agent::store::Store;
use rusqlite::Connection;

#[test]
fn project_schema_starts_at_current_version_without_legacy_tables() {
    let store = Store::memory().unwrap();
    let tables = store.table_names().unwrap();
    assert!(tables.contains(&"agent_tasks".to_string()));
    assert!(!tables.contains(&"runs".to_string()));
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert!(
        tables.contains(&"merge_batches".to_string()),
        "v2 起包含 join 批持久状态表: {tables:?}"
    );
    assert!(
        tables.contains(&"step_next_attempt_sessions".to_string()),
        "v8 包含下一 attempt 会话意图表: {tables:?}"
    );
    assert!(
        tables.contains(&"question_answer_deliveries".to_string()),
        "v9 包含 question-bound 回答两阶段投递表: {tables:?}"
    );
    assert!(tables.contains(&"run_cancel_fence".to_string()));
    assert!(tables.contains(&"run_cancel_target".to_string()));
}

#[test]
fn catalog_schema_is_independent() {
    let catalog = CatalogStore::memory().unwrap();
    assert_eq!(catalog.schema_version().unwrap(), 1);
    let tables = catalog.table_names().unwrap();
    assert!(tables.contains(&"plugin_packages".to_string()));
    assert!(tables.contains(&"plugin_pins".to_string()));
    assert!(!tables.contains(&"agent_tasks".to_string()));
}

/// 真实旧 v1 库 fixture:手工构造 v1 DDL + 业务数据 + user_version=1,
/// 打开后必须事务迁移到当前版本且数据完整。
#[test]
fn legacy_v1_database_is_migrated_with_data_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        // 真实 v1 时期的最小可用库:核心表 + 数据 + user_version=1
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
                snapshot_json TEXT, created_at TEXT NOT NULL, UNIQUE(task_id, revision));
             INSERT INTO agent_tasks (title, goal, status, active_revision, created_at, updated_at)
                 VALUES ('旧任务', '旧目标', 'running', NULL, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let store = Store::open(&db).unwrap();
    assert_eq!(
        store.schema_version().unwrap(),
        PROJECT_SCHEMA_VERSION,
        "旧 v1 库必须按 user_version 链升级到当前版本"
    );
    let tasks = store.list_tasks(false).unwrap();
    assert_eq!(tasks.len(), 1, "迁移不得丢失业务数据");
    assert_eq!(tasks[0].title, "旧任务");
    let tables = store.table_names().unwrap();
    assert!(tables.contains(&"merge_batches".to_string()));
    assert!(tables.contains(&"join_deferrals".to_string()));
    assert!(tables.contains(&"step_next_attempt_sessions".to_string()));
    assert!(tables.contains(&"question_answer_deliveries".to_string()));
    let columns = store
        .with_conn(|c| {
            let mut stmt = c.prepare("PRAGMA table_info(merge_batches)")?;
            let names = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(names)
        })
        .unwrap();
    assert!(columns.contains(&"lease_digest".to_string()));
    assert!(columns.contains(&"owner_id".to_string()));
    assert!(columns.contains(&"owner_expires_at".to_string()));
}

/// 版本高于程序支持的库必须拒绝打开(禁止隐式降级)。
#[test]
fn future_schema_version_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_tasks (id INTEGER PRIMARY KEY);
             PRAGMA user_version = 99;",
        )
        .unwrap();
    }
    let err = Store::open(&db).err().expect("高版本库必须拒绝打开");
    assert!(
        format!("{err:#}").contains("高于程序支持"),
        "错误必须明示版本不兼容: {err:#}"
    );
}

#[test]
fn v7_database_migrates_to_v8_with_backup_and_retry_intent_table() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let mut conn = Connection::open(&db).unwrap();
        mf_agent::schema::upgrade_project(&mut conn, 7).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            7
        );
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='step_next_attempt_sessions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0, "v7 起点不得提前带入 v8 DDL");
    }

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert!(store
        .table_names()
        .unwrap()
        .contains(&"step_next_attempt_sessions".to_string()));
    let artifacts = migration::published_artifact_dirs(&migration::backup_dir_for(&db)).unwrap();
    assert_eq!(artifacts.len(), 1, "7→8 恰发布一个 pre-migration backup");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifacts[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["from_version"], 7);
    assert_eq!(manifest["to_version"], PROJECT_SCHEMA_VERSION);
}

/// v8 库升级到 v9 必须带来 question-bound 回答两阶段投递表,
/// 且发布 pre-migration backup(Issue #26)。
#[test]
fn v8_database_migrates_to_v9_with_question_delivery_table() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let mut conn = Connection::open(&db).unwrap();
        mf_agent::schema::upgrade_project(&mut conn, 8).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            8
        );
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='question_answer_deliveries'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0, "v8 起点不得提前带入 v9 DDL");
    }

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert!(store
        .table_names()
        .unwrap()
        .contains(&"question_answer_deliveries".to_string()));
    let artifacts = migration::published_artifact_dirs(&migration::backup_dir_for(&db)).unwrap();
    assert_eq!(artifacts.len(), 1, "8→9 恰发布一个 pre-migration backup");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifacts[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["from_version"], 8);
    assert_eq!(manifest["to_version"], PROJECT_SCHEMA_VERSION);
    // 投递表结构契约:question 主键 + nonce 唯一 + pending/delivered 状态机。
    let columns = store
        .with_conn(|c| {
            let mut stmt = c.prepare("PRAGMA table_info(question_answer_deliveries)")?;
            let names = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(names)
        })
        .unwrap();
    for expected in [
        "question_id",
        "run_id",
        "run_handle",
        "run_revision",
        "nonce",
        "answer",
        "status",
    ] {
        assert!(
            columns.contains(&expected.to_string()),
            "投递表缺列 {expected}: {columns:?}"
        );
    }
}

#[test]
fn v9_database_migrates_to_v10_with_cancel_fence_and_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let mut conn = Connection::open(&db).unwrap();
        mf_agent::schema::upgrade_project(&mut conn, 9).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            9
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='run_cancel_fence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert!(store
        .table_names()
        .unwrap()
        .contains(&"run_cancel_fence".into()));
    let artifacts = migration::published_artifact_dirs(&migration::backup_dir_for(&db)).unwrap();
    assert_eq!(artifacts.len(), 1);
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifacts[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["from_version"], 9);
    assert_eq!(manifest["to_version"], 10);
}

#[test]
fn v4_active_merge_batch_migrates_to_v5_without_state_loss() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE merge_batches (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id INTEGER NOT NULL,
                join_step_key TEXT NOT NULL,
                revision_id INTEGER NOT NULL,
                lease_keys_json TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'ready',
                transaction_id TEXT NOT NULL DEFAULT '',
                lease_digest TEXT NOT NULL DEFAULT '',
                conflicts_json TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE(task_id, join_step_key, revision_id)
             );
             INSERT INTO merge_batches
                (task_id, join_step_key, revision_id, lease_keys_json, status,
                 transaction_id, lease_digest, conflicts_json, created_at, updated_at)
             VALUES (7, 'j', 3, '[\"lease-a\"]', 'merging', 'txn-v4',
                     'digest-v4', '[]', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z');
             PRAGMA user_version = 4;",
        )
        .unwrap();
    }
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    let rows = store.list_merge_batches(7).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "merging");
    assert_eq!(rows[0].lease_keys, vec!["lease-a"]);
    let columns = store
        .with_conn(|c| {
            let mut stmt = c.prepare("PRAGMA table_info(merge_batches)")?;
            let names = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(names)
        })
        .unwrap();
    assert!(columns.contains(&"owner_id".to_string()));
    assert!(columns.contains(&"owner_expires_at".to_string()));
}

/// F-附带:**真实完整 v1 DDL** fixture(`PROJECT_SCHEMA_V1` 原文)+
/// steps/step_deps/sessions/runs/leases/deferrals/handoffs/task_workflows
/// 全表业务数据 → 链式迁移后数据完整可读。
#[test]
fn full_v1_ddl_fixture_migrates_all_tables_with_data() {
    use mf_agent::schema::PROJECT_SCHEMA_V1;
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(PROJECT_SCHEMA_V1).unwrap();
        conn.execute_batch(
            "INSERT INTO agent_tasks (title, goal, status, active_revision, created_at, updated_at)
                 VALUES ('全量任务', 'g', 'running', 1, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z');
             INSERT INTO pipeline_revisions (task_id, revision, status, created_at)
                 VALUES (1, 1, 'active', '2024-01-01T00:00:00Z');
             INSERT INTO steps (revision_id, task_id, step_key, title, agent_profile, status, created_at, updated_at)
                 VALUES (1, 1, 'a', 'A', 'p', 'succeeded', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z'),
                        (1, 1, 'j', 'J', 'p', 'pending', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z');
             INSERT INTO step_deps (step_id, dep_step_id) VALUES (2, 1);
             INSERT INTO agent_sessions (runtime, agent_profile, title, status, created_at, updated_at)
                 VALUES ('pty', 'p', 's', 'dead', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z');
             INSERT INTO agent_runs (task_id, step_id, revision_id, session_id, status, capability_token, started_at)
                 VALUES (1, 1, 1, 1, 'succeeded', 'tok-v1', '2024-01-01T00:00:00Z');
             INSERT INTO execution_leases (lease_key, run_id, step_id, task_id, provider, path, isolated, metadata_json, status, created_at)
                 VALUES ('lease-v1', 1, 1, 1, 'worktree', 'C:/wt', 1, '{\"step_key\":\"a\"}', 'held', '2024-01-01T00:00:00Z');
             INSERT INTO join_deferrals (task_id, join_step_key, lease_key, lease_json, created_at)
                 VALUES (1, 'j', 'lease-v1',
                     '{\"id\":\"lease-v1\",\"path\":\"C:/wt\",\"isolated\":true,\"provider\":\"worktree\",\"metadata\":{}}',
                     '2024-01-01T00:00:00Z');
             INSERT INTO handoffs (task_id, step_id, run_id, handoff_json, created_at)
                 VALUES (1, 1, 1, '{\"status\":\"ok\",\"summary\":\"s\",\"changed_files\":[],\"artifacts\":[],\"blockers\":[],\"recommendations\":[],\"output\":{}}', '2024-01-01T00:00:00Z');
             INSERT INTO task_workflows (project_key, task_id, graph_json, updated_at)
                 VALUES ('proj', 1, '[]', '2024-01-01T00:00:00Z');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let store = Store::open(&db).unwrap();
    assert_eq!(
        store.schema_version().unwrap(),
        PROJECT_SCHEMA_VERSION,
        "完整 v1 库必须链式迁移到当前版本"
    );
    // steps:状态与依赖投影完整
    let steps = store.task_steps(1).unwrap();
    assert_eq!(steps.len(), 2, "steps 迁移不丢行");
    assert!(steps
        .iter()
        .any(|s| s.step_key == "a" && s.status.as_str() == "succeeded"));
    let j = steps.iter().find(|s| s.step_key == "j").unwrap();
    assert_eq!(j.deps.len(), 1, "step_deps 迁移保留依赖");
    // runs:能力令牌与状态
    let runs = store.list_runs_of_task(1).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].capability_token, "tok-v1");
    // leases:held 行可被重启恢复路径读取
    let held = store.list_held_execution_leases().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].lease_key, "lease-v1");
    // deferrals
    let deferrals = store.list_join_deferrals(Some(1)).unwrap();
    assert_eq!(deferrals.len(), 1);
    // handoffs
    let handoffs = store.list_handoffs(1).unwrap();
    assert_eq!(handoffs.len(), 1);
    // task_workflows(草稿)迁移后仍可加载
    let draft = store.load_task_workflow("proj", 1).unwrap();
    assert!(draft.is_some(), "任务本地工作流草稿迁移保留");
    // merge_batches 表经 v2+v4 迁移可用(新列 lease_digest 就位):
    // held 租约可被权威领取并离开 ready
    match store.claim_single_merge_batch(1, "lease-v1", "a", 1, "txn-f1", "owner-f1") {
        Ok(mf_agent::store::JoinMergeClaim::Claimed { leases, .. }) => {
            assert_eq!(leases.len(), 1);
            assert_eq!(leases[0].lease_key, "lease-v1");
        }
        Err(e) => panic!("迁移后的 merge_batches 必须可领取: {e:#}"),
        _ => panic!("迁移后的 merge_batches 必须可领取"),
    }
    let batches = store.list_merge_batches(1).unwrap();
    assert!(
        batches
            .iter()
            .any(|b| b.status == "merging" && b.lease_keys == vec!["lease-v1".to_string()]),
        "领取后批状态 merging 且租约集完整: {batches:?}"
    );
}
