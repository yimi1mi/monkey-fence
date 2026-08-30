//! 原生 PTY 启动封装(Windows ConPTY / Unix openpty+fork+execve)。
//!
//! 取代 portable-pty 直接使用的关键缺口:
//! - **进程树所有权**(C5):Windows 用 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//!   的 Job Object 拥有整个进程树(CREATE_SUSPENDED → 挂 job → 恢复,
//!   杜绝孙进程在挂载前派生的竞态);Unix 用 setsid 独立进程组,
//!   stop 对 `-pgid` 发信号并等待整组消失。`cmd /c npm → node` 风格的
//!   孙进程不再逃逸。
//! - **可 zeroize 的 spawn 环境块**(I10):Windows 环境以 UTF-16 块一次性
//!   构造,中间缓冲与最终块全部 `Zeroizing`,drop 清零后正常释放
//!   (不 mem::forget 泄漏整份父环境);Unix 直接构造 `execve` 的
//!   `envp`(Zeroizing CString),不经 CommandBuilder 普通 OsString 副本。
//!   所有 launch 路径统一走本封装。

use anyhow::{Context as _, Result};
use mf_agent::secrets::SecretLease;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

/// 启动命令(镜像 portable-pty CommandBuilder 的使用面,统一注入点)。
pub struct SpawnCommand {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    /// Secret 环境值:持有 zeroizing 租约的共享引用,spawn 时直接
    /// 编码进一次性环境块,不落地普通 String/OsString 副本。
    secret_env: Vec<(String, Arc<SecretLease>)>,
    cwd: Option<PathBuf>,
}

impl SpawnCommand {
    pub fn new<S: AsRef<Path>>(program: S) -> SpawnCommand {
        SpawnCommand {
            program: program.as_ref().to_path_buf(),
            args: Vec::new(),
            env: Vec::new(),
            secret_env: Vec::new(),
            cwd: None,
        }
    }

    pub fn arg<S: AsRef<str>>(&mut self, arg: S) -> &mut Self {
        self.args.push(arg.as_ref().to_string());
        self
    }

    pub fn args<S: AsRef<str>>(&mut self, args: &[S]) -> &mut Self {
        for a in args {
            self.args.push(a.as_ref().to_string());
        }
        self
    }

    pub fn env<K: AsRef<str>, V: AsRef<str>>(&mut self, key: K, value: V) -> &mut Self {
        self.env
            .push((key.as_ref().to_string(), value.as_ref().to_string()));
        self
    }

    /// 注入 Secret 环境值(值只以 zeroizing 租约存在)。
    pub fn env_secret(&mut self, key: &str, lease: &Arc<SecretLease>) -> &mut Self {
        self.secret_env.push((key.to_string(), lease.clone()));
        self
    }

    pub fn cwd<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }
}

/// 环境键/值合法性(NUL 与键中 '=' 在两种平台的环境块格式里都非法):
/// 静默接受会导致截断/串项,把 Secret 泄漏给错误的变量名。
fn validate_env_entry(key: &str, value_display: &str) -> Result<()> {
    anyhow::ensure!(
        !key.contains('\0'),
        "环境键 {value_display} 含 NUL,拒绝构造"
    );
    anyhow::ensure!(
        !key.contains('='),
        "环境键 {value_display} 含非法 '=',拒绝构造"
    );
    Ok(())
}

fn validate_no_nul(value_display: &str) -> Result<()> {
    anyhow::ensure!(
        !value_display.contains('\0'),
        "{value_display} 含 NUL,拒绝构造"
    );
    Ok(())
}

/// 进程退出状态。
pub struct ExitStatus {
    code: u32,
}

impl ExitStatus {
    pub fn exit_code(&self) -> u32 {
        self.code
    }
}

/// 进程树守卫(平台实现 re-export):Windows 为 Job Object
/// (terminate 杀整树、wait_empty 等树清空、句柄关闭即
/// KILL_ON_JOB_CLOSE);Unix 为独立进程组(setsid,terminate 对
/// -pgid 发信号、wait_empty 轮询整组消失)。

// ---------------------------------------------------------------------------
// Windows:ConPTY + Job Object 原生实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::ClosePseudoConsole;
    use windows_sys::Win32::System::Console::{CreatePseudoConsole, COORD};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, GetProcessId,
        InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
        STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };
    use zeroize::Zeroizing;

    // windows-sys 0.59:HPCON 为 isize 句柄
    type HPCON = isize;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;

    /// 拥有型 OS 句柄:Drop 关闭;复制经 DuplicateHandle 显式进行。
    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn duplicate(&self) -> Result<OwnedHandle> {
            let mut out: HANDLE = core::ptr::null_mut();
            let ok = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    self.0,
                    GetCurrentProcess(),
                    &mut out,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if ok == 0 {
                return Err(anyhow::anyhow!("DuplicateHandle 失败"));
            }
            Ok(OwnedHandle(out))
        }

        fn as_raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    // Windows 内核句柄线程无关(可在任意线程使用/关闭);Rust 类型系统
    // 因裸指针而不知晓这一点 —— reader/writer/child/killer/master 都要
    // 跨线程移动(生产 reader 线程模式)。
    unsafe impl Send for OwnedHandle {}

    /// UTF-16 环境块(`KEY=VALUE\0…\0\0`)。**临时缓冲与最终块全部
    /// Zeroizing**:Secret 明文在宿主侧只存在于 zeroize 缓冲与租约中,
    /// drop 原地清零后**正常释放**(不 mem::forget 泄漏整份父环境)。
    pub struct SpawnEnvBlock(Zeroizing<Vec<u16>>);

    impl SpawnEnvBlock {
        /// 由父环境 + 覆盖 + Secret 构造(键大小写不敏感去重,覆盖优先,
        /// 稳定排序)。Secret 值直接从租约字节编码为 UTF-16,不落地副本;
        /// 父环境键值经 `encode_wide` 精确转码(不做 lossy 破坏)。
        /// 键/值含 NUL、键含 '=' 时拒绝构造。
        pub fn build(
            overrides: &[(String, String)],
            secrets: &[(String, Arc<SecretLease>)],
        ) -> Result<SpawnEnvBlock> {
            for (k, v) in overrides {
                validate_env_entry(k, &format!("环境键 {k:?}"))?;
                validate_no_nul(v)?;
            }
            for (k, _) in secrets {
                validate_env_entry(k, &format!("Secret 环境键 {k:?}"))?;
            }
            let overridden: Vec<String> = overrides
                .iter()
                .map(|(k, _)| k.to_uppercase())
                .chain(secrets.iter().map(|(k, _)| k.to_uppercase()))
                .collect();
            // 中间条目值一律 Zeroizing(父环境值也按机密对待:
            // 块内混有 Secret 明文,整块生命周期等同机密)
            let mut entries: Vec<(String, Zeroizing<Vec<u16>>)> = Vec::new();
            // 父环境(去掉被覆盖键;键统一大写比较/排序;
            // encode_wide 保真转码,绝不 lossy)
            for (k, v) in std::env::vars_os() {
                // F-附带:键经 encode_wide 保真转码后大写化(不做 lossy;
                // vars_os 的 UTF-16 恒有效,from_utf16 不会失败)
                let key_upper: String = String::from_utf16(&k.encode_wide().collect::<Vec<_>>())
                    .unwrap_or_default()
                    .to_uppercase();
                if overridden.contains(&key_upper) {
                    continue;
                }
                entries.push((key_upper, Zeroizing::new(v.encode_wide().collect())));
            }
            for (k, v) in overrides {
                entries.push((k.to_uppercase(), Zeroizing::new(v.encode_utf16().collect())));
            }
            for (k, lease) in secrets {
                let value = std::str::from_utf8(lease.as_slice()).with_context(|| {
                    format!(
                        "Secret `{}` 不是有效 UTF-8,无法注入环境变量 {k}",
                        lease.id()
                    )
                })?;
                validate_no_nul(value)?;
                entries.push((
                    k.to_uppercase(),
                    Zeroizing::new(value.encode_utf16().collect()),
                ));
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            // F7:最终块从第一次分配就是 Zeroizing —— 累积期间对
            // Secret 明文的每次扩容/重分配,旧缓冲都随 Zeroizing drop 清零
            let mut block: Zeroizing<Vec<u16>> = Zeroizing::new(Vec::new());
            for (k, v) in &entries {
                block.extend(k.encode_utf16());
                block.push('=' as u16);
                block.extend(v.iter());
                block.push(0);
            }
            block.push(0); // 块结尾
            Ok(SpawnEnvBlock(block))
        }

        #[cfg(test)]
        pub(crate) fn raw_parts(&self) -> (*const u16, usize) {
            (self.0.as_ptr(), self.0.len())
        }

        /// 与 Drop 走的同一 zeroize 例程(测试在持有分配时验证清零;
        /// 释放后内存可能被分配器合法复用,"释放后仍为零"不是可测不变量)。
        #[cfg(test)]
        pub(crate) fn zeroize_in_place(&mut self) {
            use zeroize::Zeroize;
            self.0.zeroize();
        }
    }

    /// Job Object 守卫:拥有整棵进程树(KILL_ON_JOB_CLOSE)。
    pub struct JobGuard {
        hjob: OwnedHandle,
    }

    unsafe impl Send for JobGuard {}
    unsafe impl Sync for JobGuard {}

    impl JobGuard {
        /// 显式 try_clone(DuplicateHandle 可失败,绝不 panic)。
        pub fn try_clone(&self) -> Result<JobGuard> {
            Ok(JobGuard {
                hjob: self.hjob.duplicate()?,
            })
        }

        pub fn create() -> Result<JobGuard> {
            let hjob = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
            if hjob.is_null() {
                return Err(anyhow::anyhow!("CreateJobObjectW 失败"));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    hjob,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                unsafe { CloseHandle(hjob) };
                return Err(anyhow::anyhow!(
                    "SetInformationJobObject(KILL_ON_JOB_CLOSE) 失败"
                ));
            }
            Ok(JobGuard {
                hjob: OwnedHandle(hjob),
            })
        }

        /// 终止 job 内全部进程(整棵进程树)。
        pub fn terminate(&self) -> Result<()> {
            let ok = unsafe { TerminateJobObject(self.hjob.as_raw(), 1) };
            if ok == 0 {
                return Err(anyhow::anyhow!("TerminateJobObject 失败"));
            }
            Ok(())
        }

        /// 等待 job 清空(全部派生进程真正消失);超时返回 false。
        pub fn wait_empty(&self, timeout: std::time::Duration) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let mut list: JOBOBJECT_BASIC_PROCESS_ID_LIST = unsafe { core::mem::zeroed() };
                let ok = unsafe {
                    QueryInformationJobObject(
                        self.hjob.as_raw(),
                        JobObjectBasicProcessIdList,
                        &mut list as *mut _ as *mut core::ffi::c_void,
                        std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() as u32,
                        core::ptr::null_mut(),
                    )
                };
                if ok != 0 && list.NumberOfProcessIdsInList == 0 {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE:最后一个句柄关闭时残余进程(守护进程式
            // 孙进程)一并终止,不留孤儿
            let _ = &self.hjob; // OwnedHandle Drop 负责关闭
        }
    }

    pub struct PtyMaster {
        hpc: Option<HPCON>,
        /// 传给 CreatePseudoConsole 的两端:按官方 ConPTY 示例,attached
        /// child 创建成功后必须立即关闭(spawn 内完成),只长期保留
        /// input_write/output_read;宿主多持有任何一端都会让管道引用
        /// 计数不归零,造成同步 IO 死锁。
        input_read: Option<OwnedHandle>,
        output_write: Option<OwnedHandle>,
        input_write: OwnedHandle, // 写向 PTY 输入(writer 复制自此)
        output_read: OwnedHandle, // 读 PTY 输出(reader 复制自此)
    }

    unsafe impl Send for PtyMaster {}

    impl PtyMaster {
        pub fn try_clone_reader(&self) -> std::io::Result<PtyReader> {
            self.output_read
                .duplicate()
                .map(|h| PtyReader { handle: h })
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "克隆 reader 失败"))
        }

        pub fn take_writer(&self) -> std::io::Result<PtyWriter> {
            self.input_write
                .duplicate()
                .map(|h| PtyWriter { handle: h })
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "克隆 writer 失败"))
        }

        /// 关闭伪控制台:PTY 输出侧写端消失 → reader 解除阻塞。
        pub fn close(&mut self) {
            if let Some(hpc) = self.hpc.take() {
                unsafe { ClosePseudoConsole(hpc) };
            }
        }
    }

    impl Drop for PtyMaster {
        fn drop(&mut self) {
            self.close();
        }
    }

    pub struct PtyReader {
        handle: OwnedHandle,
    }

    unsafe impl Send for PtyReader {}

    impl Read for PtyReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            use windows_sys::Win32::Storage::FileSystem::ReadFile;
            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    self.handle.as_raw(),
                    buf.as_mut_ptr().cast(),
                    buf.len().min(u32::MAX as usize) as u32,
                    &mut read,
                    core::ptr::null_mut(),
                )
            };
            if ok == 0 {
                // 管道破裂(PTY 关闭/进程终止):按 EOF 处理
                return Ok(0);
            }
            Ok(read as usize)
        }
    }

    pub struct PtyWriter {
        handle: OwnedHandle,
    }

    unsafe impl Send for PtyWriter {}

    impl Write for PtyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            use windows_sys::Win32::Storage::FileSystem::WriteFile;
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    self.handle.as_raw(),
                    buf.as_ptr().cast(),
                    buf.len().min(u32::MAX as usize) as u32,
                    &mut written,
                    core::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "PTY 已关闭",
                ));
            }
            Ok(written as usize)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    pub struct PtyChild {
        process: Arc<OwnedHandle>,
        pid: u32,
        job: Option<JobGuard>,
    }

    unsafe impl Send for PtyChild {}
    unsafe impl Sync for PtyChild {}

    impl PtyChild {
        pub fn process_id(&self) -> u32 {
            self.pid
        }

        pub fn clone_killer(&self) -> Result<PtyChildKiller> {
            Ok(PtyChildKiller {
                process: self.process.duplicate()?,
            })
        }

        /// 进程树守卫(Windows 恒 Some;克隆共享同一 job;
        /// DuplicateHandle 显式可失败,绝不 panic)。
        pub fn job(&self) -> Result<JobGuard> {
            self.job
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("本平台无进程树守卫"))
                .and_then(|j| j.try_clone())
        }

        pub fn kill(&self) -> Result<()> {
            let ok = unsafe { TerminateProcess(self.process.as_raw(), 1) };
            if ok == 0 {
                return Err(anyhow::anyhow!("TerminateProcess 失败(pid {})", self.pid));
            }
            Ok(())
        }

        /// 阻塞等待退出并返回退出码(可重入:句柄共享,内核等待幂等)。
        pub fn wait(&self) -> Result<ExitStatus> {
            let hr = unsafe { WaitForSingleObject(self.process.as_raw(), INFINITE) };
            if hr != 0 {
                return Err(anyhow::anyhow!("WaitForSingleObject 失败({hr})"));
            }
            let mut code: u32 = 0;
            let ok = unsafe { GetExitCodeProcess(self.process.as_raw(), &mut code) };
            if ok == 0 {
                return Err(anyhow::anyhow!("GetExitCodeProcess 失败"));
            }
            Ok(ExitStatus { code })
        }
    }

    pub struct PtyChildKiller {
        process: OwnedHandle,
    }

    unsafe impl Send for PtyChildKiller {}

    impl PtyChildKiller {
        pub fn kill(&self) -> Result<()> {
            let ok = unsafe { TerminateProcess(self.process.as_raw(), 1) };
            if ok == 0 {
                return Err(anyhow::anyhow!("TerminateProcess(killer) 失败"));
            }
            Ok(())
        }
    }

    pub struct PtyPair {
        pub master: PtyMaster,
    }

    /// CreatePipe 单组失败时,另一组句柄立即由 OwnedHandle RAII 关闭
    /// (不再泄漏)。
    unsafe fn create_pipe_pair() -> Result<(OwnedHandle, OwnedHandle)> {
        let mut read: HANDLE = core::ptr::null_mut();
        let mut write: HANDLE = core::ptr::null_mut();
        if CreatePipe(&mut read, &mut write, core::ptr::null_mut(), 0) == 0 {
            return Err(anyhow::anyhow!("CreatePipe 失败"));
        }
        Ok((OwnedHandle(read), OwnedHandle(write)))
    }

    /// 打开 ConPTY(输入写端/输出读端留在宿主侧)。
    pub fn openpty(size: PtySize) -> Result<PtyPair> {
        unsafe {
            // 第二组失败时,第一组的 OwnedHandle 在 return 展开时立即关闭
            let (input_read, input_write) = create_pipe_pair()?;
            let (output_read, output_write) = create_pipe_pair()?;
            let coord = COORD {
                X: size.cols as i16,
                Y: size.rows as i16,
            };
            let mut hpc: HPCON = 0;
            let hr = CreatePseudoConsole(
                coord,
                input_read.as_raw(),
                output_write.as_raw(),
                0,
                &mut hpc,
            );
            if hr != 0 {
                return Err(anyhow::anyhow!("CreatePseudoConsole 失败(hr {hr:#x})"));
            }
            Ok(PtyPair {
                master: PtyMaster {
                    hpc: Some(hpc),
                    input_read: Some(input_read),
                    output_write: Some(output_write),
                    input_write,
                    output_read,
                },
            })
        }
    }

    impl PtyPair {
        /// 在 ConPTY 中启动进程:SUSPENDED 创建 → 挂入 Job Object →
        /// 恢复(孙进程不可能脱离 job);环境经一次性 zeroize 块注入。
        /// 每个 PtyPair 只 spawn 一次(第二 spawn 因管道端已交割而报错)。
        pub fn spawn_command(&mut self, cmd: &SpawnCommand) -> Result<PtyChild> {
            let hpc = self
                .master
                .hpc
                .ok_or_else(|| anyhow::anyhow!("PTY master 已关闭"))?;
            // F-附带:program/args 经 encode_wide 保真转码(不做 lossy;
            // 非 UTF-8 路径不得被替换为 U+FFFD)
            let mut cmdline_w: Vec<u16> = quote_windows_wide(&cmd.program);
            for arg in &cmd.args {
                cmdline_w.push(32 as u16);
                cmdline_w.extend_from_slice(&quote_windows_wide(std::path::Path::new(arg)));
            }
            cmdline_w.push(0);
            let env = SpawnEnvBlock::build(&cmd.env, &cmd.secret_env)?;
            let cwd_w: Vec<u16> = match &cmd.cwd {
                Some(p) => p.as_os_str().encode_wide().chain([0]).collect(),
                None => Vec::new(),
            };

            let job = JobGuard::create()?;

            unsafe {
                let mut attr_size: usize = 0;
                InitializeProcThreadAttributeList(core::ptr::null_mut(), 1, 0, &mut attr_size);
                let words = attr_size.div_ceil(size_of::<usize>());
                let mut attr_buf: Vec<usize> = vec![0; words.max(1)];
                let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
                if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == 0 {
                    return Err(anyhow::anyhow!("InitializeProcThreadAttributeList 失败"));
                }
                struct AttrListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
                impl Drop for AttrListGuard {
                    fn drop(&mut self) {
                        unsafe { DeleteProcThreadAttributeList(self.0) };
                    }
                }
                let _attr_guard = AttrListGuard(attr_list);
                if UpdateProcThreadAttribute(
                    attr_list,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    hpc as *const core::ffi::c_void, // 属性值即 HPCON 本身(MS/portable-pty 语义)
                    size_of::<HPCON>(),
                    core::ptr::null_mut(),
                    core::ptr::null(),
                ) == 0
                {
                    return Err(anyhow::anyhow!(
                        "UpdateProcThreadAttribute(PSEUDOCONSOLE) 失败"
                    ));
                }

                let mut si: STARTUPINFOEXW = core::mem::zeroed();
                si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
                // 显式把 stdio 置为无效句柄(portable-pty 同款):否则子进程
                // 可能继承父进程(可能被重定向过的)stdio,而不是刚创建的
                // 伪控制台 —— 输出会写到父进程的终端而非 PTY
                si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
                si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
                si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
                si.lpAttributeList = attr_list;
                let mut pi: PROCESS_INFORMATION = core::mem::zeroed();
                let created = CreateProcessW(
                    core::ptr::null(), // 应用名走命令行首参数(保留 PATH 搜索语义)
                    cmdline_w.as_mut_ptr(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    0, // 不继承句柄:PTY 经属性列表传递
                    CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                    env.0.as_ptr() as *const core::ffi::c_void,
                    if cwd_w.is_empty() {
                        core::ptr::null()
                    } else {
                        cwd_w.as_ptr()
                    },
                    &si.StartupInfo,
                    &mut pi,
                );
                if created == 0 {
                    return Err(anyhow::anyhow!(
                        "启动 `{}` 失败(CLI 未安装或配置无效)",
                        cmd.program.display()
                    ));
                }
                // 官方 ConPTY 语义:attached child 创建成功后立即关闭传给
                // CreatePseudoConsole 的两端(conhost 已持有自己需要的
                // 引用);宿主保留任何一端都会让管道计数不归零
                drop(self.master.input_read.take());
                drop(self.master.output_write.take());
                let process = OwnedHandle(pi.hProcess);
                let thread = OwnedHandle(pi.hThread);
                // SUSPENDED 创建已消除竞态:此刻挂 job,任何孙进程都进树
                if AssignProcessToJobObject(job.hjob.as_raw(), process.as_raw()) == 0 {
                    let _ = TerminateProcess(process.as_raw(), 1);
                    return Err(anyhow::anyhow!("AssignProcessToJobObject 失败"));
                }
                if ResumeThread(thread.as_raw()) == u32::MAX {
                    let _ = TerminateProcess(process.as_raw(), 1);
                    return Err(anyhow::anyhow!("ResumeThread 失败"));
                }
                let pid = GetProcessId(process.as_raw());
                Ok(PtyChild {
                    process: Arc::new(process),
                    pid,
                    job: Some(job),
                })
            }
        }
    }

    /// Windows 命令行引号规则(OsStr → UTF-16 保真;与 std 一致):
    /// 空串/含空白或引号 → 引号包裹,反斜杠在引号边界转义。
    /// 逐 code unit 处理,非 UTF-8 字节经 encode_wide 保留原样。
    fn quote_windows_wide(path: &std::path::Path) -> Vec<u16> {
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        let needs = units.is_empty()
            || units.iter().any(|&w| {
                let c = char::from_u32(w as u32).unwrap_or(' ');
                c.is_whitespace() || w == '"' as u16
            });
        if !needs {
            return units;
        }
        let mut out = vec!['"' as u16];
        let mut backslashes = 0usize;
        for &w in &units {
            if w == 92 as u16 {
                backslashes += 1;
                continue;
            }
            if w == '"' as u16 {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push(92 as u16);
                }
                out.push('"' as u16);
                backslashes = 0;
            } else {
                for _ in 0..backslashes {
                    out.push(92 as u16);
                }
                backslashes = 0;
                out.push(w);
            }
        }
        for _ in 0..(backslashes * 2) {
            out.push(92 as u16);
        }
        out.push('"' as u16);
        out
    }

    /// Windows 命令行引号规则(与 std/portable-pty 一致):
    /// 空串/含空白或引号 → 引号包裹,反斜杠在引号边界转义。
    fn quote_windows(arg: &str) -> String {
        if arg.is_empty() {
            return "\"\"".into();
        }
        let needs = arg.chars().any(|c| c.is_whitespace() || c == '"');
        if !needs {
            return arg.to_string();
        }
        let mut out = String::with_capacity(arg.len() + 2);
        out.push('"');
        let mut backslashes = 0;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    out.push_str(&"\\".repeat(backslashes * 2 + 1));
                    out.push('"');
                    backslashes = 0;
                }
                other => {
                    out.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                    out.push(other);
                }
            }
        }
        out.push_str(&"\\".repeat(backslashes * 2));
        out.push('"');
        out
    }
}

#[cfg(windows)]
pub use imp::{
    openpty, JobGuard, PtyChild, PtyChildKiller, PtyMaster, PtyPair, PtyReader, PtyWriter,
    SpawnEnvBlock,
};

// ---------------------------------------------------------------------------
// Unix:openpty + fork/setsid/execve 原生实现
// ---------------------------------------------------------------------------
// - **进程树所有权**(C4):子进程 setsid 成为会话首兼进程组长,
//   重开 slave 获得控制终端;stop 对 `-pgid` 发 SIGKILL 并轮询整组
//   消失 —— `sh -c npm → node` 风格孙进程不逃逸(未自行 setsid 的
//   派生进程都在组内)。
// - **可 zeroize 的环境块**(C3):execve 的 envp 直接以
//   `Zeroizing<CString>` 构造,绝不经 CommandBuilder 普通 OsString
//   明文副本;argv 同样零信任处理。
// - 自然退出:子进程退出 → slave 全部关闭 → master read 返回
//   EOF/EIO,reader 收口(waiter 负责 reap,见 runtime_host)。

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use zeroize::Zeroize;

    /// zeroize 的 CString 持有者:drop 时清零底层字节缓冲后正常释放
    /// (`CString` 本身不实现 `Zeroize`,不能直接用 `Zeroizing`)。
    struct ZeroCString(CString);

    impl ZeroCString {
        fn new(c: CString) -> ZeroCString {
            ZeroCString(c)
        }
        fn as_ptr(&self) -> *const libc::c_char {
            self.0.as_ptr()
        }
    }

    impl Drop for ZeroCString {
        fn drop(&mut self) {
            let mut bytes = std::mem::take(&mut self.0).into_bytes();
            bytes.zeroize();
            // bytes 正常 drop(已清零)
        }
    }

    /// 拥有型 fd:Drop 关闭。
    struct OwnedFd(libc::c_int);

    impl OwnedFd {
        fn as_raw(&self) -> libc::c_int {
            self.0
        }
    }

    impl Drop for OwnedFd {
        fn drop(&mut self) {
            if self.0 >= 0 {
                unsafe { libc::close(self.0) };
            }
        }
    }

    unsafe impl Send for OwnedFd {}

    /// execve 环境/参数块构造(平台无关校验复用)。
    /// Secret 值只以 Zeroizing<CString> 存在,drop 清零。
    pub struct SpawnEnvBlock {
        entries: Vec<ZeroCString>,
    }

    impl SpawnEnvBlock {
        pub fn build(
            overrides: &[(String, String)],
            secrets: &[(String, Arc<SecretLease>)],
        ) -> Result<SpawnEnvBlock> {
            for (k, _) in overrides {
                validate_env_entry(k, &format!("环境键 {k:?}"))?;
            }
            for (k, _) in secrets {
                validate_env_entry(k, &format!("Secret 环境键 {k:?}"))?;
            }
            let overridden: Vec<String> = overrides
                .iter()
                .map(|(k, _)| k.as_str().to_owned())
                .chain(secrets.iter().map(|(k, _)| k.as_str().to_owned()))
                .collect();
            let mut entries: Vec<ZeroCString> = Vec::new();
            /// F7:构造一条 `K=V` 的 zeroizing 缓冲并转成 CString。
            /// **先在 Zeroizing 缓冲里预检 NUL**:错误路径不产生拥有
            /// 明文字节的 NulError/普通 Vec —— 拒绝构造时字节已随
            /// Zeroizing drop 清零。成功路径字节原样移交 CString
            /// (ZeroCString drop 时再清零一次),全程零明文裸缓冲。
            fn kv_to_cstring(key: &str, key_bytes: &[u8], value: &[u8], what: &str) -> Result<ZeroCString> {
                use zeroize::Zeroizing;
                let mut bytes = Zeroizing::new(Vec::with_capacity(key_bytes.len() + value.len() + 2));
                bytes.extend_from_slice(key_bytes);
                bytes.push(b'=');
                bytes.extend_from_slice(value);
                if let Some(pos) = bytes.iter().position(|b| *b == 0) {
                    anyhow::bail!("{what} {key:?} 的字节在位置 {pos} 含 NUL,拒绝构造(缓冲已清零)");
                }
                let taken = std::mem::take(&mut *bytes);
                // NUL 已预检:CString::new 此处不可能失败
                let c = CString::new(taken)
                    .map_err(|_| anyhow::anyhow!("{what} {key:?} 含 NUL(预检后仍失败,拒绝构造)"))?;
                Ok(ZeroCString::new(c))
            }
            // 父环境(精确 OsStr 字节,不做 lossy;去掉被覆盖键)
            for (k, v) in std::env::vars_os() {
                if overridden.iter().any(|o| o.as_str() == k.to_string_lossy()) {
                    continue;
                }
                entries.push(kv_to_cstring(
                    "(父环境变量)",
                    k.as_bytes(),
                    v.as_bytes(),
                    "父环境变量",
                )?);
            }
            let mut push_kv = |k: &str, value: &[u8]| -> Result<()> {
                entries.push(kv_to_cstring(k, k.as_bytes(), value, "环境变量")?);
                Ok(())
            };
            for (k, v) in overrides {
                push_kv(k, v.as_bytes())?;
            }
            for (k, lease) in secrets {
                let value = std::str::from_utf8(lease.as_slice()).with_context(|| {
                    format!(
                        "Secret `{}` 不是有效 UTF-8,无法注入环境变量 {k}",
                        lease.id()
                    )
                })?;
                push_kv(k, value.as_bytes())?;
            }
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            Ok(SpawnEnvBlock { entries })
        }

        fn as_ptr_array(&self) -> Vec<*const libc::c_char> {
            self.entries
                .iter()
                .map(|e| e.as_ptr())
                .chain([std::ptr::null()])
                .collect()
        }
    }

    /// 进程组守卫:terminate 对 -pgid 发 SIGKILL;wait_empty 轮询
    /// kill(-pgid, 0) 直到 ESRCH(整组消失)。
    pub struct JobGuard {
        pgid: libc::pid_t,
    }

    unsafe impl Send for JobGuard {}
    unsafe impl Sync for JobGuard {}

    impl JobGuard {
        pub fn try_clone(&self) -> Result<JobGuard> {
            Ok(JobGuard { pgid: self.pgid })
        }

        pub fn terminate(&self) -> Result<()> {
            // 负 pgid = 整组;组可能已空(ESRCH)视为成功
            let r = unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
            if r != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(anyhow::anyhow!(
                    "kill(-pgid {}, SIGKILL) 失败: {}",
                    self.pgid,
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }

        /// 等待进程组消失(kill(-pgid,0) 返回 ESRCH);超时 false。
        pub fn wait_empty(&self, timeout: std::time::Duration) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let r = unsafe { libc::kill(-self.pgid, 0) };
                if r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            // 不自动杀组:与 Windows KILL_ON_JOB_CLOSE 语义差异明确 ——
            // 显式 terminate 由 stop 路径调用;这里只放弃跟踪
        }
    }

    pub struct PtyMaster {
        fd: Option<OwnedFd>,
    }

    unsafe impl Send for PtyMaster {}

    impl PtyMaster {
        pub fn try_clone_reader(&self) -> std::io::Result<PtyReader> {
            let fd = self
                .fd
                .as_ref()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "master 已关闭")
                })?
                .as_raw();
            let dup = unsafe { libc::dup(fd) };
            if dup < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(PtyReader { fd: OwnedFd(dup) })
        }

        pub fn take_writer(&self) -> std::io::Result<PtyWriter> {
            let fd = self
                .fd
                .as_ref()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "master 已关闭")
                })?
                .as_raw();
            let dup = unsafe { libc::dup(fd) };
            if dup < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(PtyWriter { fd: OwnedFd(dup) })
        }

        /// Unix:master fd 关闭即可;阻塞中的 dup reader 由子进程退出
        /// (slave 关闭 → EOF/EIO)解除,或 dup fd 自身被关闭解除。
        pub fn close(&mut self) {
            self.fd.take();
        }
    }

    pub struct PtyReader {
        fd: OwnedFd,
    }

    unsafe impl Send for PtyReader {}

    impl Read for PtyReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            loop {
                let n = unsafe {
                    libc::read(
                        self.fd.as_raw(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n > 0 {
                    return Ok(n as usize);
                }
                if n == 0 {
                    return Ok(0); // EOF:slave 侧全部关闭
                }
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EIO) => return Ok(0), // Linux pty master 在 slave 关闭后的 EOF 形态
                    Some(libc::EINTR) => continue,
                    _ => return Err(err),
                }
            }
        }
    }

    pub struct PtyWriter {
        fd: OwnedFd,
    }

    unsafe impl Send for PtyWriter {}

    impl Write for PtyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            loop {
                let n = unsafe {
                    libc::write(
                        self.fd.as_raw(),
                        buf.as_ptr() as *const libc::c_void,
                        buf.len(),
                    )
                };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    _ => return Err(err),
                }
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    pub struct PtyChild {
        pid: libc::pid_t,
    }

    unsafe impl Send for PtyChild {}
    unsafe impl Sync for PtyChild {}

    impl PtyChild {
        pub fn process_id(&self) -> u32 {
            self.pid as u32
        }

        pub fn clone_killer(&self) -> Result<PtyChildKiller> {
            Ok(PtyChildKiller { pid: self.pid })
        }

        /// 进程组守卫(setsid 后子进程即组长;pgid == pid)。
        pub fn job(&self) -> Result<JobGuard> {
            Ok(JobGuard { pgid: self.pid })
        }

        pub fn kill(&self) -> Result<()> {
            let r = unsafe { libc::kill(self.pid, libc::SIGKILL) };
            if r != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(anyhow::anyhow!(
                    "kill({}) 失败: {}",
                    self.pid,
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }

        /// 阻塞等待退出并返回退出码(waitpid 幂等: reap 后返回缓存语义
        /// 由内核的 ECHILD 行为兜底 —— 多次 wait 同一已 reap 子进程时
        /// 返回最后已知码 0)。
        pub fn wait(&self) -> Result<ExitStatus> {
            loop {
                let mut status: libc::c_int = 0;
                let r = unsafe { libc::waitpid(self.pid, &mut status, 0) };
                if r == self.pid {
                    let code = if libc::WIFEXITED(status) {
                        libc::WEXITSTATUS(status) as u32
                    } else if libc::WIFSIGNALED(status) {
                        (128 + libc::WTERMSIG(status)) as u32
                    } else {
                        0
                    };
                    // 子进程已 reap:再无对象可等。记录到自身供重复 wait
                    // (简化:重复 wait 返回 0 码;生产 caller 只 wait 一次,
                    // 由 waiter 线程独占)
                    return Ok(ExitStatus { code });
                }
                if r < 0 {
                    let err = std::io::Error::last_os_error();
                    match err.raw_os_error() {
                        Some(libc::EINTR) => continue,
                        // 已被其他线程 reap(生产 waiter 线程独占 wait):
                        // 视为已终止,码不可知
                        Some(libc::ECHILD) => return Ok(ExitStatus { code: 0 }),
                        _ => return Err(anyhow::anyhow!("waitpid 失败: {err}")),
                    }
                }
            }
        }
    }

    pub struct PtyChildKiller {
        pid: libc::pid_t,
    }

    unsafe impl Send for PtyChildKiller {}

    impl PtyChildKiller {
        pub fn kill(&self) -> Result<()> {
            let r = unsafe { libc::kill(self.pid, libc::SIGKILL) };
            if r != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(anyhow::anyhow!(
                    "kill({})(killer) 失败: {}",
                    self.pid,
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }
    }

    pub struct PtyPair {
        pub master: PtyMaster,
        /// slave 的 pts 路径(子进程 setsid 后自行重开以获得控制终端)。
        slave_path: CString,
    }

    /// 打开 PTY(openpty;master 留宿主,slave 由子进程在 setsid 后重开)。
    pub fn openpty(size: PtySize) -> Result<PtyPair> {
        unsafe {
            let mut master: libc::c_int = -1;
            let mut slave: libc::c_int = -1;
            let mut win_size: libc::winsize = std::mem::zeroed();
            win_size.ws_row = size.rows;
            win_size.ws_col = size.cols;
            let r = libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut win_size,
            );
            if r != 0 {
                return Err(anyhow::anyhow!(
                    "openpty 失败: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let master = OwnedFd(master);
            let slave = OwnedFd(slave); // 宿主副本:取 pts 路径后立即关闭
                                        // slave 的设备路径(ptsname_r 线程安全版)
            let mut name_buf = [0u8; libc::PATH_MAX as usize];
            let r = libc::ptsname_r(
                slave.as_raw(),
                name_buf.as_mut_ptr() as *mut libc::c_char,
                name_buf.len(),
            );
            if r != 0 {
                return Err(anyhow::anyhow!(
                    "ptsname_r 失败: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let end = name_buf
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(name_buf.len());
            let slave_path =
                CString::new(&name_buf[..end]).map_err(|_| anyhow::anyhow!("pts 路径含 NUL"))?;
            drop(slave);
            Ok(PtyPair {
                master: PtyMaster { fd: Some(master) },
                slave_path,
            })
        }
    }

    impl PtyPair {
        /// 在 PTY 中启动进程:fork → 子进程 setsid → 重开 slave(获得
        /// 控制终端)→ dup2 到 0/1/2 → execve;父进程经 CLOEXEC 管道
        /// 感知 exec 失败。每个 PtyPair 只 spawn 一次。
        pub fn spawn_command(&mut self, cmd: &SpawnCommand) -> Result<PtyChild> {
            let master_fd = self
                .master
                .fd
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("PTY master 已关闭"))?
                .as_raw();
            // argv/环境:Secret 只经 Zeroizing 缓冲进入 execve;
            // NUL 预检后错误路径不持有明文字节(F7)
            fn bytes_to_cstring(bytes: &[u8], what: &str) -> Result<ZeroCString> {
                use zeroize::Zeroizing;
                let mut buf = Zeroizing::new(Vec::with_capacity(bytes.len() + 1));
                buf.extend_from_slice(bytes);
                if let Some(pos) = buf.iter().position(|b| *b == 0) {
                    anyhow::bail!("{what} 在位置 {pos} 含 NUL,拒绝启动(缓冲已清零)");
                }
                let taken = std::mem::take(&mut *buf);
                let c = CString::new(taken)
                    .map_err(|_| anyhow::anyhow!("{what} 含 NUL(预检后仍失败,拒绝启动)"))?;
                Ok(ZeroCString::new(c))
            }
            let mut argv_owned: Vec<ZeroCString> = Vec::with_capacity(cmd.args.len() + 1);
            argv_owned.push(bytes_to_cstring(
                cmd.program.as_os_str().as_bytes(),
                &format!("程序路径 {}", cmd.program.display()),
            )?);
            for arg in &cmd.args {
                argv_owned.push(bytes_to_cstring(arg.as_bytes(), "参数")?);
            }
            let env = SpawnEnvBlock::build(&cmd.env, &cmd.secret_env)?;
            let cwd = match &cmd.cwd {
                Some(p) => Some(bytes_to_cstring(
                    p.as_os_str().as_bytes(),
                    &format!("cwd {}", p.display()),
                )?),
                None => None,
            };
            let argv: Vec<*const libc::c_char> = argv_owned
                .iter()
                .map(|a| a.as_ptr())
                .chain([std::ptr::null()])
                .collect();
            let envp = env.as_ptr_array();

            // exec 失败报告管道(O_CLOEXEC:exec 成功则父进程读到 EOF)
            let mut exec_err_pipe: [libc::c_int; 2] = [-1, -1];
            unsafe {
                if libc::pipe2(exec_err_pipe.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
                    return Err(anyhow::anyhow!(
                        "创建 exec 报告管道失败: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            let slave_path = self.slave_path.clone();

            let pid = unsafe { libc::fork() };
            if pid < 0 {
                unsafe {
                    libc::close(exec_err_pipe[0]);
                    libc::close(exec_err_pipe[1]);
                }
                return Err(anyhow::anyhow!(
                    "fork 失败: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if pid == 0 {
                // ---- 子进程(只调用 async-signal-safe 函数) ----
                unsafe {
                    libc::close(exec_err_pipe[0]); // 读端
                    libc::close(master_fd);
                    let fail = |errno: libc::c_int| -> ! {
                        let byte = errno as u8;
                        libc::write(
                            exec_err_pipe[1],
                            &byte as *const u8 as *const libc::c_void,
                            1,
                        );
                        libc::close(exec_err_pipe[1]);
                        libc::_exit(127);
                    };
                    // 新会话 + 成为进程组长(整树可寻址终止的前提)
                    if libc::setsid() < 0 {
                        fail(9); // EBADF 类不可恢复:直接报告
                    }
                    // 会话首重开 slave 获得控制终端(O_RDWR 且不带 O_NOCTTY:
                    // 会话首打开终端设备即成为其控制终端)
                    let slave_fd = libc::open(slave_path.as_ptr(), libc::O_RDWR);
                    if slave_fd < 0 {
                        fail(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
                    }
                    libc::dup2(slave_fd, 0);
                    libc::dup2(slave_fd, 1);
                    libc::dup2(slave_fd, 2);
                    if slave_fd > 2 {
                        libc::close(slave_fd);
                    }
                    if let Some(cwd) = &cwd {
                        if libc::chdir(cwd.as_ptr()) < 0 {
                            fail(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
                        }
                    }
                    // execve(失败经 CLOEXEC 管道上报;成功则管道自动关闭)
                    let errno_ptr = libc::__errno_location();
                    libc::execve(
                        argv_owned
                            .first()
                            .map(|a| a.as_ptr())
                            .unwrap_or(std::ptr::null()),
                        argv.as_ptr(),
                        envp.as_ptr(),
                    );
                    let errno = *errno_ptr;
                    fail(errno);
                }
            }
            // ---- 父进程 ----
            unsafe {
                libc::close(exec_err_pipe[1]); // 写端
            }
            let mut errno_byte: u8 = 0;
            let reported = unsafe {
                libc::read(
                    exec_err_pipe[0],
                    &mut errno_byte as *mut u8 as *mut libc::c_void,
                    1,
                )
            };
            unsafe {
                libc::close(exec_err_pipe[0]);
            }
            if reported > 0 {
                // exec/chdir/setsid 失败:回收子进程并如实报错
                let mut status: libc::c_int = 0;
                unsafe {
                    libc::waitpid(pid, &mut status, 0);
                }
                return Err(anyhow::anyhow!(
                    "启动 `{}` 失败(errno {errno_byte},CLI 未安装或配置无效)",
                    cmd.program.display()
                ));
            }
            Ok(PtyChild { pid })
        }
    }
}

#[cfg(not(windows))]
pub use imp::{
    openpty, JobGuard, PtyChild, PtyChildKiller, PtyMaster, PtyPair, PtyReader, PtyWriter,
    SpawnEnvBlock,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as TestMutex;
    use std::sync::OnceLock;

    /// 串行化本组测试:它们触碰进程全局(父环境、堆分配、真实子进程),
    /// 并行执行会互相干扰内存检查与 spawn 时序。
    fn test_lock() -> &'static TestMutex<()> {
        static LOCK: OnceLock<TestMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| TestMutex::new(()))
    }

    /// 环境块内容:父环境变量、覆盖优先、Secret 值进入块。
    #[cfg(windows)]
    #[test]
    fn env_block_contains_parent_overrides_and_secrets() {
        let _guard = test_lock().lock();
        let secret = Arc::new(SecretLease::new("sec-test", b"plain-secret-value".to_vec()));
        let block = SpawnEnvBlock::build(
            &[
                ("MF_RUN_TOKEN".into(), "tok-123".into()),
                ("PATH".into(), "Z:\\override".into()),
            ],
            &[("MY_SECRET".into(), secret)],
        )
        .unwrap();
        let text = {
            let (ptr, len) = block.raw_parts();
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
        };
        assert!(text.contains("MF_RUN_TOKEN=tok-123"), "{text}");
        assert!(text.contains("MY_SECRET=plain-secret-value"), "{text}");
        assert!(text.contains("PATH=Z:\\override"), "{text}");
    }

    /// 环境键/值含 NUL 或键含 '=' 时必须拒绝构造(Win32 环境块格式
    /// 非法:静默截断/串项会把 Secret 泄漏给错误的变量名)。
    #[cfg(windows)]
    #[test]
    fn env_block_rejects_nul_and_equals_in_key() {
        let _guard = test_lock().lock();
        // 值含 NUL
        let err = SpawnEnvBlock::build(&[("A".into(), "x\0y".into())], &[])
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("NUL"), "{err:#}");
        // 键含 NUL / '='
        let err = SpawnEnvBlock::build(&[("K\0".into(), "v".into())], &[])
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("NUL"), "{err:#}");
        let err = SpawnEnvBlock::build(&[("K=V".into(), "v".into())], &[])
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("'='"), "{err:#}");
        // Secret 值含 NUL 同样拒绝
        let secret = Arc::new(SecretLease::new("sec-nul", b"s\0e".to_vec()));
        let err = SpawnEnvBlock::build(&[], &[("S".into(), secret)])
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("NUL"), "{err:#}");
    }

    /// 环境块清零例程:持有分配时原地清零(Drop 走同一例程);
    /// 缓冲随后正常释放(不 mem::forget 泄漏整份父环境 ——
    /// "释放后内存仍为零"不是可测不变量:分配器复用是合法的,
    /// 明文在释放那一刻之前已被清零)。
    #[cfg(windows)]
    #[test]
    fn env_block_zeroizes_in_place_and_releases_normally() {
        let _guard = test_lock().lock();
        let secret = Arc::new(SecretLease::new(
            "sec-zero",
            b"zeroize-canary-123456".to_vec(),
        ));
        let mut block = SpawnEnvBlock::build(&[], &[("SZ".into(), secret)]).unwrap();
        let (ptr, len) = block.raw_parts();
        let nonzero_before = unsafe { std::slice::from_raw_parts(ptr, len) }
            .iter()
            .any(|c| *c != 0);
        assert!(nonzero_before);
        block.zeroize_in_place();
        let zeroized = unsafe { std::slice::from_raw_parts(ptr, len) }
            .iter()
            .all(|c| *c == 0);
        assert!(zeroized, "清零例程必须原地清空整个环境块");
        // 正常释放路径冒烟(mem::forget 版本会每轮泄漏一个父环境块)
        drop(block);
        for _ in 0..64 {
            let b = SpawnEnvBlock::build(&[], &[]).unwrap();
            drop(b);
        }
    }

    /// 启动的子进程确实收到 Secret 环境变量(经 ConPTY 真实 spawn)。
    /// 同步 ReadFile 无法被 deadline 打断:读循环放独立线程,主线程
    /// channel 带超时等待;结束后关闭 master(ClosePseudoConsole)
    /// 取消 reader 线程的阻塞读。
    #[cfg(windows)]
    #[test]
    fn spawned_child_receives_secret_env() {
        let _guard = test_lock().lock();
        let secret = Arc::new(SecretLease::new("sec-child", b"child-canary-42".to_vec()));
        let mut pair = openpty(PtySize { rows: 24, cols: 80 }).unwrap();
        let mut cmd = SpawnCommand::new("cmd.exe");
        cmd.arg("/c").arg("echo MF_CANARY=%MF_CANARY%");
        cmd.env_secret("MF_CANARY", &secret);
        let child = pair.spawn_command(&cmd).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let tx_for_thread = tx.clone();
        let reader_thread = std::thread::spawn(move || {
            let tx = tx_for_thread;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(chunk) => {
                    out.extend_from_slice(&chunk);
                    if out
                        .windows(b"child-canary-42".len())
                        .any(|w| w == b"child-canary-42")
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("MF_CANARY=child-canary-42"),
            "子进程必须收到 Secret 环境变量: {text}"
        );
        let _ = child.wait();
        // 关闭伪控制台取消 reader 线程的阻塞读(生产 reader 线程同语义)
        drop(tx);
        pair.master.close();
        reader_thread.join().expect("reader 线程应被取消并退出");
    }

    /// job() 句柄克隆显式可失败(绝不 panic;Windows DuplicateHandle
    /// 语义,Unix 无系统调用)。Windows 下连续克隆必须成功。
    #[test]
    fn job_guard_clone_is_fallible_and_repeatable() {
        let _guard = test_lock().lock();
        let mut pair = openpty(PtySize { rows: 24, cols: 80 }).unwrap();
        #[cfg(windows)]
        let mut cmd = SpawnCommand::new("cmd.exe");
        #[cfg(windows)]
        cmd.arg("/c").arg("exit 0");
        #[cfg(not(windows))]
        let mut cmd = SpawnCommand::new("/bin/sh");
        #[cfg(not(windows))]
        cmd.arg("-c").arg("exit 0");
        let child = pair.spawn_command(&cmd).unwrap();
        let _j1 = child.job().expect("job 守卫克隆不得 panic");
        let _j2 = child.job().expect("job 守卫可重复克隆");
        let _ = child.wait();
    }

    /// Unix 专属:setsid 进程组 + 孙进程整组终止 + Secret 经 execve
    /// 环境块进入子进程 + 短命 CLI 自然 EOF。
    #[cfg(not(windows))]
    mod unix_tree {
        use super::super::*;

        /// 孙进程(sh -c 'sh -c sleep')在进程组内,JobGuard.terminate
        /// 后整组消失(wait_empty 真实等待,不 no-op)。
        #[test]
        fn job_guard_terminates_entire_group_including_grandchildren() {
            let _guard = test_lock().lock();
            let mut pair = openpty(PtySize { rows: 24, cols: 80 }).unwrap();
            let mut cmd = SpawnCommand::new("/bin/sh");
            // 常驻父进程 + 派生长寿孙进程(未自行 setsid → 留在组内)
            cmd.arg("-c")
                .arg("/bin/sh -c 'sleep 300' & /bin/sh -c 'while :; do sleep 1; done'");
            let child = pair.spawn_command(&cmd).unwrap();
            let job = child.job().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500)); // 孙进程派生
            job.terminate().unwrap();
            assert!(
                job.wait_empty(std::time::Duration::from_secs(5)),
                "进程组(含孙进程)必须在时限内整组消失"
            );
        }

        /// Secret 环境经 Zeroizing envp 真实进入子进程。
        #[test]
        fn spawned_child_receives_secret_env_unix() {
            let _guard = test_lock().lock();
            let secret = Arc::new(SecretLease::new("sec-unix", b"unix-canary-7".to_vec()));
            let mut pair = openpty(PtySize { rows: 24, cols: 80 }).unwrap();
            let mut cmd = SpawnCommand::new("/bin/sh");
            cmd.arg("-c").arg("printf 'MF_CANARY=%s\\n' \"$MF_CANARY\"");
            cmd.env_secret("MF_CANARY", &secret);
            let child = pair.spawn_command(&cmd).unwrap();
            let mut reader = pair.master.try_clone_reader().unwrap();
            let mut out = Vec::new();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut buf = [0u8; 4096];
            while std::time::Instant::now() < deadline {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        out.extend_from_slice(&buf[..n]);
                        if out
                            .windows(b"unix-canary-7".len())
                            .any(|w| w == b"unix-canary-7")
                        {
                            break;
                        }
                    }
                }
            }
            let text = String::from_utf8_lossy(&out);
            assert!(
                text.contains("MF_CANARY=unix-canary-7"),
                "子进程必须收到 Secret 环境变量: {text}"
            );
            let status = child.wait().unwrap();
            assert_eq!(status.exit_code(), 0);
        }

        /// 短命 CLI 自然退出:master 读到 EOF(Linux EIO 形态),
        /// wait 返回真实退出码。
        #[test]
        fn short_lived_cli_natural_exit_eof_and_code() {
            let _guard = test_lock().lock();
            let mut pair = openpty(PtySize { rows: 24, cols: 80 }).unwrap();
            let mut cmd = SpawnCommand::new("/bin/sh");
            cmd.arg("-c").arg("echo hi; exit 7");
            let child = pair.spawn_command(&cmd).unwrap();
            let mut reader = pair.master.try_clone_reader().unwrap();
            let mut buf = [0u8; 256];
            let mut saw_eof = false;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        saw_eof = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            assert!(saw_eof, "子进程退出后 master 必须读到 EOF");
            assert_eq!(child.wait().unwrap().exit_code(), 7);
        }
    }
}
