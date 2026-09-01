//! T1a(Issue #16):统一的 schema future guard 与 Backup 前置屏障。
//!
//! 单一深模块,Project / Catalog 两个 Store 共用,线性顺序固定:
//!
//! ```text
//! 读 user_version
//!   ├─ 高于程序已知版本 → 拒绝(schema_future_version):不执行任何 DDL、
//!   │   不改写 journal 模式、不留下备份/manifest
//!   ├─ 等于已知版本 → 直接返回(不备份)
//!   └─ 低于已知版本(user_version >= 1,即确有旧数据)→
//!       SQLite Backup API 一致备份(唯一 staging → 校验 → 发布,配 manifest)
//!       → 迁移事务(DDL)→ user_version = target
//! ```
//!
//! 备份永不覆盖既有 artifact:唯一 final 目录以 `create_dir` 原子保留,
//! DB/manifest 就位后才用 `COMPLETE` marker 发布。失败只留下 staging 或
//! 无 marker 的 incomplete 诊断目录,恢复枚举永远忽略它们。
//!
//! `user_version = 0` 视为全新库初始化而非 schema 升级:没有既有数据
//! 可保,不触发备份。

use crate::schema::schema_version_of;
use anyhow::Result;
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 备份 manifest 的稳定 schema 标识(最小可扩展 v1)。
pub const BACKUP_MANIFEST_SCHEMA: &str = "mf.backup.manifest.v1";

/// 稳定错误码:与主规格 §7.5 problem code `schema_future_version` 对齐。
pub const CODE_SCHEMA_FUTURE_VERSION: &str = "schema_future_version";
/// 稳定错误码:schema 升级前的备份失败。
pub const CODE_SCHEMA_BACKUP_FAILED: &str = "schema_backup_failed";

/// 每次 `sqlite3_backup_step` 拷贝的页数;小库一步完成,大库分步让出源库。
const BACKUP_PAGES_PER_STEP: i32 = 128;
/// 分步之间让源库处理排队操作的休眠。
const BACKUP_PAUSE: Duration = Duration::from_millis(1);
const COMPLETE_MARKER: &[u8] = b"mf.backup.complete.v1\n";

/// Store 种类:错误与 manifest 中标识 source store kind。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Project,
    Catalog,
}

impl StoreKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Catalog => "catalog",
        }
    }
}

impl std::fmt::Display for StoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// schema 打开/迁移失败的稳定判别错误(不止靠脆弱 substring)。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrationError {
    /// user_version 高于程序已知版本:在任何 DDL 之前 fail-closed。
    #[error(
        "schema_future_version:{store}库 user_version v{found} 高于程序支持的 v{known},拒绝打开"
    )]
    FutureVersion {
        store: StoreKind,
        found: i64,
        known: i64,
    },
    /// 升级前备份失败:迁移未执行、user_version 未变,旧数据不动。
    #[error("schema_backup_failed:{store}库 v{from}→v{to} 升级前备份失败:{reason}")]
    BackupFailed {
        store: StoreKind,
        from: i64,
        to: i64,
        reason: String,
    },
}

impl MigrationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::FutureVersion { .. } => CODE_SCHEMA_FUTURE_VERSION,
            Self::BackupFailed { .. } => CODE_SCHEMA_BACKUP_FAILED,
        }
    }
}

/// 集中判别:anyhow 错误链中的 [`MigrationError`] → 稳定错误码。
pub fn error_code(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<MigrationError>()
        .map(|error| error.code())
}

/// 备份 manifest:只保留恢复所需的非敏感元数据。
/// 不含数据库内容、Secret/capability 明文、完整环境或可复用 Secret ref。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackupManifest {
    pub schema: String,
    pub store_kind: String,
    /// 源库文件名(不含目录):恢复时定位来源,不携带绝对路径。
    pub source_file: String,
    pub from_version: i64,
    pub to_version: i64,
    /// 与 manifest 配对的备份文件名(同目录)。
    pub backup_file: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub created_at: String,
    /// 只有 staging 校验通过并成功发布后才为 true(已发布 manifest 恒为 true)。
    pub complete: bool,
}

/// 一次成功发布的备份产物(manifest 与备份文件一一配对)。
#[derive(Debug, Clone)]
pub struct BackupArtifact {
    pub artifact_dir: PathBuf,
    pub db_path: PathBuf,
    pub manifest_path: PathBuf,
    pub commit_marker_path: PathBuf,
    pub manifest: BackupManifest,
}

/// 备份目录:库文件同目录下的 `<库名>.backups/`。
pub fn backup_dir_for(db_path: &Path) -> PathBuf {
    let name = db_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store.db".to_string());
    db_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}.backups"))
}

/// 只返回带 COMPLETE marker 且 DB/manifest 均存在的已发布 artifact。
/// crash/failure 遗留的 staging 或 incomplete 目录永远不会被恢复流程采用。
pub fn published_artifact_dirs(backup_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !backup_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut artifacts = Vec::new();
    for entry in std::fs::read_dir(backup_dir)? {
        let path = entry?.path();
        if path.is_dir()
            && matches!(
                std::fs::read(path.join("COMPLETE")),
                Ok(bytes) if bytes == COMPLETE_MARKER
            )
            && path.join("backup.db").is_file()
            && path.join("manifest.json").is_file()
        {
            artifacts.push(path);
        }
    }
    artifacts.sort();
    Ok(artifacts)
}

/// future guard:读 user_version,高于 `known` 立即拒绝。
/// 必须在任何 DDL 与 journal 模式等改写性操作之前调用。
pub fn guard_future_version(conn: &Connection, store: StoreKind, known: i64) -> Result<()> {
    let found = schema_version_of(conn)?;
    if found > known {
        return Err(MigrationError::FutureVersion {
            store,
            found,
            known,
        }
        .into());
    }
    Ok(())
}

/// Backup 前置屏障 + 版本迁移(单一深模块入口)。
///
/// `migrate` 在迁移事务内应用 `(from, to]` 区间的 DDL;`user_version` 的
/// 写入由屏障统一完成,保证「备份成功之前绝不迁移」的线性顺序。
/// 库文件路径取自连接本身,因此屏障对 Project / Catalog 完全统一。
pub fn upgrade_with_barrier(
    conn: &mut Connection,
    store: StoreKind,
    target: i64,
    migrate: &dyn Fn(&Transaction, i64, i64) -> Result<()>,
) -> Result<()> {
    let current = schema_version_of(conn)?;
    if current > target {
        return Err(MigrationError::FutureVersion {
            store,
            found: current,
            known: target,
        }
        .into());
    }
    if current == target {
        return Ok(());
    }

    if conn.path().map(str::is_empty).unwrap_or(true) {
        // 内存库没有跨进程竞争也无法持久备份;仅允许全新 v0 初始化。
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let locked_current = schema_version_of(&tx)?;
        anyhow::ensure!(locked_current == 0, "内存旧库无法生成持久备份,拒绝升级");
        migrate(&tx, locked_current, target)?;
        tx.pragma_update(None, "user_version", target)?;
        tx.commit()?;
        return Ok(());
    }

    // 独立 lock connection 的 BEGIN IMMEDIATE 是 backup→migration 线性化点。
    // Backup 从原连接读取(避免 SQLite 禁止备份处于写事务的同一连接),迁移
    // 在 lock transaction 执行;reservation 全程阻止其他 writer 插入窗口。
    let mut lock_conn = open_writer_lock_connection(conn, store, current, target)?;
    let tx = lock_conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let locked_current = schema_version_of(&tx)?;
    if locked_current > target {
        return Err(MigrationError::FutureVersion {
            store,
            found: locked_current,
            known: target,
        }
        .into());
    }
    if locked_current == target {
        tx.rollback()?;
        return Ok(());
    }

    // user_version = 0 是全新库初始化(链式应用全量 DDL),不是升级;
    // 只有 0 < current < target 的既有库才必须先备份。
    if locked_current >= 1 {
        create_verified_backup(conn, store, target)?;
    }
    migrate(&tx, locked_current, target)?;
    tx.pragma_update(None, "user_version", target)?;
    tx.commit()?;
    Ok(())
}

/// 对“版本号已是当前值、但旧开发库缺表/列”的 schema repair 也应用同一
/// Backup 屏障。`needs_repair` 在 writer lock 内复验,避免并发进程已修复后
/// 仍产生多余备份。
pub fn repair_current_with_barrier(
    conn: &mut Connection,
    store: StoreKind,
    target: i64,
    needs_repair: &dyn Fn(&Connection) -> Result<bool>,
    repair: &dyn Fn(&Transaction) -> Result<()>,
) -> Result<()> {
    guard_future_version(conn, store, target)?;
    if schema_version_of(conn)? != target || !needs_repair(conn)? {
        return Ok(());
    }

    if conn.path().map(str::is_empty).unwrap_or(true) {
        return Err(MigrationError::BackupFailed {
            store,
            from: target,
            to: target,
            reason: "内存 Catalog repair 无法生成持久备份".to_string(),
        }
        .into());
    }
    let mut lock_conn = open_writer_lock_connection(conn, store, target, target)?;
    let tx = lock_conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let locked_current = schema_version_of(&tx)?;
    if locked_current > target {
        return Err(MigrationError::FutureVersion {
            store,
            found: locked_current,
            known: target,
        }
        .into());
    }
    anyhow::ensure!(
        locked_current == target,
        "{}库 repair 期望 v{target},锁内实际 v{locked_current}",
        store
    );
    if !needs_repair(&tx)? {
        tx.rollback()?;
        return Ok(());
    }

    create_verified_backup(conn, store, target)?;
    repair(&tx)?;
    tx.pragma_update(None, "user_version", target)?;
    tx.commit()?;
    Ok(())
}

/// 兼容 Catalog v1 的 T0 字节基线:健康 current schema 不再执行 DDL,
/// 但仍在 writer lock 内幂等重申 user_version。锁内复验保证并发 future
/// writer 不能在 guard 与该写入之间把版本降回。
pub fn reaffirm_current_version_locked(
    conn: &mut Connection,
    store: StoreKind,
    target: i64,
) -> Result<()> {
    guard_future_version(conn, store, target)?;
    if conn.path().map(str::is_empty).unwrap_or(true) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let locked = schema_version_of(&tx)?;
        if locked > target {
            return Err(MigrationError::FutureVersion {
                store,
                found: locked,
                known: target,
            }
            .into());
        }
        anyhow::ensure!(
            locked == target,
            "{}库 current-version 重申期望 v{target}",
            store
        );
        tx.pragma_update(None, "user_version", target)?;
        tx.commit()?;
        return Ok(());
    }

    let mut lock_conn = open_writer_lock_connection(conn, store, target, target)?;
    let tx = lock_conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let locked = schema_version_of(&tx)?;
    if locked > target {
        return Err(MigrationError::FutureVersion {
            store,
            found: locked,
            known: target,
        }
        .into());
    }
    anyhow::ensure!(
        locked == target,
        "{}库 current-version 重申期望 v{target}",
        store
    );
    tx.pragma_update(None, "user_version", target)?;
    tx.commit()?;
    Ok(())
}

fn open_writer_lock_connection(
    source: &Connection,
    store: StoreKind,
    from: i64,
    to: i64,
) -> Result<Connection> {
    let path = source
        .path()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| MigrationError::BackupFailed {
            store,
            from,
            to,
            reason: "无法确定源库路径,拒绝无锁迁移".to_string(),
        })?;
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(|error| {
            MigrationError::BackupFailed {
                store,
                from,
                to,
                reason: format!("打开迁移 writer-lock 连接失败:{error}"),
            }
        })?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| MigrationError::BackupFailed {
            store,
            from,
            to,
            reason: format!("配置迁移 writer-lock busy timeout 失败:{error}"),
        })?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| MigrationError::BackupFailed {
            store,
            from,
            to,
            reason: format!("启用迁移 writer-lock foreign_keys 失败:{error}"),
        })?;
    Ok(connection)
}

/// 经 SQLite Backup API 生成一致备份:唯一 staging → 校验 → 发布。
///
/// 覆盖 WAL 中已提交未 checkpoint 的数据(经由源连接的 pager 读取);
/// 发布不覆盖既有备份;失败只留 staging/incomplete 诊断产物(无
/// COMPLETE marker = 从未成功)。staging 校验后归一为非 WAL 单文件,保证备份可独立
/// 只读打开与长期归档。
pub fn create_verified_backup(
    source: &Connection,
    store: StoreKind,
    to_version: i64,
) -> std::result::Result<BackupArtifact, MigrationError> {
    let from_version = schema_version_of(source).map_err(|error| MigrationError::BackupFailed {
        store,
        from: -1,
        to: to_version,
        reason: format!("读取源库 user_version 失败:{error}"),
    })?;
    let fail = |reason: String| MigrationError::BackupFailed {
        store,
        from: from_version,
        to: to_version,
        reason,
    };
    let source_db = source
        .path()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| fail("无法确定库文件路径,拒绝无来源备份".to_string()))?;
    let backup_dir = backup_dir_for(&source_db);
    let backup_dir_was_created = !backup_dir.exists();
    std::fs::create_dir_all(&backup_dir)
        .map_err(|e| fail(format!("创建备份目录 {} 失败: {e}", backup_dir.display())))?;
    restrict_current_user_only(&backup_dir).map_err(|e| {
        fail(format!(
            "收紧备份目录 ACL {} 失败:{e}",
            backup_dir.display()
        ))
    })?;

    let source_name = source_db
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store.db".to_string());
    let stem = source_db
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store".to_string());
    let artifact_id = format!(
        "{stem}.backup.{}.v{from_version}-to-v{to_version}.{}.db",
        store.as_str(),
        unique_suffix()
    );
    let staging_dir = backup_dir.join(format!(".{artifact_id}.staging"));
    let final_dir = backup_dir.join(&artifact_id);
    std::fs::create_dir(&staging_dir).map_err(|e| {
        fail(format!(
            "创建唯一备份 staging 目录 {} 失败:{e}",
            staging_dir.display()
        ))
    })?;
    restrict_current_user_only(&staging_dir).map_err(|e| {
        fail(format!(
            "收紧 staging 目录 ACL {} 失败:{e}",
            staging_dir.display()
        ))
    })?;
    let staging_db = staging_dir.join("backup.db");
    let staging_manifest = staging_dir.join("manifest.json");
    let final_db = final_dir.join("backup.db");
    let final_manifest = final_dir.join("manifest.json");
    let commit_marker = final_dir.join("COMPLETE");

    // 1) Backup API 备份(不经裸文件拷贝;WAL 已提交数据一并进入)
    let mut dest = Connection::open(&staging_db).map_err(|e| {
        fail(format!(
            "打开备份 staging {} 失败: {e}",
            staging_db.display()
        ))
    })?;
    {
        let backup = Backup::new(source, &mut dest)
            .map_err(|e| fail(format!("初始化 SQLite Backup 失败: {e}")))?;
        backup
            .run_to_completion(BACKUP_PAGES_PER_STEP, BACKUP_PAUSE, None)
            .map_err(|e| fail(format!("Backup API 执行失败(staging 保留诊断): {e}")))?;
    }
    dest.close()
        .map_err(|(_, e)| fail(format!("关闭备份 staging 失败: {e}")))?;
    restrict_current_user_only(&staging_db).map_err(|e| {
        fail(format!(
            "收紧 staging 数据库 ACL {} 失败:{e}",
            staging_db.display()
        ))
    })?;

    // 2) 校验:完整性 + 版本必须等于源库当前 user_version
    let verify =
        Connection::open(&staging_db).map_err(|e| fail(format!("打开 staging 校验失败: {e}")))?;
    let integrity: String = verify
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| fail(format!("integrity_check 执行失败: {e}")))?;
    if integrity != "ok" {
        return Err(fail(format!(
            "备份完整性校验失败: integrity_check={integrity}"
        )));
    }
    let backup_version: i64 = verify
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| fail(format!("读取备份 user_version 失败: {e}")))?;
    if backup_version != from_version {
        return Err(fail(format!(
            "备份版本不匹配: 期望 v{from_version},实际 v{backup_version}"
        )));
    }
    // 归一为独立单文件(非 WAL):备份不依赖源库的 journal 模式
    verify
        .pragma_update(None, "journal_mode", "DELETE")
        .map_err(|e| fail(format!("归一备份 journal 模式失败: {e}")))?;
    verify
        .close()
        .map_err(|(_, e)| fail(format!("关闭 staging 校验连接失败: {e}")))?;

    // 3) hash/size + manifest(只含恢复元数据)
    let (sha256, size_bytes) = sha256_file(&staging_db)
        .map_err(|e| fail(format!("流式读取 staging 计算 hash 失败: {e}")))?;
    let manifest = BackupManifest {
        schema: BACKUP_MANIFEST_SCHEMA.to_string(),
        store_kind: store.as_str().to_string(),
        source_file: source_name,
        from_version,
        to_version,
        backup_file: "backup.db".to_string(),
        sha256,
        size_bytes,
        created_at: chrono::Utc::now().to_rfc3339(),
        complete: true,
    };
    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| fail(format!("序列化 manifest 失败: {e}")))?
        + "\n";
    write_new_file(&staging_manifest, manifest_json.as_bytes())
        .map_err(|e| fail(format!("写入 staging manifest 失败:{e}")))?;
    restrict_current_user_only(&staging_manifest).map_err(|e| {
        fail(format!(
            "收紧 staging manifest ACL {} 失败:{e}",
            staging_manifest.display()
        ))
    })?;

    // 4) 发布:唯一 final 目录先原子保留名;DB/manifest 就位并收紧 ACL
    // 后,以 create_new COMPLETE marker 作为唯一发布线性化点。任一步失败
    // 都没有 marker,只能被视为 incomplete diagnostic artifact。
    publish_artifact(&staging_dir, &final_dir).map_err(&fail)?;
    if backup_dir_was_created {
        let parent = backup_dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        sync_directory(parent).map_err(|e| {
            fail(format!(
                "同步新备份目录的父目录 {} 失败:{e}",
                parent.display()
            ))
        })?;
    }

    Ok(BackupArtifact {
        artifact_dir: final_dir,
        db_path: final_db,
        manifest_path: final_manifest,
        commit_marker_path: commit_marker,
        manifest,
    })
}

fn publish_artifact(staging_dir: &Path, final_dir: &Path) -> std::result::Result<(), String> {
    publish_artifact_with_hook(staging_dir, final_dir, |_| Ok(()))
}

fn publish_artifact_with_hook(
    staging_dir: &Path,
    final_dir: &Path,
    before_manifest: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::result::Result<(), String> {
    std::fs::create_dir(final_dir).map_err(|e| {
        format!(
            "原子保留发布目录名 {} 失败(绝不覆盖):{e}",
            final_dir.display()
        )
    })?;
    restrict_current_user_only(final_dir)
        .map_err(|e| format!("收紧发布目录 ACL {} 失败:{e}", final_dir.display()))?;

    let staging_db = staging_dir.join("backup.db");
    let staging_manifest = staging_dir.join("manifest.json");
    let final_db = final_dir.join("backup.db");
    let final_manifest = final_dir.join("manifest.json");
    let marker = final_dir.join("COMPLETE");

    move_file_no_overwrite(&staging_db, &final_db)
        .map_err(|e| format!("发布备份文件 {} 失败:{e}", final_db.display()))?;
    before_manifest(final_dir).map_err(|e| format!("manifest 发布前故障注入:{e}"))?;
    move_file_no_overwrite(&staging_manifest, &final_manifest)
        .map_err(|e| format!("发布 manifest {} 失败:{e}", final_manifest.display()))?;
    restrict_current_user_only(&final_db)
        .map_err(|e| format!("收紧备份文件 ACL {} 失败:{e}", final_db.display()))?;
    restrict_current_user_only(&final_manifest)
        .map_err(|e| format!("收紧 manifest ACL {} 失败:{e}", final_manifest.display()))?;

    let published_manifest: BackupManifest = serde_json::from_slice(
        &std::fs::read(&final_manifest).map_err(|e| format!("发布后读取 manifest 校验失败:{e}"))?,
    )
    .map_err(|e| format!("发布后解析 manifest 校验失败:{e}"))?;
    let (published_hash, published_size) =
        sha256_file(&final_db).map_err(|e| format!("发布后流式校验备份失败:{e}"))?;
    if !published_manifest.complete
        || published_manifest.backup_file != "backup.db"
        || published_manifest.sha256 != published_hash
        || published_manifest.size_bytes != published_size
    {
        return Err("发布后 DB/manifest 配对校验失败,拒绝创建 COMPLETE".to_string());
    }

    std::fs::remove_dir(staging_dir).map_err(|e| format!("清理已发布的空 staging 目录失败:{e}"))?;
    write_new_file(&marker, COMPLETE_MARKER)
        .map_err(|e| format!("创建 COMPLETE 发布标记失败:{e}"))?;
    sync_directory(final_dir)
        .map_err(|e| format!("同步 artifact 目录 {} 失败:{e}", final_dir.display()))?;
    let backup_dir = final_dir
        .parent()
        .ok_or_else(|| "artifact 目录缺少 backup parent".to_string())?;
    sync_directory(backup_dir)
        .map_err(|e| format!("同步备份根目录 {} 失败:{e}", backup_dir.display()))?;
    Ok(())
}

/// 单调唯一后缀:pid + 纳秒时钟 + 进程内计数器,跨进程/并发不冲突。
fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn move_file_no_overwrite(source: &Path, target: &Path) -> std::io::Result<()> {
    let mut source_file = File::open(source)?;
    let mut target_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    std::io::copy(&mut source_file, &mut target_file)?;
    target_file.sync_all()?;
    drop(target_file);
    drop(source_file);
    std::fs::remove_file(source)
}

fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((digest, size))
}

#[cfg(unix)]
fn restrict_current_user_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(windows)]
fn restrict_current_user_only(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // Protected DACL; only the current object owner gets full access. OI/CI
    // propagate the same rule to descendants when `path` is a directory.
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)\0".encode_utf16().collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let applied = unsafe {
        SetFileSecurityW(
            path_wide.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    let error = if applied == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        LocalFree(descriptor);
    }
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(not(any(unix, windows)))]
fn restrict_current_user_only(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let flushed = unsafe { FlushFileBuffers(handle) };
    let error = if flushed == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        CloseHandle(handle);
    }
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 迁移 closure 运行时,配对备份与 manifest 必须已经发布:
    /// 证明 backup 在 migration closure 之前完成。
    #[test]
    fn backup_completes_before_migration_closure() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("unit.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE t(x); PRAGMA user_version = 2;")
                .unwrap();
        }
        let mut conn = Connection::open(&db).unwrap();
        let seen = std::cell::Cell::new(false);
        upgrade_with_barrier(&mut conn, StoreKind::Project, 5, &|tx, from, to| {
            let dir = backup_dir_for(&db);
            let artifacts = published_artifact_dirs(&dir).unwrap();
            assert_eq!(artifacts.len(), 1, "迁移事务开启时必须已有唯一 artifact");
            let manifest = std::fs::read_to_string(artifacts[0].join("manifest.json")).unwrap();
            let parsed: BackupManifest = serde_json::from_str(&manifest).unwrap();
            assert!(parsed.complete, "已发布 manifest 必须 complete");
            assert_eq!(parsed.from_version, from);
            assert_eq!(parsed.to_version, to);
            assert_eq!(parsed.store_kind, "project");
            assert!(
                artifacts[0].join(&parsed.backup_file).is_file(),
                "备份文件必须先于迁移发布"
            );
            seen.set(true);
            tx.execute_batch("CREATE TABLE v5_only(x);")?;
            Ok(())
        })
        .unwrap();
        assert!(seen.get(), "迁移 closure 必须被执行");
        assert_eq!(schema_version_of(&conn).unwrap(), 5);
        let table_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='v5_only'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table_exists, 1, "迁移 DDL 已在事务内生效");
    }

    fn make_staging_artifact(root: &Path) -> PathBuf {
        let staging = root.join("artifact.staging");
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("backup.db"), b"db").unwrap();
        std::fs::write(staging.join("manifest.json"), b"{}").unwrap();
        staging
    }

    /// 发布绝不覆盖既有 artifact 目录:原内容保持,staging 保留。
    #[test]
    fn publish_refuses_to_overwrite_existing_artifact_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = make_staging_artifact(tmp.path());
        let final_path = tmp.path().join("artifact");
        std::fs::create_dir(&final_path).unwrap();
        std::fs::write(final_path.join("sentinel"), b"sentinel").unwrap();
        assert!(publish_artifact(&staging, &final_path).is_err());
        assert_eq!(
            std::fs::read(final_path.join("sentinel")).unwrap(),
            b"sentinel"
        );
        assert!(staging.exists(), "失败产物保留用于诊断");
    }

    /// DB 就位后 manifest 发布失败也不得出现 COMPLETE marker;恢复枚举必须
    /// 忽略这个 incomplete artifact。
    #[test]
    fn manifest_publish_failure_never_marks_artifact_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = make_staging_artifact(tmp.path());
        let final_path = tmp.path().join("artifact");
        let err = publish_artifact_with_hook(&staging, &final_path, |dir| {
            std::fs::create_dir(dir.join("manifest.json"))
        })
        .unwrap_err();
        assert!(err.contains("manifest"), "错误应指向 manifest 发布:{err}");
        assert!(!final_path.join("COMPLETE").exists());
        assert!(published_artifact_dirs(tmp.path()).unwrap().is_empty());
        assert!(final_path.join("backup.db").is_file());
        assert!(staging.join("manifest.json").is_file());
        std::fs::write(final_path.join("COMPLETE"), b"").unwrap();
        assert!(
            published_artifact_dirs(tmp.path()).unwrap().is_empty(),
            "crash 遗留的空/半写 marker 也不能发布 artifact"
        );
    }

    /// 同一目录重复备份:文件名唯一,不互相覆盖。
    #[test]
    fn repeated_backups_get_unique_names_without_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("unit.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE t(x); PRAGMA user_version = 1;")
                .unwrap();
        }
        let conn = Connection::open(&db).unwrap();
        let dir = backup_dir_for(&db);
        let first = create_verified_backup(&conn, StoreKind::Project, 2).unwrap();
        let second = create_verified_backup(&conn, StoreKind::Project, 2).unwrap();
        assert_ne!(first.db_path, second.db_path, "命名必须唯一");
        assert!(first.db_path.is_file() && second.db_path.is_file());
        assert_eq!(
            published_artifact_dirs(&dir).unwrap().len(),
            2,
            "恰两个 complete artifacts"
        );
    }

    #[test]
    fn backup_artifact_is_restricted_to_current_user() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("unit.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE t(x); PRAGMA user_version = 1;")
                .unwrap();
        }
        let conn = Connection::open(&db).unwrap();
        let artifact = create_verified_backup(&conn, StoreKind::Project, 2).unwrap();
        for path in [
            backup_dir_for(&db),
            artifact.artifact_dir,
            artifact.db_path,
            artifact.manifest_path,
            artifact.commit_marker_path,
        ] {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let expected = if path.is_dir() { 0o700 } else { 0o600 };
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    expected,
                    "{} 必须仅当前用户可访问",
                    path.display()
                );
            }
            #[cfg(windows)]
            {
                let sddl = dacl_sddl(&path);
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

    #[cfg(windows)]
    fn dacl_sddl(path: &Path) -> String {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            GetFileSecurityW, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        };

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut needed = 0u32;
        unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        assert!(needed > 0);
        let mut descriptor = vec![0u8; needed as usize];
        let ok = unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr() as PSECURITY_DESCRIPTOR,
                needed,
                &mut needed,
            )
        };
        assert_ne!(
            ok,
            0,
            "GetFileSecurityW:{:?}",
            std::io::Error::last_os_error()
        );
        let mut sddl_ptr = std::ptr::null_mut();
        let mut length = 0u32;
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.as_mut_ptr() as PSECURITY_DESCRIPTOR,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut sddl_ptr,
                &mut length,
            )
        };
        assert_ne!(ok, 0);
        let result = String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(sddl_ptr, length as usize)
        });
        unsafe {
            LocalFree(sddl_ptr.cast());
        }
        result
    }

    /// future guard 单元:高版本返回稳定判别错误。
    #[test]
    fn guard_rejects_future_version_with_stable_error() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        let err = guard_future_version(&conn, StoreKind::Catalog, 1).unwrap_err();
        assert_eq!(error_code(&err), Some(CODE_SCHEMA_FUTURE_VERSION));
        match err.downcast_ref::<MigrationError>() {
            Some(MigrationError::FutureVersion {
                store,
                found,
                known,
            }) => {
                assert_eq!(*store, StoreKind::Catalog);
                assert_eq!(*found, 99);
                assert_eq!(*known, 1);
            }
            other => panic!("必须是 FutureVersion: {other:?}"),
        }
    }
}
