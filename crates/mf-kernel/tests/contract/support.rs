//! 契约测试共享 fixture 助手:全部基于 tempfile 独立数据库,不触碰
//! `~/.monkeyfence` 与真实 `session.json`。

use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

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
