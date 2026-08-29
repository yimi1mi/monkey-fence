//! 原生 PTY 启动封装(Windows ConPTY;其他平台回退 portable-pty)。
//!
//! 取代 portable-pty 直接使用的两个关键缺口:
//! - **进程树所有权**(C5):Windows 用 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//!   的 Job Object 拥有整个进程树(CREATE_SUSPENDED → 挂 job → 恢复,
//!   杜绝孙进程在挂载前派生的竞态);stop 先 TerminateJobObject 再等
//!   job 清空。`cmd /c npm → node` 风格的孙进程不再逃逸。
//! - **可 zeroize 的 spawn 环境块**(I10):环境以 UTF-16 块一次性构造
//!   (父环境 + 覆盖 + Secret),Secret 明文只经 zeroize 缓冲进入
//!   `CreateProcessW`,不产生 portable-pty `CommandBuilder` 内部的
//!   普通 OsString 长期副本。所有 launch 路径统一走本封装。

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
/// KILL_ON_JOB_CLOSE);非 Windows 为占位。

// ---------------------------------------------------------------------------
// Windows:ConPTY + Job Object 原生实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
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

    /// UTF-16 环境块(`KEY=VALUE\0…\0\0`,drop 原地清零)。
    /// Secret 明文在宿主侧只存在于本块与 zeroizing 租约中。
    pub struct SpawnEnvBlock(Vec<u16>);

    impl SpawnEnvBlock {
        /// 由父环境 + 覆盖 + Secret 构造(键大小写不敏感去重,覆盖优先,
        /// 稳定排序)。Secret 值直接从租约字节编码为 UTF-16,不落地副本。
        pub fn build(
            overrides: &[(String, String)],
            secrets: &[(String, Arc<SecretLease>)],
        ) -> Result<SpawnEnvBlock> {
            let overridden: Vec<String> = overrides
                .iter()
                .map(|(k, _)| k.to_uppercase())
                .chain(secrets.iter().map(|(k, _)| k.to_uppercase()))
                .collect();
            let mut entries: Vec<(String, Vec<u16>)> = Vec::new();
            // 父环境(去掉被覆盖键;键统一大写比较/排序)
            for (k, v) in std::env::vars_os() {
                let k = k.to_string_lossy().into_owned();
                if overridden.contains(&k.to_uppercase()) {
                    continue;
                }
                let value: Vec<u16> = v.to_string_lossy().encode_utf16().collect();
                entries.push((k.to_uppercase(), value));
            }
            for (k, v) in overrides {
                let value: Vec<u16> = v.encode_utf16().collect();
                entries.push((k.to_uppercase(), value));
            }
            for (k, lease) in secrets {
                let value = std::str::from_utf8(lease.as_slice()).with_context(|| {
                    format!(
                        "Secret `{}` 不是有效 UTF-8,无法注入环境变量 {k}",
                        lease.id()
                    )
                })?;
                let value: Vec<u16> = value.encode_utf16().collect();
                entries.push((k.to_uppercase(), value));
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut block: Vec<u16> = Vec::new();
            for (k, v) in &entries {
                block.extend(k.encode_utf16());
                block.push('=' as u16);
                block.extend(v);
                block.push(0);
            }
            block.push(0); // 块结尾
            Ok(SpawnEnvBlock(block))
        }

        #[cfg(test)]
        pub(crate) fn raw_parts(&self) -> (*const u16, usize) {
            (self.0.as_ptr(), self.0.len())
        }
    }

    impl Drop for SpawnEnvBlock {
        fn drop(&mut self) {
            // 原地清零;已清零的缓冲不再交还分配器(泄漏全零页):
            // 分配器复用会把(哪怕已清零的)环境块重新暴露给无关分配,
            // 这里彻底杜绝任何残留路径。每次 launch 一次性开销。
            for c in self.0.iter_mut() {
                *c = 0;
            }
            std::mem::forget(std::mem::take(&mut self.0));
        }
    }

    /// Job Object 守卫:拥有整棵进程树(KILL_ON_JOB_CLOSE)。
    pub struct JobGuard {
        hjob: OwnedHandle,
    }

    unsafe impl Send for JobGuard {}
    unsafe impl Sync for JobGuard {}

    impl Clone for JobGuard {
        fn clone(&self) -> Self {
            JobGuard {
                hjob: self.hjob.duplicate().expect("克隆 job 句柄失败"),
            }
        }
    }

    impl JobGuard {
        fn create() -> Result<JobGuard> {
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
            // KILL_ON_JOB_CLOSE:最后一个句柄关闭时残余进程一并终止
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

        /// 进程树守卫(Windows 恒 Some;克隆共享同一 job)。
        pub fn job(&self) -> Option<JobGuard> {
            self.job.clone()
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

    /// 打开 ConPTY(输入写端/输出读端留在宿主侧)。
    pub fn openpty(size: PtySize) -> Result<PtyPair> {
        unsafe {
            let mut input_read: HANDLE = core::ptr::null_mut();
            let mut input_write: HANDLE = core::ptr::null_mut();
            let mut output_read: HANDLE = core::ptr::null_mut();
            let mut output_write: HANDLE = core::ptr::null_mut();
            if CreatePipe(&mut input_read, &mut input_write, core::ptr::null_mut(), 0) == 0
                || CreatePipe(
                    &mut output_read,
                    &mut output_write,
                    core::ptr::null_mut(),
                    0,
                ) == 0
            {
                return Err(anyhow::anyhow!("CreatePipe 失败"));
            }
            let input_read = OwnedHandle(input_read);
            let input_write = OwnedHandle(input_write);
            let output_read = OwnedHandle(output_read);
            let output_write = OwnedHandle(output_write);
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
            let mut cmdline = quote_windows(&cmd.program.to_string_lossy());
            for arg in &cmd.args {
                cmdline.push(' ');
                cmdline.push_str(&quote_windows(arg));
            }
            let mut cmdline_w: Vec<u16> = cmdline.encode_utf16().chain([0]).collect();
            let env = SpawnEnvBlock::build(&cmd.env, &cmd.secret_env)?;
            let cwd_w: Vec<u16> = match &cmd.cwd {
                Some(p) => p
                    .as_os_str()
                    .to_string_lossy()
                    .encode_utf16()
                    .chain([0])
                    .collect(),
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
// 非 Windows:portable-pty 回退(Secret env 仍经 CommandBuilder 普通
// OsString —— zeroize 块是 Windows 专用实现;树终止尽力而为)
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::sync::Mutex;

    pub struct PtyMaster {
        master: Box<dyn portable_pty::MasterPty + Send>,
    }

    impl PtyMaster {
        pub fn try_clone_reader(&self) -> std::io::Result<PtyReader> {
            self.master.try_clone_reader()
        }
        pub fn take_writer(&self) -> std::io::Result<PtyWriter> {
            self.master.take_writer()
        }
        pub fn close(&mut self) {
            // portable-pty master Drop 自行清理
        }
    }

    pub type PtyReader = Box<dyn Read + Send>;
    pub type PtyWriter = Box<dyn Write + Send>;

    pub struct PtyPair {
        pub master: PtyMaster,
        slave: Mutex<Option<Box<dyn portable_pty::SlavePty>>>,
    }

    pub struct PtyChild {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        pid: u32,
    }

    impl PtyChild {
        pub fn process_id(&self) -> u32 {
            self.pid
        }
        pub fn clone_killer(&self) -> Result<PtyChildKiller> {
            Ok(PtyChildKiller {
                killer: self.child.clone_killer(),
            })
        }
        pub fn job(&self) -> Option<JobGuard> {
            None
        }
        pub fn kill(&self) -> Result<()> {
            self.child.kill().map_err(|e| anyhow::anyhow!("{e}"))
        }
        pub fn wait(&self) -> Result<ExitStatus> {
            let status = self.child.wait().map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(ExitStatus {
                code: status.exit_code(),
            })
        }
    }

    pub struct PtyChildKiller {
        killer: Box<dyn portable_pty::ChildKiller + Send>,
    }

    impl PtyChildKiller {
        pub fn kill(&self) -> Result<()> {
            self.killer.kill().map_err(|e| anyhow::anyhow!("{e}"))
        }
    }

    impl JobGuard {
        pub fn terminate(&self) -> Result<()> {
            Ok(())
        }
        pub fn wait_empty(&self, _timeout: std::time::Duration) -> bool {
            true
        }
    }

    pub fn openpty(size: PtySize) -> Result<PtyPair> {
        let system = portable_pty::NativePtySystem::default();
        let pair = system.openpty(portable_pty::PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(PtyPair {
            master: PtyMaster {
                master: pair.master,
            },
            slave: Mutex::new(Some(pair.slave)),
        })
    }

    impl PtyPair {
        pub fn spawn_command(&mut self, cmd: &SpawnCommand) -> Result<PtyChild> {
            let mut builder = portable_pty::CommandBuilder::new(&cmd.program);
            for a in &cmd.args {
                builder.arg(a);
            }
            for (k, v) in &cmd.env {
                builder.env(k, v);
            }
            for (k, lease) in &cmd.secret_env {
                let value = std::str::from_utf8(lease.as_slice()).with_context(|| {
                    format!(
                        "Secret `{}` 不是有效 UTF-8,无法注入环境变量 {k}",
                        lease.id()
                    )
                })?;
                builder.env(k, value);
            }
            if let Some(cwd) = &cmd.cwd {
                builder.cwd(cwd);
            }
            let mut slave = self
                .slave
                .lock()
                .take()
                .ok_or_else(|| anyhow::anyhow!("PTY slave 已消费"))?;
            let mut child = slave.spawn_command(builder)?;
            let pid = child.process_id().unwrap_or(0);
            Ok(PtyChild { child, pid })
        }
    }
}

#[cfg(not(windows))]
pub use imp::{openpty, JobGuard, PtyChild, PtyChildKiller, PtyMaster, PtyPair};

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

    /// 环境块 drop 后宿主缓冲原地清零(明文不残留可达副本)。
    #[cfg(windows)]
    #[test]
    fn env_block_zeroizes_on_drop() {
        let _guard = test_lock().lock();
        let secret = Arc::new(SecretLease::new(
            "sec-zero",
            b"zeroize-canary-123456".to_vec(),
        ));
        let block = SpawnEnvBlock::build(&[], &[("SZ".into(), secret)]).unwrap();
        let (ptr, len) = block.raw_parts();
        let nonzero_before = unsafe { std::slice::from_raw_parts(ptr, len) }
            .iter()
            .any(|c| *c != 0);
        assert!(nonzero_before);
        drop(block);
        // drop 与检查之间不做任何分配:原缓冲必须已被清零
        let zeroized = unsafe { std::slice::from_raw_parts(ptr, len) }
            .iter()
            .all(|c| *c == 0);
        assert!(zeroized, "环境块 drop 后必须原地清零");
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
}
