//! Catalog v1 → v2 幂等、只读源与原子导入契约。

use mf_agent::catalog_store::CatalogV2Store;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const SECRET_BYTES: &[u8] = b"catalog-v1-secret-ciphertext-must-not-copy";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("baseline")
        .join("catalog-v1.db")
}

fn source_with_secret() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("catalog-v1.db");
    std::fs::copy(fixture_path(), &source).unwrap();
    {
        let conn = Connection::open(&source).unwrap();
        conn.execute(
            "INSERT INTO sealed_secrets
             (secret_key, store_id, ciphertext, created_at, updated_at)
             VALUES ('provider-key', 'keyring', ?1, '2026-01-01', '2026-01-02')",
            params![SECRET_BYTES],
        )
        .unwrap();
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE").ok();
    }
    (tmp, source)
}

fn sha256_file(path: &Path) -> String {
    Sha256::digest(std::fs::read(path).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn imports_frozen_v1_projection_once_without_touching_source_or_secret_ciphertext() {
    let (_tmp, source) = source_with_secret();
    let source_hash = sha256_file(&source);
    let target_dir = source.parent().unwrap().join(".monkeyfence");
    std::fs::create_dir_all(&target_dir).unwrap();
    let target = target_dir.join("catalog-v2.db");
    let store = CatalogV2Store::open(&target).unwrap();

    let first = store.import_catalog_v1(&source).unwrap();
    assert!(!first.already_imported);
    assert_eq!(first.counts.agent_instances, 2);
    assert_eq!(first.counts.agent_instance_versions, 3);
    assert_eq!(first.counts.workflow_templates, 1);
    assert_eq!(first.counts.workflow_template_versions, 1);
    assert_eq!(first.counts.secret_refs, 1);
    assert_eq!(first.counts.plugin_pins, 2);
    assert_eq!(sha256_file(&source), source_hash, "导入不得改写 Catalog v1");
    assert_eq!(
        v2_projection(&store),
        v1_projection(&source),
        "Agent Instance/版本、模板/版本、Secret ref 与 plugin pin 必须与 T0A v1 投影等价"
    );

    store
        .with_conn(|conn| {
            let instances: Vec<(String, String, i64)> = query(
                conn,
                "SELECT instance_key, name, current_version FROM agent_instances ORDER BY instance_key",
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            assert_eq!(
                instances,
                vec![
                    ("fixture-inst-project".into(), "基线项目实例".into(), 1),
                    ("fixture-inst-user".into(), "基线用户实例".into(), 2),
                ]
            );
            let versions: Vec<(String, i64, String)> = query(
                conn,
                "SELECT instance_key, version, config_json
                 FROM agent_instance_versions ORDER BY instance_key, version",
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            assert_eq!(versions.len(), 3);
            assert!(versions.iter().all(|(_, _, json)| serde_json::from_str::<serde_json::Value>(json).is_ok()));

            let templates: Vec<(String, String, i64)> = query(
                conn,
                "SELECT template_key, name, current_version FROM workflow_templates",
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            assert_eq!(templates, vec![("tpl-baseline".into(), "基线模板".into(), 1)]);
            let pins: i64 = conn.query_row("SELECT COUNT(*) FROM plugin_pins", [], |r| r.get(0))?;
            assert_eq!(pins, 2);
            let secret_ref: (String, String) = conn.query_row(
                "SELECT secret_key, store_id FROM secret_refs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(secret_ref, ("provider-key".into(), "keyring".into()));
            let marker_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM migration_marker WHERE marker = 'catalog-v1-to-v2'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(marker_count, 1);
            Ok(())
        })
        .unwrap();

    let completed_at_before: String = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT completed_at FROM migration_marker WHERE marker = 'catalog-v1-to-v2'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    let second = store.import_catalog_v1(&source).unwrap();
    assert!(second.already_imported);
    assert_eq!(second.source_digest, first.source_digest);
    assert_eq!(second.counts, first.counts);
    let completed_at_after: String = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT completed_at FROM migration_marker WHERE marker = 'catalog-v1-to-v2'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        completed_at_after, completed_at_before,
        "第二次导入必须 no-op"
    );
    assert_eq!(sha256_file(&source), source_hash);

    drop(store);
    let mut target_bytes = std::fs::read(&target).unwrap();
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", target.display(), suffix));
        if sidecar.exists() {
            target_bytes.extend(std::fs::read(sidecar).unwrap());
        }
    }
    assert!(
        !target_bytes
            .windows(SECRET_BYTES.len())
            .any(|window| window == SECRET_BYTES),
        "Catalog v2 文件或 sidecar 不得含 Secret ciphertext"
    );
}

#[test]
fn imports_supported_early_partial_v1_via_repaired_memory_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("catalog-v1-partial.db");
    {
        let conn = Connection::open(&source).unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_instances (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 instance_key TEXT NOT NULL UNIQUE,
                 name TEXT NOT NULL,
                 agent_type TEXT NOT NULL,
                 scope TEXT NOT NULL DEFAULT 'user',
                 current_version INTEGER NOT NULL DEFAULT 1,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE agent_instance_versions (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 instance_id INTEGER NOT NULL REFERENCES agent_instances(id),
                 version INTEGER NOT NULL,
                 config_json TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 UNIQUE(instance_id, version)
             );
             INSERT INTO agent_instances
                 (instance_key, name, agent_type, scope, current_version, created_at, updated_at)
             VALUES ('early-inst', '早期实例', 'claude', 'user', 1, '2026-01-01', '2026-01-01');
             INSERT INTO agent_instance_versions(instance_id, version, config_json, created_at)
             VALUES (1, 1, '{}', '2026-01-01');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let before = sha256_file(&source);
    let store = CatalogV2Store::memory().unwrap();
    let report = store.import_catalog_v1(&source).unwrap();
    assert_eq!(report.counts.agent_instances, 1);
    assert_eq!(report.counts.agent_instance_versions, 1);
    assert_eq!(sha256_file(&source), before, "真实残缺 v1 必须保持只读");

    let source_conn =
        Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let source_columns: Vec<String> = source_conn
        .prepare("PRAGMA table_info(agent_instances)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!source_columns.iter().any(|column| column == "enabled"));
    assert!(!source_columns.iter().any(|column| column == "project_key"));

    let imported_name: String = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT name FROM agent_instances WHERE instance_key='early-inst'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(imported_name, "早期实例");
}

#[test]
fn source_change_after_marker_cannot_block_or_mutate_authoritative_v2() {
    let (_tmp, source) = source_with_secret();
    let store = CatalogV2Store::memory().unwrap();
    store.import_catalog_v1(&source).unwrap();
    let metric_key = store
        .with_conn(|conn| {
            Ok(mf_agent::observability::store_metric_key(
                conn,
                mf_agent::migration::StoreKind::Catalog,
            ))
        })
        .unwrap();
    let metrics_after_success = mf_agent::observability::storage_metrics_snapshot()
        .stores
        .get(&metric_key)
        .cloned()
        .unwrap_or_default();
    let marker_before: (String, String) = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT source_digest, imported_counts_json FROM migration_marker",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();

    {
        let conn = Connection::open(&source).unwrap();
        conn.execute(
            "UPDATE agent_instances SET name = 'changed' WHERE instance_key = 'fixture-inst-user'",
            [],
        )
        .unwrap();
    }
    let second = store.import_catalog_v1(&source).unwrap();
    assert!(second.already_imported);
    assert_eq!(
        mf_agent::observability::storage_metrics_snapshot()
            .stores
            .get(&metric_key)
            .cloned()
            .unwrap_or_default(),
        metrics_after_success,
        "marker 冲突拒绝不得累计迁移 metrics"
    );
    let marker_after: (String, String) = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT source_digest, imported_counts_json FROM migration_marker",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(marker_after, marker_before);
    let v2_name: String = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT name FROM agent_instances WHERE instance_key = 'fixture-inst-user'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(v2_name, "基线用户实例", "废弃 v1 的后续写入不得回灌 v2");
}

#[test]
fn completed_marker_allows_post_migration_plugin_gc() {
    let (_tmp, source) = source_with_secret();
    let store = CatalogV2Store::memory().unwrap();
    store.import_catalog_v1(&source).unwrap();
    store
        .with_conn(|conn| {
            conn.execute("DELETE FROM plugin_pins", [])?;
            Ok(())
        })
        .unwrap();
    let report = store.import_catalog_v1(&source).unwrap();
    assert!(report.already_imported);
    let remaining = store
        .with_conn(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM plugin_pins", [], |r| {
                r.get::<_, i64>(0)
            })?)
        })
        .unwrap();
    assert_eq!(remaining, 0, "v2 成为权威后不得从废弃 v1 回灌已 GC 的 pin");
}

#[test]
fn completed_marker_allows_reopen_after_legacy_v1_is_removed() {
    let (_tmp, source) = source_with_secret();
    let v2 = source.parent().unwrap().join("catalog-v2.db");
    let store = CatalogV2Store::open(&v2).unwrap();
    let first = store.import_catalog_v1(&source).unwrap();
    drop(store);
    std::fs::remove_file(&source).unwrap();

    let (reopened, report) = CatalogV2Store::open_migrating_v1(&v2, &source).unwrap();
    let report = report.expect("完成 marker 必须足以恢复，不再依赖废弃 v1");
    assert!(report.already_imported);
    assert_eq!(report.source_digest, first.source_digest);
    assert_eq!(report.counts, first.counts);
    assert_eq!(reopened.schema_version().unwrap(), 1);
}

#[test]
fn import_failure_rolls_back_all_rows_and_marker() {
    let (_tmp, source) = source_with_secret();
    let store = CatalogV2Store::memory().unwrap();
    let metric_key = store
        .with_conn(|conn| {
            Ok(mf_agent::observability::store_metric_key(
                conn,
                mf_agent::migration::StoreKind::Catalog,
            ))
        })
        .unwrap();
    let metrics_before = mf_agent::observability::storage_metrics_snapshot()
        .stores
        .get(&metric_key)
        .cloned()
        .unwrap_or_default();
    store
        .with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER fault_pin BEFORE INSERT ON plugin_pins
                 BEGIN SELECT RAISE(ABORT, 'fault:pin-import'); END;",
            )?;
            Ok(())
        })
        .unwrap();
    let err = store.import_catalog_v1(&source).unwrap_err();
    assert!(format!("{err:#}").contains("fault:pin-import"));
    assert_eq!(
        mf_agent::observability::storage_metrics_snapshot()
            .stores
            .get(&metric_key)
            .cloned()
            .unwrap_or_default(),
        metrics_before,
        "回滚的 Catalog 导入不得污染 metrics"
    );
    store
        .with_conn(|conn| {
            for table in [
                "agent_instances",
                "agent_instance_versions",
                "workflow_templates",
                "workflow_template_versions",
                "secret_refs",
                "plugin_pins",
                "migration_marker",
            ] {
                let count: i64 =
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                assert_eq!(count, 0, "失败后 `{table}` 不得留下半行");
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn future_catalog_v1_source_is_rejected_before_target_writes() {
    let (_tmp, source) = source_with_secret();
    {
        let conn = Connection::open(&source).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
    }
    let store = CatalogV2Store::memory().unwrap();
    let err = store.import_catalog_v1(&source).unwrap_err();
    assert_eq!(
        mf_agent::migration::error_code(&err),
        Some("schema_future_version")
    );
    store
        .with_conn(|conn| {
            for table in ["agent_instances", "plugin_pins", "migration_marker"] {
                let count: i64 =
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                assert_eq!(count, 0, "future source 不得写入 `{table}`");
            }
            Ok(())
        })
        .unwrap();
}

fn query<T>(
    conn: &Connection,
    sql: &str,
    mut map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> anyhow::Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |row| map(row))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn v1_projection(path: &Path) -> serde_json::Value {
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    projection(
        &conn,
        "SELECT instance_key, name, agent_type, scope, project_key, current_version,
                enabled, created_at, updated_at FROM agent_instances ORDER BY instance_key",
        "SELECT i.instance_key, v.version, v.config_json, v.created_at
         FROM agent_instance_versions v JOIN agent_instances i ON i.id = v.instance_id
         ORDER BY i.instance_key, v.version",
        "SELECT template_key, name, current_version, task_local, created_at, updated_at
         FROM workflow_templates ORDER BY template_key",
        "SELECT t.template_key, v.version, v.graph_json, v.created_at
         FROM workflow_template_versions v JOIN workflow_templates t ON t.id = v.template_id
         ORDER BY t.template_key, v.version",
        "SELECT secret_key, store_id, created_at, updated_at
         FROM sealed_secrets ORDER BY secret_key, store_id",
    )
}

fn v2_projection(store: &CatalogV2Store) -> serde_json::Value {
    store
        .with_conn(|conn| {
            Ok(projection(
                conn,
                "SELECT instance_key, name, agent_type, scope, project_key, current_version,
                        enabled, created_at, updated_at FROM agent_instances ORDER BY instance_key",
                "SELECT instance_key, version, config_json, created_at
                 FROM agent_instance_versions ORDER BY instance_key, version",
                "SELECT template_key, name, current_version, task_local, created_at, updated_at
                 FROM workflow_templates ORDER BY template_key",
                "SELECT template_key, version, graph_json, created_at
                 FROM workflow_template_versions ORDER BY template_key, version",
                "SELECT secret_key, store_id, created_at, updated_at
                 FROM secret_refs ORDER BY secret_key, store_id",
            ))
        })
        .unwrap()
}

fn projection(
    conn: &Connection,
    instances_sql: &str,
    versions_sql: &str,
    templates_sql: &str,
    template_versions_sql: &str,
    secret_refs_sql: &str,
) -> serde_json::Value {
    let instances: Vec<serde_json::Value> = query(conn, instances_sql, |r| {
        Ok(serde_json::json!({
            "instance_key": r.get::<_, String>(0)?,
            "name": r.get::<_, String>(1)?,
            "agent_type": r.get::<_, String>(2)?,
            "scope": r.get::<_, String>(3)?,
            "project_key": r.get::<_, Option<String>>(4)?,
            "current_version": r.get::<_, i64>(5)?,
            "enabled": r.get::<_, i64>(6)?,
            "created_at": r.get::<_, String>(7)?,
            "updated_at": r.get::<_, String>(8)?,
        }))
    })
    .unwrap();
    let versions: Vec<serde_json::Value> = query(conn, versions_sql, |r| {
        let config: String = r.get(2)?;
        Ok(serde_json::json!({
            "instance_key": r.get::<_, String>(0)?,
            "version": r.get::<_, i64>(1)?,
            "config": serde_json::from_str::<serde_json::Value>(&config).unwrap(),
            "created_at": r.get::<_, String>(3)?,
        }))
    })
    .unwrap();
    let templates: Vec<serde_json::Value> = query(conn, templates_sql, |r| {
        Ok(serde_json::json!({
            "template_key": r.get::<_, String>(0)?,
            "name": r.get::<_, String>(1)?,
            "current_version": r.get::<_, i64>(2)?,
            "task_local": r.get::<_, i64>(3)?,
            "created_at": r.get::<_, String>(4)?,
            "updated_at": r.get::<_, String>(5)?,
        }))
    })
    .unwrap();
    let template_versions: Vec<serde_json::Value> = query(conn, template_versions_sql, |r| {
        let graph: String = r.get(2)?;
        Ok(serde_json::json!({
            "template_key": r.get::<_, String>(0)?,
            "version": r.get::<_, i64>(1)?,
            "graph": serde_json::from_str::<serde_json::Value>(&graph).unwrap(),
            "created_at": r.get::<_, String>(3)?,
        }))
    })
    .unwrap();
    let secret_refs: Vec<serde_json::Value> = query(conn, secret_refs_sql, |r| {
        Ok(serde_json::json!({
            "secret_key": r.get::<_, String>(0)?,
            "store_id": r.get::<_, String>(1)?,
            "created_at": r.get::<_, String>(2)?,
            "updated_at": r.get::<_, String>(3)?,
        }))
    })
    .unwrap();
    let plugin_pins: Vec<serde_json::Value> = query(
        conn,
        "SELECT run_key, full_id, version, content_hash, created_at
         FROM plugin_pins ORDER BY run_key, full_id, version, content_hash",
        |r| {
            Ok(serde_json::json!({
                "run_key": r.get::<_, String>(0)?,
                "full_id": r.get::<_, String>(1)?,
                "version": r.get::<_, String>(2)?,
                "content_hash": r.get::<_, String>(3)?,
                "created_at": r.get::<_, String>(4)?,
            }))
        },
    )
    .unwrap();
    serde_json::json!({
        "agent_instances": instances,
        "agent_instance_versions": versions,
        "workflow_templates": templates,
        "workflow_template_versions": template_versions,
        "secret_refs": secret_refs,
        "plugin_pins": plugin_pins,
    })
}

// ---------------------------------------------------------------------------
// T4b(Issue #40):discovery 结果的 Catalog v2 additive adapter
// ---------------------------------------------------------------------------

fn installation(
    handle: &str,
    agent_type: &str,
    canonical: &str,
    source: &str,
) -> mf_agent::catalog_store::CliInstallationRecord {
    mf_agent::catalog_store::CliInstallationRecord {
        installation_handle: handle.into(),
        agent_type_id: agent_type.into(),
        executable_path: canonical.into(),
        canonical_path: canonical.into(),
        actual_version: Some("1.2.3".into()),
        source: source.into(),
        scope: "user".into(),
        health: "detected".into(),
    }
}

#[test]
fn t4b_cli_installation_upsert_is_additive_and_idempotent_by_canonical() {
    let store = CatalogV2Store::memory().unwrap();
    // 同一 Agent Type 呈现多安装(external + managed 并存)
    store
        .upsert_cli_installation(&installation(
            "inst-1",
            "codex",
            "/usr/local/bin/codex",
            "external",
        ))
        .unwrap();
    store
        .upsert_cli_installation(&installation(
            "inst-2",
            "codex",
            "/managed/codex.exe",
            "managed",
        ))
        .unwrap();
    let mut all = store.list_cli_installations(None).unwrap();
    assert_eq!(all.len(), 2, "同一 Type 双安装并存");
    // canonical 幂等:重复写入不产生第二行
    store
        .upsert_cli_installation(&installation(
            "inst-1b",
            "codex",
            "/usr/local/bin/codex",
            "external",
        ))
        .unwrap();
    all = store.list_cli_installations(None).unwrap();
    assert_eq!(all.len(), 2, "canonical 幂等去重");
    // 按 Type 过滤
    store
        .upsert_cli_installation(&installation(
            "inst-3",
            "claude",
            "/usr/local/bin/claude",
            "external",
        ))
        .unwrap();
    let codex_only = store.list_cli_installations(Some("codex")).unwrap();
    assert_eq!(codex_only.len(), 2);
    assert!(codex_only.iter().all(|r| r.agent_type_id == "codex"));
    assert_eq!(store.list_cli_installations(None).unwrap().len(), 3);
}
