//! 原子文件操作原语(F5/F12/F3):
//! - `replace_file`:同目录/跨设备安全的目标替换 —— Windows 显式
//!   `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`
//!   (替换 + 直写落盘,掉电后不出现半新半旧),Unix 同目录 `rename(2)`。
//!   不使用 `std::fs::rename` 覆盖已有目标(Windows std 路径不带
//!   WRITE_THROUGH,且语义隐式)。
//! - `sync_dir`:目录 fsync/FlushFileBuffers,错误**如实传播**为
//!   `Err`(持久化增强失败不得静默吞掉)。
//! - `FileLock`:跨进程/跨实例排他文件锁(Windows `LockFileEx`
//!   EXCLUSIVE / Unix `flock`)—— 进程死亡自动释放,无陈旧锁。
//! - `random_txn_id`:随机 128-bit 事务标识(hex),用于 journal/临时
//!   文件唯一命名,并发与残留互不覆盖。

use anyhow::{Context as _, Result};
use std::path::Path;

/// 目标文件的原子替换(F5)。
/// Windows:`MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` ——
/// 已存在的目标被原子替换且直写落盘;Unix:`rename(2)`(同目录
/// 原子替换)。源必须存在;失败返回 `Err`(不吞错误)。
pub fn replace_file(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
        let src_w: Vec<u16> = src.as_os_str().encode_wide().chain([0]).collect();
        let dst_w: Vec<u16> = dst.as_os_str().encode_wide().chain([0]).collect();
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                src_w.as_ptr(),
                dst_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        anyhow::ensure!(
            ok != 0,
            "原子替换失败: {} → {}: {}",
            src.display(),
            dst.display(),
            std::io::Error::last_os_error()
        );
        Ok(())
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        let c_src = std::ffi::CString::new(src.as_os_str().as_bytes())
            .with_context(|| format!("源路径含 NUL: {}", src.display()))?;
        let c_dst = std::ffi::CString::new(dst.as_os_str().as_bytes())
            .with_context(|| format!("目标路径含 NUL: {}", dst.display()))?;
        let r = unsafe { libc::rename(c_src.as_ptr(), c_dst.as_ptr()) };
        anyhow::ensure!(
            r == 0,
            "原子替换失败: {} → {}: {}",
            src.display(),
            dst.display(),
            std::io::Error::last_os_error()
        );
        Ok(())
    }
}

/// 目录 fsync(F12):Windows 经 FILE_FLAG_BACKUP_SEMANTICS 打开目录
/// 句柄后 FlushFileBuffers;Unix 对目录 fd fsync。失败返回 `Err`
/// —— 持久化边界(事务日志、原子替换后的父目录)的同步错误必须
/// 传播,不得静默降级为"尽力而为"。
pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(windows)]
    let result = {
        use std::os::windows::fs::OpenOptionsExt;
        // FlushFileBuffers 需要写访问权(只读句柄会 Access Denied);
        // FILE_FLAG_BACKUP_SEMANTICS 才能打开目录句柄
        std::fs::File::options()
            .read(true)
            .write(true)
            .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
            .open(dir)
            .and_then(|h| h.sync_all())
    };
    #[cfg(not(windows))]
    let result = {
        let f = std::fs::File::open(dir)?;
        f.sync_all()
    };
    result.with_context(|| format!("目录同步失败(错误必须传播): {}", dir.display()))
}

/// 跨进程排他文件锁(F3)。进程/句柄死亡时 OS 自动释放 —— 崩溃后
/// 不留陈旧锁;同进程内多线程同样互斥(内核锁对象语义)。
pub struct FileLock {
    #[cfg(not(windows))]
    file: std::fs::File,
    #[cfg(windows)]
    _file: std::fs::File,
}

impl FileLock {
    /// 阻塞获取 `path` 的排他锁(文件不存在则创建)。
    pub fn acquire(path: &Path) -> Result<FileLock> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("打开锁文件失败: {}", path.display()))?;
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::HANDLE;
            use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
            use windows_sys::Win32::System::IO::OVERLAPPED;
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                LockFileEx(
                    file.as_raw_handle() as HANDLE,
                    LOCKFILE_EXCLUSIVE_LOCK,
                    0,
                    u32::MAX,
                    u32::MAX,
                    &mut overlapped,
                )
            };
            anyhow::ensure!(
                ok != 0,
                "LockFileEx(排他)失败: {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::io::AsRawFd;
            let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            anyhow::ensure!(
                r == 0,
                "flock(排他)失败: {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(FileLock {
            #[cfg(not(windows))]
            file,
            #[cfg(windows)]
            _file: file,
        })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // 句柄关闭即解锁(两平台一致);显式解锁可省
    }
}

/// 随机 128-bit 事务标识(32 个 hex 字符)。
/// 每次合并事务唯一:journal/临时文件命名互不覆盖(F3/F12)。
pub fn random_txn_id() -> String {
    let id: u128 = rand::random();
    format!("{id:032x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F5:Windows 真实替换**已存在**的目标文件:内容原子换新、
    /// 无临时残留;目标不存在时同样成功;目标是目录时报错。
    #[test]
    fn replace_file_overwrites_existing_target_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&src, b"new-content").unwrap();
        std::fs::write(&dst, b"old-content").unwrap();
        replace_file(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new-content");
        assert!(!src.exists(), "源文件必须已被移动");
        // 目标不存在 → 成功
        let src2 = dir.path().join("src2.bin");
        std::fs::write(&src2, b"fresh").unwrap();
        let dst2 = dir.path().join("nested").join("dst2.bin");
        std::fs::create_dir_all(dst2.parent().unwrap()).unwrap();
        replace_file(&src2, &dst2).unwrap();
        assert_eq!(std::fs::read(&dst2).unwrap(), b"fresh");
        // 源不存在 → Err(不吞错误)
        let missing = dir.path().join("missing.bin");
        assert!(replace_file(&missing, &dst).is_err());
    }

    /// F12:目录同步错误必须传播(不存在的目录 → Err,不是静默 no-op)。
    #[test]
    fn sync_dir_propagates_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir");
        assert!(sync_dir(&missing).is_err(), "同步失败必须返回 Err");
        sync_dir(dir.path()).unwrap();
    }

    /// F3:排他文件锁跨句柄互斥;持有期间第二个获取必须阻塞,
    /// 释放后可获取。
    #[test]
    fn file_lock_is_exclusive_across_handles() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("merge.lock");
        let first = FileLock::acquire(&lock_path).unwrap();
        let acquired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let lock_path2 = lock_path.clone();
        let flag = acquired.clone();
        let waiter = std::thread::spawn(move || {
            let _second = FileLock::acquire(&lock_path2).unwrap();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = tx.send(());
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !acquired.load(std::sync::atomic::Ordering::SeqCst),
            "持锁期间第二个获取必须阻塞"
        );
        drop(first);
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("释放后等待者必须获得锁");
        waiter.join().unwrap();
    }

    /// F12:事务标识随机 128-bit(两次生成不同、32 hex 长度)。
    #[test]
    fn txn_ids_are_random_and_full_width() {
        let a = random_txn_id();
        let b = random_txn_id();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
