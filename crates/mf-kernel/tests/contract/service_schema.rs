//! T1d 契约(Issue #19):service-v1.db schema。
//!
//! 覆盖:§3.4 全部 8 张表与列形状、singleton 种子(meta/root_state)、
//! 唯一约束与 CHECK、future-version fail-closed 无副作用、重开稳定性、
//! 当前用户 ACL。全部使用 tempfile fixture,不触碰真实用户目录。

use crate::support::{
    assert_unique_index, column_names_of, columns_of, dacl_sddl, dir_entries, journal_mode_of,
    read_only, schema_objects_of, sha256_file, table_names_of, user_version_of,
};
use mf_kernel::project_registry::{ProjectStatus, ServiceStore};
use mf_kernel::service_schema::{
    error_code, ServiceSchemaError, CODE_SCHEMA_FUTURE_VERSION, SERVICE_SCHEMA_VERSION,
};
use rusqlite::{params, Connection};

/// 全部 fixture 的库文件都直接放在 tempdir 根(非 `.monkeyfence`),
/// 只有 ACL 专项测试显式建专用目录。
fn service_db(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("service-v1.db")
}

/// 全新库:恰好 §3.4 的 8 张表,user_version=1,重开不产生多余 schema 对象。
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
        "project_registry",
        "root_state",
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
            .all(|c| c.notnull || c.pk),
        "全部列 NOT NULL(状态由导入方决定)"
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

/// future guard:`user_version = 2`(> 1)打开拒绝,稳定错误码,库不动
/// (字节不变、无 DDL、无 journal 模式改写、无 sidecar)。
#[test]
fn future_version_fails_closed_without_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let db = service_db(&tmp);
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE future_marker (id INTEGER PRIMARY KEY, note TEXT NOT NULL);
             INSERT INTO future_marker (id, note) VALUES (1, 'v2-data');
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }
    let hash_before = sha256_file(&db);
    let schema_before = schema_objects_of(&db);
    assert_eq!(journal_mode_of(&db), "delete");

    let err = match ServiceStore::open(&db) {
        Ok(_) => panic!("v2 service 库必须 fail-closed"),
        Err(err) => err,
    };
    assert_eq!(
        error_code(&err),
        Some(CODE_SCHEMA_FUTURE_VERSION),
        "必须返回稳定错误码: {err:#}"
    );
    match err.downcast_ref::<ServiceSchemaError>() {
        Some(ServiceSchemaError::FutureVersion { found, known }) => {
            assert_eq!(*found, 2);
            assert_eq!(*known, SERVICE_SCHEMA_VERSION);
        }
        other => panic!("必须是 FutureVersion 判别值: {other:?}"),
    }

    assert_eq!(
        sha256_file(&db),
        hash_before,
        "拒绝路径不得改写数据库文件字节"
    );
    assert_eq!(user_version_of(&db), 2, "user_version 不变");
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
    assert_eq!(note, "v2-data", "既有数据不动");
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
