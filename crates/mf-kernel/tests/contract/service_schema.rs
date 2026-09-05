//! T1d 契约(Issue #19):service-v1.db schema。
//!
//! 覆盖:service authority 全部表与列形状、singleton 种子(meta/root_state)、
//! 唯一约束与 CHECK、future-version fail-closed 无副作用、重开稳定性、
//! 当前用户 ACL。全部使用 tempfile fixture,不触碰真实用户目录。

use crate::support::{
    assert_unique_index, column_names_of, columns_of, dacl_sddl, dir_entries, journal_mode_of,
    read_only, schema_objects_of, sha256_file, table_names_of, user_version_of,
};
use mf_kernel::project_registry::{ProjectStatus, ServiceStore};
use mf_kernel::service_schema::{
    error_code, guard_future_version, ServiceSchemaError, CODE_SCHEMA_FUTURE_VERSION,
    SERVICE_SCHEMA_V1, SERVICE_SCHEMA_V2_DELTA, SERVICE_SCHEMA_V3_DELTA, SERVICE_SCHEMA_V4_DELTA,
    SERVICE_SCHEMA_VERSION,
};
use rusqlite::{params, Connection};

/// 全部 fixture 的库文件都直接放在 tempdir 根(非 `.monkeyfence`),
/// 只有 ACL 专项测试显式建专用目录。
fn service_db(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("service-v1.db")
}

/// 全新库:基础表 + operation_step + run_capability,user_version=
/// 当前版本,重开不产生多余 schema 对象。
#[test]
fn fresh_service_db_has_exactly_spec_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    let store = ServiceStore::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), SERVICE_SCHEMA_VERSION);
    assert_eq!(user_version_of(&db), SERVICE_SCHEMA_VERSION);

    let expected = [
        "audit",
        "command_intent",
        "durable_feature",
        "meta",
        "migration_marker",
        "operation",
        "operation_step",
        "project_registry",
        "root_state",
        "run_capability",
    ];
    assert_eq!(table_names_of(&db), expected.to_vec());
    drop(store);

    // 重开:版本与 schema 对象集合不变(无重复 DDL 残留)。
    let before = schema_objects_of(&db);
    let reopened = ServiceStore::open(&db).unwrap();
    assert_eq!(user_version_of(&db), SERVICE_SCHEMA_VERSION);
    assert_eq!(schema_objects_of(&db), before);
    drop(reopened);
}

/// `meta`/`root_state` singleton:恰好一行,root_state 建库即 `off`
/// (§3.4:Core 启动强制 mode=off),instance_id 重开稳定。
#[test]
fn meta_and_root_state_singletons_with_stable_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    drop(ServiceStore::open(&db).unwrap());

    let conn = read_only(&db);
    let meta_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(meta_count, 1, "meta 必须是 singleton");
    let (instance_id, schema_version, owner_epoch): (String, i64, i64) = conn
        .query_row(
            "SELECT instance_id, schema_version, owner_epoch FROM meta WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(
        crate::support::is_uuid_v7(&instance_id),
        "instance_id 应为 UUIDv7"
    );
    assert_eq!(schema_version, SERVICE_SCHEMA_VERSION);
    assert_eq!(owner_epoch, 0, "建库 owner epoch 低位水位从 0 起");

    let (mode, root_epoch, enabled_at): (String, i64, Option<String>) = conn
        .query_row(
            "SELECT mode, root_epoch, enabled_at FROM root_state WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(mode, "off");
    assert_eq!(root_epoch, 0);
    assert_eq!(enabled_at, None);
    drop(conn);

    // 重开:instance_id 是持久身份,不因重开轮换。
    drop(ServiceStore::open(&db).unwrap());
    let reopened: String = read_only(&db)
        .query_row("SELECT instance_id FROM meta WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(reopened, instance_id);
}

#[test]
fn service_v1_upgrades_to_current_after_verified_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V1).unwrap();
        conn.execute(
            "INSERT INTO meta(id, instance_id, schema_version, owner_epoch)
             VALUES(1, '018f0000-0000-7000-8000-000000000001', 1, 9)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO root_state(id, mode, root_epoch) VALUES(1, 'off', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_registry
             (project_handle, public_id, canonical_root, display_path, registered_at, status)
             VALUES('proj_keep', 'pub_keep', '/keep', '/keep', '2026-09-01', 'registered')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let backup_dir = mf_agent::migration::backup_dir_for(&db);
    let store = ServiceStore::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), SERVICE_SCHEMA_VERSION);
    assert_eq!(
        store.list_projects().unwrap()[0].project_handle,
        "proj_keep"
    );
    assert!(column_names_of(&db, "command_intent").contains(&"problem_code".to_string()));
    assert!(column_names_of(&db, "operation_step").contains(&"step_id".to_string()));
    let meta_version: i64 = read_only(&db)
        .query_row("SELECT schema_version FROM meta WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(meta_version, SERVICE_SCHEMA_VERSION);

    let artifacts = mf_agent::migration::published_artifact_dirs(&backup_dir).unwrap();
    assert_eq!(artifacts.len(), 1, "跨版本升级必须先发布一个完整备份");
    let backup = artifacts[0].join("backup.db");
    assert_eq!(user_version_of(&backup), 1);
    assert!(!column_names_of(&backup, "command_intent")
        .iter()
        .any(|column| column == "problem_code"));
    let old_project: String = read_only(&backup)
        .query_row("SELECT project_handle FROM project_registry", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(old_project, "proj_keep");
}

/// T1g v2→v3:既有 service-v2 数据(operation/intent 行)升级后完整保留;
/// 备份是 v2;只知 v2 的旧 bundle 对 v3 库 fail-closed(回滚走
/// side-by-side/备份,而不是旧二进制直开)。
#[test]
fn service_v2_upgrades_to_current_preserving_operation_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V1).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V2_DELTA).unwrap();
        conn.execute(
            "INSERT INTO meta(id, instance_id, schema_version, owner_epoch)
             VALUES(1, '018f0000-0000-7000-8000-000000000002', 2, 4)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO root_state(id, mode, root_epoch) VALUES(1, 'off', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO command_intent
                 (command_id, semantic_digest, target_store, aggregate, principal, client_id,
                  controller_epoch, root_epoch, state, created_at, problem_code)
             VALUES('018f0000-0000-7000-8000-0000000000d1', 'd', 'project:proj_x', 'wf',
                     'user', 'client', 3, NULL, 'applied',
                     '2026-09-01T00:00:00Z', NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO operation
                 (operation_handle, command_id, kind, state, saga_state, progress_json,
                  created_at, updated_at)
             VALUES('op_018f0000-0000-7000-8000-0000000000d2',
                    '018f0000-0000-7000-8000-0000000000d1', 'install', 'completed',
                    '{}', '{}', '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
    }
    let backup_dir = mf_agent::migration::backup_dir_for(&db);
    let store = ServiceStore::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), SERVICE_SCHEMA_VERSION);
    // 旧数据逐行保留(旧代码语义不被 v3 迁移改写)。
    let (state, problem): (String, Option<String>) = read_only(&db)
        .query_row(
            "SELECT state, problem_code FROM command_intent WHERE command_id=?1",
            ["018f0000-0000-7000-8000-0000000000d1"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), problem), ("applied", None));
    let (op_state, kind): (String, String) = read_only(&db)
        .query_row(
            "SELECT state, kind FROM operation WHERE operation_handle=?1",
            ["op_018f0000-0000-7000-8000-0000000000d2"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((op_state.as_str(), kind.as_str()), ("completed", "install"));
    assert_eq!(
        counts_of(&db, "operation_step"),
        0,
        "v2 库没有 operation_step;迁移只建表不补造"
    );
    // 备份是升级前的 v2 原样。
    let artifacts = mf_agent::migration::published_artifact_dirs(&backup_dir).unwrap();
    assert_eq!(artifacts.len(), 1);
    let backup = artifacts[0].join("backup.db");
    assert_eq!(user_version_of(&backup), 2);
    assert!(
        !table_names_of(&backup)
            .iter()
            .any(|table| table == "operation_step"),
        "v2 备份不含 v3 表"
    );
    drop(store);

    // 回滚保护:只知 v2 的旧 bundle 对 v3 库 fail-closed(稳定错误码)。
    let conn = Connection::open(&db).unwrap();
    let err = guard_future_version(&conn, 2).unwrap_err();
    assert!(format!("{err:#}").contains("schema_future_version"));
    match err.downcast_ref::<ServiceSchemaError>() {
        Some(ServiceSchemaError::FutureVersion { found, known }) => {
            assert_eq!((*found, *known), (SERVICE_SCHEMA_VERSION, 2));
        }
        other => panic!("必须是 FutureVersion 判别值: {other:?}"),
    }
    // v3 delta 幂等:对已是 v3 的库重放 delta 不报错、不重复建对象。
    let objects_before = schema_objects_of(&db);
    conn.execute_batch(SERVICE_SCHEMA_V3_DELTA).unwrap();
    assert_eq!(schema_objects_of(&db), objects_before);
}

/// v3→v4 必须先备份；既有 registry/operation_step 行保持原样，只新增
/// 空 capability authority。
#[test]
fn service_v3_upgrades_to_v4_preserving_existing_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V1).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V2_DELTA).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V3_DELTA).unwrap();
        conn.execute(
            "INSERT INTO meta(id, instance_id, schema_version, owner_epoch)
             VALUES(1, '018f0000-0000-7000-8000-000000000003', 3, 7)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO root_state(id, mode, root_epoch) VALUES(1, 'off', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_registry
             (project_handle, public_id, canonical_root, display_path, registered_at, status)
             VALUES('proj_keep', 'pub_keep', '/keep', '/keep', '2026-09-01', 'registered')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
    }
    let backup_dir = mf_agent::migration::backup_dir_for(&db);
    drop(ServiceStore::open(&db).unwrap());
    assert_eq!(user_version_of(&db), 5);
    assert_eq!(counts_of(&db, "run_capability"), 0);
    let project: String = read_only(&db)
        .query_row("SELECT project_handle FROM project_registry", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(project, "proj_keep");
    let artifacts = mf_agent::migration::published_artifact_dirs(&backup_dir).unwrap();
    assert_eq!(artifacts.len(), 1);
    let backup = artifacts[0].join("backup.db");
    assert_eq!(user_version_of(&backup), 3);
    assert!(!table_names_of(&backup).contains(&"run_capability".to_owned()));

    let conn = Connection::open(&db).unwrap();
    let before = schema_objects_of(&db);
    conn.execute_batch(SERVICE_SCHEMA_V4_DELTA).unwrap();
    assert_eq!(schema_objects_of(&db), before);
}

fn counts_of(db: &std::path::Path, table: &str) -> i64 {
    read_only(db)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// `project_registry` 列形状(§3.4 逐列)与唯一约束。
#[test]
fn project_registry_shape_matches_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    drop(ServiceStore::open(&db).unwrap());

    assert_eq!(
        column_names_of(&db, "project_registry"),
        vec![
            "project_handle",
            "public_id",
            "canonical_root",
            "display_path",
            "registered_at",
            "status",
            // v5:display_name(自定义显示名;NULL=回退路径名)
            "display_name",
        ]
    );
    let handle = columns_of(&db, "project_registry")
        .into_iter()
        .find(|c| c.name == "project_handle")
        .unwrap();
    assert!(handle.pk, "project_handle 是主键");
    assert!(
        columns_of(&db, "project_registry")
            .iter()
            // display_name(v5)可空:NULL=回退路径名;其余列 NOT NULL
            .filter(|c| c.name != "display_name")
            .all(|c| c.notnull || c.pk),
        "除 display_name 外全部 NOT NULL(状态由导入方决定)"
    );
    assert_unique_index(&db, "project_registry", &["canonical_root"]);
    assert_unique_index(&db, "project_registry", &["public_id"]);
}

/// `command_intent`/`operation` 列形状、可空 root_epoch、状态 CHECK 与 FK。
#[test]
fn command_intent_and_operation_shape_and_constraints() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    drop(ServiceStore::open(&db).unwrap());

    assert_eq!(
        column_names_of(&db, "command_intent"),
        vec![
            "command_id",
            "semantic_digest",
            "target_store",
            "aggregate",
            "principal",
            "client_id",
            "controller_epoch",
            "root_epoch",
            "state",
            "created_at",
            "resolved_at",
            "problem_code",
        ]
    );
    let root_epoch = columns_of(&db, "command_intent")
        .into_iter()
        .find(|c| c.name == "root_epoch")
        .unwrap();
    assert!(!root_epoch.notnull, "root_epoch 可空(§3.4 root_epoch?)");

    assert_eq!(
        column_names_of(&db, "operation"),
        vec![
            "operation_handle",
            "command_id",
            "kind",
            "state",
            "saga_state",
            "progress_json",
            "created_at",
            "updated_at",
        ]
    );

    // 独立写连接验证 CHECK/FK(不经生产读路径)。
    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute(
        "INSERT INTO command_intent
             (command_id, semantic_digest, target_store, aggregate, principal, client_id,
              controller_epoch, root_epoch, state, created_at)
         VALUES ('cmd-1', 'digest', 'project', 'wf', 'user', 'client-1', 7, NULL,
                 'reserved', '2026-09-01T00:00:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO command_intent
             (command_id, semantic_digest, target_store, aggregate, principal, client_id,
              controller_epoch, root_epoch, state, created_at)
         VALUES ('cmd-2', 'digest', 'project', 'wf', 'user', 'client-1', 7, 3,
                 'applied', '2026-09-01T00:00:00Z')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO command_intent
                 (command_id, semantic_digest, target_store, aggregate, principal, client_id,
                  controller_epoch, state, created_at)
             VALUES ('cmd-bad', 'digest', 'project', 'wf', 'user', 'client-1', 1,
                     'bogus', '2026-09-01T00:00:00Z')",
            []
        )
        .is_err(),
        "command_intent.state CHECK 必须拒绝未知值"
    );
    assert!(
        conn.execute(
            "INSERT INTO operation
                 (operation_handle, command_id, kind, state, created_at, updated_at)
             VALUES ('op-x', 'missing-command', 'install', 'accepted',
                     '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
            []
        )
        .is_err(),
        "operation.command_id 必须外键约束 command_intent"
    );
    assert!(
        conn.execute(
            "INSERT INTO operation
                 (operation_handle, command_id, kind, state, created_at, updated_at)
             VALUES ('op-bad', 'cmd-1', 'install', 'bogus',
                     '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
            []
        )
        .is_err(),
        "operation.state CHECK 必须拒绝未知值"
    );
}

/// `operation_step`(T1g v3)列形状、状态 CHECK、FK 级联与 step_id 唯一。
#[test]
fn operation_step_shape_and_constraints() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    drop(ServiceStore::open(&db).unwrap());

    assert_eq!(
        column_names_of(&db, "operation_step"),
        vec![
            "operation_handle",
            "step_index",
            "role",
            "step_id",
            "target_store",
            "aggregate",
            "semantic_digest",
            "expected_json",
            "compensates",
            "state",
            "result_json",
            "problem_code",
            "created_at",
            "updated_at",
        ]
    );
    assert_unique_index(&db, "operation_step", &["step_id"]);

    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute(
        "INSERT INTO command_intent
             (command_id, semantic_digest, target_store, aggregate, principal, client_id,
              controller_epoch, state, created_at)
         VALUES ('cmd-step', 'd', 'project:p', 'wf', 'u', 'c', 1, 'reserved',
                 '2026-09-01T00:00:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO operation
             (operation_handle, command_id, kind, state, created_at, updated_at)
         VALUES ('op_step', 'cmd-step', 'k', 'running', '2026-09-01T00:00:00Z',
                 '2026-09-01T00:00:00Z')",
        [],
    )
    .unwrap();
    for (sql, reason) in [
        (
            "INSERT INTO operation_step
                 (operation_handle, step_index, role, step_id, target_store, aggregate,
                  semantic_digest, state, created_at, updated_at)
             VALUES ('missing', 0, 'forward', 's0', 'project:p', 'wf', 'd', 'pending',
                     '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
            "operation_step 必须外键约束 operation",
        ),
        (
            "INSERT INTO operation_step
                 (operation_handle, step_index, role, step_id, target_store, aggregate,
                  semantic_digest, state, created_at, updated_at)
             VALUES ('op_step', 0, 'bogus', 's0', 'project:p', 'wf', 'd', 'pending',
                     '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
            "role CHECK 必须拒绝未知值",
        ),
        (
            "INSERT INTO operation_step
                 (operation_handle, step_index, role, step_id, target_store, aggregate,
                  semantic_digest, state, created_at, updated_at)
             VALUES ('op_step', 0, 'forward', 's0', 'project:p', 'wf', 'd', 'bogus',
                     '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
            "state CHECK 必须拒绝未知值",
        ),
    ] {
        assert!(conn.execute(sql, []).is_err(), "{reason}");
    }
    for index in 0..2 {
        conn.execute(
            "INSERT INTO operation_step
                 (operation_handle, step_index, role, step_id, target_store, aggregate,
                  semantic_digest, state, created_at, updated_at)
             VALUES ('op_step', ?1, 'forward', ?2, 'project:p', 'wf', 'd', 'pending',
                     '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
            params![index, format!("s{index}")],
        )
        .unwrap();
    }
    // step_id 全局唯一(跨 operation)。
    assert!(
        conn.execute(
            "INSERT INTO operation_step
                 (operation_handle, step_index, role, step_id, target_store, aggregate,
                  semantic_digest, state, created_at, updated_at)
             VALUES ('op_step', 2, 'forward', 's0', 'project:p', 'wf', 'd', 'pending',
                     '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
            [],
        )
        .is_err(),
        "step_id 必须全局唯一"
    );
    // 级联清理:operation 删除时 step 行随行消失(GC 只删终态 operation)。
    conn.execute("DELETE FROM operation WHERE operation_handle='op_step'", [])
        .unwrap();
    assert_eq!(counts_of(&db, "operation_step"), 0);
}

/// v4 capability authority 只保存 keyed HMAC/opaque handles，并以两个
/// UNIQUE 约束和状态 CHECK 封住歧义写入。
#[test]
fn run_capability_shape_and_constraints() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    drop(ServiceStore::open(&db).unwrap());

    assert_eq!(
        column_names_of(&db, "run_capability"),
        vec![
            "token_hmac",
            "project_handle",
            "agent_run_handle",
            "state",
            "issued_at",
            "revoked_at",
        ]
    );
    assert_unique_index(&db, "run_capability", &["token_hmac"]);
    assert_unique_index(
        &db,
        "run_capability",
        &["project_handle", "agent_run_handle"],
    );

    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute(
        "INSERT INTO project_registry
         (project_handle, public_id, canonical_root, display_path, registered_at, status)
         VALUES('proj_cap', 'pub_cap', '/cap', '/cap', '2026-09-01', 'registered')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO run_capability
         (token_hmac, project_handle, agent_run_handle, state, issued_at)
         VALUES('h1', 'proj_cap', 'run_a', 'active', '2026-09-01')",
        [],
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT INTO run_capability
             (token_hmac, project_handle, agent_run_handle, state, issued_at)
             VALUES('h1', 'proj_cap', 'run_b', 'active', '2026-09-01')",
            [],
        )
        .is_err());
    assert!(conn
        .execute(
            "INSERT INTO run_capability
             (token_hmac, project_handle, agent_run_handle, state, issued_at)
             VALUES('h2', 'proj_cap', 'run_a', 'active', '2026-09-01')",
            [],
        )
        .is_err());
    assert!(conn
        .execute(
            "INSERT INTO run_capability
             (token_hmac, project_handle, agent_run_handle, state, issued_at)
             VALUES('h3', 'proj_cap', 'run_c', 'bogus', '2026-09-01')",
            [],
        )
        .is_err());
    assert!(conn
        .execute(
            "INSERT INTO run_capability
             (token_hmac, project_handle, agent_run_handle, state, issued_at)
             VALUES('h4', 'missing', 'run_d', 'active', '2026-09-01')",
            [],
        )
        .is_err());
}

/// `audit`/`durable_feature`/`migration_marker` 列形状。
#[test]
fn remaining_tables_shape_matches_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    drop(ServiceStore::open(&db).unwrap());

    assert_eq!(
        column_names_of(&db, "audit"),
        vec!["id", "kind", "summary_json", "created_at"]
    );
    assert_eq!(
        column_names_of(&db, "durable_feature"),
        vec!["feature", "min_reader_version", "writer_enabled_at"]
    );
    assert_eq!(
        column_names_of(&db, "migration_marker"),
        vec!["name", "payload_json", "created_at"]
    );
    // marker 幂等语义:name 为主键,重复写同名 marker 被拒绝。
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO migration_marker (name, payload_json, created_at)
         VALUES ('m1', '{}', '2026-09-01T00:00:00Z')",
        [],
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT INTO migration_marker (name, payload_json, created_at)
             VALUES ('m1', '{}', '2026-09-01T00:00:00Z')",
            []
        )
        .is_err());

    conn.execute(
        "INSERT INTO audit(kind, summary_json, created_at)
         VALUES ('owner_handoff', '{}', '2026-09-01T00:00:00Z')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute("UPDATE audit SET kind='changed' WHERE id=1", [])
            .is_err(),
        "append-only audit 必须拒绝 UPDATE"
    );
    conn.execute("DELETE FROM audit WHERE id=1", []).unwrap();
}

/// `project_registry.status` CHECK(registered|missing)。
#[test]
fn project_registry_status_check_rejects_unknown_values() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    drop(ServiceStore::open(&db).unwrap());

    let conn = Connection::open(&db).unwrap();
    let insert = |status: &str| {
        conn.execute(
            "INSERT INTO project_registry
                 (project_handle, public_id, canonical_root, display_path,
                  registered_at, status)
             VALUES (?1, ?2, ?3, ?4, '2026-09-01T00:00:00Z', ?5)",
            params![
                format!("proj_handle_{status}"),
                format!("public_{status}"),
                format!("root_{status}"),
                "display",
                status
            ],
        )
    };
    insert("registered").unwrap();
    insert("missing").unwrap();
    assert!(insert("bogus").is_err(), "status CHECK 必须拒绝未知值");
}

/// future guard:高于当前版本时打开拒绝,稳定错误码,库不动
/// (字节不变、无 DDL、无 journal 模式改写、无 sidecar)。
#[test]
fn future_version_fails_closed_without_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE future_marker (id INTEGER PRIMARY KEY, note TEXT NOT NULL);
             INSERT INTO future_marker (id, note) VALUES (1, 'future-data');
             PRAGMA user_version = {};",
            SERVICE_SCHEMA_VERSION + 1
        ))
        .unwrap();
    }
    let hash_before = sha256_file(&db);
    let schema_before = schema_objects_of(&db);
    assert_eq!(journal_mode_of(&db), "delete");

    let err = match ServiceStore::open(&db) {
        Ok(_) => panic!("future service 库必须 fail-closed"),
        Err(err) => err,
    };
    assert_eq!(
        error_code(&err),
        Some(CODE_SCHEMA_FUTURE_VERSION),
        "必须返回稳定错误码: {err:#}"
    );
    match err.downcast_ref::<ServiceSchemaError>() {
        Some(ServiceSchemaError::FutureVersion { found, known }) => {
            assert_eq!(*found, SERVICE_SCHEMA_VERSION + 1);
            assert_eq!(*known, SERVICE_SCHEMA_VERSION);
        }
        other => panic!("必须是 FutureVersion 判别值: {other:?}"),
    }

    assert_eq!(
        sha256_file(&db),
        hash_before,
        "拒绝路径不得改写数据库文件字节"
    );
    assert_eq!(
        user_version_of(&db),
        SERVICE_SCHEMA_VERSION + 1,
        "user_version 不变"
    );
    assert_eq!(
        journal_mode_of(&db),
        "delete",
        "拒绝路径不得改写 journal 模式"
    );
    assert_eq!(
        schema_objects_of(&db),
        schema_before,
        "拒绝路径不得执行任何 DDL"
    );
    assert_eq!(
        dir_entries(tmp.path()),
        vec!["service-v1.db".to_string()],
        "目录内不得出现任何新伴随文件"
    );
    let note: String = read_only(&db)
        .query_row("SELECT note FROM future_marker WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(note, "future-data", "既有数据不动");
}

/// 同为 user_version=1 的其他/残缺 SQLite 文件不能被静默当成
/// service-v1；拒绝发生在 ACL、pragma 与 repair DDL 之前。
#[test]
fn current_version_schema_mismatch_fails_without_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE foreign_v1(id INTEGER PRIMARY KEY, note TEXT NOT NULL);
             INSERT INTO foreign_v1 VALUES(1, 'keep');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let hash_before = sha256_file(&db);
    let schema_before = schema_objects_of(&db);
    let err = match ServiceStore::open(&db) {
        Ok(_) => panic!("残缺/其他 v1 文件必须拒绝"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("service_schema_mismatch"));
    assert_eq!(sha256_file(&db), hash_before);
    assert_eq!(schema_objects_of(&db), schema_before);
    assert_eq!(journal_mode_of(&db), "delete");
    assert_eq!(dir_entries(tmp.path()), vec!["service-v1.db"]);
}

#[test]
fn same_named_but_wrong_trigger_definition_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    drop(ServiceStore::open(&db).unwrap());
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TRIGGER audit_immutable_update;
             CREATE TRIGGER audit_immutable_update
             BEFORE UPDATE ON project_registry BEGIN SELECT 1; END;",
        )
        .unwrap();
    }
    let err = match ServiceStore::open(&db) {
        Ok(_) => panic!("同名但错误 target/body 的 trigger 不得通过 v1 指纹"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("service_schema_mismatch"));
}

/// 重开后数据仍在:registry 行经生产读接口完整返回(含 status 解析)。
#[test]
fn reopen_preserves_registry_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    let store = ServiceStore::open(&db).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO project_registry
             (project_handle, public_id, canonical_root, display_path, registered_at, status)
         VALUES ('proj_keep', 'pub', '/keep', '/keep', '2026-09-01T00:00:00Z', 'registered')",
        [],
    )
    .unwrap();
    drop(conn);
    drop(store);

    let reopened = ServiceStore::open(&db).unwrap();
    let projects = reopened.list_projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].project_handle, "proj_keep");
    assert_eq!(projects[0].canonical_root, "/keep");
    assert_eq!(projects[0].status, ProjectStatus::Registered);
}

/// 专用目录下的 service 库:数据库文件与 `.monkeyfence` 目录都只授权
/// 当前用户(§3.8:数据库文件当前用户 ACL)。
#[test]
fn service_db_and_dedicated_dir_restricted_to_current_user() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".monkeyfence");
    let db = home.join("service-v1.db");
    drop(ServiceStore::open(&db).unwrap());

    for path in [&home, &db] {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let expected = if path.is_dir() { 0o700 } else { 0o600 };
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                expected,
                "{} 必须仅当前用户可访问",
                path.display()
            );
        }
        #[cfg(windows)]
        {
            let sddl = dacl_sddl(path);
            assert!(
                sddl.contains(";;;OW)"),
                "{} 必须只授权 object owner:{sddl}",
                path.display()
            );
            for broad in [";;;WD)", ";;;AU)", ";;;BU)", ";;;BG)"] {
                assert!(
                    !sddl.contains(broad),
                    "{} 不得授权宽泛主体 {broad}:{sddl}",
                    path.display()
                );
            }
        }
    }
}
