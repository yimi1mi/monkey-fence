//! 原生 PTY 启动封装(Windows ConPTY / Unix openpty+fork+execve)。
//!
//! T3a(Issue #29)自 `crates/mf/src/pty_spawn.rs` 迁入 mf-terminal;
//! 平台实现位于 `windows.rs`/`unix.rs`,本文件持有公共契约。
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
pub use imp_common::{
    openpty, JobGuard, PtyChild, PtyChildKiller, PtyMaster, PtyPair, PtyReader, PtyWriter,
    SpawnEnvBlock,
};

mod imp_common {
    #[cfg(not(windows))]
    pub use super::unix::{
        openpty, JobGuard, PtyChild, PtyChildKiller, PtyMaster, PtyPair, PtyReader, PtyWriter,
        SpawnEnvBlock,
    };
    #[cfg(windows)]
    pub use super::windows::{
        openpty, JobGuard, PtyChild, PtyChildKiller, PtyMaster, PtyPair, PtyReader, PtyWriter,
        SpawnEnvBlock,
    };
}

#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
mod unix;

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
