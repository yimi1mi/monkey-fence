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
use rusqlite::Connection;
use std::path::Path;
use std::sync::Arc;

pub struct CatalogStore {
    conn: Mutex<Connection>,
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
}
