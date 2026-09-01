//! Catalog v1 → v2 一次性幂等导入。
//!
//! v1 始终通过只读连接读取，v2 在单事务内写入全部业务投影与 marker；
//! 失败不会留下半导入。Secret 只迁移 `(secret_key, store_id)` 引用，
//! ciphertext 仅参与内存中的 source digest，绝不写进 v2。

use crate::catalog_store::{CatalogStore, CatalogV2Store};
use crate::schema::{schema_version_of, CATALOG_SCHEMA_VERSION};
use anyhow::{Context as _, Result};
use rusqlite::backup::Backup;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

pub const CATALOG_V1_IMPORT_MARKER: &str = "catalog-v1-to-v2";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogImportCounts {
    pub agent_instances: usize,
    pub agent_instance_versions: usize,
    pub workflow_templates: usize,
    pub workflow_template_versions: usize,
    pub secret_refs: usize,
    pub plugin_pins: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogV1ImportReport {
    pub source_digest: String,
    pub counts: CatalogImportCounts,
    pub already_imported: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AgentInstanceRow {
    instance_key: String,
    name: String,
    agent_type: String,
    scope: String,
    project_key: Option<String>,
    current_version: i64,
    enabled: i64,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct AgentInstanceVersionRow {
    instance_key: String,
    version: i64,
    config_json: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct WorkflowTemplateRow {
    template_key: String,
    name: String,
    current_version: i64,
    task_local: i64,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct WorkflowTemplateVersionRow {
    template_key: String,
    version: i64,
    graph_json: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct SecretRefRow {
    secret_key: String,
    store_id: String,
    ciphertext_digest: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct PluginPinRow {
    run_key: String,
    full_id: String,
    version: String,
    content_hash: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct CatalogV1Snapshot {
    schema_version: i64,
    agent_instances: Vec<AgentInstanceRow>,
    agent_instance_versions: Vec<AgentInstanceVersionRow>,
    workflow_templates: Vec<WorkflowTemplateRow>,
    workflow_template_versions: Vec<WorkflowTemplateVersionRow>,
    secret_refs: Vec<SecretRefRow>,
    plugin_pins: Vec<PluginPinRow>,
}

impl CatalogV1Snapshot {
    fn counts(&self) -> CatalogImportCounts {
        CatalogImportCounts {
            agent_instances: self.agent_instances.len(),
            agent_instance_versions: self.agent_instance_versions.len(),
            workflow_templates: self.workflow_templates.len(),
            workflow_template_versions: self.workflow_template_versions.len(),
            secret_refs: self.secret_refs.len(),
            plugin_pins: self.plugin_pins.len(),
        }
    }

    fn digest(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self)?;
        Ok(Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }
}

impl CatalogV2Store {
    pub fn import_catalog_v1(&self, source_path: &Path) -> Result<CatalogV1ImportReport> {
        let started = std::time::Instant::now();
        // 完成 marker 是 v2 已成为权威的 publication barrier。之后旧程序
        // 即使继续改写/删除 v1，也不得阻止 Core 打开 v2。
        if let Some(report) = self.completed_catalog_v1_import()? {
            return Ok(report);
        }
        anyhow::ensure!(
            source_path.is_file(),
            "Catalog v1 不存在: {}",
            source_path.display()
        );
        if let Some(target_path) = self.path() {
            let source = std::fs::canonicalize(source_path)
                .with_context(|| format!("解析 Catalog v1 路径失败: {}", source_path.display()))?;
            let target = std::fs::canonicalize(target_path)
                .with_context(|| format!("解析 Catalog v2 路径失败: {}", target_path.display()))?;
            anyhow::ensure!(source != target, "Catalog v1 与 v2 不得使用同一文件");
        }

        let snapshot = load_v1_snapshot(source_path)?;
        let source_digest = snapshot.digest()?;
        let counts = snapshot.counts();
        let imported = self.with_tx(|tx| {
            // 并发 import 的第二个 writer 在取得锁后复验 marker；第一条
            // 已完整提交时直接收敛，不误报 target-not-empty。
            let marker_exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM migration_marker WHERE marker = ?1)",
                params![CATALOG_V1_IMPORT_MARKER],
                |row| row.get(0),
            )?;
            if marker_exists {
                return Ok(false);
            }
            for table in [
                "agent_instances",
                "agent_instance_versions",
                "workflow_templates",
                "workflow_template_versions",
                "secret_refs",
                "plugin_pins",
            ] {
                let count: i64 =
                    tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
                anyhow::ensure!(
                    count == 0,
                    "catalog_v2_import_target_not_empty:{table} 已有 {count} 行但缺少 marker"
                );
            }

            for row in &snapshot.agent_instances {
                tx.execute(
                    "INSERT INTO agent_instances
                     (instance_key, name, agent_type, scope, project_key, current_version,
                      enabled, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        row.instance_key,
                        row.name,
                        row.agent_type,
                        row.scope,
                        row.project_key,
                        row.current_version,
                        row.enabled,
                        row.created_at,
                        row.updated_at
                    ],
                )?;
            }
            for row in &snapshot.agent_instance_versions {
                tx.execute(
                    "INSERT INTO agent_instance_versions
                     (instance_key, version, config_json, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        row.instance_key,
                        row.version,
                        row.config_json,
                        row.created_at
                    ],
                )?;
            }
            for row in &snapshot.workflow_templates {
                tx.execute(
                    "INSERT INTO workflow_templates
                     (template_key, name, current_version, task_local, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        row.template_key,
                        row.name,
                        row.current_version,
                        row.task_local,
                        row.created_at,
                        row.updated_at
                    ],
                )?;
            }
            for row in &snapshot.workflow_template_versions {
                tx.execute(
                    "INSERT INTO workflow_template_versions
                     (template_key, version, graph_json, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        row.template_key,
                        row.version,
                        row.graph_json,
                        row.created_at
                    ],
                )?;
            }
            for row in &snapshot.secret_refs {
                tx.execute(
                    "INSERT INTO secret_refs
                     (secret_key, store_id, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![row.secret_key, row.store_id, row.created_at, row.updated_at],
                )?;
            }
            for row in &snapshot.plugin_pins {
                tx.execute(
                    "INSERT INTO plugin_pins
                     (run_key, full_id, version, content_hash, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        row.run_key,
                        row.full_id,
                        row.version,
                        row.content_hash,
                        row.created_at
                    ],
                )?;
            }
            tx.execute(
                "INSERT INTO migration_marker
                 (marker, source_schema_version, source_digest, imported_counts_json, completed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    CATALOG_V1_IMPORT_MARKER,
                    CATALOG_SCHEMA_VERSION,
                    source_digest,
                    serde_json::to_string(&counts)?,
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
            Ok(true)
        })?;
        if !imported {
            return self
                .completed_catalog_v1_import()?
                .ok_or_else(|| anyhow::anyhow!("Catalog v1 并发导入完成后 marker 消失"));
        }

        let metric_key = self.with_conn(|conn| {
            Ok(crate::observability::store_metric_key(
                conn,
                crate::migration::StoreKind::Catalog,
            ))
        })?;
        let imported_rows = counts.agent_instances
            + counts.agent_instance_versions
            + counts.workflow_templates
            + counts.workflow_template_versions
            + counts.secret_refs
            + counts.plugin_pins;
        crate::observability::record_catalog_import(
            &metric_key,
            started.elapsed().as_millis(),
            imported_rows,
        );

        log::info!(
            "catalog_v1_import_complete source_schema_version={} duration_ms={} instances={} instance_versions={} templates={} template_versions={} secret_refs={} plugin_pins={}",
            CATALOG_SCHEMA_VERSION,
            started.elapsed().as_millis(),
            counts.agent_instances,
            counts.agent_instance_versions,
            counts.workflow_templates,
            counts.workflow_template_versions,
            counts.secret_refs,
            counts.plugin_pins
        );
        Ok(CatalogV1ImportReport {
            source_digest,
            counts,
            already_imported: false,
        })
    }

    pub(crate) fn completed_catalog_v1_import(&self) -> Result<Option<CatalogV1ImportReport>> {
        let marker: Option<(i64, String, String)> = self.with_conn(|conn| {
            conn.query_row(
                "SELECT source_schema_version, source_digest, imported_counts_json
                 FROM migration_marker WHERE marker = ?1",
                params![CATALOG_V1_IMPORT_MARKER],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(Into::into)
        })?;
        let Some((recorded_version, source_digest, recorded_counts)) = marker else {
            return Ok(None);
        };
        anyhow::ensure!(
            recorded_version == CATALOG_SCHEMA_VERSION,
            "catalog_v2_marker_inconsistent:source schema version 损坏"
        );
        anyhow::ensure!(
            source_digest.len() == 64 && source_digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "catalog_v2_marker_inconsistent:source digest 损坏"
        );
        let counts: CatalogImportCounts = serde_json::from_str(&recorded_counts)
            .context("Catalog v2 migration marker counts 损坏")?;
        Ok(Some(CatalogV1ImportReport {
            source_digest,
            counts,
            already_imported: true,
        }))
    }
}

fn load_v1_snapshot(path: &Path) -> Result<CatalogV1Snapshot> {
    let source = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("只读打开 Catalog v1 失败: {}", path.display()))?;
    crate::migration::guard_future_version(
        &source,
        crate::migration::StoreKind::Catalog,
        CATALOG_SCHEMA_VERSION,
    )?;
    let version = schema_version_of(&source)?;
    anyhow::ensure!(
        version == CATALOG_SCHEMA_VERSION,
        "catalog_v1_schema_mismatch:期望 v{}，实际 v{}",
        CATALOG_SCHEMA_VERSION,
        version
    );
    let mut conn = Connection::open_in_memory()?;
    {
        let backup = Backup::new(&source, &mut conn).context("创建 Catalog v1 内存快照失败")?;
        backup
            .run_to_completion(128, Duration::from_millis(1), None)
            .context("读取 Catalog v1 一致快照失败")?;
    }
    drop(source);
    CatalogStore::repair_import_snapshot(&mut conn)?;

    conn.pragma_update(None, "query_only", "ON")?;
    let tx = conn.transaction()?;
    let agent_instances = query_rows(
        &tx,
        "SELECT instance_key, name, agent_type, scope, project_key, current_version,
                enabled, created_at, updated_at
         FROM agent_instances ORDER BY instance_key",
        |row| {
            Ok(AgentInstanceRow {
                instance_key: row.get(0)?,
                name: row.get(1)?,
                agent_type: row.get(2)?,
                scope: row.get(3)?,
                project_key: row.get(4)?,
                current_version: row.get(5)?,
                enabled: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        },
    )?;
    let agent_instance_versions = query_rows(
        &tx,
        "SELECT i.instance_key, v.version, v.config_json, v.created_at
         FROM agent_instance_versions v
         JOIN agent_instances i ON i.id = v.instance_id
         ORDER BY i.instance_key, v.version",
        |row| {
            Ok(AgentInstanceVersionRow {
                instance_key: row.get(0)?,
                version: row.get(1)?,
                config_json: row.get(2)?,
                created_at: row.get(3)?,
            })
        },
    )?;
    let workflow_templates = query_rows(
        &tx,
        "SELECT template_key, name, current_version, task_local, created_at, updated_at
         FROM workflow_templates ORDER BY template_key",
        |row| {
            Ok(WorkflowTemplateRow {
                template_key: row.get(0)?,
                name: row.get(1)?,
                current_version: row.get(2)?,
                task_local: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            })
        },
    )?;
    let workflow_template_versions = query_rows(
        &tx,
        "SELECT t.template_key, v.version, v.graph_json, v.created_at
         FROM workflow_template_versions v
         JOIN workflow_templates t ON t.id = v.template_id
         ORDER BY t.template_key, v.version",
        |row| {
            Ok(WorkflowTemplateVersionRow {
                template_key: row.get(0)?,
                version: row.get(1)?,
                graph_json: row.get(2)?,
                created_at: row.get(3)?,
            })
        },
    )?;
    let secret_refs = query_rows(
        &tx,
        "SELECT secret_key, store_id, ciphertext, created_at, updated_at
         FROM sealed_secrets ORDER BY secret_key, store_id",
        |row| {
            let ciphertext: Vec<u8> = row.get(2)?;
            Ok(SecretRefRow {
                secret_key: row.get(0)?,
                store_id: row.get(1)?,
                ciphertext_digest: Sha256::digest(ciphertext)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        },
    )?;
    let plugin_pins = query_rows(
        &tx,
        "SELECT run_key, full_id, version, content_hash, created_at
         FROM plugin_pins ORDER BY run_key, full_id, version, content_hash",
        |row| {
            Ok(PluginPinRow {
                run_key: row.get(0)?,
                full_id: row.get(1)?,
                version: row.get(2)?,
                content_hash: row.get(3)?,
                created_at: row.get(4)?,
            })
        },
    )?;
    tx.commit()?;
    Ok(CatalogV1Snapshot {
        schema_version: version,
        agent_instances,
        agent_instance_versions,
        workflow_templates,
        workflow_template_versions,
        secret_refs,
        plugin_pins,
    })
}

fn query_rows<T>(
    conn: &Connection,
    sql: &str,
    mut map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |row| map(row))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
