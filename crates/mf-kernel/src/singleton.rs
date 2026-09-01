//! CoreOwnerLock、owner epoch 与 stale discovery fencing(canonical spec
//! §2.3/§2.4 L-OWNER/§11.1)。
//!
//! 每 OS 用户一个 Core 的所有权由两层共同保证:OS 级 per-user 互斥
//! (Windows `Local\MonkeyFence.Core` 命名 mutex / Unix flock)是跨进程
//! 仲裁者;`core.lock`(pid、owner epoch、build)持久化 owner 身份与
//! epoch 水位,`discovery.json`(instance id、port、pid、build、heartbeat
//! 时间戳)供 launcher/tray 发现。L-OWNER = 原子 acquire OS 互斥 +
//! 更新 lock/discovery 记录,由 [`CoreOwnerLock::acquire`] 一次完成。
//!
//! stale discovery fencing(§11.1):新 owner 仅在「前任 pid 不存在,或
//! discovery 心跳已过期(3×heartbeat,派生)」且互斥可取时接管;pid 存活
//! 且心跳新鲜的记录永远阻止接管(不误杀活 Core)。接管后 owner epoch
//! 单调 +1;被接管(陈旧)的 owner 此后既不能更新 discovery,也不能释放
//! 新 owner 的锁——它的一切写路径先复验 lock 文件 epoch 与 discovery
//! 归属,一旦失配即被永久 fencing。
//!
//! 有序释放(§2.3 `stores_closed → handed_off`):release 只接受
//! [`StoresClosedProof`],该证明只能由 [`ShutdownFlow`] 按
//! freezing → draining → stores_closed 顺序产生;释放顺序固定为
//! 标记 lock released(保留 epoch 水位)→ 移除 discovery → 最后释放 OS
//! 互斥。freeze/drain 的真实语义(拒绝新 command、publication barrier
//! 等待)属 shutdown.rs(后续 ticket),此处固化顺序与证明契约。
//!
//! 平台缝隙(OwnerMutexSource/OwnerClock/ProcessLivenessProbe)全部有
//! 确定性 fake,跨进程真实路径由 `owner_lock_probe` 二进制与
//! `tests/contract/owner_lock.rs` 验证。本模块不接管 `crates/mf` AppCtx
//! 的任何权威状态(T1e dark data);真实生产路径由 standalone Core(T6)装配。

use crate::limits::LifecycleLimits;
use crate::project_registry::ServiceStore;
use chrono::{DateTime, SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Windows per-user 命名 mutex(§11.1;`Local\` 命名空间,生产固定名)。
pub const CORE_MUTEX_NAME: &str = r"Local\MonkeyFence.Core";
/// owner 身份水位文件名(`~/.monkeyfence/core.lock`)。
pub const CORE_LOCK_FILE_NAME: &str = "core.lock";
/// discovery 文件名(平台 per-user 目录下)。
pub const DISCOVERY_FILE_NAME: &str = "discovery.json";
/// Unix 平台的互斥文件名(flock;与 lock 记录文件分离)。
pub const CORE_FLOCK_FILE_NAME: &str = "core.flock";
/// 启动竞争的 acquire 超时(败者快速失败向胜者转发 open 意图,§11.1;
/// 工程默认,非 A7 参数)。
pub const DEFAULT_ACQUIRE_TIMEOUT_MS: u64 = 500;

// ─────────────────────────── 平台缝隙:时钟 ───────────────────────────

/// owner 侧时间源(heartbeat 时间戳与 stale 判定)。
pub trait OwnerClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// 真实系统时钟。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemOwnerClock;

impl OwnerClock for SystemOwnerClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// 确定性 fake 时钟:显式推进,不随真实时间流动。
#[derive(Debug)]
pub struct FakeOwnerClock {
    now: Mutex<DateTime<Utc>>,
}

impl FakeOwnerClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    pub fn advance_ms(&self, ms: i64) {
        let mut now = self.now.lock();
        *now += chrono::Duration::milliseconds(ms);
    }
}

impl OwnerClock for FakeOwnerClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock()
    }
}

// ─────────────────────── 平台缝隙:进程存活探针 ───────────────────────

/// pid 存活探针(stale fencing 的「旧 pid 不存在」判据)。
/// 不可判定时按存活处理(fencing 保守方向)。
pub trait ProcessLivenessProbe: Send + Sync {
    fn is_alive(&self, pid: u32) -> bool;
}

/// 真实 OS 探针:Windows `OpenProcess`/`GetExitCodeProcess`;Unix `kill(0)`。
#[derive(Debug, Clone, Copy, Default)]
pub struct OsProcessLivenessProbe;

#[cfg(windows)]
impl ProcessLivenessProbe for OsProcessLivenessProbe {
    fn is_alive(&self, pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            // 打不开:ERROR_INVALID_PARAMETER(87)表示进程已退出;
            // 权限不足等其它原因按存活(保守,不误判活 Core)。
            let code = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or_default();
            return code != 0 && code != 87;
        }
        let mut exit_code: u32 = 0;
        let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        unsafe {
            CloseHandle(handle);
        }
        queried != 0 && exit_code == STILL_ACTIVE as u32
    }
}

#[cfg(unix)]
impl ProcessLivenessProbe for OsProcessLivenessProbe {
    fn is_alive(&self, pid: u32) -> bool {
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result == 0 {
            return true;
        }
        // EPERM:进程存在但无权限发信号 → 按存活。
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// 确定性 fake 探针:默认全部死亡,`mark_alive` 显式标记存活。
#[derive(Debug, Default)]
pub struct FakeProcessLivenessProbe {
    alive: Mutex<HashSet<u32>>,
}

impl FakeProcessLivenessProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_alive(&self, pid: u32) {
        self.alive.lock().insert(pid);
    }

    pub fn mark_dead(&self, pid: u32) {
        self.alive.lock().remove(&pid);
    }
}

impl ProcessLivenessProbe for FakeProcessLivenessProbe {
    fn is_alive(&self, pid: u32) -> bool {
        self.alive.lock().contains(&pid)
    }
}

// ──────────────────────── 平台缝隙:owner 互斥 ────────────────────────

/// OS 互斥持有凭证:drop 即释放(持有者进程死亡时由 OS 回收)。
pub trait OwnerMutexGuard: Send {
    /// 返回的 permit 覆盖「检查 epoch → 写/删记录」整个 publication；
    /// fake 用 generation registry lock，真实平台由 OS mutex/flock 保证。
    fn publication_permit(&self) -> Result<Box<dyn OwnerPublicationPermit + '_>, OwnerLockError>;
}

pub trait OwnerPublicationPermit {}

struct OsOwnerPublicationPermit;
impl OwnerPublicationPermit for OsOwnerPublicationPermit {}

/// per-user owner 互斥源。跨进程唯一仲裁;abandoned(持有者死亡)视为可取。
pub trait OwnerMutexSource: Send + Sync {
    /// 尝试持有;超时/已被持有 → [`OwnerLockError::MutexHeld`]。
    fn acquire(&self, timeout: Duration) -> Result<Box<dyn OwnerMutexGuard>, OwnerLockError>;
}

/// Windows 命名 mutex(真实跨进程;同一线程对已持有的 mutex 重入会成功,
/// 进程内二次 acquire 的防线是 fencing 检查——见模块文档)。
#[cfg(windows)]
pub struct WindowsNamedOwnerMutex {
    name: String,
}

#[cfg(windows)]
impl WindowsNamedOwnerMutex {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[cfg(windows)]
impl OwnerMutexSource for WindowsNamedOwnerMutex {
    fn acquire(&self, timeout: Duration) -> Result<Box<dyn OwnerMutexGuard>, OwnerLockError> {
        let timeout_ms = wait_timeout_ms(timeout);
        let name = self.name.clone();
        let owned = Arc::new(AtomicBool::new(false));
        let thread_owned = owned.clone();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        // Win32 mutex ownership属于获取它的线程。专用线程同时 acquire /
        // ReleaseMutex，使 CoreOwnerLock 可安全跨线程持有和 drop。
        let thread = std::thread::Builder::new()
            .name("mf-core-owner-mutex".into())
            .spawn(move || {
                use windows_sys::Win32::Foundation::{
                    CloseHandle, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
                };
                use windows_sys::Win32::Security::Authorization::{
                    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                };
                use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
                use windows_sys::Win32::System::Threading::{
                    CreateMutexW, ReleaseMutex, WaitForSingleObject,
                };
                let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
                let sddl: Vec<u16> = "D:P(A;;GA;;;OW)\0".encode_utf16().collect();
                let mut descriptor = std::ptr::null_mut();
                let converted = unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        sddl.as_ptr(),
                        SDDL_REVISION_1,
                        &mut descriptor,
                        std::ptr::null_mut(),
                    )
                };
                if converted == 0 {
                    let _ = acquired_tx.send(Err(OwnerLockError::io(
                        "构造 owner mutex DACL",
                        std::io::Error::last_os_error(),
                    )));
                    return;
                }
                let attributes = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                };
                let handle = unsafe { CreateMutexW(&attributes, 0, wide.as_ptr()) };
                unsafe { LocalFree(descriptor) };
                if handle.is_null() {
                    let _ = acquired_tx.send(Err(OwnerLockError::io(
                        "CreateMutexW",
                        std::io::Error::last_os_error(),
                    )));
                    return;
                }
                let wait = unsafe {
                    WaitForSingleObject(handle, timeout_ms.min(u64::from(u32::MAX)) as u32)
                };
                match wait {
                    WAIT_OBJECT_0 | WAIT_ABANDONED => {
                        thread_owned.store(true, Ordering::Release);
                        if acquired_tx.send(Ok(())).is_ok() {
                            let _ = release_rx.recv();
                        }
                        thread_owned.store(false, Ordering::Release);
                        unsafe {
                            ReleaseMutex(handle);
                            CloseHandle(handle);
                        }
                    }
                    WAIT_TIMEOUT => {
                        unsafe { CloseHandle(handle) };
                        let _ = acquired_tx.send(Err(OwnerLockError::MutexHeld { timeout_ms }));
                    }
                    _ => {
                        let error = std::io::Error::last_os_error();
                        unsafe { CloseHandle(handle) };
                        let _ =
                            acquired_tx.send(Err(OwnerLockError::io("WaitForSingleObject", error)));
                    }
                }
            })
            .map_err(|error| OwnerLockError::io("启动 owner mutex 线程", error))?;
        match acquired_rx.recv() {
            Ok(Ok(())) => Ok(Box::new(WindowsOwnerMutexHandle {
                release: Some(release_tx),
                thread: Some(thread),
                owned,
            })),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                let _ = thread.join();
                Err(OwnerLockError::io(
                    "等待 owner mutex 线程",
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, error.to_string()),
                ))
            }
        }
    }
}

#[cfg(windows)]
struct WindowsOwnerMutexHandle {
    release: Option<std::sync::mpsc::SyncSender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    owned: Arc<AtomicBool>,
}

#[cfg(windows)]
impl Drop for WindowsOwnerMutexHandle {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
impl OwnerMutexGuard for WindowsOwnerMutexHandle {
    fn publication_permit(&self) -> Result<Box<dyn OwnerPublicationPermit + '_>, OwnerLockError> {
        if !self.owned.load(Ordering::Acquire) {
            return Err(OwnerLockError::MutexOwnershipLost);
        }
        Ok(Box::new(OsOwnerPublicationPermit))
    }
}

/// Unix flock(真实跨进程:独立的 open file description 互相排斥,
/// 持有者死亡时内核自动释放)。
#[cfg(unix)]
pub struct UnixFlockOwnerMutex {
    path: PathBuf,
}

#[cfg(unix)]
impl UnixFlockOwnerMutex {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[cfg(unix)]
impl OwnerMutexSource for UnixFlockOwnerMutex {
    fn acquire(&self, timeout: Duration) -> Result<Box<dyn OwnerMutexGuard>, OwnerLockError> {
        use std::os::unix::io::AsRawFd;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| OwnerLockError::io("创建 flock 目录", error))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|error| OwnerLockError::io("打开 flock 文件", error))?;
        crate::platform_acl::restrict_current_user_only(&self.path)
            .map_err(|error| OwnerLockError::io("收紧 flock 文件 ACL", error))?;
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked == 0 {
            return Ok(Box::new(UnixOwnerMutexHandle { _file: file }));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(OwnerLockError::MutexHeld {
                timeout_ms: wait_timeout_ms(timeout),
            });
        }
        Err(OwnerLockError::io("flock", error))
    }
}

#[cfg(unix)]
struct UnixOwnerMutexHandle {
    _file: std::fs::File,
}

#[cfg(unix)]
impl OwnerMutexGuard for UnixOwnerMutexHandle {
    fn publication_permit(&self) -> Result<Box<dyn OwnerPublicationPermit + '_>, OwnerLockError> {
        Ok(Box::new(OsOwnerPublicationPermit))
    }
}

/// 确定性 fake 互斥:进程内按名字真互斥(跨实例、不重入),用于
/// 非 Windows 平台与注入式场景的契约测试。
pub struct FakeOwnerMutex {
    name: String,
}

impl FakeOwnerMutex {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    fn registry() -> &'static Mutex<HashMap<String, u64>> {
        static REGISTRY: std::sync::OnceLock<Mutex<HashMap<String, u64>>> =
            std::sync::OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// 模拟持有者死亡/持有线程退出后 OS 回收互斥:名字立即变为可取,
    /// 但已发出的 guard 对象仍留在持有者手里(它若继续误用写路径,
    /// 必须被 fencing 拦下)。
    pub fn simulate_os_reclaim(&self) {
        Self::registry().lock().remove(&self.name);
    }

    pub fn is_held(&self) -> bool {
        Self::registry().lock().contains_key(&self.name)
    }
}

impl OwnerMutexSource for FakeOwnerMutex {
    fn acquire(&self, _timeout: Duration) -> Result<Box<dyn OwnerMutexGuard>, OwnerLockError> {
        let mut registry = Self::registry().lock();
        if registry.contains_key(&self.name) {
            return Err(OwnerLockError::MutexHeld { timeout_ms: 0 });
        }
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        registry.insert(self.name.clone(), generation);
        Ok(Box::new(FakeOwnerMutexGuard {
            name: self.name.clone(),
            generation,
        }))
    }
}

struct FakeOwnerMutexGuard {
    name: String,
    generation: u64,
}

impl Drop for FakeOwnerMutexGuard {
    fn drop(&mut self) {
        let mut registry = FakeOwnerMutex::registry().lock();
        if registry.get(&self.name) == Some(&self.generation) {
            registry.remove(&self.name);
        }
    }
}

struct FakeOwnerPublicationPermit<'a> {
    _registry: parking_lot::MutexGuard<'a, HashMap<String, u64>>,
}

impl OwnerPublicationPermit for FakeOwnerPublicationPermit<'_> {}

impl OwnerMutexGuard for FakeOwnerMutexGuard {
    fn publication_permit(&self) -> Result<Box<dyn OwnerPublicationPermit + '_>, OwnerLockError> {
        let registry = FakeOwnerMutex::registry().lock();
        if registry.get(&self.name) != Some(&self.generation) {
            return Err(OwnerLockError::MutexOwnershipLost);
        }
        Ok(Box::new(FakeOwnerPublicationPermit {
            _registry: registry,
        }))
    }
}

fn wait_timeout_ms(timeout: Duration) -> u64 {
    timeout.as_millis().min(u128::from(u32::MAX)) as u64
}

/// 生产平台互斥:Windows 命名 mutex / Unix flock / 其它平台 fake 兜底
/// (仅保证可编译契约;首发平台见 §1.3 支持矩阵)。
pub fn platform_owner_mutex(mutex_name: &str, flock_path: &Path) -> Box<dyn OwnerMutexSource> {
    #[cfg(windows)]
    let _ = flock_path;
    #[cfg(unix)]
    let _ = mutex_name;
    #[cfg(windows)]
    {
        Box::new(WindowsNamedOwnerMutex::new(mutex_name))
    }
    #[cfg(unix)]
    {
        Box::new(UnixFlockOwnerMutex::new(flock_path))
    }
    #[cfg(not(any(windows, unix)))]
    {
        Box::new(FakeOwnerMutex::new(mutex_name))
    }
}

// ─────────────────────────── 路径与记录 ───────────────────────────

/// owner 锁与 discovery 的文件落点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerLockPaths {
    /// 生产:`~/.monkeyfence/core.lock`(pid、owner epoch、build)。
    pub lock_path: PathBuf,
    /// 生产:平台 per-user 目录(§11.1:Windows `%LOCALAPPDATA%\MonkeyFence`、
    /// Linux `~/.local/state/monkeyfence`、macOS
    /// `~/Library/Application Support/MonkeyFence`)。
    pub discovery_path: PathBuf,
}

impl OwnerLockPaths {
    /// 生产路径(§11.1)。
    pub fn platform_default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            lock_path: home.join(".monkeyfence").join(CORE_LOCK_FILE_NAME),
            discovery_path: platform_discovery_path(),
        }
    }

    /// 测试/工具:同一目录内的 `core.lock` + `discovery.json`。
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            lock_path: dir.join(CORE_LOCK_FILE_NAME),
            discovery_path: dir.join(DISCOVERY_FILE_NAME),
        }
    }

    /// Unix flock 互斥文件(与 lock 记录分离;Windows 不使用)。
    pub fn flock_path(&self) -> PathBuf {
        self.lock_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(CORE_FLOCK_FILE_NAME)
    }
}

#[cfg(windows)]
fn platform_discovery_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MonkeyFence")
        .join(DISCOVERY_FILE_NAME)
}

#[cfg(target_os = "linux")]
fn platform_discovery_path() -> PathBuf {
    dirs::state_dir()
        .or_else(|| dirs::data_local_dir())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("monkeyfence")
        .join(DISCOVERY_FILE_NAME)
}

#[cfg(target_os = "macos")]
fn platform_discovery_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MonkeyFence")
        .join(DISCOVERY_FILE_NAME)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_discovery_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".monkeyfence")
        .join(DISCOVERY_FILE_NAME)
}

/// `core.lock` 记录(§11.1:pid、owner epoch、build;`released` 是本实现
/// 的交接水位标记——干净释放后保留 epoch 供下一次递增)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreLockRecord {
    pub pid: u32,
    pub owner_epoch: u64,
    pub build: String,
    pub released: bool,
}

/// `discovery.json` 记录(§11.1:instance id、port、pid、build、heartbeat
/// 时间戳;heartbeat 为 RFC3339 毫秒精度)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryRecord {
    pub instance_id: String,
    pub port: u16,
    pub pid: u32,
    pub build: String,
    pub heartbeat_at: String,
}

// ───────────────────────────── 错误 ─────────────────────────────

/// owner 锁操作的稳定错误码全集(供 launcher/status 分支;web problem
/// 映射在 WebGateway ticket 落位)。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OwnerLockError {
    /// OS 互斥在超时内不可取:另一 Core 持有(或未释放)。败者据此向胜者
    /// 转发 open 意图后退出(§11.1)。
    #[error("mutex_held:owner 互斥在 {timeout_ms}ms 内不可取")]
    MutexHeld { timeout_ms: u64 },
    /// 前任 pid 存活且 discovery 心跳新鲜:fencing 拒绝接管,不误杀活 Core。
    #[error("owner_active:前任 owner pid {pid} 存活且 discovery 心跳未过期,拒绝接管")]
    OwnerActive { pid: u32 },
    #[error("owner_mutex_lost:owner 已不再持有 OS 互斥")]
    MutexOwnershipLost,
    /// 本对象已被更高 owner epoch 接管:不得更新 discovery、不得释放新
    /// owner 的锁。
    #[error(
        "owner_superseded:当前 owner epoch {current_owner_epoch} 已接管本对象(epoch {owner_epoch})"
    )]
    Superseded {
        owner_epoch: u64,
        current_owner_epoch: u64,
    },
    /// shutdown 流程顺序错误(必须 freezing → draining → stores_closed)。
    #[error(
        "shutdown_order:stores_closed 证明需要按 owning → freezing → draining → stores_closed 推进,当前停在 {stage}"
    )]
    ShutdownOrder { stage: &'static str },
    /// 证明的 owner epoch 与锁不匹配。
    #[error("stores_closed_proof_mismatch:证明 epoch {proof_epoch} 与锁 epoch {owner_epoch} 不符")]
    ProofMismatch { proof_epoch: u64, owner_epoch: u64 },
    #[error("owner_already_released:owner 锁已完成释放")]
    AlreadyReleased,
    /// core.lock 无法解析:fail-closed,拒绝接管(不覆盖未知内容)。
    #[error("lock_file_corrupt:{} 无法解析,拒绝接管", path.display())]
    LockFileCorrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// discovery.json 无法解析。owner 写路径 fail-closed;接管判定按
    /// 「心跳不可证明新鲜」处理。
    #[error("discovery_corrupt:{} 无法解析", path.display())]
    DiscoveryCorrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// A7 生命周期参数越界。
    #[error(transparent)]
    InvalidLimits(#[from] crate::limits::LifecycleLimitsError),
    #[error("owner_epoch_exhausted:owner epoch 已达 u64::MAX")]
    EpochExhausted,
    #[error("owner_epoch_store:{0}")]
    EpochStore(String),
    #[error("{context}:{source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl OwnerLockError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MutexHeld { .. } => "mutex_held",
            Self::OwnerActive { .. } => "owner_active",
            Self::MutexOwnershipLost => "owner_mutex_lost",
            Self::Superseded { .. } => "owner_superseded",
            Self::ShutdownOrder { .. } => "shutdown_order",
            Self::ProofMismatch { .. } => "stores_closed_proof_mismatch",
            Self::AlreadyReleased => "owner_already_released",
            Self::LockFileCorrupt { .. } => "lock_file_corrupt",
            Self::DiscoveryCorrupt { .. } => "discovery_corrupt",
            Self::InvalidLimits(_) => "invalid_limits",
            Self::EpochExhausted => "owner_epoch_exhausted",
            Self::EpochStore(_) => "owner_epoch_store",
            Self::Io { .. } => "io_error",
        }
    }

    fn io(context: &str, source: std::io::Error) -> Self {
        Self::Io {
            context: context.to_string(),
            source,
        }
    }
}

// ───────────────────── owner epoch 持久低水位 ─────────────────────

pub trait OwnerEpochStore: Send + Sync {
    /// 在自身原子事务中返回 `max(current, predecessor_floor) + 1`。
    fn next_epoch(&self, predecessor_floor: u64) -> Result<u64, OwnerLockError>;
}

/// 测试/工具默认：以前任 lock record 为水位。生产必须改用
/// [`ServiceOwnerEpochStore`]，从 service.meta 防 lock 文件丢失回退。
#[derive(Debug, Clone, Copy, Default)]
pub struct RecordOwnerEpochStore;

impl OwnerEpochStore for RecordOwnerEpochStore {
    fn next_epoch(&self, predecessor_floor: u64) -> Result<u64, OwnerLockError> {
        predecessor_floor
            .checked_add(1)
            .ok_or(OwnerLockError::EpochExhausted)
    }
}

pub struct ServiceOwnerEpochStore {
    service_path: PathBuf,
}

impl ServiceOwnerEpochStore {
    pub fn new(service_path: impl Into<PathBuf>) -> Self {
        Self {
            service_path: service_path.into(),
        }
    }
}

impl OwnerEpochStore for ServiceOwnerEpochStore {
    fn next_epoch(&self, predecessor_floor: u64) -> Result<u64, OwnerLockError> {
        // 仅在 CoreOwnerLock 已取得 OS mutex 后打开/迁移 service DB；
        // 启动败者不能在 owner 仲裁前写 authoritative Store。
        let service = ServiceStore::open(&self.service_path)
            .map_err(|error| OwnerLockError::EpochStore(format!("{error:#}")))?;
        service
            .with_tx(|tx| {
                let current: i64 =
                    tx.query_row("SELECT owner_epoch FROM meta WHERE id=1", [], |row| {
                        row.get(0)
                    })?;
                anyhow::ensure!(current >= 0, "service.meta.owner_epoch 为负");
                let floor = (current as u64).max(predecessor_floor);
                let next = floor
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("owner_epoch_exhausted"))?;
                let next_i64 =
                    i64::try_from(next).map_err(|_| anyhow::anyhow!("owner_epoch_exhausted"))?;
                tx.execute("UPDATE meta SET owner_epoch=?1 WHERE id=1", [next_i64])?;
                Ok(next)
            })
            .map_err(|error| {
                if format!("{error:#}").contains("owner_epoch_exhausted") {
                    OwnerLockError::EpochExhausted
                } else {
                    OwnerLockError::EpochStore(format!("{error:#}"))
                }
            })
    }
}

// ─────────────────────────── acquire 装配 ───────────────────────────

/// [`CoreOwnerLock::acquire`] 的装配。
pub struct OwnerLockSetup {
    pub paths: OwnerLockPaths,
    pub mutex: Box<dyn OwnerMutexSource>,
    pub clock: Arc<dyn OwnerClock>,
    pub liveness: Arc<dyn ProcessLivenessProbe>,
    pub epoch_store: Arc<dyn OwnerEpochStore>,
    pub build: String,
    /// acquire 时写入 discovery 的初始 port(0 = 尚未绑定;owner 随后用
    /// `set_discovery_port` 修正——真实 gateway 绑定在后续 ticket)。
    pub port: u16,
    pub acquire_timeout: Duration,
    pub limits: LifecycleLimits,
    #[doc(hidden)]
    pub after_lock_publish: Option<Arc<dyn Fn() -> Result<(), OwnerLockError> + Send + Sync>>,
    #[doc(hidden)]
    pub after_release_lock_publish:
        Option<Arc<dyn Fn() -> Result<(), OwnerLockError> + Send + Sync>>,
}

impl OwnerLockSetup {
    /// 测试/工具装配基线(fake 或真实缝隙由调用方注入),字段可再覆写。
    pub fn new(
        paths: OwnerLockPaths,
        mutex: Box<dyn OwnerMutexSource>,
        clock: Arc<dyn OwnerClock>,
        liveness: Arc<dyn ProcessLivenessProbe>,
    ) -> Self {
        Self {
            paths,
            mutex,
            clock,
            liveness,
            epoch_store: Arc::new(RecordOwnerEpochStore),
            build: "test".to_string(),
            port: 0,
            acquire_timeout: Duration::from_millis(200),
            limits: LifecycleLimits::default(),
            after_lock_publish: None,
            after_release_lock_publish: None,
        }
    }

    /// 生产装配:平台互斥(CORE_MUTEX_NAME)+ 系统时钟 + 真实存活探针。
    pub fn platform(service_path: impl Into<PathBuf>, build: impl Into<String>, port: u16) -> Self {
        let paths = OwnerLockPaths::platform_default();
        let mutex = platform_owner_mutex(CORE_MUTEX_NAME, &paths.flock_path());
        Self::new(
            paths,
            mutex,
            Arc::new(SystemOwnerClock),
            Arc::new(OsProcessLivenessProbe),
        )
        .with_build(build)
        .with_port(port)
        .with_epoch_store(Arc::new(ServiceOwnerEpochStore::new(service_path)))
        .with_acquire_timeout(Duration::from_millis(DEFAULT_ACQUIRE_TIMEOUT_MS))
    }

    pub fn with_build(mut self, build: impl Into<String>) -> Self {
        self.build = build.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = timeout;
        self
    }

    pub fn with_epoch_store(mut self, epoch_store: Arc<dyn OwnerEpochStore>) -> Self {
        self.epoch_store = epoch_store;
        self
    }

    #[doc(hidden)]
    pub fn with_after_lock_publish(
        mut self,
        hook: Arc<dyn Fn() -> Result<(), OwnerLockError> + Send + Sync>,
    ) -> Self {
        self.after_lock_publish = Some(hook);
        self
    }

    #[doc(hidden)]
    pub fn with_after_release_lock_publish(
        mut self,
        hook: Arc<dyn Fn() -> Result<(), OwnerLockError> + Send + Sync>,
    ) -> Self {
        self.after_release_lock_publish = Some(hook);
        self
    }
}

// ─────────────────────────── CoreOwnerLock ───────────────────────────

/// 当前 Core 的 owner 身份:持有 OS 互斥,独占 owner epoch 写路径。
///
/// 不可 Clone;`release` 消费 self 并要求 [`StoresClosedProof`]。
/// 未 release 即 drop 等价 crash:不写任何文件,留给 fencing 判定。
pub struct CoreOwnerLock {
    guard: Mutex<Option<Box<dyn OwnerMutexGuard>>>,
    paths: OwnerLockPaths,
    owner_epoch: u64,
    pid: u32,
    build: String,
    instance_id: String,
    port: AtomicU16,
    clock: Arc<dyn OwnerClock>,
    fenced: AtomicBool,
    superseded_by_epoch: AtomicU64,
    released: AtomicBool,
    after_release_lock_publish: Option<Arc<dyn Fn() -> Result<(), OwnerLockError> + Send + Sync>>,
}

impl Drop for CoreOwnerLock {
    fn drop(&mut self) {
        // crash 语义:只释放 OS 互斥,不改写任何文件(§11.1 fencing 依据
        // 留存的 lock/discovery 记录判定接管)。
        self.guard.lock().take();
    }
}

impl std::fmt::Debug for CoreOwnerLock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CoreOwnerLock")
            .field("owner_epoch", &self.owner_epoch)
            .field("instance_id", &self.instance_id)
            .field("pid", &self.pid)
            .field("superseded", &self.fenced.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl CoreOwnerLock {
    /// L-OWNER:原子 acquire OS 互斥 + 更新 lock/discovery 记录。
    ///
    /// fencing(§11.1):前任记录存在且未 released 时,仅当前任 pid 不存在
    /// 或 discovery 心跳过期才接管;pid 存活且心跳新鲜 →
    /// [`OwnerLockError::OwnerActive`]。接管/首发一律 epoch = 前任 epoch+1
    /// (无前任从 1 起),写入两个记录并收紧当前用户 ACL。
    pub fn acquire(setup: OwnerLockSetup) -> Result<Self, OwnerLockError> {
        setup.limits.validate()?;
        let OwnerLockSetup {
            paths,
            mutex,
            clock,
            liveness,
            epoch_store,
            build,
            port,
            acquire_timeout,
            limits,
            after_lock_publish,
            after_release_lock_publish,
        } = setup;
        let stale_after_ms = limits.discovery_stale_after_ms();

        // 1) OS 互斥是唯一跨进程仲裁者;败者在这里失败。
        let guard = mutex.acquire(acquire_timeout)?;

        // 2) 前任记录:fencing 判定,corrupt 一律 fail-closed 不覆盖。
        let predecessor = read_core_lock(&paths.lock_path)?;
        if let Some(record) = &predecessor {
            if !record.released {
                let pid_alive = liveness.is_alive(record.pid);
                let heartbeat_fresh = discovery_heartbeat_is_fresh(
                    &paths.discovery_path,
                    clock.as_ref(),
                    stale_after_ms,
                );
                if pid_alive && heartbeat_fresh {
                    return Err(OwnerLockError::OwnerActive { pid: record.pid });
                }
            }
        }

        // 3) epoch 水位单调 +1,写两个记录(原子替换 + 当前用户 ACL)。
        let predecessor_floor = predecessor
            .as_ref()
            .map(|record| record.owner_epoch)
            .unwrap_or(0);
        let owner_epoch = epoch_store.next_epoch(predecessor_floor)?;
        let pid = std::process::id();
        let instance_id = uuid::Uuid::now_v7().to_string();
        let heartbeat_at = rfc3339_now(clock.as_ref());
        ensure_private_parent(&paths.lock_path)?;
        ensure_private_parent(&paths.discovery_path)?;
        let old_lock = read_optional_raw(&paths.lock_path)?;
        let old_discovery = read_optional_raw(&paths.discovery_path)?;
        let publish = write_lock_record(
            &paths.lock_path,
            &CoreLockRecord {
                pid,
                owner_epoch,
                build: build.clone(),
                released: false,
            },
        )
        .and_then(|_| {
            if let Some(hook) = &after_lock_publish {
                hook()?;
            }
            write_discovery_record(
                &paths.discovery_path,
                &DiscoveryRecord {
                    instance_id: instance_id.clone(),
                    port,
                    pid,
                    build: build.clone(),
                    heartbeat_at,
                },
            )
        });
        if let Err(error) = publish {
            // 两记录无法跨目录原子 rename；失败时在仍持有 OS mutex 的
            // 窗口内恢复原始字节，避免半发布 epoch 毒化下次 acquire。
            let rollback_lock = restore_raw(&paths.lock_path, old_lock.as_deref());
            let rollback_discovery = restore_raw(&paths.discovery_path, old_discovery.as_deref());
            if let Err(rollback) = rollback_lock.and(rollback_discovery) {
                return Err(OwnerLockError::io(
                    "回滚部分发布的 owner 记录",
                    std::io::Error::other(format!("publish={error}; rollback={rollback}")),
                ));
            }
            return Err(error);
        }
        log::info!("owner_lock_acquired epoch={owner_epoch} pid={pid}");
        Ok(Self {
            guard: Mutex::new(Some(guard)),
            paths,
            owner_epoch,
            pid,
            build,
            instance_id,
            port: AtomicU16::new(port),
            clock,
            fenced: AtomicBool::new(false),
            superseded_by_epoch: AtomicU64::new(owner_epoch),
            released: AtomicBool::new(false),
            after_release_lock_publish,
        })
    }

    pub fn owner_epoch(&self) -> u64 {
        self.owner_epoch
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// 是否已被更高 epoch 接管(永久 fencing,launcher 诊断用)。
    pub fn is_superseded(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
    }

    /// 刷新 discovery heartbeat(§11.1:Core 按 discovery_heartbeat_ms 周期)。
    /// 陈旧 owner 复验失败即被永久 fencing,discovery 保持新 owner 内容。
    pub fn heartbeat(&self) -> Result<(), OwnerLockError> {
        self.refresh_discovery(|record| {
            record.heartbeat_at = rfc3339_now(self.clock.as_ref());
        })
    }

    /// owner 修正 discovery 的 port(gateway 绑定后);权限与 fencing
    /// 语义同 [`Self::heartbeat`]。
    pub fn set_discovery_port(&self, port: u16) -> Result<(), OwnerLockError> {
        self.refresh_discovery(|record| {
            record.port = port;
            record.heartbeat_at = rfc3339_now(self.clock.as_ref());
        })
    }

    /// 有序释放(§2.3 stores_closed → handed_off):证明 → 标记 lock
    /// released(保留 epoch 水位)→ 移除 discovery → 最后释放 OS 互斥。
    /// 已被新 owner 接管时拒绝:不动新 owner 的任何文件。
    pub fn release(&self, proof: StoresClosedProof) -> Result<OwnerReleaseReport, OwnerLockError> {
        // guard slot 同时是本 owner 的 lifecycle/publication gate：与
        // heartbeat/set_port、双 release 全程互斥。
        let mut guard_slot = self.guard.lock();
        if self.released.load(Ordering::Acquire) {
            return Err(OwnerLockError::AlreadyReleased);
        }
        let owner_guard = guard_slot.as_ref().ok_or(OwnerLockError::AlreadyReleased)?;
        let publication = match owner_guard.publication_permit() {
            Ok(permit) => permit,
            Err(OwnerLockError::MutexOwnershipLost) => {
                let current = read_core_lock(&self.paths.lock_path)?
                    .map(|record| record.owner_epoch)
                    .unwrap_or(0);
                self.mark_superseded(current);
                return Err(OwnerLockError::Superseded {
                    owner_epoch: self.owner_epoch,
                    current_owner_epoch: current,
                });
            }
            Err(error) => return Err(error),
        };
        if proof.owner_epoch != self.owner_epoch {
            return Err(OwnerLockError::ProofMismatch {
                proof_epoch: proof.owner_epoch,
                owner_epoch: self.owner_epoch,
            });
        }
        if self.fenced.load(Ordering::Acquire) {
            return Err(OwnerLockError::Superseded {
                owner_epoch: self.owner_epoch,
                current_owner_epoch: self.superseded_by_epoch.load(Ordering::Acquire),
            });
        }
        match read_core_lock(&self.paths.lock_path)? {
            Some(record)
                if record.owner_epoch == self.owner_epoch
                    && record.pid == self.pid
                    && !record.released => {}
            Some(record) => {
                self.mark_superseded(record.owner_epoch);
                return Err(OwnerLockError::Superseded {
                    owner_epoch: self.owner_epoch,
                    current_owner_epoch: record.owner_epoch,
                });
            }
            // 锁文件丢失:不知当前真实水位,拒绝盲写。
            None => {
                return Err(OwnerLockError::Superseded {
                    owner_epoch: self.owner_epoch,
                    current_owner_epoch: 0,
                })
            }
        }

        let old_lock = read_optional_raw(&self.paths.lock_path)?;
        let old_discovery = read_optional_raw(&self.paths.discovery_path)?;
        let publish = write_lock_record(
            &self.paths.lock_path,
            &CoreLockRecord {
                pid: self.pid,
                owner_epoch: self.owner_epoch,
                build: self.build.clone(),
                released: true,
            },
        )
        .and_then(|_| {
            if let Some(hook) = &self.after_release_lock_publish {
                hook()?;
            }
            remove_record_durable(&self.paths.discovery_path)
        });
        if let Err(error) = publish {
            let rollback = restore_raw(&self.paths.lock_path, old_lock.as_deref())
                .and_then(|_| restore_raw(&self.paths.discovery_path, old_discovery.as_deref()));
            if let Err(rollback) = rollback {
                return Err(OwnerLockError::io(
                    "回滚失败的 owner release",
                    std::io::Error::other(format!("release={error}; rollback={rollback}")),
                ));
            }
            return Err(error);
        }
        // 顺序保证:全部文件更新完成后才释放 OS 互斥。
        self.released.store(true, Ordering::Release);
        drop(publication);
        guard_slot.take();
        log::info!("owner_lock_released epoch={}", self.owner_epoch);
        Ok(OwnerReleaseReport {
            owner_epoch: self.owner_epoch,
        })
    }

    /// §2.3 owning → freezing → draining → stores_closed 的顺序门。
    pub fn shutdown_flow(&self) -> ShutdownFlow {
        ShutdownFlow {
            owner_epoch: self.owner_epoch,
            stage: ShutdownStage::Owning,
        }
    }

    /// owner 写 discovery 前的 fencing 复验:lock 文件 epoch/pid 与
    /// discovery 归属必须仍是本 owner;失配即永久 fencing。
    fn refresh_discovery(
        &self,
        mutate: impl FnOnce(&mut DiscoveryRecord),
    ) -> Result<(), OwnerLockError> {
        let guard_slot = self.guard.lock();
        if self.released.load(Ordering::Acquire) {
            return Err(OwnerLockError::AlreadyReleased);
        }
        let owner_guard = guard_slot.as_ref().ok_or(OwnerLockError::AlreadyReleased)?;
        let _publication = match owner_guard.publication_permit() {
            Ok(permit) => permit,
            Err(OwnerLockError::MutexOwnershipLost) => {
                let current = read_core_lock(&self.paths.lock_path)?
                    .map(|record| record.owner_epoch)
                    .unwrap_or(0);
                self.mark_superseded(current);
                return Err(OwnerLockError::Superseded {
                    owner_epoch: self.owner_epoch,
                    current_owner_epoch: current,
                });
            }
            Err(error) => return Err(error),
        };
        if self.fenced.load(Ordering::Acquire) {
            return Err(OwnerLockError::Superseded {
                owner_epoch: self.owner_epoch,
                current_owner_epoch: self.superseded_by_epoch.load(Ordering::Acquire),
            });
        }
        match read_core_lock(&self.paths.lock_path)? {
            Some(record)
                if record.owner_epoch == self.owner_epoch
                    && record.pid == self.pid
                    && !record.released => {}
            Some(record) => {
                self.mark_superseded(record.owner_epoch);
                return Err(OwnerLockError::Superseded {
                    owner_epoch: self.owner_epoch,
                    current_owner_epoch: record.owner_epoch,
                });
            }
            None => {
                self.mark_superseded(0);
                return Err(OwnerLockError::Superseded {
                    owner_epoch: self.owner_epoch,
                    current_owner_epoch: 0,
                });
            }
        }
        let mut record = match read_discovery(&self.paths.discovery_path)? {
            Some(record) if record.instance_id == self.instance_id => record,
            Some(_) => {
                // discovery 无 epoch 字段(§11.1 固定五字段);归属已易主,
                // 水位以 lock 文件为准,此处以 u64::MAX 标记归属失配。
                self.mark_superseded(u64::MAX);
                return Err(OwnerLockError::Superseded {
                    owner_epoch: self.owner_epoch,
                    current_owner_epoch: u64::MAX,
                });
            }
            // 文件丢失:只有互斥持有者(本 owner)能走到这里,重建自身记录。
            None => DiscoveryRecord {
                instance_id: self.instance_id.clone(),
                port: self.port.load(Ordering::Acquire),
                pid: self.pid,
                build: self.build.clone(),
                heartbeat_at: rfc3339_now(self.clock.as_ref()),
            },
        };
        mutate(&mut record);
        write_discovery_record(&self.paths.discovery_path, &record)?;
        self.port.store(record.port, Ordering::Release);
        Ok(())
    }

    fn mark_superseded(&self, current_owner_epoch: u64) {
        self.fenced.store(true, Ordering::Release);
        self.superseded_by_epoch
            .store(current_owner_epoch, Ordering::Release);
        log::warn!(
            "owner_lock_superseded epoch={} by={current_owner_epoch}",
            self.owner_epoch
        );
    }
}

/// 干净释放的结果回执。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerReleaseReport {
    pub owner_epoch: u64,
}

// ───────────────────── stores_closed 证明与顺序门 ─────────────────────

/// §2.3 `stores_closed` 已发生的显式证明。
///
/// 唯一构造路径是 [`ShutdownFlow::stores_closed`];release 以类型系统
/// 强制「必须先关 Store 才能释放 owner 锁」。证明绑定产生它的 owner
/// epoch,跨 epoch 无效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoresClosedProof {
    owner_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownStage {
    Owning,
    Freezing,
    Draining,
}

impl ShutdownStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Owning => "owning",
            Self::Freezing => "freezing",
            Self::Draining => "draining",
        }
    }
}

/// §2.3 状态机 owning → freezing → draining → stores_closed 的最小实现。
/// T1e 只固化顺序与证明;freeze/drain 的真实语义(拒绝新 command、
/// publication barrier 等待)在 shutdown.rs(后续 ticket)落地。
#[derive(Debug)]
pub struct ShutdownFlow {
    owner_epoch: u64,
    stage: ShutdownStage,
}

impl ShutdownFlow {
    /// owning → freezing:拒绝新 command / Agent Session / Installation Job /
    /// Root Mode enable;旋转 Controller/Root/writer lease epoch(§2.3)。
    pub fn freeze(self) -> Result<Self, OwnerLockError> {
        self.transition(ShutdownStage::Owning, ShutdownStage::Freezing)
    }

    /// freezing → draining:publication barrier 上等待已线性化命令、
    /// PTY input queue、outbox、可中断 Operation drain(§2.3)。
    pub fn drain(self) -> Result<Self, OwnerLockError> {
        self.transition(ShutdownStage::Freezing, ShutdownStage::Draining)
    }

    /// draining → stores_closed:flush Transcript/outbox/receipt、关闭全部
    /// Store 句柄后,产出 release 所需证明。
    pub fn stores_closed(self) -> Result<StoresClosedProof, OwnerLockError> {
        let flow = self.transition(ShutdownStage::Draining, ShutdownStage::Draining)?;
        Ok(StoresClosedProof {
            owner_epoch: flow.owner_epoch,
        })
    }

    fn transition(
        mut self,
        expected: ShutdownStage,
        next: ShutdownStage,
    ) -> Result<Self, OwnerLockError> {
        if self.stage != expected {
            return Err(OwnerLockError::ShutdownOrder {
                stage: self.stage.as_str(),
            });
        }
        self.stage = next;
        Ok(self)
    }
}

// ─────────────────────────── 文件读写 ───────────────────────────

/// 读取 `core.lock`(无文件 → None;解析失败 fail-closed)。
pub fn read_core_lock(path: &Path) -> Result<Option<CoreLockRecord>, OwnerLockError> {
    let Some(bytes) = read_record_bytes(path, "core.lock")? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| OwnerLockError::LockFileCorrupt {
            path: path.to_path_buf(),
            source,
        })
}

/// 读取 `discovery.json`(无文件 → None;解析失败 fail-closed)。
pub fn read_discovery(path: &Path) -> Result<Option<DiscoveryRecord>, OwnerLockError> {
    let Some(bytes) = read_record_bytes(path, "discovery.json")? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| OwnerLockError::DiscoveryCorrupt {
            path: path.to_path_buf(),
            source,
        })
}

fn read_record_bytes(path: &Path, name: &str) -> Result<Option<Vec<u8>>, OwnerLockError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(OwnerLockError::io(&format!("读取 {name}"), error)),
    }
}

/// discovery 记录是否 stale(§11.1:age ≥ 3×heartbeat;heartbeat 无法
/// 解析时按 stale——不能证明新鲜)。
pub fn discovery_is_stale(
    record: &DiscoveryRecord,
    now: DateTime<Utc>,
    stale_after_ms: u64,
) -> bool {
    match DateTime::parse_from_rfc3339(&record.heartbeat_at) {
        Ok(heartbeat) => {
            now.signed_duration_since(heartbeat)
                >= chrono::Duration::milliseconds(stale_after_ms as i64)
        }
        Err(_) => true,
    }
}

fn discovery_heartbeat_is_fresh(
    discovery_path: &Path,
    clock: &dyn OwnerClock,
    stale_after_ms: u64,
) -> bool {
    match read_discovery(discovery_path) {
        Ok(Some(record)) => !discovery_is_stale(&record, clock.now(), stale_after_ms),
        // 缺失/损坏都证明不了「新鲜」:互斥已在本方手里,按可接管处理。
        Ok(None) => false,
        Err(error) => {
            log::warn!("owner_lock_discovery_unreadable code={}", error.code());
            false
        }
    }
}

fn rfc3339_now(clock: &dyn OwnerClock) -> String {
    clock.now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 建目录并收紧为当前用户独占(仅本产品拥有的叶子目录)。
fn ensure_private_parent(path: &Path) -> Result<(), OwnerLockError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let existed = parent.exists();
            std::fs::create_dir_all(parent)
                .map_err(|error| OwnerLockError::io("创建 owner 文件目录", error))?;
            crate::platform_acl::restrict_current_user_only(parent)
                .map_err(|error| OwnerLockError::io("收紧 owner 文件目录 ACL", error))?;
            sync_directory(parent)
                .map_err(|error| OwnerLockError::io("同步 owner 文件目录", error))?;
            if !existed {
                if let Some(grandparent) =
                    parent.parent().filter(|path| !path.as_os_str().is_empty())
                {
                    sync_directory(grandparent)
                        .map_err(|error| OwnerLockError::io("同步 owner 新目录父级", error))?;
                }
            }
        }
    }
    Ok(())
}

fn write_lock_record(path: &Path, record: &CoreLockRecord) -> Result<(), OwnerLockError> {
    atomic_write_json(path, record)
}

fn write_discovery_record(path: &Path, record: &DiscoveryRecord) -> Result<(), OwnerLockError> {
    atomic_write_json(path, record)
}

/// 原子写 + 当前用户 ACL:同目录临时文件 → 收紧 → fsync → rename。
fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), OwnerLockError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| OwnerLockError::Io {
        context: "序列化 owner 记录失败".to_string(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()),
    })?;
    atomic_write_bytes(path, &bytes)
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), OwnerLockError> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7().simple()));
    let publish = || -> Result<(), OwnerLockError> {
        let mut file = std::fs::File::create(&temp)
            .map_err(|error| OwnerLockError::io("创建 owner 记录临时文件", error))?;
        file.write_all(bytes)
            .map_err(|error| OwnerLockError::io("写 owner 记录临时文件", error))?;
        file.sync_all()
            .map_err(|error| OwnerLockError::io("同步 owner 记录临时文件", error))?;
        crate::platform_acl::restrict_current_user_only(&temp)
            .map_err(|error| OwnerLockError::io("收紧 owner 记录 ACL", error))?;
        std::fs::rename(&temp, path)
            .map_err(|error| OwnerLockError::io("原子替换 owner 记录", error))?;
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            sync_directory(parent)
                .map_err(|error| OwnerLockError::io("同步 owner 记录目录", error))?;
        }
        Ok(())
    };
    let result = publish();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn read_optional_raw(path: &Path) -> Result<Option<Vec<u8>>, OwnerLockError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(OwnerLockError::io("读取 owner 记录原始字节", error)),
    }
}

fn restore_raw(path: &Path, bytes: Option<&[u8]>) -> Result<(), OwnerLockError> {
    if let Some(bytes) = bytes {
        return atomic_write_bytes(path, bytes);
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
                sync_directory(parent)
                    .map_err(|error| OwnerLockError::io("同步 owner 回滚目录", error))?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OwnerLockError::io("回滚 owner 记录", error)),
    }
}

fn remove_record_durable(path: &Path) -> Result<(), OwnerLockError> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
                sync_directory(parent)
                    .map_err(|error| OwnerLockError::io("同步记录删除目录", error))?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OwnerLockError::io("移除 owner 记录", error)),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let flushed = unsafe { FlushFileBuffers(handle) };
    let error = (flushed == 0).then(std::io::Error::last_os_error);
    unsafe { CloseHandle(handle) };
    error.map_or(Ok(()), Err)
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
