//! Project Registry:service-v1.db 的访问层(canonical spec §3.4/§3.5/§3.8)。
//!
//! T1d 交付:库打开(future guard fail-closed + 当前用户 ACL)与
//! session.json → `project_registry` 幂等导入。command_intent / operation /
//! audit 的写路径属后续 ticket;本层不接管 `crates/mf` AppCtx 的权威项目
//! 列表——GPUI 会话状态仍是 AppCtx 的事实源,service 库当前是 dark data。

use crate::platform_acl::restrict_service_database_to_current_user;
use crate::service_schema::{
    guard_future_version, schema_version_of, seed_singletons, service_db_path,
    service_schema_ready, service_schema_v1_ready, table_names_of, validate_singletons,
    SERVICE_SCHEMA_V1, SERVICE_SCHEMA_V2_DELTA, SERVICE_SCHEMA_VERSION,
};
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// session.json → Project Registry 导入的幂等 marker 名。
pub const SESSION_IMPORT_MARKER: &str = "session_json.project_registry.v1";

/// 已登记 Project 的读视图(`project_registry` 行)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredProject {
    pub project_handle: String,
    pub public_id: String,
    pub canonical_root: String,
    pub display_path: String,
    pub registered_at: String,
    pub status: ProjectStatus,
}

/// §3.4:`status(registered|missing)`;`missing` 保留供用户清理,不删目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    Registered,
    Missing,
}

impl ProjectStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Missing => "missing",
        }
    }
}

/// 一次 session.json 导入的结果(重复调用与中断重跑见 [`SessionImportStatus`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionImportReport {
    pub status: SessionImportStatus,
    /// 本次实际新插入的行数(已存在的 canonical_root 不重复计入)。
    pub imported: usize,
    /// session.json 内部因 canonical root 相同被合并的条目数。
    pub duplicates_skipped: usize,
    /// 状态为 `missing` 的条目数(路径不存在,保留待用户清理)。
    pub missing: usize,
    /// 可用 foreground(路径存在且已登记)的 canonical root。
    pub foreground: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionImportStatus {
    /// 本次完成导入(同一事务写入 registry 行与 marker)。
    Imported,
    /// marker 已存在:导入一次性完成,重复调用是 no-op。
    AlreadyImported,
    /// session.json 不存在:无迁移对象,不写 marker。
    NoSessionFile,
}

/// service 库访问入口。
pub struct ServiceStore {
    conn: Mutex<Connection>,
    /// 同一 Core 内跨 CommandCoordinator 串行 reserve→L-CMD→finalize 与
    /// startup reconcile；跨进程唯一性由 #20 CoreOwnerLock 保证。
    command_gate: Mutex<()>,
}

/// session.json 中与本迁移相关的字段;`open_files`、active file、
/// selected_task_id、GPUI panel/layout 一律不读(§3.5 非迁移目标,
/// serde 默认忽略未知字段),旧格式(只有 projects/foreground)同样可解析。
#[derive(Debug, serde::Deserialize)]
struct SessionDoc {
    #[serde(default)]
    projects: Vec<PathBuf>,
    #[serde(default)]
    foreground: Option<PathBuf>,
}

impl ServiceStore {
    pub fn open(path: &Path) -> Result<Arc<ServiceStore>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 service 库所在目录失败: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开 service 库失败: {}", path.display()))?;
        Self::init(conn).map(Arc::new)
    }

    /// 默认路径(`~/.monkeyfence/service-v1.db`,可用 `MF_SERVICE_DB` 重定向)。
    pub fn open_default() -> Result<Arc<ServiceStore>> {
        Self::open(&service_db_path())
    }

    fn init(mut conn: Connection) -> Result<ServiceStore> {
        // future guard 先于任何 DDL/pragma:高版本库 fail-closed 且不留痕迹。
        guard_future_version(&conn, SERVICE_SCHEMA_VERSION)?;
        let current = schema_version_of(&conn)?;
        if current == SERVICE_SCHEMA_VERSION && !service_schema_ready(&conn)? {
            anyhow::bail!("service_schema_mismatch:文件标记为 service-v1，但 schema 指纹不完整");
        }
        if current == 1 && !service_schema_v1_ready(&conn)? {
            anyhow::bail!("service_schema_mismatch:文件标记为旧 service v1，但 schema 指纹不完整");
        }
        restrict_service_database_to_current_user(&conn)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        if current < SERVICE_SCHEMA_VERSION {
            // v0 全新初始化无需备份；v1→v2 必须先走统一 SQLite Backup
            // 屏障，再在单事务加 problem_code 并更新 meta/user_version。
            mf_agent::migration::upgrade_with_barrier(
                &mut conn,
                mf_agent::migration::StoreKind::Service,
                SERVICE_SCHEMA_VERSION,
                &|tx, from, to| {
                    if from < 1 && to >= 1 {
                        tx.execute_batch(SERVICE_SCHEMA_V1)?;
                        seed_singletons(tx)?;
                    }
                    if from < 2 && to >= 2 {
                        tx.execute_batch(SERVICE_SCHEMA_V2_DELTA)?;
                    }
                    tx.execute(
                        "UPDATE meta SET schema_version=?1 WHERE id=1",
                        [SERVICE_SCHEMA_VERSION],
                    )?;
                    anyhow::ensure!(
                        service_schema_ready(tx)?,
                        "service_schema_mismatch:迁移后的 schema 指纹不完整"
                    );
                    validate_singletons(tx)
                },
            )?;
        }

        anyhow::ensure!(
            service_schema_ready(&conn)?,
            "service_schema_mismatch:service-v1 schema 指纹不完整"
        );
        validate_singletons(&conn)?;

        // 持久 WAL 只在初始化成功后启用;sidecar 一并收紧 ACL。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        restrict_service_database_to_current_user(&conn)?;
        log::info!(
            "store_open store=service schema_version={} tables={}",
            schema_version_of(&conn)?,
            table_names_of(&conn)?.len()
        );
        Ok(ServiceStore {
            conn: Mutex::new(conn),
            command_gate: Mutex::new(()),
        })
    }

    pub fn schema_version(&self) -> Result<i64> {
        schema_version_of(&self.conn.lock())
    }

    #[allow(dead_code)] // T1 command recovery reader; wired by T2 facade.
    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock();
        f(&conn)
    }

    /// CoreOwnerLock/CommandCoordinator 在 service DB 内推进 owner epoch、
    /// intent 与 terminal problem 的原子事务缝隙。
    pub(crate) fn with_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    pub(crate) fn command_gate(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.command_gate.lock()
    }

    /// 全部已登记 Project(按 canonical_root 排序,确定性读序)。
    pub fn list_projects(&self) -> Result<Vec<RegisteredProject>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT project_handle, public_id, canonical_root, display_path,
                    registered_at, status
             FROM project_registry ORDER BY canonical_root",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(RegisteredProject {
                    project_handle: row.get(0)?,
                    public_id: row.get(1)?,
                    canonical_root: row.get(2)?,
                    display_path: row.get(3)?,
                    registered_at: row.get(4)?,
                    status: parse_status(&row.get::<_, String>(5)?)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// session.json → `project_registry` 幂等导入(§3.5)。
    ///
    /// - 只导入 Project 列表与可用 foreground;`open_files`、active file、
    ///   GPUI panel/layout 不迁移;原 session.json 只读,字节不变。
    /// - canonical root 去重(相同目录的多种拼写合并为一行,首个拼写作为
    ///   `display_path`);缺失路径登记为 `missing`,不创建也不删除目录。
    /// - registry 行与 marker 在同一事务提交:任意步失败保持未迁移
    ///   (§3.6),旧数据不动;marker 已存在时是 no-op;rows 先落、marker
    ///   未写的崩溃残留由 `ON CONFLICT DO NOTHING` 在重跑时收敛(既有行
    ///   的 handle/registered_at 不变)。
    pub fn import_session_projects(&self, session_json: &Path) -> Result<SessionImportReport> {
        let mut conn = self.conn.lock();
        if marker_exists(&conn, SESSION_IMPORT_MARKER)? {
            return Ok(already_imported_report());
        }
        let bytes = match std::fs::read(session_json) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SessionImportReport {
                    status: SessionImportStatus::NoSessionFile,
                    imported: 0,
                    duplicates_skipped: 0,
                    missing: 0,
                    foreground: None,
                });
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "读取 session.json 失败(不写 marker,保持未迁移): {}:{error}",
                    session_json.display()
                ));
            }
        };
        let doc: SessionDoc = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "session.json 解析失败(不写 marker,保持未迁移): {}",
                session_json.display()
            )
        })?;

        // 条目 = Project 列表(内部去重)+ 可用 foreground(§3.5:可用即
        // 登记;已与列表中某条目同目录或自身不可用时不再产生额外条目)。
        struct Entry {
            canonical_root: String,
            display_path: String,
            registered: bool,
        }
        let mut entries: Vec<Entry> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for path in &doc.projects {
            let (canonical, warning) = canonical_root_of(path);
            let registered = warning.is_none();
            if let Some(warning) = warning {
                log::warn!("session_import {warning}");
            }
            let key = canonical.to_string_lossy().into_owned();
            if !index.contains_key(&key) {
                index.insert(key.clone(), entries.len());
                entries.push(Entry {
                    canonical_root: key,
                    display_path: path.to_string_lossy().into_owned(),
                    registered,
                });
            }
        }
        let duplicates_skipped = doc.projects.len() - entries.len();
        if let Some(foreground) = &doc.foreground {
            let (canonical, warning) = canonical_root_of(foreground);
            let available = warning.is_none();
            if let Some(warning) = warning {
                log::warn!("session_import foreground {warning}");
            }
            let key = canonical.to_string_lossy().into_owned();
            if available && !index.contains_key(&key) {
                index.insert(key.clone(), entries.len());
                entries.push(Entry {
                    canonical_root: key,
                    display_path: foreground.to_string_lossy().into_owned(),
                    registered: true,
                });
            }
        }
        let missing = entries.iter().filter(|entry| !entry.registered).count();
        let foreground = doc
            .foreground
            .as_ref()
            .map(|path| canonical_root_of(path).0.to_string_lossy().into_owned())
            .filter(|key| {
                entries
                    .iter()
                    .any(|entry| &entry.canonical_root == key && entry.registered)
            });

        let now = chrono::Utc::now().to_rfc3339();
        let mut imported = 0usize;
        // 多连接/多进程可能同时通过事务外的快速 marker 检查。IMMEDIATE
        // writer lock 后必须重查，确保后到者收敛为幂等 no-op，而不是在
        // marker 主键或 busy 上报错。
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if marker_exists(&tx, SESSION_IMPORT_MARKER)? {
            return Ok(already_imported_report());
        }
        for entry in &entries {
            // 已有同 canonical root 的行(此前导入的崩溃残留)保持原样:
            // handle 与 registered_at 永不改写。
            imported += tx
                .execute(
                    "INSERT INTO project_registry
                         (project_handle, public_id, canonical_root, display_path,
                          registered_at, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(canonical_root) DO NOTHING",
                    params![
                        new_project_handle(),
                        new_public_id(),
                        entry.canonical_root,
                        entry.display_path,
                        now,
                        if entry.registered {
                            ProjectStatus::Registered.as_str()
                        } else {
                            ProjectStatus::Missing.as_str()
                        }
                    ],
                )
                .context("写入 project_registry 失败")?;
        }
        let payload = serde_json::json!({
            "source": session_json
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session.json".to_string()),
            "imported": imported,
            "duplicates_skipped": duplicates_skipped,
            "missing": missing,
            "foreground": foreground,
        });
        tx.execute(
            "INSERT INTO migration_marker (name, payload_json, created_at) VALUES (?1, ?2, ?3)",
            params![
                SESSION_IMPORT_MARKER,
                payload.to_string(),
                chrono::Utc::now().to_rfc3339()
            ],
        )
        .context("写入 migration_marker 失败")?;
        tx.commit()?;
        log::info!(
            "session_import imported={imported} duplicates_skipped={duplicates_skipped} missing={missing} foreground={}",
            foreground.as_deref().unwrap_or("<none>")
        );
        Ok(SessionImportReport {
            status: SessionImportStatus::Imported,
            imported,
            duplicates_skipped,
            missing,
            foreground,
        })
    }
}

fn already_imported_report() -> SessionImportReport {
    SessionImportReport {
        status: SessionImportStatus::AlreadyImported,
        imported: 0,
        duplicates_skipped: 0,
        missing: 0,
        foreground: None,
    }
}

fn marker_exists(conn: &Connection, name: &str) -> Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM migration_marker WHERE name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?;
    Ok(exists.is_some())
}

/// `project_registry.status` 解析:未知值 fail-closed(不静默归入 registered)。
fn parse_status(raw: &str) -> std::result::Result<ProjectStatus, rusqlite::Error> {
    match raw {
        "registered" => Ok(ProjectStatus::Registered),
        "missing" => Ok(ProjectStatus::Missing),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(format!(
                "未知 project_registry.status: {other}"
            ))),
        )),
    }
}

/// 生成 Project 的持久 opaque handle(§7.1:`proj_` + UUIDv7 前缀风格)。
/// handle 永不复用、不得由 rowid/路径派生。
fn new_project_handle() -> String {
    format!("proj_{}", uuid::Uuid::now_v7())
}

/// `public_id`:spec §3.4 列出该列但未定义语义;取独立 UUIDv7(与 handle
/// 分离、UNIQUE),供未来对外展示/引用,不复用 handle。
fn new_public_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 去掉 Windows canonicalize 产生的 `\\?\` 扩展长度前缀(仅普通盘符路径)。
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        // 只处理 `\\?\C:\...` 形式;其他(命名管道等)原样保留
        if rest
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic())
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// 规范化 Project root:优先 canonicalize(真实大小写/separator 归一),
/// 失败(路径暂不存在)回退词法绝对路径并给出警告。与 `crates/mf` 的
/// `normalize_project_path` 同一口径,保证 GPUI 会话与 service registry
/// 对同一目录产生相同 canonical 字符串;不存在路径的回退口径是
/// `std::path::absolute`(不做大小写折叠,Unix 上大小写敏感)。
pub fn canonical_root_of(path: &Path) -> (PathBuf, Option<String>) {
    match std::fs::canonicalize(path) {
        Ok(canonical) => (strip_verbatim(&canonical), None),
        Err(error) => {
            let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
            let warning = format!(
                "路径无法规范化,使用绝对路径: {}(canonicalize 失败: {error})",
                absolute.display()
            );
            (strip_verbatim(&absolute), Some(warning))
        }
    }
}
