//! 目录库:用户级 Agent Instance、工作流模板、加密 Secret 与插件包记录。
//!
//! 与项目库(`store::Store`)相互独立;一个 MonkeyFence 进程只打开一个目录库,
//! 默认位于 `~/.monkeyfence/catalog-v1.db`。本阶段只提供 schema 与连接管理,
//! 读写 API 随后续里程碑补全。

use crate::schema::{
    catalog_db_path, initialize_schema, schema_version_of, table_names_of, CATALOG_SCHEMA_V1,
    CATALOG_SCHEMA_VERSION,
};
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Arc;

pub struct CatalogStore {
    conn: Mutex<Connection>,
}

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

    fn init(conn: Connection) -> Result<CatalogStore> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        initialize_schema(&conn, CATALOG_SCHEMA_V1, CATALOG_SCHEMA_VERSION)
            .context("初始化目录库 v1 schema 失败")?;
        Ok(CatalogStore {
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
}
