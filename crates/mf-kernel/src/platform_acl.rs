//! 数据库文件与专用目录的当前用户 ACL 收紧(§3.8:数据库文件当前用户 ACL)。
//!
//! 与 `crates/mf-agent/src/migration.rs` 同一实现口径,刻意不跨 crate 复用:
//! mf-kernel 是未来的 Core 内核 crate,不反向依赖 mf-agent;#18 Catalog v2
//! 并行改动 mf-agent 时两侧互不影响。收紧只作用于 service 库自身与其
//! `.monkeyfence` 专用父目录,绝不误改其他目录。

use crate::service_schema::is_service_home;
use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// 收紧活动 service 库及其专用父目录为当前用户独占。
///
/// 必须在 future-version guard 成功之后调用,保证拒绝未来版本的路径
/// 不修改文件元数据。相对裸文件名没有"专用目录",因此只收紧文件本体,
/// 不会误改进程当前工作目录 ACL。WAL/SHM sidecar 已存在时同步收紧。
pub(crate) fn restrict_service_database_to_current_user(conn: &Connection) -> Result<()> {
    let Some(raw_path) = conn.path().filter(|path| !path.is_empty()) else {
        return Ok(());
    };
    let path = PathBuf::from(raw_path);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if is_service_home(parent) {
            restrict_current_user_only(parent).map_err(|error| {
                anyhow::anyhow!("收紧 service 库目录 ACL {} 失败:{error}", parent.display())
            })?;
        }
    }
    restrict_current_user_only(&path)
        .map_err(|error| anyhow::anyhow!("收紧 service 库 ACL {} 失败:{error}", path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        if sidecar.exists() {
            restrict_current_user_only(&sidecar).map_err(|error| {
                anyhow::anyhow!("收紧 SQLite sidecar ACL {} 失败:{error}", sidecar.display())
            })?;
        }
    }
    Ok(())
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
