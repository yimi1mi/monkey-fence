//! Project Registry:service-v1.db 的访问层(canonical spec §3.4/§3.5/§3.8)。
//!
//! T1d 交付:库打开(future guard fail-closed + 当前用户 ACL)与
//! session.json → `project_registry` 幂等导入。T1f/T1g 已增加 command /
//! Operation/reconcile seam；T2a runtime 已接管 Project Store 打开与
//! workflow.rename tracer 的 registry/command 链。audit 生产写路径与其余
//! legacy mutation 随后续 T2 tickets 迁移。

use crate::platform_acl::restrict_service_database_to_current_user;
use crate::service_schema::{
    guard_future_version, schema_version_of, seed_singletons, service_db_path,
    service_schema_ready, service_schema_v1_ready, service_schema_v2_ready,
    service_schema_v3_ready, table_names_of, validate_singletons, SERVICE_SCHEMA_V1,
    SERVICE_SCHEMA_V2_DELTA, SERVICE_SCHEMA_V3_DELTA, SERVICE_SCHEMA_V4_DELTA,
    SERVICE_SCHEMA_VERSION,
};
use anyhow::{Context as _, Result};
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

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

/// capability authority 的持久状态。`settled` / `revoked` 都是终态；
/// `quarantined` 表示曾观察到冲突，resolve 必须稳定归类为多命中。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCapabilityState {
    Active,
    Settled,
    Revoked,
    Quarantined,
}

impl RunCapabilityState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Settled => "settled",
            Self::Revoked => "revoked",
            Self::Quarantined => "quarantined",
        }
    }
}

/// resolve 对外只返回 opaque handles 与状态；token HMAC 也不离开 authority。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCapability {
    pub project: crate::handles::ProjectStoreHandle,
    pub agent_run: crate::handles::AgentRunHandle,
    pub state: RunCapabilityState,
    pub issued_at: String,
    pub revoked_at: Option<String>,
}

/// 稳定的零/一/多分类。数据库用 quarantine tombstone 表示冲突，避免
/// 重启后重新猜测某一条映射是 winner。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunCapabilityResolution {
    Zero,
    One(RunCapability),
    Many,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunCapabilityRegistration {
    Registered(RunCapability),
    Quarantined,
}

/// capability token 的独立 domain key。密钥只在进程内以 Zeroizing 保存，
/// 不实现 Debug/Display/Serialize；生产值位于当前用户 OS keyring。
#[derive(Clone)]
pub struct RunCapabilityKey(Zeroizing<Vec<u8>>);

impl RunCapabilityKey {
    /// 生产入口：独立于 command digest 的 domain key，首次生成 256 bit。
    pub fn load_or_create() -> Result<Self> {
        const SERVICE: &str = "MonkeyFence";
        const ACCOUNT: &str = "service-run-capability-v1";
        static INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = INIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = keyring::Entry::new(SERVICE, ACCOUNT)
            .context("创建 run capability keyring entry 失败")?;
        let key = match entry.get_password() {
            Ok(encoded) => decode_key_hex(&Zeroizing::new(encoded))?,
            Err(keyring::Error::NoEntry) => {
                let mut key = Zeroizing::new(vec![0_u8; 32]);
                rand::thread_rng().fill_bytes(&mut key);
                let encoded = Zeroizing::new(encode_hex(&key));
                entry
                    .set_password(&encoded)
                    .context("保存 run capability keyring 失败")?;
                key
            }
            Err(error) => return Err(anyhow::anyhow!("读取 run capability keyring 失败:{error}")),
        };
        Self::from_bytes(key)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(bytes: Vec<u8>) -> Result<Self> {
        Self::from_bytes(Zeroizing::new(bytes))
    }

    fn from_bytes(bytes: Zeroizing<Vec<u8>>) -> Result<Self> {
        anyhow::ensure!(bytes.len() >= 32, "run capability key 至少 256 bit");
        Ok(Self(bytes))
    }

    fn token_hmac(&self, token: &[u8]) -> Result<String> {
        anyhow::ensure!(!token.is_empty(), "run capability token 不能为空");
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0)
            .map_err(|_| anyhow::anyhow!("run capability HMAC 初始化失败"))?;
        mac.update(b"MonkeyFence/run-capability/v1\0");
        mac.update(token);
        Ok(encode_hex(&mac.finalize().into_bytes()))
    }
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
        if current == 2 && !service_schema_v2_ready(&conn)? {
            anyhow::bail!("service_schema_mismatch:文件标记为旧 service v2，但 schema 指纹不完整");
        }
        if current == 3 && !service_schema_v3_ready(&conn)? {
            anyhow::bail!("service_schema_mismatch:文件标记为旧 service v3，但 schema 指纹不完整");
        }
        restrict_service_database_to_current_user(&conn)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        if current < SERVICE_SCHEMA_VERSION {
            // v0 全新初始化无需备份；v1→v2/v2→v3 必须先走统一 SQLite
            // Backup 屏障，再在单事务应用 delta 并更新 meta/user_version。
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
                    if from < 3 && to >= 3 {
                        tx.execute_batch(SERVICE_SCHEMA_V3_DELTA)?;
                    }
                    if from < 4 && to >= 4 {
                        tx.execute_batch(SERVICE_SCHEMA_V4_DELTA)?;
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

    /// 注册 token → (Project, Agent Run) authority 映射。
    ///
    /// 完全相同的 token→(project,run) 是 crash/restart 安全的幂等重放；
    /// token 映射到不同 pair、pair 映射到不同 token 或相同 run 跨
    /// Project 才会在同一 IMMEDIATE 事务内 quarantine 冲突映射。
    pub fn register_run_capability(
        &self,
        key: &RunCapabilityKey,
        token: &[u8],
        project: &crate::handles::ProjectStoreHandle,
        agent_run: &crate::handles::AgentRunHandle,
    ) -> Result<RunCapabilityRegistration> {
        let token_hmac = key.token_hmac(token)?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure_capability_project(&tx, project, true)?;

        let by_token = capability_identity_by_token(&tx, &token_hmac)?;
        let by_pair = capability_identity_by_pair(&tx, project.as_str(), agent_run.as_str())?;
        let cross_project: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM run_capability
                 WHERE agent_run_handle=?1 AND project_handle<>?2
             )",
            params![agent_run.as_str(), project.as_str()],
            |row| row.get(0),
        )?;

        let exact = by_token.as_ref().is_some_and(|identity| {
            identity.project_handle == project.as_str()
                && identity.agent_run_handle == agent_run.as_str()
        }) && by_pair
            .as_ref()
            .is_some_and(|identity| identity.token_hmac == token_hmac);
        if exact && !cross_project {
            let capability = capability_by_token_hmac(&tx, &token_hmac)?
                .ok_or_else(|| anyhow::anyhow!("run_capability_inconsistent:幂等行消失"))?;
            tx.commit()?;
            return Ok(if capability.state == RunCapabilityState::Quarantined {
                RunCapabilityRegistration::Quarantined
            } else {
                RunCapabilityRegistration::Registered(capability)
            });
        }

        let conflict = by_token.is_some() || by_pair.is_some() || cross_project;
        if conflict {
            quarantine_capability_conflicts(
                &tx,
                &token_hmac,
                project.as_str(),
                agent_run.as_str(),
                &now,
            )?;
            // 仅 cross-project 冲突时 token 与 pair 仍均可插入；保存一条
            // quarantined tombstone，让该 token 重启后继续解析为 Many。
            if by_token.is_none() && by_pair.is_none() {
                tx.execute(
                    "INSERT INTO run_capability
                         (token_hmac, project_handle, agent_run_handle, state, issued_at, revoked_at)
                     VALUES (?1, ?2, ?3, 'quarantined', ?4, ?4)",
                    params![token_hmac, project.as_str(), agent_run.as_str(), now],
                )?;
            }
            tx.commit()?;
            return Ok(RunCapabilityRegistration::Quarantined);
        }

        tx.execute(
            "INSERT INTO run_capability
                 (token_hmac, project_handle, agent_run_handle, state, issued_at, revoked_at)
             VALUES (?1, ?2, ?3, 'active', ?4, NULL)",
            params![token_hmac, project.as_str(), agent_run.as_str(), now],
        )?;
        let capability = capability_by_token_hmac(&tx, &token_hmac)?
            .ok_or_else(|| anyhow::anyhow!("run_capability_inconsistent:注册行消失"))?;
        tx.commit()?;
        Ok(RunCapabilityRegistration::Registered(capability))
    }

    /// token resolve 的稳定零/一/多分类。quarantined tombstone 永远返回
    /// Many；settled/revoked 仍返回 One + terminal state，调用方不可复活。
    pub fn resolve_run_capability(
        &self,
        key: &RunCapabilityKey,
        token: &[u8],
    ) -> Result<RunCapabilityResolution> {
        let token_hmac = key.token_hmac(token)?;
        resolve_capability_by_hmac(&self.conn.lock(), &token_hmac)
    }

    /// 成功结算后封存 capability；只允许 active→settled，重复调用保持
    /// 原终态并返回稳定 resolve 结果。
    pub fn settle_run_capability(
        &self,
        key: &RunCapabilityKey,
        token: &[u8],
    ) -> Result<RunCapabilityResolution> {
        self.transition_run_capability(key, token, RunCapabilityState::Settled)
    }

    /// 单 token 主动撤销；只允许 active→revoked。
    pub fn revoke_run_capability(
        &self,
        key: &RunCapabilityKey,
        token: &[u8],
    ) -> Result<RunCapabilityResolution> {
        self.transition_run_capability(key, token, RunCapabilityState::Revoked)
    }

    /// Project closing/unregister 的批量 fence。返回本次 active→revoked
    /// 行数；settled/quarantined 等终态不改写。
    pub fn revoke_project_run_capabilities(
        &self,
        project: &crate::handles::ProjectStoreHandle,
    ) -> Result<usize> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE run_capability
             SET state='revoked', revoked_at=?2
             WHERE project_handle=?1 AND state='active'",
            params![project.as_str(), now],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// 迁移旧映射时一律 quarantine，绝不把未经 authority 注册的 token
    /// 直接激活。冲突行与 tombstone 的处理和正常注册共用同一规则。
    pub fn backfill_quarantined_run_capability(
        &self,
        key: &RunCapabilityKey,
        token: &[u8],
        project: &crate::handles::ProjectStoreHandle,
        agent_run: &crate::handles::AgentRunHandle,
    ) -> Result<()> {
        let token_hmac = key.token_hmac(token)?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure_capability_project(&tx, project, false)?;
        quarantine_capability_conflicts(
            &tx,
            &token_hmac,
            project.as_str(),
            agent_run.as_str(),
            &now,
        )?;
        tx.execute(
            "INSERT INTO run_capability
                 (token_hmac, project_handle, agent_run_handle, state, issued_at, revoked_at)
             VALUES (?1, ?2, ?3, 'quarantined', ?4, ?4)
             ON CONFLICT DO NOTHING",
            params![token_hmac, project.as_str(), agent_run.as_str(), now],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn transition_run_capability(
        &self,
        key: &RunCapabilityKey,
        token: &[u8],
        target: RunCapabilityState,
    ) -> Result<RunCapabilityResolution> {
        anyhow::ensure!(
            matches!(
                target,
                RunCapabilityState::Settled | RunCapabilityState::Revoked
            ),
            "run capability 只允许进入 settled/revoked"
        );
        let token_hmac = key.token_hmac(token)?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE run_capability SET state=?2, revoked_at=?3
             WHERE token_hmac=?1 AND state='active'",
            params![token_hmac, target.as_str(), now],
        )?;
        let resolution = resolve_capability_by_hmac(&tx, &token_hmac)?;
        tx.commit()?;
        Ok(resolution)
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

    /// 运行时显式登记 Project。canonical root 唯一，重复打开复用既有
    /// opaque handle；先前 missing 的路径恢复后只推进 status，不更换 handle。
    pub fn register_project_path(&self, path: &Path) -> Result<RegisteredProject> {
        let (canonical, warning) = canonical_root_of(path);
        if let Some(warning) = warning {
            anyhow::bail!("project_register_failed:{warning}");
        }
        let canonical_root = canonical.to_string_lossy().into_owned();
        let display_path = path.to_string_lossy().into_owned();
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO project_registry
                 (project_handle, public_id, canonical_root, display_path, registered_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'registered')
             ON CONFLICT(canonical_root) DO UPDATE SET status='registered'",
            params![
                new_project_handle(),
                new_public_id(),
                canonical_root,
                display_path,
                now,
            ],
        )?;
        let project = tx.query_row(
            "SELECT project_handle, public_id, canonical_root, display_path,
                    registered_at, status
             FROM project_registry WHERE canonical_root=?1",
            [&canonical_root],
            |row| {
                Ok(RegisteredProject {
                    project_handle: row.get(0)?,
                    public_id: row.get(1)?,
                    canonical_root: row.get(2)?,
                    display_path: row.get(3)?,
                    registered_at: row.get(4)?,
                    status: parse_status(&row.get::<_, String>(5)?)?,
                })
            },
        )?;
        tx.commit()?;
        Ok(project)
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

struct CapabilityIdentity {
    token_hmac: String,
    project_handle: String,
    agent_run_handle: String,
}

fn capability_identity_by_token(
    conn: &Connection,
    token_hmac: &str,
) -> Result<Option<CapabilityIdentity>> {
    Ok(conn
        .query_row(
            "SELECT token_hmac, project_handle, agent_run_handle
             FROM run_capability WHERE token_hmac=?1",
            [token_hmac],
            |row| {
                Ok(CapabilityIdentity {
                    token_hmac: row.get(0)?,
                    project_handle: row.get(1)?,
                    agent_run_handle: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn capability_identity_by_pair(
    conn: &Connection,
    project_handle: &str,
    agent_run_handle: &str,
) -> Result<Option<CapabilityIdentity>> {
    Ok(conn
        .query_row(
            "SELECT token_hmac, project_handle, agent_run_handle
             FROM run_capability
             WHERE project_handle=?1 AND agent_run_handle=?2",
            params![project_handle, agent_run_handle],
            |row| {
                Ok(CapabilityIdentity {
                    token_hmac: row.get(0)?,
                    project_handle: row.get(1)?,
                    agent_run_handle: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn capability_by_token_hmac(conn: &Connection, token_hmac: &str) -> Result<Option<RunCapability>> {
    let raw: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT project_handle, agent_run_handle, state, issued_at, revoked_at
             FROM run_capability WHERE token_hmac=?1",
            [token_hmac],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    raw.map(|(project, agent_run, state, issued_at, revoked_at)| {
        Ok(RunCapability {
            project: crate::handles::ProjectStoreHandle::parse(project)
                .context("run_capability project_handle 损坏")?,
            agent_run: crate::handles::AgentRunHandle::parse(agent_run)
                .context("run_capability agent_run_handle 损坏")?,
            state: parse_run_capability_state(&state)?,
            issued_at,
            revoked_at,
        })
    })
    .transpose()
}

fn resolve_capability_by_hmac(
    conn: &Connection,
    token_hmac: &str,
) -> Result<RunCapabilityResolution> {
    Ok(match capability_by_token_hmac(conn, token_hmac)? {
        None => RunCapabilityResolution::Zero,
        Some(capability) if capability.state == RunCapabilityState::Quarantined => {
            RunCapabilityResolution::Many
        }
        Some(capability) => RunCapabilityResolution::One(capability),
    })
}

fn ensure_capability_project(
    conn: &Connection,
    project: &crate::handles::ProjectStoreHandle,
    require_registered: bool,
) -> Result<()> {
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM project_registry WHERE project_handle=?1",
            [project.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let status = status.ok_or_else(|| anyhow::anyhow!("run_capability_project_unknown"))?;
    if require_registered {
        anyhow::ensure!(status == "registered", "run_capability_project_inactive");
    }
    Ok(())
}

fn quarantine_capability_conflicts(
    conn: &Connection,
    token_hmac: &str,
    project_handle: &str,
    agent_run_handle: &str,
    now: &str,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE run_capability
         SET state='quarantined', revoked_at=COALESCE(revoked_at, ?4)
         WHERE token_hmac=?1
            OR (project_handle=?2 AND agent_run_handle=?3)
            OR agent_run_handle=?3",
        params![token_hmac, project_handle, agent_run_handle, now],
    )?)
}

fn parse_run_capability_state(raw: &str) -> Result<RunCapabilityState> {
    match raw {
        "active" => Ok(RunCapabilityState::Active),
        "settled" => Ok(RunCapabilityState::Settled),
        "revoked" => Ok(RunCapabilityState::Revoked),
        "quarantined" => Ok(RunCapabilityState::Quarantined),
        _ => anyhow::bail!("run_capability_state_invalid"),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_key_hex(encoded: &str) -> Result<Zeroizing<Vec<u8>>> {
    anyhow::ensure!(encoded.len() % 2 == 0, "run capability keyring 格式非法");
    let mut bytes = Zeroizing::new(Vec::with_capacity(encoded.len() / 2));
    for pair in encoded.as_bytes().chunks_exact(2) {
        let high = decode_hex_nibble(pair[0])?;
        let low = decode_hex_nibble(pair[1])?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn decode_hex_nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => anyhow::bail!("run capability keyring 格式非法"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handles::{AgentRunHandle, ProjectStoreHandle};
    use std::sync::Barrier;

    fn agent_run() -> AgentRunHandle {
        AgentRunHandle::parse(uuid::Uuid::now_v7().to_string()).unwrap()
    }

    fn project(store: &ServiceStore, root: &Path) -> ProjectStoreHandle {
        std::fs::create_dir_all(root).unwrap();
        ProjectStoreHandle::parse(store.register_project_path(root).unwrap().project_handle)
            .unwrap()
    }

    #[test]
    fn capability_authority_persists_only_hmac_and_transitions_terminally() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("service.db");
        let store = ServiceStore::open(&db).unwrap();
        let project = project(&store, &tmp.path().join("project"));
        let run = agent_run();
        let key = RunCapabilityKey::for_test(vec![0x41; 32]).unwrap();
        let token = b"plaintext-capability-must-not-persist";

        let registered = store
            .register_run_capability(&key, token, &project, &run)
            .unwrap();
        assert!(matches!(
            registered,
            RunCapabilityRegistration::Registered(_)
        ));
        assert!(matches!(
            store.resolve_run_capability(&key, token).unwrap(),
            RunCapabilityResolution::One(RunCapability {
                state: RunCapabilityState::Active,
                ..
            })
        ));
        for _ in 0..3 {
            assert!(matches!(
                store
                    .register_run_capability(&key, token, &project, &run)
                    .unwrap(),
                RunCapabilityRegistration::Registered(RunCapability {
                    state: RunCapabilityState::Active,
                    ..
                })
            ));
        }
        let row_count: i64 = store
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM run_capability", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(row_count, 1, "exact replay 必须收敛为原 authority 行");
        let stored_hmac: String = store
            .with_conn(|conn| {
                Ok(
                    conn.query_row("SELECT token_hmac FROM run_capability", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(stored_hmac.len(), 64);
        assert_ne!(stored_hmac.as_bytes(), token);
        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("service.db")
                && entry.file_type().unwrap().is_file()
            {
                let durable_text =
                    String::from_utf8_lossy(&std::fs::read(entry.path()).unwrap()).into_owned();
                assert!(
                    !durable_text.contains(std::str::from_utf8(token).unwrap()),
                    "数据库及 sidecar 都不得出现 token 明文"
                );
            }
        }

        assert!(matches!(
            store.settle_run_capability(&key, token).unwrap(),
            RunCapabilityResolution::One(RunCapability {
                state: RunCapabilityState::Settled,
                revoked_at: Some(_),
                ..
            })
        ));
        // terminal state 不可被 revoke 复活/改写。
        assert!(matches!(
            store.revoke_run_capability(&key, token).unwrap(),
            RunCapabilityResolution::One(RunCapability {
                state: RunCapabilityState::Settled,
                ..
            })
        ));
    }

    #[test]
    fn registration_conflicts_and_cross_project_duplicates_quarantine() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ServiceStore::open(&tmp.path().join("service.db")).unwrap();
        let p1 = project(&store, &tmp.path().join("p1"));
        let p2 = project(&store, &tmp.path().join("p2"));
        let key = RunCapabilityKey::for_test(vec![0x42; 32]).unwrap();

        let r2 = agent_run();
        store
            .register_run_capability(&key, b"cross-token", &p1, &r2)
            .unwrap();
        assert_eq!(
            store
                .register_run_capability(&key, b"cross-token", &p2, &agent_run())
                .unwrap(),
            RunCapabilityRegistration::Quarantined
        );
        assert_eq!(
            store.resolve_run_capability(&key, b"cross-token").unwrap(),
            RunCapabilityResolution::Many
        );

        let same_pair_run = agent_run();
        store
            .register_run_capability(&key, b"pair-a", &p1, &same_pair_run)
            .unwrap();
        assert_eq!(
            store
                .register_run_capability(&key, b"pair-b", &p1, &same_pair_run)
                .unwrap(),
            RunCapabilityRegistration::Quarantined
        );
        assert_eq!(
            store.resolve_run_capability(&key, b"pair-a").unwrap(),
            RunCapabilityResolution::Many
        );

        let shared_run = agent_run();
        store
            .register_run_capability(&key, b"cross-a", &p1, &shared_run)
            .unwrap();
        assert_eq!(
            store
                .register_run_capability(&key, b"cross-b", &p2, &shared_run)
                .unwrap(),
            RunCapabilityRegistration::Quarantined
        );
        assert_eq!(
            store.resolve_run_capability(&key, b"cross-a").unwrap(),
            RunCapabilityResolution::Many
        );
        assert_eq!(
            store.resolve_run_capability(&key, b"cross-b").unwrap(),
            RunCapabilityResolution::Many
        );
    }

    #[test]
    fn concurrent_exact_replay_converges_to_one_active_row() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("service.db");
        let bootstrap = ServiceStore::open(&db).unwrap();
        let project = project(&bootstrap, &tmp.path().join("project"));
        drop(bootstrap);
        let first = ServiceStore::open(&db).unwrap();
        let second = ServiceStore::open(&db).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let run = agent_run();
        let key = RunCapabilityKey::for_test(vec![0x43; 32]).unwrap();

        let spawn = |store: Arc<ServiceStore>, token: &'static [u8]| {
            let barrier = barrier.clone();
            let project = project.clone();
            let run = run.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.register_run_capability(&key, token, &project, &run)
            })
        };
        let first_task = spawn(first, b"same-race-token");
        let second_task = spawn(second, b"same-race-token");
        let a = first_task.join().unwrap().unwrap();
        let b = second_task.join().unwrap().unwrap();
        assert!(matches!(
            a,
            RunCapabilityRegistration::Registered(RunCapability {
                state: RunCapabilityState::Active,
                ..
            })
        ));
        assert!(matches!(
            b,
            RunCapabilityRegistration::Registered(RunCapability {
                state: RunCapabilityState::Active,
                ..
            })
        ));

        let verify = ServiceStore::open(&db).unwrap();
        verify
            .with_conn(|conn| {
                let (count, active): (i64, i64) = conn.query_row(
                    "SELECT COUNT(*), SUM(state='active') FROM run_capability",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                anyhow::ensure!(count == 1 && active == 1);
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            verify
                .resolve_run_capability(&key, b"same-race-token")
                .unwrap(),
            RunCapabilityResolution::One(RunCapability {
                state: RunCapabilityState::Active,
                ..
            })
        ));
    }

    #[test]
    fn project_mapping_backfill_and_bulk_revoke_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ServiceStore::open(&tmp.path().join("service.db")).unwrap();
        let key = RunCapabilityKey::for_test(vec![0x44; 32]).unwrap();
        let unknown = ProjectStoreHandle::generate();
        assert!(store
            .register_run_capability(&key, b"unknown", &unknown, &agent_run())
            .unwrap_err()
            .to_string()
            .contains("run_capability_project_unknown"));

        let active = project(&store, &tmp.path().join("active"));
        let run1 = agent_run();
        let run2 = agent_run();
        store
            .register_run_capability(&key, b"active-1", &active, &run1)
            .unwrap();
        store
            .register_run_capability(&key, b"active-2", &active, &run2)
            .unwrap();
        assert_eq!(store.revoke_project_run_capabilities(&active).unwrap(), 2);
        for token in [b"active-1".as_slice(), b"active-2".as_slice()] {
            assert!(matches!(
                store.resolve_run_capability(&key, token).unwrap(),
                RunCapabilityResolution::One(RunCapability {
                    state: RunCapabilityState::Revoked,
                    ..
                })
            ));
        }

        let missing = ProjectStoreHandle::generate();
        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO project_registry
                     (project_handle, public_id, canonical_root, display_path, registered_at, status)
                     VALUES(?1, ?2, ?3, ?3, '2026-09-01', 'missing')",
                    params![missing.as_str(), uuid::Uuid::now_v7().to_string(), "/missing"],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(store
            .register_run_capability(&key, b"missing-active", &missing, &agent_run())
            .unwrap_err()
            .to_string()
            .contains("run_capability_project_inactive"));
        store
            .backfill_quarantined_run_capability(&key, b"legacy-token", &missing, &agent_run())
            .unwrap();
        assert_eq!(
            store.resolve_run_capability(&key, b"legacy-token").unwrap(),
            RunCapabilityResolution::Many
        );
    }
}
