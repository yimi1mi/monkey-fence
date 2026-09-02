//! T1b 契约(Issue #17):Project Store v6→v7 expand-only 迁移。
//!
//! - 精确 schema(project_meta / 双 revision / public_handle / identity /
//!   presentation / position / receipt / outbox / transcript):用
//!   `PRAGMA table_info/index_list/foreign_key_list` 与功能行为断言,
//!   不做脆弱 substring;
//! - v6 打开严格走 #16 Backup 屏障:先完整备份 artifact,再单事务
//!   CREATE/ALTER + handle/identity 回填 + `user_version=7`;
//! - 故障矩阵:DDL 后、workflow handle 回填后、node/edge 回填中途失败,
//!   每次都回到完整 v6(无半迁移/半 identity),源业务不动,backup 可保留;
//! - graph_json 解析失败、重复 node key、未知 dependency 必须 fail-closed 回滚;
//! - T0A v6 fixture 副本迁移前后业务投影与提交 golden 等价;
//! - v1/v5 旧库经 v6 链再 v7,只产生一个 pre-migration 逻辑快照。
//!
//! 全部基于 tempfile/fixture 副本,不触碰真实用户目录。

use crate::support::{
    assert_unique_index, build_legacy_v1_db, build_v6_project_db, business_snapshot, column_of,
    columns_of, foreign_keys_of, graph_json, handle_snapshot, is_uuid_v7, read_only,
    schema_objects_of, sole_manifest, user_version_of,
};
use mf_agent::migration;
use mf_agent::schema::PROJECT_SCHEMA_VERSION;
use mf_agent::store::Store;
use rusqlite::Connection;
use std::path::Path;

fn two_node_workflow() -> Vec<(&'static str, &'static [&'static str])> {
    vec![("fetch", &[]), ("build", &["fetch"])]
}

/// 迁移完成的库(含一个两节点一依赖的项目工作流 + 各聚合表业务行)。
fn migrated_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_v6_project_db(
        &db,
        &[("w1", "工作流一", &graph_json(&two_node_workflow()))],
    );
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    drop(store);
    (tmp, db)
}

// ---------------------------------------------------------------------------
// 精确 schema
// ---------------------------------------------------------------------------

/// v7 新表与 ALTER 列与 canonical spec §3.2 完全一致。
#[test]
fn v7_schema_matches_spec_exactly() {
    let (_tmp, db) = migrated_fixture();

    // project_meta singleton:id PK + CHECK(id=1),collection revision 默认 1
    let cols = columns_of(&db, "project_meta");
    assert_eq!(
        cols.len(),
        2,
        "project_meta 只有 id 与 revision 两列: {cols:?}"
    );
    let id = column_of(&db, "project_meta", "id");
    assert_eq!(id.col_type, "INTEGER");
    assert!(id.pk, "id 是主键(真 singleton)");
    let revision = column_of(&db, "project_meta", "workflow_collection_revision");
    assert_eq!(revision.col_type, "INTEGER");
    assert!(revision.notnull);
    assert_eq!(revision.dflt.as_deref(), Some("1"));
    let (mid, mrev): (i64, i64) = read_only(&db)
        .query_row(
            "SELECT id, workflow_collection_revision FROM project_meta",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((mid, mrev), (1, 1), "迁移必须种入唯一 singleton 行 (1,1)");

    // project_workflows:public_handle + 语义/展示双 revision
    for (column, dflt) in [
        ("public_handle", Some("''")),
        ("semantic_revision", Some("1")),
        ("presentation_revision", Some("1")),
    ] {
        let meta = column_of(&db, "project_workflows", column);
        assert!(meta.notnull, "project_workflows.{column} NOT NULL");
        assert_eq!(
            meta.dflt.as_deref(),
            dflt,
            "project_workflows.{column} 默认值"
        );
    }
    assert_eq!(
        column_of(&db, "project_workflows", "public_handle").col_type,
        "TEXT"
    );
    assert_unique_index(&db, "project_workflows", &["public_handle"]);

    // 既有持久 aggregate:public_handle NOT NULL UNIQUE;缺 revision 的补 DEFAULT 1
    for table in [
        "agent_tasks",
        "steps",
        "agent_sessions",
        "agent_runs",
        "ad_hoc_sessions",
    ] {
        let handle = column_of(&db, table, "public_handle");
        assert!(handle.notnull, "{table}.public_handle NOT NULL");
        assert_eq!(handle.dflt.as_deref(), Some("''"));
        assert_unique_index(&db, table, &["public_handle"]);
        let revision = column_of(&db, table, "revision");
        assert!(revision.notnull, "{table}.revision NOT NULL");
        assert_eq!(
            revision.dflt.as_deref(),
            Some("1"),
            "{table}.revision DEFAULT 1"
        );
    }
    let handle = column_of(&db, "pipeline_revisions", "public_handle");
    assert!(handle.notnull);
    assert_eq!(handle.dflt.as_deref(), Some("''"));
    assert_unique_index(&db, "pipeline_revisions", &["public_handle"]);
    // pipeline_revisions 的 revision 是既有每任务版本号,迁移不得改写其语义
    let existing = column_of(&db, "pipeline_revisions", "revision");
    assert_eq!(existing.dflt, None, "既有 revision 列保持无默认");
    assert!(!existing.pk);

    // workflow_node_identity
    assert_eq!(
        columns_of(&db, "workflow_node_identity")
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["workflow_handle", "node_key", "node_handle"]
    );
    for column in ["workflow_handle", "node_key", "node_handle"] {
        assert!(
            column_of(&db, "workflow_node_identity", column).notnull,
            "workflow_node_identity.{column} NOT NULL"
        );
    }
    assert_unique_index(&db, "workflow_node_identity", &["node_handle"]);
    assert_unique_index(
        &db,
        "workflow_node_identity",
        &["workflow_handle", "node_key"],
    );
    assert!(
        foreign_keys_of(&db, "workflow_node_identity")
            .iter()
            .any(|fk| fk.from_column == "workflow_handle"
                && fk.ref_table == "project_workflows"
                && fk.ref_column == "public_handle"
                && fk.on_delete == "CASCADE"),
        "node identity 必须随工作流删除级联清理"
    );

    // workflow_edge_identity
    assert_eq!(
        columns_of(&db, "workflow_edge_identity")
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "workflow_handle",
            "upstream_node_key",
            "downstream_node_key",
            "edge_handle"
        ]
    );
    for column in [
        "workflow_handle",
        "upstream_node_key",
        "downstream_node_key",
        "edge_handle",
    ] {
        assert!(
            column_of(&db, "workflow_edge_identity", column).notnull,
            "workflow_edge_identity.{column} NOT NULL"
        );
    }
    assert_unique_index(&db, "workflow_edge_identity", &["edge_handle"]);
    assert_unique_index(
        &db,
        "workflow_edge_identity",
        &[
            "workflow_handle",
            "upstream_node_key",
            "downstream_node_key",
        ],
    );
    assert!(
        foreign_keys_of(&db, "workflow_edge_identity")
            .iter()
            .any(|fk| fk.from_column == "workflow_handle"
                && fk.ref_table == "project_workflows"
                && fk.ref_column == "public_handle"
                && fk.on_delete == "CASCADE"),
        "edge identity 必须随工作流删除级联清理"
    );

    // workflow_presentation:三块展示 JSON,迁移不伪造旧坐标(空表)
    assert_eq!(
        columns_of(&db, "workflow_presentation")
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "workflow_handle",
            "viewport_json",
            "collapse_json",
            "layout_json"
        ]
    );
    assert!(column_of(&db, "workflow_presentation", "workflow_handle").notnull);
    for column in ["viewport_json", "collapse_json", "layout_json"] {
        let meta = column_of(&db, "workflow_presentation", column);
        assert!(!meta.notnull, "presentation.{column} 可空(不伪造)");
        assert_eq!(meta.dflt, None);
    }
    assert_unique_index(&db, "workflow_presentation", &["workflow_handle"]);
    assert!(
        foreign_keys_of(&db, "workflow_presentation")
            .iter()
            .any(|fk| fk.from_column == "workflow_handle"
                && fk.ref_table == "project_workflows"
                && fk.ref_column == "public_handle"
                && fk.on_delete == "CASCADE"),
        "presentation 必须随工作流删除级联清理"
    );
    let presentation_rows: i64 = read_only(&db)
        .query_row("SELECT COUNT(*) FROM workflow_presentation", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(presentation_rows, 0, "迁移不得伪造 presentation 行");

    // node_position:node_handle → x/y;迁移保持空
    assert_eq!(
        columns_of(&db, "node_position")
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["node_handle", "x", "y"]
    );
    assert!(column_of(&db, "node_position", "node_handle").notnull);
    assert_eq!(column_of(&db, "node_position", "x").col_type, "REAL");
    assert!(column_of(&db, "node_position", "x").notnull);
    assert_eq!(column_of(&db, "node_position", "y").col_type, "REAL");
    assert!(column_of(&db, "node_position", "y").notnull);
    assert_unique_index(&db, "node_position", &["node_handle"]);
    assert!(
        foreign_keys_of(&db, "node_position")
            .iter()
            .any(|fk| fk.from_column == "node_handle"
                && fk.ref_table == "workflow_node_identity"
                && fk.ref_column == "node_handle"
                && fk.on_delete == "CASCADE"),
        "position 必须随节点 identity 删除级联清理"
    );
    let position_rows: i64 = read_only(&db)
        .query_row("SELECT COUNT(*) FROM node_position", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        position_rows, 0,
        "迁移时 node_position 保持空,由 Web 首次布局后写"
    );

    // command_receipt
    assert_eq!(
        columns_of(&db, "command_receipt")
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "command_id",
            "semantic_digest",
            "aggregate_handle",
            "result_revisions",
            "state",
            "created_at",
            "finalized_at"
        ]
    );
    for column in [
        "command_id",
        "semantic_digest",
        "aggregate_handle",
        "result_revisions",
        "state",
        "created_at",
    ] {
        assert!(
            column_of(&db, "command_receipt", column).notnull,
            "command_receipt.{column} NOT NULL"
        );
    }
    assert_eq!(
        column_of(&db, "command_receipt", "result_revisions").dflt,
        Some("'{}'".to_string())
    );
    assert!(column_of(&db, "command_receipt", "finalized_at")
        .dflt
        .is_none());
    assert_unique_index(&db, "command_receipt", &["command_id"]);

    // projection_outbox:只有 store-local outbox_id,不预存全局 seq
    assert_eq!(
        columns_of(&db, "projection_outbox")
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["outbox_id", "event_json", "published_at"],
        "outbox 不得预存全局 stream seq"
    );
    assert!(column_of(&db, "projection_outbox", "outbox_id").pk);
    assert_eq!(
        column_of(&db, "projection_outbox", "outbox_id").col_type,
        "INTEGER"
    );
    assert!(column_of(&db, "projection_outbox", "event_json").notnull);
    assert!(!column_of(&db, "projection_outbox", "published_at").notnull);

    // terminal_transcript
    assert_eq!(
        columns_of(&db, "terminal_transcript")
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "session_handle",
            "terminal_epoch",
            "final_state",
            "durable_through_seq",
            "exit_code",
            "exit_signal",
            "as_of_seq",
            // v11(T3d)expand-only 补列:GC LRU 时间戳与 UUID epoch 文本
            "updated_at",
            "terminal_epoch_v2"
        ]
    );
    assert!(column_of(&db, "terminal_transcript", "session_handle").pk);
    for column in [
        "session_handle",
        "terminal_epoch",
        "final_state",
        "durable_through_seq",
        "as_of_seq",
    ] {
        assert!(
            column_of(&db, "terminal_transcript", column).notnull,
            "terminal_transcript.{column} NOT NULL"
        );
    }
    for column in ["exit_code", "exit_signal"] {
        assert!(!column_of(&db, "terminal_transcript", column).notnull);
    }

    // terminal_transcript_segment
    assert_eq!(
        columns_of(&db, "terminal_transcript_segment")
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["session_handle", "seq_start", "seq_end", "bytes"]
    );
    assert_eq!(
        column_of(&db, "terminal_transcript_segment", "bytes").col_type,
        "BLOB"
    );
    assert_unique_index(
        &db,
        "terminal_transcript_segment",
        &["session_handle", "seq_start"],
    );
    assert!(
        foreign_keys_of(&db, "terminal_transcript_segment")
            .iter()
            .any(|fk| fk.from_column == "session_handle"
                && fk.ref_table == "terminal_transcript"
                && fk.ref_column == "session_handle"),
        "segment 必须引用 transcript 会话"
    );
}

/// 约束的行为证明(不靠 DDL 文本):singleton、唯一、CHECK、自增、FK。
#[test]
fn v7_constraints_enforced_by_behavior() {
    let (_tmp, db) = migrated_fixture();
    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();

    // project_meta 是真 singleton:CHECK(id=1) 拒绝 id=2,PK 拒绝重复 id=1
    assert!(
        conn.execute("INSERT INTO project_meta (id) VALUES (2)", [])
            .is_err(),
        "CHECK(id=1) 必须拒绝第二 id"
    );
    assert!(
        conn.execute("INSERT INTO project_meta (id) VALUES (1)", [])
            .is_err(),
        "主键必须拒绝重复 singleton 行"
    );

    // command_receipt:command_id 唯一
    conn.execute(
        "INSERT INTO command_receipt
             (command_id, semantic_digest, aggregate_handle, state, created_at)
         VALUES ('cmd-1', 'd', 'h', 'applied', 't')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO command_receipt
                 (command_id, semantic_digest, aggregate_handle, state, created_at)
             VALUES ('cmd-1', 'd2', 'h2', 'applied', 't2')",
            [],
        )
        .is_err(),
        "command_id 必须 UNIQUE"
    );

    // projection_outbox:store-local AUTOINCREMENT
    conn.execute(
        "INSERT INTO projection_outbox (event_json) VALUES ('{}')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO projection_outbox (event_json) VALUES ('{}')",
        [],
    )
    .unwrap();
    let ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT outbox_id FROM projection_outbox ORDER BY outbox_id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(ids, vec![1, 2], "outbox_id 由库分配,应用不提供全局 seq");

    // terminal_transcript:final_state CHECK 全集
    conn.execute(
        "INSERT INTO terminal_transcript
             (session_handle, terminal_epoch, final_state, durable_through_seq, as_of_seq)
         VALUES ('s-live', 1, 'live', 0, 0)",
        [],
    )
    .unwrap();
    for state in ["complete", "crash_incomplete", "lost"] {
        conn.execute(
            &format!(
                "INSERT INTO terminal_transcript
                     (session_handle, terminal_epoch, final_state, durable_through_seq, as_of_seq)
                 VALUES ('s-{state}', 1, '{state}', 0, 0)"
            ),
            [],
        )
        .unwrap_or_else(|e| panic!("final_state={state} 必须合法: {e}"));
    }
    assert!(
        conn.execute(
            "INSERT INTO terminal_transcript
                 (session_handle, terminal_epoch, final_state, durable_through_seq, as_of_seq)
             VALUES ('s-bad', 1, 'unknown', 0, 0)",
            [],
        )
        .is_err(),
        "final_state CHECK 必须 fail-closed"
    );

    // segment:FK + 范围 CHECK + 联合唯一
    assert!(
        conn.execute(
            "INSERT INTO terminal_transcript_segment
                 (session_handle, seq_start, seq_end, bytes)
             VALUES ('no-such-session', 1, 1, X'00')",
            [],
        )
        .is_err(),
        "segment 必须有 FK 父行"
    );
    conn.execute(
        "INSERT INTO terminal_transcript_segment (session_handle, seq_start, seq_end, bytes)
         VALUES ('s-live', 1, 4, X'0102')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO terminal_transcript_segment (session_handle, seq_start, seq_end, bytes)
             VALUES ('s-live', 1, 2, X'03')",
            [],
        )
        .is_err(),
        "(session_handle, seq_start) 必须唯一"
    );
    assert!(
        conn.execute(
            "INSERT INTO terminal_transcript_segment (session_handle, seq_start, seq_end, bytes)
             VALUES ('s-live', 5, 4, X'04')",
            [],
        )
        .is_err(),
        "seq_end >= seq_start 范围 CHECK"
    );
    assert!(
        conn.execute(
            "INSERT INTO terminal_transcript_segment (session_handle, seq_start, seq_end, bytes)
             VALUES ('s-live', 0, 2, X'05')",
            [],
        )
        .is_err(),
        "seq_start >= 1 范围 CHECK"
    );
}

// ---------------------------------------------------------------------------
// Backup 屏障与幂等
// ---------------------------------------------------------------------------

/// v6 打开:恰一个 6→7 备份 artifact;再次打开无新备份、handle/revision 稳定。
#[test]
fn v6_upgrade_backs_up_once_and_reopen_is_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_v6_project_db(
        &db,
        &[("w1", "工作流一", &graph_json(&two_node_workflow()))],
    );
    assert_eq!(user_version_of(&db), 6);

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    let backup_dir = migration::backup_dir_for(&db);
    let (manifest_path, manifest) = sole_manifest(&backup_dir);
    assert_eq!(manifest["store_kind"], "project");
    assert_eq!(manifest["from_version"], 6, "备份必须是 v6 迁移前快照");
    assert_eq!(manifest["to_version"], PROJECT_SCHEMA_VERSION);
    assert_eq!(manifest["complete"], true);
    let backup_db = manifest_path.parent().unwrap().join("backup.db");
    assert_eq!(user_version_of(&backup_db), 6, "备份停留在迁移前版本");
    let workflow_rows: i64 = read_only(&backup_db)
        .query_row("SELECT COUNT(*) FROM project_workflows", [], |r| r.get(0))
        .unwrap();
    assert_eq!(workflow_rows, 1, "备份保留业务数据");
    let v7_tables_in_backup: i64 = read_only(&backup_db)
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE name IN ('project_meta','workflow_node_identity','command_receipt')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v7_tables_in_backup, 0, "备份必须是 v7 DDL 之前的快照");

    let snapshot = handle_snapshot(&db);
    drop(store);

    // 重跑已完成迁移:无新备份,全部 handle/revision 稳定
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert_eq!(
        migration::published_artifact_dirs(&backup_dir)
            .unwrap()
            .len(),
        1,
        "第二次打开不得新增备份"
    );
    assert_eq!(
        handle_snapshot(&db),
        snapshot,
        "handle 与 revision 不得漂移"
    );
    let revisions: Vec<(i64, i64, i64)> = {
        let conn = read_only(&db);
        let mut stmt = conn
            .prepare(
                "SELECT 1 FROM project_workflows
                 WHERE semantic_revision <> 1 OR presentation_revision <> 1",
            )
            .unwrap();
        let drifted = stmt.exists([]).unwrap();
        assert!(!drifted, "重开不得改写 revision");
        Vec::new()
    };
    drop(revisions);
    drop(store);
}

/// current schema 正常打开不产生备份;future schema fail-closed(与 #16 契约同口径)。
#[test]
fn current_v7_opens_without_backup_and_future_v8_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let store = Store::open(&db).unwrap();
        assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
        assert!(
            !migration::backup_dir_for(&db).exists(),
            "全新库初始化不是升级,不备份"
        );
    }
    // 直接在同一库上伪造 future user_version:高于已知版本必须拒绝
    {
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", PROJECT_SCHEMA_VERSION + 1)
            .unwrap();
    }
    let hash_before = crate::support::sha256_file(&db);
    let schema_before = schema_objects_of(&db);
    let err = match Store::open(&db) {
        Ok(_) => panic!("future schema 必须 fail-closed"),
        Err(err) => err,
    };
    assert_eq!(
        migration::error_code(&err),
        Some(migration::CODE_SCHEMA_FUTURE_VERSION),
        "future schema 必须以稳定错误码 fail-closed: {err:#}"
    );
    assert_eq!(user_version_of(&db), PROJECT_SCHEMA_VERSION + 1);
    assert_eq!(crate::support::sha256_file(&db), hash_before);
    assert_eq!(schema_objects_of(&db), schema_before);
}

// ---------------------------------------------------------------------------
// 业务 golden 等价(T0A v6 fixture 副本)
// ---------------------------------------------------------------------------

#[path = "../common/baseline.rs"]
mod baseline;

/// T0A v6 fixture 副本迁移到 v7 后,业务投影与提交的 golden 逐字节等价。
#[test]
fn migrated_fixture_business_projection_matches_committed_golden() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("project-v6.db");
    std::fs::copy(baseline::fixtures_dir().join("project-v6.db"), &db).unwrap();
    assert_eq!(baseline::raw_schema_version(&db).unwrap(), 6);

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    let dump = baseline::dump_project(&store).unwrap();
    let expected = std::fs::read_to_string(
        baseline::fixtures_dir()
            .join("expected")
            .join("project-v6.dump.json"),
    )
    .unwrap()
    .replace("\r\n", "\n");
    assert_eq!(
        baseline::canonical_json(&dump).unwrap(),
        expected,
        "v7 迁移后 Task/Revision/Step/Run/Settlement/Handoff/Session/project workflow \
         业务投影必须与提交 golden 等价"
    );

    // 迁移同时为 fixture 工作流补齐 identity(节点 n1/n2 + 边 n1→n2)
    let conn = read_only(&db);
    let (nodes, edges): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM workflow_node_identity),
                    (SELECT COUNT(*) FROM workflow_edge_identity)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((nodes, edges), (2, 1));
    let (workflow_handle, n1): (String, String) = conn
        .query_row(
            "SELECT i.workflow_handle, i.node_handle
             FROM workflow_node_identity i
             JOIN project_workflows w ON w.public_handle = i.workflow_handle
             WHERE w.workflow_key = 'baseline-flow' AND i.node_key = 'n1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(is_uuid_v7(&workflow_handle) && is_uuid_v7(&n1));
    let edge_endpoints: (String, String) = conn
        .query_row(
            "SELECT upstream_node_key, downstream_node_key
             FROM workflow_edge_identity WHERE workflow_handle = ?1",
            [&workflow_handle],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(edge_endpoints, ("n1".to_string(), "n2".to_string()));
}

// ---------------------------------------------------------------------------
// 故障矩阵:任一点失败不留半迁移
// ---------------------------------------------------------------------------

fn assert_migration_failed_cleanly(
    db: &Path,
    schema_before: &[(String, String)],
    business_before: &serde_json::Value,
) -> anyhow::Result<()> {
    let err = match Store::open(&db) {
        Ok(_) => panic!("故障注入点必须让迁移失败"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("fault:") || message.contains("损坏") || message.contains("graph"),
        "失败必须来自注入点而非无关错误: {message}"
    );
    assert_eq!(user_version_of(db), 6, "user_version 必须仍是 6");
    assert_eq!(
        schema_objects_of(db),
        *schema_before,
        "回滚不得留下任何 v7 schema/identity 半状态"
    );
    assert_eq!(
        business_snapshot(db),
        *business_before,
        "源业务数据必须完全不动"
    );
    // backup 可保留:失败尝试产生的是完整 v6 备份(允许存在,不得是半产物)
    let artifacts = migration::published_artifact_dirs(&migration::backup_dir_for(db))?;
    assert!(artifacts.len() >= 1, "屏障在迁移前已发布备份,允许保留");
    Ok(())
}

/// 故障点 1:DDL 之后、聚合表 handle 回填之中失败(trigger 注入)。
#[test]
fn fault_after_ddl_rolls_back_to_intact_v6() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_v6_project_db(
        &db,
        &[("w1", "工作流一", &graph_json(&two_node_workflow()))],
    );
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fault_aggregate_handle BEFORE UPDATE ON agent_tasks
             WHEN NEW.public_handle <> OLD.public_handle
             BEGIN SELECT RAISE(ABORT, 'fault:aggregate-handle-backfill'); END;",
        )
        .unwrap();
    let schema_before = schema_objects_of(&db);
    let business_before = business_snapshot(&db);
    let metric_key = {
        let conn = Connection::open(&db).unwrap();
        mf_agent::observability::store_metric_key(&conn, migration::StoreKind::Project)
    };
    let metrics_before = mf_agent::observability::storage_metrics_snapshot()
        .stores
        .get(&metric_key)
        .cloned()
        .unwrap_or_default();

    assert_migration_failed_cleanly(&db, &schema_before, &business_before).unwrap();
    let err_text = {
        match Store::open(&db) {
            Ok(_) => panic!("必须失败"),
            Err(err) => format!("{err:#}"),
        }
    };
    assert!(
        err_text.contains("fault:aggregate-handle-backfill"),
        "失败点必须是注入的聚合 handle 回填: {err_text}"
    );
    let metrics_after_failure = mf_agent::observability::storage_metrics_snapshot()
        .stores
        .get(&metric_key)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        metrics_after_failure, metrics_before,
        "回滚的迁移不得累计 migration/backfill/GC 指标"
    );

    // 移除故障后重跑:迁移成功且完整
    Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TRIGGER fault_aggregate_handle;")
        .unwrap();
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    let (nodes, edges, handles): (i64, i64, i64) = {
        let conn = read_only(&db);
        conn.query_row(
            "SELECT (SELECT COUNT(*) FROM workflow_node_identity),
                    (SELECT COUNT(*) FROM workflow_edge_identity),
                    (SELECT COUNT(*) FROM agent_tasks WHERE public_handle <> '')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!((nodes, edges, handles), (2, 1, 1), "重跑后 identity 完整");
}

/// 故障点 2:workflow handle 回填之后、identity 回填开始处失败
/// (第一个工作流 graph_json 损坏)。
#[test]
fn fault_after_workflow_handle_backfill_rolls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_v6_project_db(
        &db,
        &[
            ("a-bad", "坏图", "{not-json"),
            ("z-ok", "好图", &graph_json(&two_node_workflow())),
        ],
    );
    let schema_before = schema_objects_of(&db);
    let business_before = business_snapshot(&db);

    assert_migration_failed_cleanly(&db, &schema_before, &business_before).unwrap();
    let err_text = {
        match Store::open(&db) {
            Ok(_) => panic!("必须失败"),
            Err(err) => format!("{err:#}"),
        }
    };
    assert!(
        err_text.contains("a-bad"),
        "错误必须指明损坏的工作流: {err_text}"
    );

    // 修复损坏行后重跑成功
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE project_workflows SET graph_json = ?1 WHERE workflow_key = 'a-bad'",
            rusqlite::params![graph_json(&[("only", &[])])],
        )
        .unwrap();
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    let (nodes, edges): (i64, i64) = {
        let conn = read_only(&db);
        conn.query_row(
            "SELECT (SELECT COUNT(*) FROM workflow_node_identity),
                    (SELECT COUNT(*) FROM workflow_edge_identity)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!((nodes, edges), (3, 1), "两个工作流的 identity 都完整");
}

/// 故障点 3:node/edge 回填中途失败(第一个工作流已完成 identity,
/// 第二个工作流结构非法:重复 node key / 未知 dependency)。
#[test]
fn fault_mid_identity_backfill_rolls_back() {
    for (label, bad_graph) in [
        ("duplicate-node-key", graph_json(&[("a", &[]), ("a", &[])])),
        (
            "unknown-dependency",
            graph_json(&[("a", &[]), ("b", &["missing"])]),
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("workflow-v1.db");
        build_v6_project_db(
            &db,
            &[
                ("a-ok", "好图", &graph_json(&two_node_workflow())),
                ("z-bad", "坏图", &bad_graph),
            ],
        );
        let schema_before = schema_objects_of(&db);
        let business_before = business_snapshot(&db);

        assert_migration_failed_cleanly(&db, &schema_before, &business_before)
            .unwrap_or_else(|e| panic!("{label}: {e:#}"));
        let err_text = {
            match Store::open(&db) {
                Ok(_) => panic!("{label}: 必须失败"),
                Err(err) => format!("{err:#}"),
            }
        };
        assert!(
            err_text.contains("z-bad"),
            "{label}: 错误必须指明非法工作流: {err_text}"
        );

        // 单独验证合法工作流的业务行在失败后仍可读(源业务不动)
        let name: String = read_only(&db)
            .query_row(
                "SELECT name FROM project_workflows WHERE workflow_key = 'a-ok'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "好图");

        // 修复后重跑:两个工作流 identity 完整,且全部 handle 是真实 UUIDv7
        Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE project_workflows SET graph_json = ?1 WHERE workflow_key = 'z-bad'",
                rusqlite::params![graph_json(&[("only", &[])])],
            )
            .unwrap();
        let store = Store::open(&db).unwrap();
        assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
        let handles = crate::support::all_persistent_handles(&db);
        // 6 聚合行 + 2 工作流 + a-ok(2 节点 1 边) + z-bad 修复后 1 节点
        assert_eq!(handles.len(), 12, "{label}: 持久对象 handle 计数");
        assert!(
            handles.iter().all(|h| is_uuid_v7(h)),
            "{label}: 全部为 UUIDv7"
        );
        let mut unique = handles.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), handles.len(), "{label}: handle 全局唯一");
    }
}

// ---------------------------------------------------------------------------
// 旧版本链:v1 / v5 经 v6 链到 v7,单个 pre-migration 快照
// ---------------------------------------------------------------------------

/// v1 残缺库(只有 agent_tasks/pipeline_revisions)也能走完链到完整 v7:
/// 幂等重放 v1–v6 地基后再回填,不得把缺表库直接标 v7。
#[test]
fn legacy_v1_upgrades_through_chain_with_single_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_legacy_v1_db(&db, &["链式迁移"]);

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    let titles: Vec<String> = store
        .list_tasks(false)
        .unwrap()
        .into_iter()
        .map(|t| t.title)
        .collect();
    assert_eq!(titles, vec!["链式迁移".to_string()]);

    let (_, manifest) = sole_manifest(&migration::backup_dir_for(&db));
    assert_eq!(manifest["from_version"], 1, "备份是 v1 迁移前快照");
    assert_eq!(manifest["to_version"], PROJECT_SCHEMA_VERSION);
    for table in [
        "agent_tasks",
        "pipeline_revisions",
        "steps",
        "agent_sessions",
        "agent_runs",
        "ad_hoc_sessions",
        "project_workflows",
    ] {
        assert!(
            columns_of(&db, table)
                .iter()
                .any(|column| column.name == "public_handle"),
            "残缺 v1 升级后必须补齐 {table}.public_handle"
        );
    }
    // 既有业务行获得 handle 回填
    let handles: Vec<String> = {
        let conn = read_only(&db);
        let mut stmt = conn
            .prepare("SELECT public_handle FROM agent_tasks")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(handles.len(), 1);
    assert!(is_uuid_v7(&handles[0]));
}

/// v5 库(链式生成 + 业务行)→ v6 → v7 单事务完成,恰一个 from=5 备份。
#[test]
fn legacy_v5_upgrades_through_v6_to_v7_with_single_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let mut conn = Connection::open(&db).unwrap();
        mf_agent::schema::upgrade_project(&mut conn, 5).unwrap();
        conn.execute(
            "INSERT INTO agent_tasks
                 (title, goal, status, active_revision, created_at, updated_at)
             VALUES ('v5任务', 'g', 'running', NULL, '2024-01-01', '2024-01-01')",
            [],
        )
        .unwrap();
    }
    assert_eq!(user_version_of(&db), 5);

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    assert_eq!(store.list_tasks(false).unwrap().len(), 1);
    let (_, manifest) = sole_manifest(&migration::backup_dir_for(&db));
    assert_eq!(manifest["from_version"], 5);
    assert_eq!(manifest["to_version"], PROJECT_SCHEMA_VERSION);
    // 全链一次事务:不产生中间 v6 备份
    assert_eq!(
        migration::published_artifact_dirs(&migration::backup_dir_for(&db))
            .unwrap()
            .len(),
        1
    );
}
