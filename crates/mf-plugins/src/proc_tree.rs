//! Worker 进程树守卫(F11):Windows Job Object / Unix PGID。
//!
//! - Windows:spawn 后立即 `AssignProcessToJobObject`
//!   (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`)—— 终止用
//!   `TerminateJobObject`(整树,含 `cmd /c npm → node` 式孙进程),
//!   `QueryInformationJobObject` 轮询整树清空,全部有界且检查结果;
//! - Unix:启动时 `process_group(0)` 独立进程组,`kill(-pgid, SIGKILL)`
//!   整树 + `waitpid(WNOHANG)` 有界轮询 reap。
//! 注:Windows std spawn 无法 CREATE_SUSPENDED + 恢复(线程句柄不暴露),
//! 挂 job 存在微秒级孙进程逃逸窗口 —— 远小于 `taskkill /T` 另起进程
//! 的窗口,遗留风险已记录。

use anyhow::{Context as _, Result};
use std::process::Child;
use std::time::Duration;

/// 拥有整棵 worker 进程树的守卫。
pub struct ProcTreeGuard {
    child_pid: u32,
    #[cfg(windows)]
    job: Option<JobHandle>,
}

#[cfg(windows)]
struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for JobHandle {}
#[cfg(windows)]
unsafe impl Sync for JobHandle {}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

impl ProcTreeGuard {
    /// 在子进程 spawn 后立即建立(spawn 与挂 job 之间的窗口见模块注释)。
    pub fn attach(child: &mut Child) -> Result<ProcTreeGuard> {
        let child_pid = child.id();
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            let job = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
            anyhow::ensure!(!job.is_null(), "CreateJobObjectW 失败");
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                anyhow::bail!("SetInformationJobObject(KILL_ON_JOB_CLOSE) 失败");
            }
            let ok = unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) };
            if ok == 0 {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                anyhow::bail!(
                    "AssignProcessToJobObject 失败: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(ProcTreeGuard {
                child_pid,
                job: Some(JobHandle(job)),
            })
        }
        #[cfg(not(windows))]
        {
            let _ = &mut *child;
            Ok(ProcTreeGuard { child_pid })
        }
    }

    fn signal_tree(&self) -> Result<()> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            if let Some(handle) = &self.job {
                let ok = unsafe { TerminateJobObject(handle.0, 1) };
                anyhow::ensure!(
                    ok != 0,
                    "TerminateJobObject 失败: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let pgid = self.child_pid as libc::pid_t;
            let r = unsafe { libc::kill(-pgid, libc::SIGKILL) };
            if r != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                anyhow::bail!(
                    "kill(-pgid {pgid}, SIGKILL) 失败: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        }
    }

    fn wait_tree_empty(&self, timeout: Duration) -> Result<()> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::{
                JobObjectBasicProcessIdList, QueryInformationJobObject,
                JOBOBJECT_BASIC_PROCESS_ID_LIST,
            };
            let Some(handle) = &self.job else {
                return Ok(());
            };
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let mut list: JOBOBJECT_BASIC_PROCESS_ID_LIST = unsafe { core::mem::zeroed() };
                let ok = unsafe {
                    QueryInformationJobObject(
                        handle.0,
                        JobObjectBasicProcessIdList,
                        &mut list as *mut _ as *mut core::ffi::c_void,
                        core::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() as u32,
                        core::ptr::null_mut(),
                    )
                };
                if ok != 0 && list.NumberOfProcessIdsInList == 0 {
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("worker 进程树未在 {timeout:?} 内清空");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        #[cfg(not(windows))]
        {
            let pgid = self.child_pid as libc::pid_t;
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let r = unsafe { libc::kill(-pgid, 0) };
                if r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("worker 进程组未在 {timeout:?} 内消失");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }

    /// 兼容入口：发整树信号并等待。需要持有直接子进程的调用方应使用
    /// `kill_tree_bounded`，它会先 reap 组长再等待 PGID 清空。
    pub fn terminate_tree(&self, timeout: Duration) -> Result<()> {
        self.signal_tree()?;
        self.wait_tree_empty(timeout)
    }
}

/// 终止整树 + reap 直接子进程,全部有界且检查结果(F11)。
pub fn kill_tree_bounded(
    child: &mut Child,
    guard: Option<&ProcTreeGuard>,
    tree_timeout: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + tree_timeout;
    let mut errors = Vec::new();
    if let Some(guard) = guard {
        if let Err(e) = guard
            .signal_tree()
            .context("向 worker 进程树发终止信号失败")
        {
            errors.push(format!("{e:#}"));
        }
    }
    // 无论整树信号是否成功，都尝试杀/有界 reap 直接子进程。Unix zombie
    // 在 wait 之前仍属于 PGID，必须先 reap 再等组清空。
    match child.kill() {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {} // 已退出(std kill 语义)
        Err(e) => {
            use std::io::ErrorKind;
            if e.raw_os_error().is_none_or(|c| c != 0) && e.kind() != ErrorKind::NotFound {
                errors.push(format!("kill worker 子进程失败: {e}"));
            }
        }
    }
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                errors.push("reap worker 子进程超时".into());
                break;
            }
            Err(e) => {
                errors.push(format!("reap worker 子进程失败: {e}"));
                break;
            }
        }
    }
    if let Some(guard) = guard {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if let Err(e) = guard.wait_tree_empty(remaining) {
            errors.push(format!("{e:#}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", errors.join("; "))
    }
}
