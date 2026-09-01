//! 契约测试共享 fixture 助手:全部基于 tempfile 独立数据库,不触碰
//! `~/.monkeyfence` 与真实 `session.json`。

use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub fn sha256_file(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(fs::read(path).unwrap());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 独立只读连接:任何校验都不经由生产写路径。
pub fn read_only(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

pub fn user_version_of(path: &Path) -> i64 {
    read_only(path)
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

pub fn journal_mode_of(path: &Path) -> String {
    read_only(path)
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap()
}

/// schema 对象规范化快照(`type:name → DDL`):证明「DDL 未执行」
/// 不依赖脆弱的文件字节比较。
pub fn schema_objects_of(path: &Path) -> Vec<(String, String)> {
    let conn = read_only(path);
    let mut stmt = conn
        .prepare(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            r.get::<_, String>(2)?,
        ))
    })
    .unwrap()
    .collect::<std::result::Result<Vec<_>, _>>()
    .unwrap()
}

pub fn table_names_of(path: &Path) -> Vec<String> {
    schema_objects_of(path)
        .into_iter()
        .filter(|(key, _)| key.starts_with("table:"))
        .map(|(key, _)| key.trim_start_matches("table:").to_string())
        .collect()
}

pub fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `PRAGMA table_info` 的列元组(按 DDL 顺序):精确断言列集合。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMeta {
    pub name: String,
    pub col_type: String,
    pub notnull: bool,
    pub dflt: Option<String>,
    pub pk: bool,
}

pub fn columns_of(path: &Path, table: &str) -> Vec<ColumnMeta> {
    let conn = read_only(path);
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let cols = stmt
        .query_map([], |r| {
            Ok(ColumnMeta {
                name: r.get(1)?,
                col_type: r.get(2)?,
                notnull: r.get::<_, i64>(3)? != 0,
                dflt: r.get(4)?,
                pk: r.get::<_, i64>(5)? != 0,
            })
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert!(!cols.is_empty(), "表 {table} 必须存在");
    cols
}

pub fn column_names_of(path: &Path, table: &str) -> Vec<String> {
    columns_of(path, table)
        .into_iter()
        .map(|c| c.name)
        .collect()
}

/// 非部分唯一索引覆盖的列集合(`PRAGMA index_list` + `index_info`)。
pub fn unique_index_sets(path: &Path, table: &str) -> Vec<Vec<String>> {
    let conn = read_only(path);
    let mut stmt = conn
        .prepare(&format!("PRAGMA index_list({table})"))
        .unwrap();
    let indexes: Vec<(String, bool, bool)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(1)?,
                r.get::<_, i64>(2)? != 0,
                r.get::<_, i64>(4)? != 0,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    drop(stmt);
    indexes
        .into_iter()
        .filter(|(_, unique, partial)| *unique && !*partial)
        .map(|(name, _, _)| {
            let mut info = conn.prepare(&format!("PRAGMA index_info({name})")).unwrap();
            info.query_map([], |r| r.get(2))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        })
        .collect()
}

/// 断言 `{table}` 上存在覆盖恰好 `cols` 列的唯一索引。
pub fn assert_unique_index(path: &Path, table: &str, cols: &[&str]) {
    let wanted: Vec<String> = cols.iter().map(|c| c.to_string()).collect();
    let sets = unique_index_sets(path, table);
    assert!(
        sets.contains(&wanted),
        "{table} 必须有唯一索引覆盖 {cols:?},实际: {sets:?}"
    );
}

/// 解析为真实 UUIDv7(版本号 4 bit 必须是 7)。
pub fn is_uuid_v7(handle: &str) -> bool {
    uuid::Uuid::parse_str(handle)
        .map(|u| u.get_version_num() == 7)
        .unwrap_or(false)
}

/// `project_registry` 全行快照(不含随机 handle 的稳定部分 + handle 一起,
/// 按 canonical_root 排序):重复导入/崩溃重跑的前后对比基准。
pub fn registry_rows(path: &Path) -> Vec<Vec<String>> {
    let conn = read_only(path);
    let mut stmt = conn
        .prepare(
            "SELECT project_handle, public_id, canonical_root, display_path,
                    registered_at, status
             FROM project_registry ORDER BY canonical_root",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok(vec![
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ])
    })
    .unwrap()
    .collect::<std::result::Result<Vec<_>, _>>()
    .unwrap()
}

/// 全部用户表的全文本投影:证明某字符串(如 open_files 路径)在库内
/// 任何表、任何列都不存在。
pub fn dump_all_text(path: &Path) -> String {
    let conn = read_only(path);
    let mut tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    tables.sort();
    let mut out = String::new();
    for table in tables {
        out.push_str(&table);
        out.push('\n');
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let col_count = stmt.column_count();
        let text: Vec<String> = stmt
            .query_map([], move |r| {
                let mut row_text = String::new();
                for i in 0..col_count {
                    if let rusqlite::types::ValueRef::Text(value) = r.get_ref(i)? {
                        row_text.push_str(&String::from_utf8_lossy(value));
                        row_text.push('\u{1}');
                    }
                }
                Ok(row_text)
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        out.push_str(&text.join("\n"));
        out.push('\n');
    }
    out
}

/// 写一个最小 session.json fixture,返回其内容字节。
pub fn write_session_json(path: &Path, value: &serde_json::Value) -> Vec<u8> {
    let bytes = serde_json::to_vec_pretty(value).unwrap();
    fs::write(path, &bytes).unwrap();
    bytes
}

// ─────────────────── T1e CoreOwnerLock 测试助手 ───────────────────

/// 每个测试独立的 owner 互斥名(避免测试间、以及与真实 Core 的
/// `Local\MonkeyFence.Core` 互斥冲突;Unix 平台互斥走 flock 文件,不使用名字)。
pub fn unique_mutex_name(tag: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        r"Local\MonkeyFence.Test.{tag}.{}.{}",
        std::process::id(),
        serial
    )
}

/// 跨进程契约测试的 probe 二进制路径(同包 bin,Cargo 注入)。
pub fn owner_lock_probe_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_owner_lock_probe"))
}

/// 轮询等待文件出现并读取内容(跨进程 ack 同步;超时返回 None)。
pub fn wait_for_file(path: &Path, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            return Some(text);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// 断言文件仅当前用户可访问(§11.1:discovery 记录与锁文件权限仅当前用户)。
pub fn assert_current_user_only(path: &Path) {
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

/// T1e owner lock 的确定性 fake 装配:fake 互斥/时钟/存活探针 + tempdir。
/// `mutex_handle()` 返回同名的 fake 互斥实例(全局注册表按名字互斥),
/// 供 `simulate_os_reclaim`/`is_held` 注入与观察。
pub struct OwnerFixture {
    pub dir: tempfile::TempDir,
    pub paths: mf_kernel::singleton::OwnerLockPaths,
    pub mutex_name: String,
    pub clock: std::sync::Arc<mf_kernel::singleton::FakeOwnerClock>,
    pub liveness: std::sync::Arc<mf_kernel::singleton::FakeProcessLivenessProbe>,
}

impl OwnerFixture {
    pub fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = mf_kernel::singleton::OwnerLockPaths::in_dir(dir.path());
        let start = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        Self {
            paths,
            mutex_name: unique_mutex_name(tag),
            clock: std::sync::Arc::new(mf_kernel::singleton::FakeOwnerClock::new(start)),
            liveness: std::sync::Arc::new(mf_kernel::singleton::FakeProcessLivenessProbe::new()),
            dir,
        }
    }

    pub fn mutex_handle(&self) -> mf_kernel::singleton::FakeOwnerMutex {
        mf_kernel::singleton::FakeOwnerMutex::new(&self.mutex_name)
    }

    pub fn setup(&self) -> mf_kernel::singleton::OwnerLockSetup {
        mf_kernel::singleton::OwnerLockSetup::new(
            self.paths.clone(),
            Box::new(self.mutex_handle()),
            self.clock.clone(),
            self.liveness.clone(),
        )
    }

    /// 真实缝隙装配(Windows 命名 mutex / Unix flock + 系统时钟 + OS 探针),
    /// 用于进程内真实互斥与跨进程路径。
    pub fn real_setup(&self) -> mf_kernel::singleton::OwnerLockSetup {
        mf_kernel::singleton::OwnerLockSetup::new(
            self.paths.clone(),
            mf_kernel::singleton::platform_owner_mutex(&self.mutex_name, &self.paths.flock_path()),
            std::sync::Arc::new(mf_kernel::singleton::SystemOwnerClock),
            std::sync::Arc::new(mf_kernel::singleton::OsProcessLivenessProbe),
        )
        .with_acquire_timeout(Duration::from_millis(300))
    }
}

/// 读取 DACL 的 SDDL 文本(Windows ACL 断言用)。
#[cfg(windows)]
pub fn dacl_sddl(path: &Path) -> String {
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
    let result =
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sddl_ptr, length as usize) });
    unsafe {
        LocalFree(sddl_ptr.cast());
    }
    result
}
