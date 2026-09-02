//! Unix openpty + fork/setsid/execve 实现(自 pty_spawn 迁入,T3a)。
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
        fn kv_to_cstring(
            key: &str,
            key_bytes: &[u8],
            value: &[u8],
            what: &str,
        ) -> Result<ZeroCString> {
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
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "master 已关闭"))?
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
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "master 已关闭"))?
            .as_raw();
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(PtyWriter { fd: OwnedFd(dup) })
    }

    /// 真实 resize(§8.5):对 master fd 执行 TIOCSWINSZ,内核向
    /// 前台进程组发送 SIGWINCH。rows/cols 为 0 视为非法(fail-closed)。
    pub fn resize(&self, size: PtySize) -> Result<()> {
        let fd = self
            .fd
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("master 已关闭,resize 失败"))?
            .as_raw();
        if size.rows == 0 || size.cols == 0 {
            anyhow::bail!(
                "resize 尺寸必须非零(rows={}, cols={})",
                size.rows,
                size.cols
            );
        }
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
        if rc != 0 {
            return Err(anyhow::anyhow!(
                "TIOCSWINSZ 失败:{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
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

#[cfg(test)]
mod resize_tests {
    use super::*;

    /// 真实 resize 到达内核:重开 slave(与子进程相同的观察点)读回
    /// TIOCGWINSZ,必须看到 master.resize 的新尺寸。
    #[test]
    fn resize_reaches_kernel_winsize() {
        let pair = openpty(PtySize { rows: 24, cols: 80 }).expect("openpty");
        pair.master
            .resize(PtySize {
                rows: 40,
                cols: 120,
            })
            .expect("resize 应成功");
        let slave_fd =
            unsafe { libc::open(pair.slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        assert!(slave_fd >= 0, "重开 slave 失败");
        let mut ws: libc::winsize = std::mem::zeroed();
        let rc = unsafe { libc::ioctl(slave_fd, libc::TIOCGWINSZ, &mut ws) };
        unsafe { libc::close(slave_fd) };
        assert_eq!(rc, 0, "TIOCGWINSZ 读取失败");
        assert_eq!((ws.ws_row, ws.ws_col), (40, 120), "slave 必须看到新尺寸");

        assert!(pair.master.resize(PtySize { rows: 0, cols: 80 }).is_err());
        assert!(pair.master.resize(PtySize { rows: 24, cols: 0 }).is_err());
        let mut closed = pair;
        closed.master.close();
        assert!(closed
            .master
            .resize(PtySize { rows: 24, cols: 80 })
            .is_err());
    }
}
