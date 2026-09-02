//! Windows ConPTY + Job Object 实现(自 pty_spawn 迁入,T3a)。
use super::*;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::ClosePseudoConsole;
use windows_sys::Win32::System::Console::{CreatePseudoConsole, ResizePseudoConsole, COORD};
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
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
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

    /// 真实 resize(§8.5):通知 ConPTY 新尺寸,前台 CUI 应用在下次
    /// 控制台读取时看到新值。rows/cols 为 0 视为非法(fail-closed,
    /// 不把 0 传给 OS);上界钳制防 i16 溢出。
    pub fn resize(&self, size: PtySize) -> Result<()> {
        let hpc = self
            .hpc
            .ok_or_else(|| anyhow::anyhow!("master 已关闭,resize 失败"))?;
        if size.rows == 0 || size.cols == 0 {
            anyhow::bail!(
                "resize 尺寸必须非零(rows={}, cols={})",
                size.rows,
                size.cols
            );
        }
        let rows = size.rows.min(32_767);
        let cols = size.cols.min(32_767);
        let coord = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        // S_OK = 0;ConPTY 内部会向 attached 进程传播新尺寸。
        let hr = unsafe { ResizePseudoConsole(hpc, coord) };
        if hr != 0 {
            return Err(anyhow::anyhow!("ResizePseudoConsole 失败:0x{hr:08X}"));
        }
        Ok(())
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

#[cfg(test)]
mod resize_tests {
    use super::*;

    /// 真实 resize 到达 OS:ConPTY 句柄存在时 ResizePseudoConsole 成功,
    /// 连续 resize 幂等;master 关闭后 fail-closed。
    #[test]
    fn resize_reaches_conpty_and_fails_closed_after_close() {
        let mut pair = openpty(PtySize { rows: 24, cols: 80 }).expect("openpty");
        pair.master
            .resize(PtySize {
                rows: 40,
                cols: 120,
            })
            .expect("首次 resize 应成功");
        pair.master
            .resize(PtySize {
                rows: 30,
                cols: 100,
            })
            .expect("二次 resize 应成功");
        // 尺寸 0 fail-closed(不传给 OS)
        assert!(pair.master.resize(PtySize { rows: 0, cols: 80 }).is_err());
        assert!(pair.master.resize(PtySize { rows: 24, cols: 0 }).is_err());
        pair.master.close();
        assert!(pair.master.resize(PtySize { rows: 24, cols: 80 }).is_err());
    }
}
