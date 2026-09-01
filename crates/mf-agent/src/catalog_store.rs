//! 目录库:用户级 Agent Instance、工作流模板、加密 Secret 与插件包记录。
//!
//! 与项目库(`store::Store`)相互独立;一个 MonkeyFence 进程只打开一个目录库,
//! 默认位于 `~/.monkeyfence/catalog-v1.db`。本阶段只提供 schema 与连接管理,
//! 读写 API 随后续里程碑补全。

use crate::agent_instance::{
    AgentInstance, AgentInstanceDraft, AgentInstanceOverrides, AgentInstanceSnapshot,
    AgentInstanceVersion,
};
use crate::model::InstanceScope;
use crate::schema::{
    catalog_db_path, catalog_v2_db_path, schema_version_of, table_names_of, CATALOG_SCHEMA_V1,
    CATALOG_SCHEMA_VERSION, CATALOG_V2_SCHEMA_V1, CATALOG_V2_SCHEMA_VERSION,
};
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct CatalogStore {
    conn: Mutex<Connection>,
}

/// 新 Catalog 的独立文件/版本链。T1c 阶段它是 dark data，legacy
/// `CatalogStore` 仍只指向 v1；二者绝不双写。
pub struct CatalogV2Store {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

pub const CATALOG_V2_REQUIRED_TABLES: &[&str] = &[
    "agent_type_catalog",
    "cli_installations",
    "installation_receipts",
    "installation_jobs",
    "provider_profiles",
    "provider_model_cache",
    "agent_instances",
    "agent_instance_versions",
    "workflow_templates",
    "workflow_template_versions",
    "secret_refs",
    "plugin_pins",
    "command_receipt",
    "projection_outbox",
    "migration_marker",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPinRecord {
    pub run_key: String,
    pub full_id: String,
    pub version: String,
    pub content_hash: String,
    pub created_at: String,
}

impl CatalogStore {
    pub fn open(path: &Path) -> Result<Arc<CatalogStore>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录库所在目录失败: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开目录库失败: {}", path.display()))?;
        Self::init(conn).map(Arc::new)
    }

    /// 默认路径(`~/.monkeyfence/catalog-v1.db`,可用 `MF_CATALOG_DB` 重定向)。
    pub fn open_default() -> Result<Arc<CatalogStore>> {
        Self::open(&catalog_db_path())
    }

    pub fn memory() -> Result<Arc<CatalogStore>> {
        Self::init(Connection::open_in_memory()?).map(Arc::new)
    }

    fn init(mut conn: Connection) -> Result<CatalogStore> {
        // T1a:future guard 先于任何 DDL/pragma(高版本目录库 fail-closed;
        // 现状缺陷是 initialize_schema 会无条件把 user_version 写回 v1)
        crate::migration::guard_future_version(
            &conn,
            crate::migration::StoreKind::Catalog,
            CATALOG_SCHEMA_VERSION,
        )?;
        let current = schema_version_of(&conn)?;
        reject_catalog_v2_layout(&conn)?;
        crate::migration::restrict_active_database_to_current_user(&conn)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        if current == 0 {
            // 全新库无旧数据可备份,但 DDL 仍在 writer lock 内完成并复验版本。
            crate::migration::upgrade_with_barrier(
                &mut conn,
                crate::migration::StoreKind::Catalog,
                CATALOG_SCHEMA_VERSION,
                &|tx, _, _| {
                    tx.execute_batch(CATALOG_SCHEMA_V1)?;
                    ensure_agent_instances_columns(tx)
                },
            )?;
        } else if catalog_schema_needs_repair(&conn)? {
            // 早期 v1 开发库可能缺表/索引/后续列。它们虽然版本号已经是
            // current,实际 ALTER/CREATE 仍属于 schema repair,必须先备份。
            crate::migration::repair_current_with_barrier(
                &mut conn,
                crate::migration::StoreKind::Catalog,
                CATALOG_SCHEMA_VERSION,
                &catalog_schema_needs_repair,
                &|tx| {
                    tx.execute_batch(CATALOG_SCHEMA_V1)?;
                    ensure_agent_instances_columns(tx)
                },
            )?;
        } else {
            // 健康 v1 不再执行任何 DDL;仅在 lock 内重申版本,保持 T0
            // Catalog fixture 的既有 header 字节语义。
            crate::migration::reaffirm_current_version_locked(
                &mut conn,
                crate::migration::StoreKind::Catalog,
                CATALOG_SCHEMA_VERSION,
            )?;
        }

        // 持久 WAL 模式只在初始化/repair/current-version lock 成功后启用。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        crate::migration::restrict_active_database_to_current_user(&conn)?;
        log::info!(
            "store_open store=catalog schema_version={} outbox_depth=0",
            schema_version_of(&conn)?
        );
        let metric_key =
            crate::observability::store_metric_key(&conn, crate::migration::StoreKind::Catalog);
        crate::observability::record_store_open(
            &metric_key,
            crate::migration::StoreKind::Catalog,
            schema_version_of(&conn)?,
            0,
        );
        Ok(CatalogStore {
            conn: Mutex::new(conn),
        })
    }

    /// 导入器在 SQLite 一致内存快照上补齐受支持的早期残缺 v1。
    /// 调用方持有的真实 v1 文件始终只读，不执行 repair/DDL。
    pub(crate) fn repair_import_snapshot(conn: &mut Connection) -> Result<()> {
        anyhow::ensure!(
            schema_version_of(conn)? == CATALOG_SCHEMA_VERSION,
            "catalog_v1_schema_mismatch:导入快照版本不是 v{}",
            CATALOG_SCHEMA_VERSION
        );
        reject_catalog_v2_layout(conn)?;
        let tx = conn.transaction()?;
        tx.execute_batch(CATALOG_SCHEMA_V1)?;
        ensure_agent_instances_columns(&tx)?;
        tx.commit()?;
        Ok(())
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

    /// 在写事务中执行(多行写入的原子性)。
    pub fn with_tx<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    pub fn record_plugin_pin(&self, pin: &PluginPinRecord) -> Result<bool> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "INSERT OR IGNORE INTO plugin_pins
                 (run_key, full_id, version, content_hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    pin.run_key,
                    pin.full_id,
                    pin.version,
                    pin.content_hash,
                    pin.created_at,
                ],
            )?;
            Ok(changed == 1)
        })
    }

    pub fn list_plugin_pins(&self) -> Result<Vec<PluginPinRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT run_key, full_id, version, content_hash, created_at
                 FROM plugin_pins
                 ORDER BY run_key, full_id, version, content_hash",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(PluginPinRecord {
                        run_key: row.get(0)?,
                        full_id: row.get(1)?,
                        version: row.get(2)?,
                        content_hash: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn remove_plugin_pins_for_run(&self, run_key: &str) -> Result<Vec<PluginPinRecord>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows = {
            let mut stmt = tx.prepare(
                "SELECT run_key, full_id, version, content_hash, created_at
                 FROM plugin_pins WHERE run_key = ?1
                 ORDER BY full_id, version, content_hash",
            )?;
            let records = stmt
                .query_map(params![run_key], |row| {
                    Ok(PluginPinRecord {
                        run_key: row.get(0)?,
                        full_id: row.get(1)?,
                        version: row.get(2)?,
                        content_hash: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            records
        };
        tx.execute(
            "DELETE FROM plugin_pins WHERE run_key = ?1",
            params![run_key],
        )?;
        tx.commit()?;
        Ok(rows)
    }

    /// 指定插件包(full_id + content_hash)的活动 pin 数。
    /// 内置合成插件(content_hash 为空)按 full_id + 空哈希精确计数。
    pub fn plugin_pin_count_of_plugin(&self, full_id: &str, content_hash: &str) -> Result<usize> {
        self.with_conn(|conn| {
            let count = conn.query_row(
                "SELECT COUNT(*) FROM plugin_pins WHERE full_id = ?1 AND content_hash = ?2",
                params![full_id, content_hash],
                |row| row.get::<_, i64>(0),
            )?;
            Ok(count.max(0) as usize)
        })
    }

    pub fn plugin_pin_count(&self, content_hash: &str) -> Result<usize> {
        self.with_conn(|conn| {
            let count = conn.query_row(
                "SELECT COUNT(*) FROM plugin_pins WHERE content_hash = ?1",
                params![content_hash],
                |row| row.get::<_, i64>(0),
            )?;
            Ok(count.max(0) as usize)
        })
    }

    // ---------- Agent Instance ----------

    /// 创建实例:插入行 + 版本 1(同一事务,失败不留半行)。
    pub fn create_agent_instance(&self, draft: AgentInstanceDraft) -> Result<AgentInstance> {
        draft
            .validate()
            .map_err(|e| anyhow::anyhow!("Agent Instance 草案非法: {e}"))?;
        self.with_tx(|tx| {
            let ts = now();
            let key = gen_instance_key();
            tx.execute(
                "INSERT INTO agent_instances
                    (instance_key, name, agent_type, scope, project_key, current_version, enabled, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?7)",
                params![
                    key,
                    draft.name,
                    draft.agent_type,
                    draft.scope.as_str(),
                    draft.project_key,
                    draft.enabled as i64,
                    ts
                ],
            )?;
            let rowid = tx.last_insert_rowid();
            insert_version_row(tx, rowid, 1, &draft, &ts)?;
            instance_row(tx, &key)?
                .ok_or_else(|| anyhow::anyhow!("instance 插入后读取失败"))
        })
    }

    /// 编辑实例:只影响下一次启动 —— 追加新版本行并推进 current_version,
    /// 既有快照与已冻结 Revision 不变。
    pub fn update_agent_instance(
        &self,
        id: &str,
        draft: AgentInstanceDraft,
    ) -> Result<AgentInstance> {
        draft
            .validate()
            .map_err(|e| anyhow::anyhow!("Agent Instance 草案非法: {e}"))?;
        self.with_tx(|tx| {
            let existing = instance_row(tx, id)?
                .ok_or_else(|| anyhow::anyhow!("Agent Instance `{id}` 不存在"))?;
            let rowid = instance_rowid(tx, id)?
                .ok_or_else(|| anyhow::anyhow!("Agent Instance `{id}` 不存在"))?;
            let next = existing.current_version + 1;
            let ts = now();
            tx.execute(
                "UPDATE agent_instances
                 SET name = ?2, agent_type = ?3, scope = ?4, project_key = ?5,
                     current_version = ?6, enabled = ?7, updated_at = ?8
                 WHERE instance_key = ?1",
                params![
                    id,
                    draft.name,
                    draft.agent_type,
                    draft.scope.as_str(),
                    draft.project_key,
                    next,
                    draft.enabled as i64,
                    ts
                ],
            )?;
            insert_version_row(tx, rowid, next, &draft, &ts)?;
            instance_row(tx, id)?.ok_or_else(|| anyhow::anyhow!("instance 更新后读取失败"))
        })
    }

    pub fn get_agent_instance(&self, id: &str) -> Result<Option<AgentInstance>> {
        self.with_conn(|c| instance_row(c, id))
    }

    /// 列出实例;`project` 为 None 时只返回用户作用域;Some(key) 时返回
    /// 用户作用域 + 绑定该 key 的项目作用域实例(跨项目互不可见)。
    pub fn list_agent_instances(&self, project: Option<&str>) -> Result<Vec<AgentInstance>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT instance_key FROM agent_instances
                 WHERE (scope = 'user' OR (?1 IS NOT NULL AND scope = 'project' AND project_key = ?1))
                 ORDER BY instance_key",
            )?;
            let keys: Vec<String> = stmt
                .query_map(params![project], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            Ok(keys
                .iter()
                .filter_map(|k| instance_row(c, k).ok().flatten())
                .collect())
        })
    }

    pub fn set_agent_instance_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Option<AgentInstance>> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE agent_instances SET enabled = ?2, updated_at = ?3 WHERE instance_key = ?1",
                params![id, enabled as i64, now()],
            )?;
            instance_row(c, id)
        })
    }

    /// 删除实例(含版本行)。返回是否真的删除了。
    pub fn delete_agent_instance(&self, id: &str) -> Result<bool> {
        self.with_tx(|tx| {
            let rowid = instance_rowid(tx, id)?;
            if let Some(rowid) = rowid {
                tx.execute(
                    "DELETE FROM agent_instance_versions WHERE instance_id = ?1",
                    params![rowid],
                )?;
            }
            let rows = tx.execute(
                "DELETE FROM agent_instances WHERE instance_key = ?1",
                params![id],
            )?;
            Ok(rows == 1)
        })
    }

    /// 当前版本的解析快照(`overrides` 为项目覆盖,可空)。
    pub fn snapshot_agent_instance(
        &self,
        id: &str,
        overrides: Option<&AgentInstanceOverrides>,
    ) -> Result<AgentInstanceSnapshot> {
        self.snapshot_agent_instance_version(id, None, overrides)
    }

    /// 指定版本的解析快照;`version = None` 表示当前版本,
    /// `Some(v)` 固定历史版本(Revision 冻结后重放用)。
    pub fn snapshot_agent_instance_version(
        &self,
        id: &str,
        version: Option<i64>,
        overrides: Option<&AgentInstanceOverrides>,
    ) -> Result<AgentInstanceSnapshot> {
        self.with_conn(|c| {
            let instance = instance_row(c, id)?
                .ok_or_else(|| anyhow::anyhow!("Agent Instance `{id}` 不存在"))?;
            let rowid = instance_rowid(c, id)?
                .ok_or_else(|| anyhow::anyhow!("Agent Instance `{id}` 不存在"))?;
            let version = match version {
                Some(v) => v,
                None => instance.current_version,
            };
            let ver = version_row(c, rowid, version, id)?
                .ok_or_else(|| anyhow::anyhow!("Agent Instance `{id}` 版本 {version} 不存在"))?;
            let snapshot = AgentInstanceSnapshot::resolve(&instance, &ver);
            Ok(match overrides {
                Some(o) if !o.is_empty() => snapshot.apply_overrides(o),
                _ => snapshot,
            })
        })
    }

    /// 版本历史(升序)。
    pub fn agent_instance_versions(&self, id: &str) -> Result<Vec<AgentInstanceVersion>> {
        self.with_conn(|c| {
            let rowid = instance_rowid(c, id)?
                .ok_or_else(|| anyhow::anyhow!("Agent Instance `{id}` 不存在"))?;
            let mut stmt = c.prepare(
                "SELECT version, config_json, created_at FROM agent_instance_versions
                 WHERE instance_id = ?1 ORDER BY version",
            )?;
            let rows: Vec<(i64, String, String)> = stmt
                .query_map(params![rowid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            rows.into_iter()
                .map(|(version, json, created_at)| {
                    let mut v: AgentInstanceVersion = serde_json::from_str(&json)
                        .with_context(|| format!("Agent Instance `{id}` 版本行损坏"))?;
                    v.instance_id = id.to_string();
                    v.version = version;
                    v.created_at = created_at;
                    Ok(v)
                })
                .collect()
        })
    }

    // ---------- Workflow 模板 ----------

    /// 保存模板草案:同 key 追加不可变版本行并推进 current_version,
    /// 既有版本(与已冻结快照)不受影响。
    pub fn save_template(
        &self,
        draft: &crate::workflow::WorkflowTemplateDraft,
    ) -> Result<crate::workflow::WorkflowTemplateVersion> {
        if draft.key.trim().is_empty() || draft.name.trim().is_empty() {
            anyhow::bail!("模板 key/name 不能为空");
        }
        if draft.nodes.is_empty() {
            anyhow::bail!("模板至少需要一个节点");
        }
        self.with_tx(|tx| {
            let ts = now();
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT current_version FROM workflow_templates WHERE template_key = ?1",
                    params![draft.key],
                    |r| r.get(0),
                )
                .optional()?;
            let (version, task_local) = match existing {
                Some(current) => (current + 1, None),
                None => (1, Some(draft.task_local)),
            };
            if let Some(local) = task_local {
                tx.execute(
                    "INSERT INTO workflow_templates
                        (template_key, name, current_version, task_local, created_at, updated_at)
                     VALUES (?1, ?2, 1, ?3, ?4, ?4)",
                    params![draft.key, draft.name, local as i64, ts],
                )?;
            } else {
                tx.execute(
                    "UPDATE workflow_templates
                     SET name = ?2, current_version = ?3, updated_at = ?4
                     WHERE template_key = ?1",
                    params![draft.key, draft.name, version, ts],
                )?;
            }
            let nodes_json = serde_json::to_string(&draft.nodes)?;
            tx.execute(
                "INSERT INTO workflow_template_versions (template_id, version, graph_json, created_at)
                 VALUES ((SELECT id FROM workflow_templates WHERE template_key = ?1), ?2, ?3, ?4)",
                params![draft.key, version, nodes_json, ts],
            )?;
            Self::template_version_rowid_tx(tx, tx.last_insert_rowid())?
                .ok_or_else(|| anyhow::anyhow!("模板版本插入后读取失败"))
        })
    }

    /// 按版本行 rowid 读取固定版本(编译/冻结都走它)。
    pub fn template_version(
        &self,
        version_id: i64,
    ) -> Result<Option<crate::workflow::WorkflowTemplateVersion>> {
        self.with_conn(|c| Self::template_version_rowid_tx(c, version_id))
    }

    fn template_version_rowid_tx(
        c: &Connection,
        version_id: i64,
    ) -> Result<Option<crate::workflow::WorkflowTemplateVersion>> {
        let row: Option<(String, i64, String, String)> = c
            .query_row(
                "SELECT t.template_key, v.version, v.graph_json, v.created_at
                 FROM workflow_template_versions v
                 JOIN workflow_templates t ON t.id = v.template_id
                 WHERE v.id = ?1",
                params![version_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        match row {
            Some((template_key, version, graph_json, created_at)) => {
                let nodes: Vec<crate::workflow::WorkflowNodeDraft> =
                    serde_json::from_str(&graph_json)
                        .with_context(|| format!("模板 `{template_key}` 版本 {version} 行损坏"))?;
                Ok(Some(crate::workflow::WorkflowTemplateVersion {
                    version_id,
                    template_key,
                    version,
                    nodes,
                    created_at,
                }))
            }
            None => Ok(None),
        }
    }

    /// 模板版本历史(升序)。
    pub fn template_versions(
        &self,
        key: &str,
    ) -> Result<Vec<crate::workflow::WorkflowTemplateVersion>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT v.id FROM workflow_template_versions v
                 JOIN workflow_templates t ON t.id = v.template_id
                 WHERE t.template_key = ?1 ORDER BY v.version",
            )?;
            let ids: Vec<i64> = stmt
                .query_map(params![key], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            Ok(ids
                .iter()
                .filter_map(|id| Self::template_version_rowid_tx(c, *id).ok().flatten())
                .collect())
        })
    }

    /// 列出模板;`include_task_local = false` 只返回全局模板
    /// (任务本地模板默认私有,设计 §9.1)。
    pub fn list_templates(
        &self,
        include_task_local: bool,
    ) -> Result<Vec<crate::workflow::WorkflowTemplate>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT template_key, name, current_version, task_local
                 FROM workflow_templates
                 WHERE (?1 OR task_local = 0)
                 ORDER BY template_key",
            )?;
            let rows = stmt
                .query_map(params![include_task_local], |r| {
                    Ok(crate::workflow::WorkflowTemplate {
                        key: r.get(0)?,
                        name: r.get(1)?,
                        current_version: r.get(2)?,
                        task_local: r.get::<_, i64>(3)? != 0,
                    })
                })?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
    }

    /// 任务本地模板显式"另存为模板":提升为全局(设计 §9.1)。
    pub fn promote_template_to_global(&self, key: &str) -> Result<()> {
        let n = self.with_conn(|c| {
            Ok(c.execute(
                "UPDATE workflow_templates SET task_local = 0, updated_at = ?2
                 WHERE template_key = ?1",
                params![key, now()],
            )?)
        })?;
        if n == 0 {
            anyhow::bail!("模板 `{key}` 不存在");
        }
        Ok(())
    }
}

impl CatalogV2Store {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 Catalog v2 目录失败: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开 Catalog v2 失败: {}", path.display()))?;
        Self::init(conn, Some(path.to_path_buf())).map(Arc::new)
    }

    pub fn open_default() -> Result<Arc<Self>> {
        Self::open(&catalog_v2_db_path())
    }

    /// T1c 一次性启动入口：创建/打开 v2 后只读导入默认 v1。
    /// 此方法不替换 legacy `CatalogStore::open_default`，因此在 Bridge
    /// 切换前不会形成生产双写。
    pub fn open_default_migrating_v1() -> Result<(
        Arc<Self>,
        Option<crate::catalog_migration::CatalogV1ImportReport>,
    )> {
        Self::open_migrating_v1(&catalog_v2_db_path(), &catalog_db_path())
    }

    /// 显式路径入口供 bootstrap 与契约测试使用。fresh profile 没有 v1
    /// 是正常状态：创建空 v2 并返回 `None`，不伪造迁移 marker。
    pub fn open_migrating_v1(
        v2_path: &Path,
        v1_path: &Path,
    ) -> Result<(
        Arc<Self>,
        Option<crate::catalog_migration::CatalogV1ImportReport>,
    )> {
        let store = Self::open(v2_path)?;
        let report = if let Some(report) = store.completed_catalog_v1_import()? {
            Some(report)
        } else if v1_path.is_file() {
            Some(store.import_catalog_v1(v1_path)?)
        } else {
            None
        };
        Ok((store, report))
    }

    pub fn memory() -> Result<Arc<Self>> {
        Self::init(Connection::open_in_memory()?, None).map(Arc::new)
    }

    fn init(mut conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        crate::migration::guard_future_version(
            &conn,
            crate::migration::StoreKind::Catalog,
            CATALOG_V2_SCHEMA_VERSION,
        )?;
        let current = schema_version_of(&conn)?;
        if current == CATALOG_V2_SCHEMA_VERSION && !catalog_v2_schema_ready(&conn)? {
            anyhow::bail!(
                "catalog_v2_schema_mismatch:文件标记为 Catalog v2 schema v{}，但缺少 v2 指纹；拒绝把 catalog-v1.db 当作 v2 打开",
                CATALOG_V2_SCHEMA_VERSION
            );
        }

        crate::migration::restrict_active_database_to_current_user(&conn)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        if current == 0 {
            crate::migration::upgrade_with_barrier(
                &mut conn,
                crate::migration::StoreKind::Catalog,
                CATALOG_V2_SCHEMA_VERSION,
                &|tx, _, _| {
                    tx.execute_batch(CATALOG_V2_SCHEMA_V1)?;
                    Ok(())
                },
            )?;
        }
        anyhow::ensure!(
            catalog_v2_schema_ready(&conn)?,
            "catalog_v2_schema_mismatch:Catalog v2 初始化后 schema 指纹不完整"
        );

        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        crate::migration::restrict_active_database_to_current_user(&conn)?;
        let outbox_depth: i64 = conn.query_row(
            "SELECT COUNT(*) FROM projection_outbox WHERE published_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        let metric_key =
            crate::observability::store_metric_key(&conn, crate::migration::StoreKind::Catalog);
        crate::observability::record_store_open(
            &metric_key,
            crate::migration::StoreKind::Catalog,
            CATALOG_V2_SCHEMA_VERSION,
            outbox_depth,
        );
        log::info!(
            "store_open store=catalog-v2 schema_version={} outbox_depth={}",
            CATALOG_V2_SCHEMA_VERSION,
            outbox_depth
        );
        Ok(Self {
            conn: Mutex::new(conn),
            path,
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

    pub(crate) fn with_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

fn catalog_v2_schema_ready(conn: &Connection) -> Result<bool> {
    let tables: BTreeSet<String> = table_names_of(conn)?.into_iter().collect();
    if CATALOG_V2_REQUIRED_TABLES
        .iter()
        .any(|table| !tables.contains(*table))
    {
        return Ok(false);
    }
    for (table, required_columns) in [
        (
            "cli_installations",
            &[
                "installation_handle",
                "agent_type_id",
                "executable_path",
                "canonical_path",
                "actual_version",
                "source",
                "scope",
                "health",
                "receipt_handle",
                "detected_at",
            ][..],
        ),
        (
            "provider_profiles",
            &[
                "profile_handle",
                "provider_type_id",
                "name",
                "base_url",
                "secret_ref",
                "config_json",
                "revision",
                "created_at",
                "updated_at",
            ][..],
        ),
        (
            "command_receipt",
            &[
                "command_id",
                "semantic_digest",
                "aggregate_handle",
                "result_revisions",
                "state",
                "created_at",
                "finalized_at",
            ][..],
        ),
        (
            "projection_outbox",
            &["outbox_id", "event_json", "published_at"][..],
        ),
        (
            "migration_marker",
            &[
                "marker",
                "source_schema_version",
                "source_digest",
                "imported_counts_json",
                "completed_at",
            ][..],
        ),
        (
            "agent_instances",
            &[
                "instance_key",
                "name",
                "agent_type",
                "scope",
                "project_key",
                "current_version",
                "enabled",
                "created_at",
                "updated_at",
            ][..],
        ),
        (
            "agent_instance_versions",
            &["instance_key", "version", "config_json", "created_at"][..],
        ),
        (
            "workflow_templates",
            &[
                "template_key",
                "name",
                "current_version",
                "task_local",
                "created_at",
                "updated_at",
            ][..],
        ),
        (
            "workflow_template_versions",
            &["template_key", "version", "graph_json", "created_at"][..],
        ),
        (
            "secret_refs",
            &["secret_key", "store_id", "created_at", "updated_at"][..],
        ),
        (
            "plugin_pins",
            &[
                "run_key",
                "full_id",
                "version",
                "content_hash",
                "created_at",
            ][..],
        ),
    ] {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns: BTreeSet<String> = stmt
            .query_map([], |row| row.get(1))?
            .collect::<std::result::Result<_, _>>()?;
        let expected: BTreeSet<String> = required_columns
            .iter()
            .map(|column| (*column).to_string())
            .collect();
        if columns != expected {
            return Ok(false);
        }
    }
    for object in [
        "idx_cli_installations_agent_type",
        "idx_catalog_v2_plugin_pins_hash",
        "installation_receipts_immutable_update",
        "installation_receipts_immutable_delete",
        "installation_receipts_immutable_reinsert",
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
            params![object],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(false);
        }
    }
    Ok(true)
}

const CATALOG_REQUIRED_TABLES: &[&str] = &[
    "agent_instances",
    "agent_instance_versions",
    "workflow_templates",
    "workflow_template_versions",
    "sealed_secrets",
    "plugin_packages",
    "plugin_pins",
];
const CATALOG_REQUIRED_INDEXES: &[&str] = &["idx_plugin_pins_hash"];
const CATALOG_COLUMN_REPAIRS: &[(&str, &str)] = &[
    (
        "enabled",
        "ALTER TABLE agent_instances ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1",
    ),
    (
        "project_key",
        "ALTER TABLE agent_instances ADD COLUMN project_key TEXT",
    ),
    (
        "task_local",
        "ALTER TABLE workflow_templates ADD COLUMN task_local INTEGER NOT NULL DEFAULT 0",
    ),
];

/// v1/v2 当前都从 user_version=1 起步，因此必须用 v2 独有地基表区分
/// 两条版本链。检查发生在 ACL、pragma 与任何 repair DDL 之前。
fn reject_catalog_v2_layout(conn: &Connection) -> Result<()> {
    const V2_DISCRIMINATORS: &[&str] = &[
        "agent_type_catalog",
        "cli_installations",
        "provider_profiles",
        "migration_marker",
    ];
    let tables: BTreeSet<String> = table_names_of(conn)?.into_iter().collect();
    anyhow::ensure!(
        !V2_DISCRIMINATORS.iter().all(|name| tables.contains(*name)),
        "catalog_schema_kind_mismatch:拒绝把 Catalog v2 当作 v1 打开"
    );
    Ok(())
}

fn catalog_schema_needs_repair(conn: &Connection) -> Result<bool> {
    for (kind, names) in [
        ("table", CATALOG_REQUIRED_TABLES),
        ("index", CATALOG_REQUIRED_INDEXES),
    ] {
        for name in names {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2)",
                params![kind, name],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(true);
            }
        }
    }
    Ok(!missing_catalog_column_repairs(conn)?.is_empty())
}

fn missing_catalog_column_repairs(conn: &Connection) -> Result<Vec<&'static str>> {
    let mut missing = Vec::new();
    for (column, alter) in CATALOG_COLUMN_REPAIRS {
        let table = alter
            .split_whitespace()
            .nth(2)
            .unwrap_or_default()
            .trim_end_matches(';');
        let existing: Vec<String> = {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let cols = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|c| c.ok())
                .collect();
            cols
        };
        if !existing.iter().any(|c| c == column) {
            missing.push(*alter);
        }
    }
    Ok(missing)
}

/// 补齐旧开发库缺失的列(CREATE IF NOT EXISTS 不会补列;幂等)。
fn ensure_agent_instances_columns(conn: &Connection) -> Result<()> {
    for alter in missing_catalog_column_repairs(conn)? {
        conn.execute(alter, [])
            .with_context(|| format!("补齐目录库列失败:{alter}"))?;
    }
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// 稳定实例键:时间 + 计数器哈希(与 `store::gen_capability_token` 同风格)。
fn gen_instance_key() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h1 = std::collections::hash_map::RandomState::new().build_hasher();
    let mut h2 = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    h1.write_u64(nanos);
    h1.write_u64(n);
    h2.write_u64(nanos ^ 0x9e37_79b9_7f4a_7c15);
    h2.write_u64(n);
    format!("inst_{:016x}{:016x}", h1.finish(), h2.finish())
}

/// 版本行内容(敏感字段不落明文;payload 整体序列化进 config_json)。
#[derive(serde::Serialize)]
struct VersionPayload<'a> {
    name: &'a str,
    agent_type: &'a str,
    run_mode: &'a crate::model::RunMode,
    executable: &'a str,
    argv: &'a [String],
    env: &'a [(String, String)],
    config: &'a serde_json::Value,
    execution_contract: &'a serde_json::Value,
    sealed_secret_ids: &'a [String],
}

fn insert_version_row(
    tx: &rusqlite::Transaction,
    instance_rowid: i64,
    version: i64,
    draft: &AgentInstanceDraft,
    ts: &str,
) -> Result<()> {
    let payload = VersionPayload {
        name: &draft.name,
        agent_type: &draft.agent_type,
        run_mode: &draft.run_mode,
        executable: &draft.executable,
        argv: &draft.argv,
        env: &draft.env,
        config: &draft.config,
        execution_contract: &draft.execution_contract,
        sealed_secret_ids: &draft.sealed_secret_ids,
    };
    tx.execute(
        "INSERT INTO agent_instance_versions (instance_id, version, config_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            instance_rowid,
            version,
            serde_json::to_string(&payload)?,
            ts
        ],
    )?;
    Ok(())
}

/// 版本行 FK 指向整数 rowid(`agent_instances.id`);
/// 对外仍以稳定字符串键(instance_key)标识实例。
fn instance_rowid(c: &Connection, id: &str) -> Result<Option<i64>> {
    c.query_row(
        "SELECT id FROM agent_instances WHERE instance_key = ?1",
        params![id],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn instance_row(c: &Connection, id: &str) -> Result<Option<AgentInstance>> {
    c.query_row(
        "SELECT instance_key, name, agent_type, scope, project_key, current_version, enabled
         FROM agent_instances WHERE instance_key = ?1",
        params![id],
        |r| {
            Ok(AgentInstance {
                id: r.get(0)?,
                name: r.get(1)?,
                agent_type: r.get(2)?,
                scope: InstanceScope::parse(&r.get::<_, String>(3)?).unwrap_or(InstanceScope::User),
                project_key: r.get(4)?,
                current_version: r.get(5)?,
                enabled: r.get::<_, i64>(6)? != 0,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
fn version_row(
    c: &Connection,
    instance_rowid: i64,
    version: i64,
    instance_key: &str,
) -> Result<Option<AgentInstanceVersion>> {
    let row: Option<(String, String)> = c
        .query_row(
            "SELECT config_json, created_at FROM agent_instance_versions
             WHERE instance_id = ?1 AND version = ?2",
            params![instance_rowid, version],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((json, created_at)) => {
            let mut v: AgentInstanceVersion = serde_json::from_str(&json).with_context(|| {
                format!("Agent Instance `{instance_key}` 版本 {version} 行损坏")
            })?;
            v.instance_id = instance_key.to_string();
            v.version = version;
            v.created_at = created_at;
            Ok(Some(v))
        }
        None => Ok(None),
    }
}
