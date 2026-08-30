//! 路径安全原语(F6):消除「校验后替换」(TOCTOU)窗口。
//!
//! 所有文件操作都通过**已验证的目录句柄**进行:
//! - Unix:`openat` 家族 —— 根目录 `open(O_DIRECTORY)`,逐级子目录
//!   `openat(O_DIRECTORY|O_NOFOLLOW)`(symlink 穿越直接 ELOOP),
//!   读/写/删除/改名全部 `*at` 句柄相对(目录 inode 不受后续路径
//!   替换影响);
//! - Windows:根目录 `CreateFileW(BACKUP_SEMANTICS)`,子项一律
//!   `NtCreateFile(RootDirectory=父句柄, FILE_OPEN_REPARSE_POINT)` +
//!   `GetFileInformationByHandle` 验证**最终句柄**不是 reparse point
//!   (junction/symlink),写入经同目录唯一临时文件(create 语义,
//!   已存在即失败)+ 句柄相对 rename(`FileRenameInformationEx`,
//!   POSIX 语义)+ 目录 FlushFileBuffers。
//!
//! 由此:目录链在打开时逐级验证,之后的读写不再按路径解析 ——
//! 验证后把中间目录替换为 symlink/junction 无法把写入重定向到
//! 仓库之外;预置在随机临时名上的 reparse 也因 create-if-not-exists
//! 与终句柄验证而无法生效。

use anyhow::{Context as _, Result};
use std::path::Path;

/// 单个路径分量的静态校验(非空、无分隔符、非 `.`/`..`、无 NUL)。
fn validate_component(name: &str) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "路径分段不得为空");
    anyhow::ensure!(
        !name.contains('/') && !name.contains('\\'),
        "路径分段不得含分隔符: {name:?}"
    );
    anyhow::ensure!(name != "." && name != "..", "路径分段不得为 `.`/`..`");
    anyhow::ensure!(!name.contains('\0'), "路径分段不得含 NUL");
    Ok(())
}

/// 把(词法上已校验的)相对路径拆成全部分量。
fn split_components(rel: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for seg in rel.split('/') {
        validate_component(seg).with_context(|| format!("相对路径非法: {rel:?}"))?;
        out.push(seg.to_string());
    }
    anyhow::ensure!(!out.is_empty(), "相对路径不得为空");
    Ok(out)
}

// ---------------------------------------------------------------------------
// Unix:openat 句柄相对实现
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;

    pub struct SafeDir {
        fd: RawFd,
    }

    // fd 与线程无关;跨线程移动目录句柄安全
    unsafe impl Send for SafeDir {}

    fn cstr(name: &str) -> Result<CString> {
        CString::new(name.as_bytes()).with_context(|| format!("分段含 NUL: {name:?}"))
    }

    fn openat_dir(at: RawFd, name: &str, nofollow: bool) -> Result<SafeDir> {
        let c = cstr(name)?;
        let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        if nofollow {
            flags |= libc::O_NOFOLLOW;
        }
        let fd = unsafe { libc::openat(at, c.as_ptr(), flags) };
        anyhow::ensure!(
            fd >= 0,
            "打开目录分段 {name:?} 失败: {}",
            std::io::Error::last_os_error()
        );
        Ok(SafeDir { fd })
    }

    impl SafeDir {
        /// 根是受信输入(调度器/宿主给出),本身不做 NOFOLLOW;
        /// 之后所有子分量一律 NOFOLLOW(symlink → ELOOP 拒绝)。
        pub fn open_root(root: &Path) -> Result<SafeDir> {
            let c = CString::new(root.as_os_str().as_bytes())
                .with_context(|| format!("根路径含 NUL: {}", root.display()))?;
            let fd = unsafe {
                libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
            };
            anyhow::ensure!(
                fd >= 0,
                "打开根目录失败 {}: {}",
                root.display(),
                std::io::Error::last_os_error()
            );
            Ok(SafeDir { fd })
        }

        pub fn child(&self, name: &str, create: bool) -> Result<SafeDir> {
            validate_component(name)?;
            match openat_dir(self.fd, name, true) {
                Ok(d) => Ok(d),
                Err(e) => {
                    if !create {
                        return Err(e);
                    }
                    // 仅 ENOENT 才创建;其他错误(含 ELOOP=symlink)如实拒绝
                    let errno = std::io::Error::last_os_error().raw_os_error();
                    if errno != Some(libc::ENOENT) {
                        return Err(e.context(format!("打开子目录 {name:?} 失败")));
                    }
                    let c = cstr(name)?;
                    let r = unsafe { libc::mkdirat(self.fd, c.as_ptr(), 0o755) };
                    anyhow::ensure!(
                        r == 0
                            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST),
                        "创建子目录 {name:?} 失败: {}",
                        std::io::Error::last_os_error()
                    );
                    openat_dir(self.fd, name, true)
                }
            }
        }

        /// 原子写入:同目录唯一临时文件(O_EXCL|O_NOFOLLOW,已存在即
        /// 失败)+ fsync + `renameat`(同目录句柄相对)+ 目录 fsync。
        pub fn write_file(&self, name: &str, content: &[u8]) -> Result<()> {
            use std::io::Write as _;
            validate_component(name)?;
            let tmp = format!(".{name}.mfs-{}", crate::fs_atomic::random_txn_id());
            let tmp_c = cstr(&tmp)?;
            let fd = unsafe {
                libc::openat(
                    self.fd,
                    tmp_c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o644,
                )
            };
            anyhow::ensure!(
                fd >= 0,
                "创建临时文件失败({tmp:?}): {}",
                std::io::Error::last_os_error()
            );
            let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
            let result = (|| -> Result<()> {
                f.write_all(content)?;
                f.flush()?;
                f.sync_all()?;
                Ok(())
            })();
            drop(f); // 关闭(成功或失败路径都不泄漏 fd)
            result?;
            let name_c = cstr(name)?;
            let r = unsafe { libc::renameat(self.fd, tmp_c.as_ptr(), self.fd, name_c.as_ptr()) };
            anyhow::ensure!(
                r == 0,
                "句柄相对改名失败({tmp:?} → {name:?}): {}",
                std::io::Error::last_os_error()
            );
            self.sync()
        }

        pub fn read_file(&self, name: &str) -> Result<Option<Vec<u8>>> {
            use std::io::Read as _;
            validate_component(name)?;
            let c = cstr(name)?;
            let fd = unsafe {
                libc::openat(
                    self.fd,
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ENOENT) {
                    return Ok(None);
                }
                anyhow::bail!("打开文件 {name:?} 失败(symlink 拒绝): {err}");
            }
            let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut out = Vec::new();
            f.read_to_end(&mut out)
                .with_context(|| format!("读取文件 {name:?} 失败"))?;
            Ok(Some(out))
        }

        pub fn remove_file(&self, name: &str) -> Result<()> {
            validate_component(name)?;
            let c = cstr(name)?;
            let r = unsafe { libc::unlinkat(self.fd, c.as_ptr(), 0) };
            anyhow::ensure!(
                r == 0,
                "句柄相对删除失败({name:?}): {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }

        pub fn sync(&self) -> Result<()> {
            let r = unsafe { libc::fsync(self.fd) };
            anyhow::ensure!(
                r == 0,
                "目录 fsync 失败: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
    }

    impl SafeDir {
        #[cfg(test)]
        pub(crate) fn handle_raw(&self) -> std::os::fd::RawFd {
            self.fd
        }
    }

    impl Drop for SafeDir {
        fn drop(&mut self) {
            if self.fd >= 0 {
                unsafe { libc::close(self.fd) };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Windows:NtCreateFile 句柄相对 + 最终句柄 reparse 验证
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, GetFileInformationByHandle, ReadFile, WriteFile,
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{NtCreateFile, NtSetInformationFile};
    use windows_sys::Win32::Foundation::UNICODE_STRING;

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const DELETE_ACCESS: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;
    const FILE_OPEN: u32 = 1;
    const FILE_CREATE: u32 = 2;
    const FILE_OPEN_IF: u32 = 3;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;
    const FILE_INFORMATION_CLASS_RENAME: i32 = 65; // FileRenameInformationEx
    const FILE_INFORMATION_CLASS_RENAME_LEGACY: i32 = 10; // FileRenameInformation
    const FILE_INFORMATION_CLASS_DISPOSITION: i32 = 13; // FileDispositionInformation

    fn last_os_error_string() -> String {
        std::io::Error::last_os_error().to_string()
    }

    /// NTSTATUS 成功(>= 0)。
    fn nt_success(status: i32) -> bool {
        status >= 0
    }

    pub struct SafeDir {
        handle: HANDLE,
    }

    unsafe impl Send for SafeDir {}

    impl Drop for SafeDir {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
            }
        }
    }

    /// 验证句柄:目录且**非 reparse point**(junction/symlink 以
    /// FILE_ATTRIBUTE_REPARSE_POINT 呈现)—— 终句柄验证,不信任路径。
    fn verify_real_dir(handle: HANDLE, what: &str) -> Result<()> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
        anyhow::ensure!(ok != 0, "查询 {what} 属性失败: {}", last_os_error_string());
        anyhow::ensure!(
            info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            "{what} 不是目录"
        );
        anyhow::ensure!(
            info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "{what} 是符号链接/接合点,拒绝"
        );
        Ok(())
    }

    /// 验证文件句柄非 reparse(终句柄验证:即使路径在打开后被替换,
    /// 我们持有的句柄指向打开瞬间绑定的对象)。
    fn verify_not_reparse(handle: HANDLE, what: &str) -> Result<()> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
        anyhow::ensure!(ok != 0, "查询 {what} 属性失败: {}", last_os_error_string());
        anyhow::ensure!(
            info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "{what} 是符号链接/接合点,拒绝"
        );
        Ok(())
    }

    /// `NtCreateFile(RootDirectory=父句柄, 单分量名)` —— 句柄相对打开,
    /// 绝不按完整路径解析(验证后替换中间目录无法重定向)。
    /// `FILE_OPEN_REPARSE_POINT`:reparse 打开自身而非穿越;随后由
    /// verify_* 检查属性拒绝。
    /// 返回原始 NTSTATUS(供「不存在」语义判定;NtCreateFile 不设置
    /// Win32 last_error)。
    const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
    const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;

    fn nt_open_relative_raw(
        dir: HANDLE,
        name: &str,
        desired_access: u32,
        disposition: u32,
        directory: bool,
        _what: &str,
    ) -> (i32, HANDLE) {
        if validate_component(name).is_err() {
            return (0xC000_000Du32 as i32 /* STATUS_INVALID_PARAMETER */, core::ptr::null_mut());
        }
        let mut name_u16: Vec<u16> = name.encode_utf16().collect();
        let byte_len = (name_u16.len() * 2) as u16;
        let mut uname = UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: name_u16.as_mut_ptr(),
        };
        let mut oa: OBJECT_ATTRIBUTES = unsafe { std::mem::zeroed() };
        oa.Length = std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32;
        oa.RootDirectory = dir;
        oa.ObjectName = &mut uname;
        oa.Attributes = OBJ_CASE_INSENSITIVE;
        let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        let mut options = FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT;
        if directory {
            options |= FILE_DIRECTORY_FILE;
        }
        let mut handle: HANDLE = core::ptr::null_mut();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                desired_access,
                &oa,
                &mut iosb,
                core::ptr::null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                disposition,
                options,
                core::ptr::null(),
                0,
            )
        };
        (status, handle)
    }

    fn nt_open_relative(
        dir: HANDLE,
        name: &str,
        desired_access: u32,
        disposition: u32,
        directory: bool,
        what: &str,
    ) -> Result<HANDLE> {
        let (status, handle) =
            nt_open_relative_raw(dir, name, desired_access, disposition, directory, what);
        anyhow::ensure!(
            nt_success(status),
            "句柄相对打开 {what}({name:?})失败: NTSTATUS {status:#010x}"
        );
        anyhow::ensure!(!handle.is_null(), "句柄相对打开 {what}({name:?})未返回句柄");
        Ok(handle)
    }

    /// 句柄相对 rename(FileRenameInformationEx,POSIX 语义 + 已存在即替换;
    /// 旧系统不支持时回退 FileRenameInformation.ReplaceIfExists)。
    fn nt_rename(
        file: HANDLE,
        dir: HANDLE,
        name: &str,
        replace_if_exists: bool,
    ) -> Result<()> {
        let name_u16: Vec<u16> = name.encode_utf16().collect();
        // FILE_RENAME_INFORMATION(x64):union(4+4 填充)/RootDirectory(8)/
        // FileNameLength(4)/FileName(变长 UTF-16)
        let mut buf: Vec<u8> = Vec::with_capacity(24 + name_u16.len() * 2);
        buf.extend_from_slice(&0u32.to_le_bytes()); // 占位,按 class 填
        buf.extend_from_slice(&0u32.to_le_bytes()); // 对齐填充
        buf.extend_from_slice(&(dir as usize).to_le_bytes());
        buf.extend_from_slice(&((name_u16.len() * 2) as u32).to_le_bytes());
        for u in &name_u16 {
            buf.extend_from_slice(&u.to_le_bytes());
        }
        let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        // Ex 版本:flags = REPLACE_IF_EXISTS | POSIX_SEMANTICS
        let flags: u32 = FILE_RENAME_FLAG_POSIX_SEMANTICS
            | if replace_if_exists {
                FILE_RENAME_FLAG_REPLACE_IF_EXISTS
            } else {
                0
            };
        buf[0..4].copy_from_slice(&flags.to_le_bytes());
        let status_ex = unsafe {
            NtSetInformationFile(
                file,
                &mut iosb,
                buf.as_ptr() as *const core::ffi::c_void,
                buf.len() as u32,
                FILE_INFORMATION_CLASS_RENAME,
            )
        };
        if nt_success(status_ex) {
            return Ok(());
        }
        // 回退:legacy FileRenameInformation(BOOLEAN ReplaceIfExists)
        let replace: u32 = u32::from(replace_if_exists);
        buf[0..4].copy_from_slice(&replace.to_le_bytes());
        let status = unsafe {
            NtSetInformationFile(
                file,
                &mut iosb,
                buf.as_ptr() as *const core::ffi::c_void,
                buf.len() as u32,
                FILE_INFORMATION_CLASS_RENAME_LEGACY,
            )
        };
        anyhow::ensure!(
            nt_success(status),
            "句柄相对改名失败(→ {name:?}): NTSTATUS ex={status_ex:#010x} legacy={status:#010x}"
        );
        Ok(())
    }

    impl SafeDir {
        #[cfg(test)]
        pub(crate) fn handle_raw(&self) -> HANDLE {
            self.handle
        }

        /// 根是受信输入;BACKUP_SEMANTICS 打开目录句柄并验证属性。
        pub fn open_root(root: &Path) -> Result<SafeDir> {
            let wide: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    core::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    core::ptr::null_mut(),
                )
            };
            // Win32 CreateFileW 失败返回 INVALID_HANDLE_VALUE(非 null)
            anyhow::ensure!(
                handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
                "打开根目录失败 {}: {}",
                root.display(),
                last_os_error_string()
            );
            verify_real_dir(handle, &format!("根目录 {}", root.display()))?;
            Ok(SafeDir { handle })
        }

        pub fn child(&self, name: &str, create: bool) -> Result<SafeDir> {
            let disposition = if create { FILE_OPEN_IF } else { FILE_OPEN };
            let handle = nt_open_relative(
                self.handle,
                name,
                GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE,
                disposition,
                true,
                "子目录",
            )?;
            verify_real_dir(handle, &format!("子目录 {name:?}"))?;
            Ok(SafeDir { handle })
        }

        /// 原子写入:唯一随机临时名(FILE_CREATE,已存在即失败 —— 预置
        /// reparse 无法劫持)+ WriteFile + FlushFileBuffers + 句柄相对
        /// rename(替换已存在的目标)+ 目录 FlushFileBuffers。
        pub fn write_file(&self, name: &str, content: &[u8]) -> Result<()> {
            validate_component(name)?;
            let tmp = format!(".{name}.mfs-{}", crate::fs_atomic::random_txn_id());
            let tmp_handle = nt_open_relative(
                self.handle,
                &tmp,
                GENERIC_WRITE | DELETE_ACCESS | SYNCHRONIZE,
                FILE_CREATE,
                false,
                "临时文件",
            )?;
            let result = (|| -> Result<()> {
                let mut written: u32 = 0;
                let ok = unsafe {
                    WriteFile(
                        tmp_handle,
                        content.as_ptr().cast(),
                        content.len().min(u32::MAX as usize) as u32,
                        &mut written,
                        core::ptr::null_mut(),
                    )
                };
                anyhow::ensure!(ok != 0, "写入临时文件失败: {}", last_os_error_string());
                anyhow::ensure!(written as usize == content.len(), "临时文件写入不完整");
                let ok = unsafe { FlushFileBuffers(tmp_handle) };
                anyhow::ensure!(ok != 0, "临时文件落盘失败: {}", last_os_error_string());
                nt_rename(tmp_handle, self.handle, name, true)
            })();
            unsafe { windows_sys::Win32::Foundation::CloseHandle(tmp_handle) };
            result?;
            self.sync()
        }

        pub fn read_file(&self, name: &str) -> Result<Option<Vec<u8>>> {
            let (status, handle) =
                nt_open_relative_raw(self.handle, name, GENERIC_READ | SYNCHRONIZE, FILE_OPEN, false, "文件");
            if status == STATUS_OBJECT_NAME_NOT_FOUND || status == STATUS_OBJECT_PATH_NOT_FOUND {
                return Ok(None); // 不存在(单分量名:路径不存在即名字不存在)
            }
            anyhow::ensure!(
                nt_success(status),
                "句柄相对打开文件({name:?})失败: NTSTATUS {status:#010x}"
            );
            let handle: HANDLE = handle;
            let result = (|| -> Result<Option<Vec<u8>>> {
                verify_not_reparse(handle, &format!("文件 {name:?}"))?;
                let mut out = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let mut read: u32 = 0;
                    let ok = unsafe {
                        ReadFile(
                            handle,
                            buf.as_mut_ptr().cast(),
                            buf.len() as u32,
                            &mut read,
                            core::ptr::null_mut(),
                        )
                    };
                    anyhow::ensure!(ok != 0, "读取文件 {name:?} 失败: {}", last_os_error_string());
                    if read == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..read as usize]);
                }
                Ok(Some(out))
            })();
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            result
        }

        pub fn remove_file(&self, name: &str) -> Result<()> {
            let handle = nt_open_relative(
                self.handle,
                name,
                DELETE_ACCESS | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
                FILE_OPEN,
                false,
                "待删除文件",
            )?;
            let result = (|| -> Result<()> {
                verify_not_reparse(handle, &format!("待删除文件 {name:?}"))?;
                // FileDispositionInformation { BOOLEAN DeleteFile = TRUE }
                let info: [u8; 1] = [1];
                let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
                let status = unsafe {
                    NtSetInformationFile(
                        handle,
                        &mut iosb,
                        info.as_ptr() as *const core::ffi::c_void,
                        info.len() as u32,
                        FILE_INFORMATION_CLASS_DISPOSITION,
                    )
                };
                anyhow::ensure!(
                    nt_success(status),
                    "句柄相对删除失败({name:?}): NTSTATUS {status:#010x}"
                );
                Ok(())
            })();
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            result
        }

        pub fn sync(&self) -> Result<()> {
            let ok = unsafe { FlushFileBuffers(self.handle) };
            anyhow::ensure!(ok != 0, "目录 FlushFileBuffers 失败: {}", last_os_error_string());
            Ok(())
        }
    }
}

/// 已验证的目录句柄:子项读写全部句柄相对。
pub struct SafeDir(imp::SafeDir);

impl SafeDir {
    pub fn open_root(root: &Path) -> Result<SafeDir> {
        Ok(SafeDir(imp::SafeDir::open_root(root)?))
    }
    pub fn child(&self, name: &str, create: bool) -> Result<SafeDir> {
        Ok(SafeDir(self.0.child(name, create)?))
    }
    pub fn write_file(&self, name: &str, content: &[u8]) -> Result<()> {
        self.0.write_file(name, content)
    }
    pub fn read_file(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.0.read_file(name)
    }
    pub fn remove_file(&self, name: &str) -> Result<()> {
        self.0.remove_file(name)
    }
    pub fn sync(&self) -> Result<()> {
        self.0.sync()
    }
    #[cfg(test)]
    pub(crate) fn handle_raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0.handle_raw()
    }
}

/// 打开相对路径的父目录(逐级句柄相对验证;`create_dirs` 允许建立
/// 缺失的中间目录),返回(父目录句柄, 终段文件名)。
/// 这是合并应用/回滚/重放与事务日志写入的唯一寻址方式。
pub fn open_parent_for(root: &Path, rel: &str, create_dirs: bool) -> Result<(SafeDir, String)> {
    let parts = split_components(rel)?;
    anyhow::ensure!(parts.len() >= 1, "相对路径至少一个分量: {rel:?}");
    let file_name = parts[parts.len() - 1].clone();
    let mut dir = SafeDir::open_root(root)?;
    for seg in &parts[..parts.len() - 1] {
        dir = dir.child(seg, create_dirs)?;
    }
    Ok((dir, file_name))
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助:Windows 用 `mklink /J` 建 junction(无需特权);
    /// Unix 用 `std::os::unix::fs::symlink`。创建失败(权限)时跳过。
    fn make_dir_link(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            let out = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            if !out.status.success() {
                eprintln!("跳过:当前环境无法创建 junction: {}", String::from_utf8_lossy(&out.stderr));
                return false;
            }
            true
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    /// F6:目录链中的 junction/symlink 分量必须被拒绝 —— 即使词法上
    /// 是纯 normal 分量;仓库外零写入。
    #[test]
    fn junction_component_in_chain_is_rejected() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("evil.txt"), b"escaped").unwrap();
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        if !make_dir_link(&link, outside.path()) {
            return;
        }
        // 打开 link 分量:必须报错(reparse 拒绝),不得穿透
        let err = match open_parent_for(root.path(), "link/evil.txt", false) {
            Err(e) => e,
            Ok(_) => panic!("junction 分量必须被拒绝"),
        };
        assert!(
            format!("{err:#}").contains("符号链接") || format!("{err:#}").contains("接合"),
            "{err:#}"
        );
        // 已打开的根不受影响:正常文件照常读写
        let (dir, name) = open_parent_for(root.path(), "normal.txt", false).unwrap();
        dir.write_file(&name, b"ok").unwrap();
        assert_eq!(dir.read_file(&name).unwrap().as_deref(), Some(b"ok".as_slice()));
    }

    /// F6:句柄打开后的磁盘布局变化不影响后续写入的落点 ——
    /// 已验证目录句柄锚定目录 inode 本身;「验证后替换中间目录」
    /// 无法把写入重定向到仓库之外(替换本身也会因句柄占用失败)。
    #[test]
    fn verified_handle_pins_writes_after_validation() {
        let root = tempfile::tempdir().unwrap();
        let (dir, name) = open_parent_for(root.path(), "sub/inner.txt", true).unwrap();
        // 打开后试图把 sub 替换成指向外部的 junction:目录有打开句柄,
        // 删除/替换必须失败(Windows 共享语义;Unix unlink 后句柄仍锚定)
        let outside = tempfile::tempdir().unwrap();
        let sub = root.path().join("sub");
        let _replaced = make_dir_link(&sub, outside.path()); // 可能失败(句柄占用)—— 正是期望
        dir.write_file(&name, b"contained").unwrap();
        // 写入必须落在被句柄锚定的原目录里,绝不出现在 outside
        assert!(
            !outside.path().join("inner.txt").exists(),
            "写入不得被重定向到仓库之外"
        );
        // 内容可从原路径或句柄读回(原目录仍存在时两者一致)
        if sub.is_dir() && std::fs::symlink_metadata(&sub).map(|m| !m.file_type().is_symlink()).unwrap_or(false) {
            assert_eq!(std::fs::read(sub.join("inner.txt")).unwrap(), b"contained");
        }
    }

    /// F6:预置在目标名上的 reparse(目录形态 junction)必须拒绝
    /// 读写 —— 终句柄验证兜底。
    #[test]
    fn reparse_at_final_name_is_rejected_on_read() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"outside").unwrap();
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("escaped.txt");
        if !make_dir_link(&link, outside.path()) {
            return;
        }
        let dir = SafeDir::open_root(root.path()).unwrap();
        // 读:句柄相对打开 reparse 自身 → 终句柄验证拒绝
        let err = dir.read_file("escaped.txt").unwrap_err();
        assert!(
            format!("{err:#}").contains("符号链接") || format!("{err:#}").contains("接合"),
            "{err:#}"
        );
        // 写:替换语义把链接本身替换为真实文件(链接文件在 repo 内被
        // 原子覆盖,绝不穿越)—— 无论拒绝还是替换,外部字节不得被动,
        // 且替换后的终态必须不是 reparse
        let write_result = dir.write_file("escaped.txt", b"evil");
        if write_result.is_ok() {
            let meta = std::fs::symlink_metadata(root.path().join("escaped.txt")).unwrap();
            assert!(
                !meta.file_type().is_symlink(),
                "替换后的终态不得仍是符号链接/接合点"
            );
        }
        assert_eq!(
            std::fs::read(outside.path().join("secret.txt")).unwrap(),
            b"outside",
            "外部目标字节不得被改动(零逃逸)"
        );
    }

    /// F6:基本读写删语义 —— 嵌套创建、幂等替换、不存在读 None、删除。
    #[test]
    fn write_read_remove_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let (dir, name) = open_parent_for(root.path(), "a/b/c/file.txt", true).unwrap();
        assert_eq!(dir.read_file(&name).unwrap(), None);
        dir.write_file(&name, b"v1").unwrap();
        dir.write_file(&name, b"v2").unwrap(); // 替换已存在
        assert_eq!(dir.read_file(&name).unwrap().as_deref(), Some(b"v2".as_slice()));
        assert!(root.path().join("a/b/c/file.txt").exists());
        // 无残留临时文件
        let leftovers: Vec<_> = std::fs::read_dir(root.path().join("a/b/c"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".mfs-"))
            .collect();
        assert!(leftovers.is_empty(), "不得残留临时文件: {leftovers:?}");
        dir.remove_file(&name).unwrap();
        assert_eq!(dir.read_file(&name).unwrap(), None);
    }

    /// F6:非法分量(绝对、`..`、空段、反斜杠)在句柄层同样拒绝。
    #[test]
    fn invalid_components_rejected() {
        let root = tempfile::tempdir().unwrap();
        let backslash_path = format!("a{}b", std::path::MAIN_SEPARATOR);
        let mut bad_paths: Vec<&str> =
            vec!["../evil.txt", "/abs", "a//b", ".", "..", "a/b/.."];
        for bad in &bad_paths {
            assert!(
                open_parent_for(root.path(), bad, true).is_err(),
                "非法路径必须拒绝: {bad}"
            );
        }
        // Windows 反斜杠分隔符同样是非法分量(Unix 主分隔符则合法,
        // 不在此断言)
        if std::path::MAIN_SEPARATOR == '\\' {
            assert!(open_parent_for(root.path(), &backslash_path, true).is_err());
        }
        let _ = &mut bad_paths;
    }
}
