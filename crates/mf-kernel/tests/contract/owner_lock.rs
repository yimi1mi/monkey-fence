//! T1e 契约(Issue #20):CoreOwnerLock、owner epoch 与有序释放。
//!
//! 覆盖:L-OWNER acquire 写入 lock/discovery 记录与当前用户 ACL、
//! 进程内互斥(fake 缝隙)、owner epoch 跨 release/crash/stale 单调递增、
//! release 必须持有按序产生的 stores_closed 证明、corrupt lock fail-closed;
//! Windows 首发路径另含真实命名 mutex(同线程重入被 fencing 拦截、跨线程
//! 互斥)与真实跨进程 probe(竞争唯一 owner、crash 接管、干净释放)。
//! 全部使用 tempfile,不触碰真实 `~/.monkeyfence`。

use crate::support::{assert_current_user_only, is_uuid_v7, OwnerFixture};
use mf_kernel::singleton::{
    read_core_lock, read_discovery, CoreOwnerLock, OwnerLockError, ServiceOwnerEpochStore,
    CORE_LOCK_FILE_NAME, CORE_MUTEX_NAME, DISCOVERY_FILE_NAME,
};
use std::sync::Arc;
use std::time::Duration;

/// 按序产出 stores_closed 证明(owning → freezing → draining → stores_closed)。
fn stores_closed_proof(
    lock: &CoreOwnerLock,
) -> Result<mf_kernel::singleton::StoresClosedProof, OwnerLockError> {
    lock.shutdown_flow()
        .freeze()
        .and_then(|flow| flow.drain())
        .and_then(|flow| flow.stores_closed())
}

/// spec §11.1 的生产互斥名与文件名(防漂移)。
#[test]
fn spec_constants_are_pinned() {
    assert_eq!(CORE_MUTEX_NAME, r"Local\MonkeyFence.Core");
    assert_eq!(CORE_LOCK_FILE_NAME, "core.lock");
    assert_eq!(DISCOVERY_FILE_NAME, "discovery.json");
}

/// 全新 acquire:epoch 从 1 起,lock/discovery 记录字段齐备(§11.1),
/// 两个文件均当前用户 ACL;instance id 是 UUIDv7。
#[test]
fn fresh_acquire_writes_lock_and_discovery_with_owner_only_acl() {
    let fx = OwnerFixture::new("fresh");
    let lock = CoreOwnerLock::acquire(fx.setup()).unwrap();

    assert_eq!(lock.owner_epoch(), 1);
    let record = read_core_lock(&fx.paths.lock_path).unwrap().unwrap();
    assert_eq!(record.pid, std::process::id());
    assert_eq!(record.owner_epoch, 1);
    assert_eq!(record.build, "test");
    assert!(!record.released, "活动 owner 的 lock 记录不是 released");

    let discovery = read_discovery(&fx.paths.discovery_path).unwrap().unwrap();
    assert_eq!(discovery.pid, std::process::id());
    assert_eq!(discovery.port, 0);
    assert_eq!(discovery.build, "test");
    assert!(is_uuid_v7(&discovery.instance_id));
    assert_eq!(discovery.instance_id, lock.instance_id().to_string());
    chrono::DateTime::parse_from_rfc3339(&discovery.heartbeat_at)
        .expect("heartbeat 是 RFC3339 时间戳");

    assert_current_user_only(&fx.paths.lock_path);
    assert_current_user_only(&fx.paths.discovery_path);

    // 干净收尾:释放后互斥可被下一次 acquire 取得。
    let proof = stores_closed_proof(&lock).unwrap();
    lock.release(proof).unwrap();
}

/// 进程内互斥(fake 缝隙,不重入):持有期间第二次 acquire 失败,
/// 干净释放后可重新 acquire 且 epoch 递增。
#[test]
fn held_mutex_blocks_second_acquire_until_release() {
    let fx = OwnerFixture::new("held");
    let first = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert!(fx.mutex_handle().is_held());

    let error = CoreOwnerLock::acquire(fx.setup()).unwrap_err();
    assert!(
        matches!(error, OwnerLockError::MutexHeld { .. }),
        "持有期间二次 acquire 必须失败,实际:{error}"
    );
    // 失败路径不写任何文件:水位仍是第一个 owner。
    assert_eq!(
        read_core_lock(&fx.paths.lock_path)
            .unwrap()
            .unwrap()
            .owner_epoch,
        1
    );

    let proof = stores_closed_proof(&first).unwrap();
    first.release(proof).unwrap();
    assert!(!fx.mutex_handle().is_held(), "干净释放后互斥空闲");

    let second = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(second.owner_epoch(), 2, "释放水位保留,epoch 单调 +1");
}

/// owner epoch 跨「干净释放 / crash(drop,不写文件)/ pid 死亡 / 心跳过期」
/// 全链路单调递增;pid 存活且心跳新鲜时拒绝接管(不误杀活 Core)。
#[test]
fn owner_epoch_monotonic_across_release_crash_and_stale() {
    let fx = OwnerFixture::new("epoch");
    let pid = std::process::id();

    // 干净释放:1 → 2。
    let first = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(first.owner_epoch(), 1);
    let proof = stores_closed_proof(&first).unwrap();
    first.release(proof).unwrap();

    // crash(drop 不写文件):fake 探针默认 pid 已死 → 接管,epoch 3。
    let second = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(second.owner_epoch(), 2);
    fx.mutex_handle().simulate_os_reclaim();
    drop(second);
    let third = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(third.owner_epoch(), 3);

    // crash 但 pid 仍存活(fake 标记存活)且心跳新鲜:拒绝接管。
    fx.mutex_handle().simulate_os_reclaim();
    drop(third);
    fx.liveness.mark_alive(pid);
    let refused = CoreOwnerLock::acquire(fx.setup()).unwrap_err();
    assert!(
        matches!(refused, OwnerLockError::OwnerActive { pid: p } if p == pid),
        "活 owner 不被接管,实际:{refused}"
    );

    // 心跳过期(3×heartbeat)后同一 pid 也可接管,epoch 4。
    fx.clock.advance_ms(15_000);
    let fourth = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(fourth.owner_epoch(), 4);
    fx.mutex_handle().simulate_os_reclaim();
    drop(fourth);
}

#[test]
fn service_meta_watermark_prevents_epoch_reuse_after_lock_loss() {
    let fx = OwnerFixture::new("service-watermark");
    let service_path = fx.paths.lock_path.parent().unwrap().join("service-v1.db");
    let epoch_store = Arc::new(ServiceOwnerEpochStore::new(&service_path));
    struct OrderCheckingMutex {
        service_path: std::path::PathBuf,
        inner: mf_kernel::singleton::FakeOwnerMutex,
    }
    impl mf_kernel::singleton::OwnerMutexSource for OrderCheckingMutex {
        fn acquire(
            &self,
            timeout: Duration,
        ) -> Result<Box<dyn mf_kernel::singleton::OwnerMutexGuard>, OwnerLockError> {
            assert!(
                !self.service_path.exists(),
                "启动败者不得在 OS owner mutex 前打开/迁移 service Store"
            );
            self.inner.acquire(timeout)
        }
    }
    let first_setup = mf_kernel::singleton::OwnerLockSetup::new(
        fx.paths.clone(),
        Box::new(OrderCheckingMutex {
            service_path: service_path.clone(),
            inner: mf_kernel::singleton::FakeOwnerMutex::new(format!("{}-order", fx.mutex_name)),
        }),
        fx.clock.clone(),
        fx.liveness.clone(),
    )
    .with_epoch_store(epoch_store.clone());
    let first = CoreOwnerLock::acquire(first_setup).unwrap();
    assert_eq!(first.owner_epoch(), 1);
    let proof = stores_closed_proof(&first).unwrap();
    first.release(proof).unwrap();

    std::fs::remove_file(&fx.paths.lock_path).unwrap();
    let second = CoreOwnerLock::acquire(fx.setup().with_epoch_store(epoch_store)).unwrap();
    assert_eq!(second.owner_epoch(), 2, "core.lock 丢失也不得复用 epoch 1");
    let meta_epoch: i64 = rusqlite::Connection::open(&service_path)
        .unwrap()
        .query_row("SELECT owner_epoch FROM meta WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(meta_epoch, 2);
    let proof = stores_closed_proof(&second).unwrap();
    second.release(proof).unwrap();
}

#[test]
fn fake_reclaimed_guard_cannot_release_new_generation() {
    let fx = OwnerFixture::new("fake-generation");
    let stale = CoreOwnerLock::acquire(fx.setup()).unwrap();
    fx.mutex_handle().simulate_os_reclaim();
    let current = CoreOwnerLock::acquire(fx.setup()).unwrap();
    drop(stale);
    assert!(
        fx.mutex_handle().is_held(),
        "旧 guard drop 不得释放新 generation"
    );
    current.heartbeat().unwrap();
    let proof = stores_closed_proof(&current).unwrap();
    current.release(proof).unwrap();
}

/// release 必须持有按序产生的 stores_closed 证明:乱序拒绝、跨 epoch
/// 证明拒绝;正确顺序下按 lock released → 移除 discovery → 互斥空闲
/// 的顺序完成,水位保留供下次递增。
#[test]
fn release_requires_ordered_stores_closed_proof() {
    let fx = OwnerFixture::new("release");

    // 乱序:owning 直接 stores_closed / freeze 后直接 stores_closed 都拒绝。
    let lock = CoreOwnerLock::acquire(fx.setup()).unwrap();
    let flow = lock.shutdown_flow();
    assert!(matches!(
        flow.stores_closed(),
        Err(OwnerLockError::ShutdownOrder { .. })
    ));
    let flow = lock.shutdown_flow().freeze().unwrap();
    assert!(matches!(
        flow.stores_closed(),
        Err(OwnerLockError::ShutdownOrder { .. })
    ));
    assert!(matches!(
        lock.shutdown_flow().drain(),
        Err(OwnerLockError::ShutdownOrder { .. })
    ));

    // 跨 epoch 证明:epoch1 的证明不能释放 epoch2 的锁；失败路径仍持有
    // OS mutex，必须用当前 owner 的正确证明显式释放。
    let stale_proof = stores_closed_proof(&lock).unwrap();
    fx.mutex_handle().simulate_os_reclaim();
    drop(lock);
    let next = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(next.owner_epoch(), 2);
    let mismatch = next.release(stale_proof).unwrap_err();
    assert!(
        matches!(mismatch, OwnerLockError::ProofMismatch { .. }),
        "跨 epoch 证明必须拒绝,实际:{mismatch}"
    );
    assert!(
        fx.mutex_handle().is_held(),
        "proof mismatch 不得释放 owner mutex"
    );
    let proof = stores_closed_proof(&next).unwrap();
    next.release(proof).unwrap();

    // 正确顺序:lock released=true(水位保留)→ discovery 移除 → 互斥空闲。
    let final_owner = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(final_owner.owner_epoch(), 3);
    let report = {
        let proof = stores_closed_proof(&final_owner).unwrap();
        final_owner.release(proof).unwrap()
    };
    assert_eq!(report.owner_epoch, 3);
    let record = read_core_lock(&fx.paths.lock_path).unwrap().unwrap();
    assert_eq!(record.owner_epoch, 3);
    assert!(record.released, "干净释放保留 epoch 水位并标记 released");
    assert!(read_discovery(&fx.paths.discovery_path).unwrap().is_none());
    assert!(!fx.mutex_handle().is_held());

    // released 水位允许立即接管且继续单调。
    let after = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(after.owner_epoch(), 4);
}

#[test]
fn release_serializes_heartbeat_and_double_release() {
    let fx = OwnerFixture::new("release-race");
    let owner = Arc::new(CoreOwnerLock::acquire(fx.setup()).unwrap());
    let proof = stores_closed_proof(&owner).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let heartbeat = {
        let owner = owner.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            owner.heartbeat()
        })
    };
    let release_a = {
        let owner = owner.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            owner.release(proof)
        })
    };
    let release_b = {
        let owner = owner.clone();
        std::thread::spawn(move || {
            barrier.wait();
            owner.release(proof)
        })
    };
    let heartbeat = heartbeat.join().unwrap();
    let releases = [release_a.join().unwrap(), release_b.join().unwrap()];
    assert_eq!(releases.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        releases
            .iter()
            .filter(|result| matches!(result, Err(OwnerLockError::AlreadyReleased)))
            .count(),
        1
    );
    assert!(heartbeat.is_ok() || matches!(heartbeat, Err(OwnerLockError::AlreadyReleased)));
    assert!(read_discovery(&fx.paths.discovery_path).unwrap().is_none());
    assert!(
        read_core_lock(&fx.paths.lock_path)
            .unwrap()
            .unwrap()
            .released
    );
}

#[test]
fn release_io_failure_keeps_owner_mutex_for_retry() {
    let fx = OwnerFixture::new("release-io");
    let owner = CoreOwnerLock::acquire(fx.setup()).unwrap();
    let proof = stores_closed_proof(&owner).unwrap();
    let original_lock = std::fs::read(&fx.paths.lock_path).unwrap();
    std::fs::write(&fx.paths.lock_path, b"{ corrupt").unwrap();
    assert!(matches!(
        owner.release(proof),
        Err(OwnerLockError::LockFileCorrupt { .. })
    ));
    assert!(
        fx.mutex_handle().is_held(),
        "release Err 不得暴露新 owner 窗口"
    );
    assert!(matches!(
        CoreOwnerLock::acquire(fx.setup()),
        Err(OwnerLockError::MutexHeld { .. })
    ));
    std::fs::write(&fx.paths.lock_path, original_lock).unwrap();
    owner.release(proof).unwrap();
    assert!(!fx.mutex_handle().is_held());
}

#[test]
fn acquire_partial_publication_rolls_back_records_before_mutex_release() {
    let fx = OwnerFixture::new("acquire-rollback");
    let setup = fx.setup().with_after_lock_publish(Arc::new(|| {
        Err(OwnerLockError::Io {
            context: "fault:after-lock".into(),
            source: std::io::Error::other("injected"),
        })
    }));
    let error = CoreOwnerLock::acquire(setup).unwrap_err();
    assert!(format!("{error:#}").contains("fault:after-lock"));
    assert!(read_core_lock(&fx.paths.lock_path).unwrap().is_none());
    assert!(read_discovery(&fx.paths.discovery_path).unwrap().is_none());
    assert!(!fx.mutex_handle().is_held());
    let leftovers: Vec<_> = std::fs::read_dir(fx.paths.lock_path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "失败 publication 不留临时文件:{leftovers:?}"
    );

    let owner = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(owner.owner_epoch(), 1, "回滚后不得留下伪 epoch 水位");
    let proof = stores_closed_proof(&owner).unwrap();
    owner.release(proof).unwrap();
}

#[test]
fn service_epoch_watermark_remains_monotonic_across_publish_failure() {
    let fx = OwnerFixture::new("service-publish-gap");
    let service_path = fx.paths.lock_path.parent().unwrap().join("service-v1.db");
    let epoch_store = Arc::new(ServiceOwnerEpochStore::new(&service_path));
    let setup = fx
        .setup()
        .with_epoch_store(epoch_store.clone())
        .with_after_lock_publish(Arc::new(|| {
            Err(OwnerLockError::Io {
                context: "fault:service-publish".into(),
                source: std::io::Error::other("injected"),
            })
        }));
    assert!(CoreOwnerLock::acquire(setup).is_err());
    assert!(read_core_lock(&fx.paths.lock_path).unwrap().is_none());
    let first_watermark: i64 = rusqlite::Connection::open(&service_path)
        .unwrap()
        .query_row("SELECT owner_epoch FROM meta WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(first_watermark, 1);

    let owner = CoreOwnerLock::acquire(fx.setup().with_epoch_store(epoch_store)).unwrap();
    assert_eq!(
        owner.owner_epoch(),
        2,
        "失败 publication 允许 epoch gap 但绝不复用"
    );
    let proof = stores_closed_proof(&owner).unwrap();
    owner.release(proof).unwrap();
}

#[test]
fn release_partial_publication_rolls_back_and_is_retryable() {
    let fx = OwnerFixture::new("release-rollback");
    let fail_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let hook = {
        let fail_once = fail_once.clone();
        Arc::new(move || {
            if fail_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
                Err(OwnerLockError::Io {
                    context: "fault:after-release-lock".into(),
                    source: std::io::Error::other("injected"),
                })
            } else {
                Ok(())
            }
        })
    };
    let owner = CoreOwnerLock::acquire(fx.setup().with_after_release_lock_publish(hook)).unwrap();
    let proof = stores_closed_proof(&owner).unwrap();
    let error = owner.release(proof).unwrap_err();
    assert!(format!("{error:#}").contains("fault:after-release-lock"));
    assert!(
        !read_core_lock(&fx.paths.lock_path)
            .unwrap()
            .unwrap()
            .released
    );
    assert!(read_discovery(&fx.paths.discovery_path).unwrap().is_some());
    assert!(fx.mutex_handle().is_held());

    owner.release(proof).unwrap();
    assert!(
        read_core_lock(&fx.paths.lock_path)
            .unwrap()
            .unwrap()
            .released
    );
    assert!(read_discovery(&fx.paths.discovery_path).unwrap().is_none());
    assert!(!fx.mutex_handle().is_held());
}

/// core.lock 损坏:fail-closed 拒绝接管,不覆盖未知内容,不写 discovery。
#[test]
fn corrupt_lock_file_blocks_acquire_fail_closed() {
    let fx = OwnerFixture::new("corrupt");
    std::fs::write(&fx.paths.lock_path, b"{ not json").unwrap();

    let error = CoreOwnerLock::acquire(fx.setup()).unwrap_err();
    assert!(
        matches!(error, OwnerLockError::LockFileCorrupt { .. }),
        "corrupt lock 必须 fail-closed,实际:{error}"
    );
    assert_eq!(
        std::fs::read(&fx.paths.lock_path).unwrap(),
        b"{ not json",
        "拒绝路径不改写 lock 文件"
    );
    assert!(!fx.paths.discovery_path.exists(), "拒绝路径不写 discovery");
}

#[test]
fn owner_epoch_exhaustion_fails_closed_without_wrap() {
    let fx = OwnerFixture::new("epoch-max");
    let record = serde_json::json!({
        "pid": 1,
        "owner_epoch": u64::MAX,
        "build": "future",
        "released": true,
    });
    std::fs::write(
        &fx.paths.lock_path,
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();
    let before = std::fs::read(&fx.paths.lock_path).unwrap();
    assert!(matches!(
        CoreOwnerLock::acquire(fx.setup()),
        Err(OwnerLockError::EpochExhausted)
    ));
    assert_eq!(std::fs::read(&fx.paths.lock_path).unwrap(), before);
    assert!(read_discovery(&fx.paths.discovery_path).unwrap().is_none());
    assert!(!fx.mutex_handle().is_held());
}

// ───────────────── Windows 首发路径:真实命名 mutex ─────────────────

/// Win32 mutex 由专用 owner 线程持有，因此调用线程二次 acquire 也不能
/// 利用 thread-affine reentrancy 绕过唯一 owner。
#[cfg(windows)]
#[test]
fn real_named_mutex_same_caller_cannot_reenter() {
    let fx = OwnerFixture::new("reent");
    let first = CoreOwnerLock::acquire(fx.real_setup()).unwrap();
    assert_eq!(first.owner_epoch(), 1);

    let refused = CoreOwnerLock::acquire(fx.real_setup()).unwrap_err();
    assert_eq!(
        refused.code(),
        "mutex_held",
        "同一调用线程也不得重入真实 owner mutex:{refused}"
    );

    // 第一个 owner 仍可心跳:失败路径未破坏其所有权。
    first.heartbeat().unwrap();
    let proof = stores_closed_proof(&first).unwrap();
    first.release(proof).unwrap();
}

/// CoreOwnerLock 可在另一线程完成 stores_closed→release；底层 helper
/// 仍由原 acquire 线程执行 ReleaseMutex，不能留下永久占用。
#[cfg(windows)]
#[test]
fn real_named_mutex_release_is_safe_after_cross_thread_move() {
    let fx = OwnerFixture::new("xthread-release");
    let first = CoreOwnerLock::acquire(fx.real_setup()).unwrap();
    let proof = stores_closed_proof(&first).unwrap();
    std::thread::spawn(move || first.release(proof).unwrap())
        .join()
        .unwrap();
    let next = CoreOwnerLock::acquire(fx.real_setup()).unwrap();
    assert_eq!(next.owner_epoch(), 2);
    let proof = stores_closed_proof(&next).unwrap();
    next.release(proof).unwrap();
}

/// 真实命名 mutex 跨线程互斥:另一线程 acquire 超时失败;持有者
/// crash(同进程存活、心跳过期)后跨线程接管,epoch 递增。
#[cfg(windows)]
#[test]
fn real_named_mutex_excludes_across_threads() {
    use mf_kernel::singleton::{platform_owner_mutex, OsProcessLivenessProbe, OwnerLockSetup};
    use std::sync::Arc;

    let fx = OwnerFixture::new("xthread");
    // 真实互斥 + fake 时钟(同一时钟产生与判定心跳)+ 真实存活探针。
    let first = {
        let setup = OwnerLockSetup::new(
            fx.paths.clone(),
            platform_owner_mutex(&fx.mutex_name, &fx.paths.flock_path()),
            fx.clock.clone(),
            Arc::new(OsProcessLivenessProbe),
        )
        .with_acquire_timeout(Duration::from_millis(2_000));
        CoreOwnerLock::acquire(setup).unwrap()
    };

    // 另一线程在持有时必须 mutex_held。
    let refused = {
        let paths = fx.paths.clone();
        let mutex_name = fx.mutex_name.clone();
        let flock = fx.paths.flock_path();
        let clock = fx.clock.clone();
        std::thread::spawn(move || {
            CoreOwnerLock::acquire(
                OwnerLockSetup::new(
                    paths,
                    platform_owner_mutex(&mutex_name, &flock),
                    clock,
                    Arc::new(OsProcessLivenessProbe),
                )
                .with_acquire_timeout(Duration::from_millis(300)),
            )
            .map(|lock| lock.owner_epoch())
        })
    }
    .join()
    .unwrap()
    .unwrap_err();
    assert_eq!(refused.code(), "mutex_held", "跨线程必须互斥:{refused}");

    // crash 模拟(同进程,drop 不写文件)后:pid 存活 + 心跳新鲜 → 拒绝;
    // 心跳过期(fake 时钟推进 3×heartbeat)→ 跨线程接管,epoch 递增。
    drop(first);
    let still_fresh = {
        let paths = fx.paths.clone();
        let mutex_name = fx.mutex_name.clone();
        let flock = fx.paths.flock_path();
        let clock = fx.clock.clone();
        std::thread::spawn(move || {
            CoreOwnerLock::acquire(
                OwnerLockSetup::new(
                    paths,
                    platform_owner_mutex(&mutex_name, &flock),
                    clock,
                    Arc::new(OsProcessLivenessProbe),
                )
                .with_acquire_timeout(Duration::from_millis(300)),
            )
            .map(|lock| lock.owner_epoch())
        })
    }
    .join()
    .unwrap()
    .unwrap_err();
    assert_eq!(
        still_fresh.code(),
        "owner_active",
        "同进程存活 + 心跳新鲜必须拒绝:{still_fresh}"
    );

    fx.clock.advance_ms(15_000);
    let takeover = {
        let paths = fx.paths.clone();
        let mutex_name = fx.mutex_name.clone();
        let flock = fx.paths.flock_path();
        let clock = fx.clock.clone();
        std::thread::spawn(move || {
            CoreOwnerLock::acquire(
                OwnerLockSetup::new(
                    paths,
                    platform_owner_mutex(&mutex_name, &flock),
                    clock,
                    Arc::new(OsProcessLivenessProbe),
                )
                .with_acquire_timeout(Duration::from_millis(2_000)),
            )
            .map(|lock| lock.owner_epoch())
        })
    };
    assert_eq!(takeover.join().unwrap().unwrap(), 2);
}

// ───────────────── Windows 首发路径:真实跨进程 probe ─────────────────

#[cfg(windows)]
mod probe {
    use crate::support::{owner_lock_probe_exe, unique_mutex_name, wait_for_file, OwnerFixture};
    use mf_kernel::singleton::{read_core_lock, read_discovery, CoreOwnerLock};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn spawn(state_dir: &Path, extra: &[&str]) -> Command {
        let mut command = Command::new(owner_lock_probe_exe());
        command
            .arg("--state-dir")
            .arg(state_dir)
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        command
    }

    /// 解析 probe stdout 的全部 JSON 行。
    fn json_lines(output: &std::process::Output) -> Vec<serde_json::Value> {
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("probe 输出必须是 JSON 行"))
            .collect()
    }

    /// 6 个真实子进程同时竞争:恰好一个 owner(唯一 ack、唯一存活持有者),
    /// 败者退出;胜者收到 stdin EOF 后干净释放,水位保留。
    #[test]
    fn probe_processes_race_yields_exactly_one_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mutex = unique_mutex_name("race");

        let mut racers: Vec<(std::process::Child, PathBuf)> = Vec::new();
        for index in 0..6 {
            let ack = dir.join(format!("ack-{index}.json"));
            let mut command = spawn(
                dir,
                &[
                    "--mode",
                    "hold",
                    "--mutex-name",
                    &mutex,
                    "--timeout-ms",
                    "3000",
                    "--ack-file",
                    ack.to_str().unwrap(),
                ],
            );
            racers.push((command.spawn().expect("启动 probe 子进程"), ack));
        }

        // 败者自行退出(互斥超时);胜者持有等待 stdin。等待恰好 5 个退出。
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let mut exited = 0;
            for (child, _) in &mut racers {
                if child.try_wait().unwrap().is_some() {
                    exited += 1;
                }
            }
            if exited == racers.len() - 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "竞争未收敛到唯一持有者(已退出 {exited}/6)"
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        // 恰好一个 ack(epoch 1):第二个 owner 若存在必然也写 ack。
        let acks: Vec<serde_json::Value> = racers
            .iter()
            .filter_map(|(_, ack)| std::fs::read_to_string(ack).ok())
            .map(|text| serde_json::from_str(&text).unwrap())
            .collect();
        assert_eq!(acks.len(), 1, "双进程/多进程竞争只有一个 owner");
        assert_eq!(acks[0]["epoch"], 1);
        let paths = mf_kernel::singleton::OwnerLockPaths::in_dir(dir);
        let lock_record = read_core_lock(&paths.lock_path).unwrap().unwrap();
        assert_eq!(lock_record.owner_epoch, 1);
        assert!(!lock_record.released, "胜者仍持有");
        let discovery = read_discovery(&paths.discovery_path).unwrap().unwrap();
        assert_eq!(discovery.pid, acks[0]["pid"].as_u64().unwrap() as u32);

        // 放行全部(胜者干净释放;败者已退出,stdin 关闭无害)。
        for (child, _) in &mut racers {
            drop(child.stdin.take());
            let _ = child.wait();
        }
        let lock_record = read_core_lock(&paths.lock_path).unwrap().unwrap();
        assert!(lock_record.released, "胜者干净释放");
        assert_eq!(lock_record.owner_epoch, 1, "释放保留 epoch 水位");
        assert!(read_discovery(&paths.discovery_path).unwrap().is_none());
    }

    /// 真实跨进程:probe 持有期间父进程与第三个进程都拿不到;probe
    /// crash 后父进程以真实存活探针接管(pid 已死),epoch 递增;父进程
    /// 干净释放后新 probe 进程可取(released 水位),epoch 再递增。
    #[test]
    fn crash_takeover_and_clean_release_across_processes() {
        let fx = OwnerFixture::new("xtakeover");
        let dir = fx.dir.path();
        let mutex = fx.mutex_name.clone();
        let ack_path = dir.join("probe-ack.json");

        let mut holder = spawn(
            dir,
            &[
                "--mode",
                "hold",
                "--mutex-name",
                &mutex,
                "--timeout-ms",
                "5000",
                "--ack-file",
                ack_path.to_str().unwrap(),
                "--crash-after-hold",
            ],
        )
        .spawn()
        .expect("启动持有 probe");
        let ack_text =
            wait_for_file(&ack_path, Duration::from_secs(20)).expect("probe 获取 owner 并写 ack");
        let ack: serde_json::Value = serde_json::from_str(&ack_text).unwrap();
        assert_eq!(ack["epoch"], 1);
        assert!(ack["pid"].as_u64().unwrap() != 0);

        // 父进程:互斥被真实跨进程持有 → mutex_held。
        let refused = CoreOwnerLock::acquire(fx.real_setup()).unwrap_err();
        assert_eq!(refused.code(), "mutex_held", "持有期间父进程被拒");

        // 第三个 probe 进程同样拿不到。
        let third = spawn(
            dir,
            &[
                "--mode",
                "probe",
                "--mutex-name",
                &mutex,
                "--timeout-ms",
                "300",
            ],
        )
        .output()
        .unwrap();
        let lines = json_lines(&third);
        assert_eq!(lines[0]["acquired"], false);
        assert_eq!(lines[0]["code"], "mutex_held");

        // crash:stdin EOF → abort(不写文件);父进程以真实探针接管
        // (probe pid 已退出 → 接管允许,即使其心跳仍新鲜)。
        drop(holder.stdin.take());
        let _ = holder.wait();
        let parent = CoreOwnerLock::acquire(
            fx.real_setup()
                .with_acquire_timeout(Duration::from_millis(5_000)),
        )
        .expect("crash 后接管");
        assert_eq!(parent.owner_epoch(), 2, "接管递增 owner epoch");
        let lock_record = read_core_lock(&fx.paths.lock_path).unwrap().unwrap();
        assert_eq!(lock_record.pid, std::process::id());
        assert!(!lock_record.released);
        let discovery = read_discovery(&fx.paths.discovery_path).unwrap().unwrap();
        assert_ne!(
            discovery.instance_id,
            ack["instance_id"].as_str().unwrap(),
            "discovery 已归属新 owner"
        );
        parent.heartbeat().unwrap();

        // 父进程干净释放 → 新 probe 进程从 released 水位接管,epoch 3。
        let proof = parent
            .shutdown_flow()
            .freeze()
            .unwrap()
            .drain()
            .unwrap()
            .stores_closed()
            .unwrap();
        parent.release(proof).unwrap();
        let next = spawn(
            dir,
            &[
                "--mode",
                "probe",
                "--mutex-name",
                &mutex,
                "--timeout-ms",
                "5000",
            ],
        )
        .output()
        .unwrap();
        let lines = json_lines(&next);
        assert_eq!(lines[0]["acquired"], true, "干净释放后可跨进程接管");
        assert_eq!(lines[0]["epoch"], 3);
        assert_eq!(lines[0]["released"], true);
    }
}
