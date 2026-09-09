//! U2 回归:Core 实例隔离装配契约(真实 owner lock 仲裁验证)。
//! - 默认生产装配(未设 MF_CORE_INSTANCE_DIR):互斥名恒为 CORE_MUTEX_NAME,
//!   owner/discovery 落用户级路径——两个同默认装配互斥(ADR 0005 单例)。
//! - 隔离装配(MF_CORE_INSTANCE_DIR 指向独立根):互斥名/owner lock/
//!   discovery 全部落该根目录下;两个不同根的隔离实例可同时持有,
//!   同一根的第二个实例仍被仲裁。
//! - 仅重定向 MF_SERVICE_DB 不构成隔离(仍共享用户级 owner lock)。

use crate::singleton::{
    core_mutex_name_for, instance_namespace_root, CoreOwnerLock, OwnerLockSetup,
};
use tempfile::TempDir;

/// 进程级环境变量守护:构造时设置,drop 时恢复原值(串行测试)。
struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: Option<String>) -> Self {
        let original = std::env::var(key).ok();
        match &value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        EnvGuard { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

fn setup_for(service_db: &std::path::Path, build_tag: &str) -> OwnerLockSetup {
    // owner/discovery 落点由 MF_CORE_INSTANCE_DIR 决定(生产装配);
    // 测试把默认(用户级)路径也重定向到临时目录,避免触碰真实用户文件
    // ——通过 HOME 重定向实现:platform_default 读 dirs::home_dir()。
    OwnerLockSetup::platform(service_db, build_tag, 0)
        .with_acquire_timeout(std::time::Duration::from_millis(400))
}

fn home_guard() -> (EnvGuard, TempDir) {
    // Windows dirs::home_dir() 优先读 USERPROFILE
    let tmp = TempDir::new().unwrap();
    let guard = EnvGuard::set(
        "USERPROFILE",
        Some(tmp.path().to_string_lossy().to_string()),
    );
    (guard, tmp)
}

#[test]
fn default_setup_keeps_legacy_singleton_contract() {
    // 用户日常 Core 正持有默认互斥——默认契约断言只验证名称/路径派生
    // (真实默认仲裁行为由隔离测试的同根仲裁用例覆盖:同一命名空间的
    // 第二个装配必然 MutexHeld)。
    let _ns = EnvGuard::set("MF_CORE_INSTANCE_DIR", None);
    let (_home, home_tmp) = home_guard();

    // 互斥名与旧契约完全一致(默认不派生后缀)
    assert_eq!(
        core_mutex_name_for(&home_tmp.path().join("a-service.db")),
        crate::singleton::CORE_MUTEX_NAME
    );
    // 任意路径(含 MF_SERVICE_DB 重定向)都不改变默认互斥名
    assert_eq!(
        core_mutex_name_for(std::path::Path::new(r"Z:\elsewhere\svc.db")),
        crate::singleton::CORE_MUTEX_NAME
    );
    // owner/discovery 仍落用户级(HOME 重定向后的 .monkeyfence)
    let setup = setup_for(&home_tmp.path().join("a-service.db"), "iso-a");
    let lock_dir = setup.paths.lock_path.parent().unwrap();
    assert!(
        lock_dir.ends_with(".monkeyfence"),
        "owner/discovery 必须留在用户级目录:{}",
        lock_dir.display()
    );
    drop(setup);
}

#[test]
fn isolated_setups_get_full_namespace_and_coexist() {
    let root_a = TempDir::new().unwrap();
    let root_b = TempDir::new().unwrap();
    let (_home, _home_tmp) = home_guard();

    let guard_a = EnvGuard::set(
        "MF_CORE_INSTANCE_DIR",
        Some(root_a.path().to_string_lossy().to_string()),
    );
    assert_eq!(instance_namespace_root().as_deref(), Some(root_a.path()));
    let name_a = core_mutex_name_for(&root_a.path().join("service.db"));
    assert_ne!(name_a, crate::singleton::CORE_MUTEX_NAME);

    let owner_a = CoreOwnerLock::acquire(setup_for(&root_a.path().join("service.db"), "iso-a"))
        .expect("isolated instance A acquires");

    // 根 B 独立命名空间:B 可与 A 并存
    let guard_b = EnvGuard::set(
        "MF_CORE_INSTANCE_DIR",
        Some(root_b.path().to_string_lossy().to_string()),
    );
    let name_b = core_mutex_name_for(&root_b.path().join("service.db"));
    assert_ne!(name_a, name_b, "不同隔离根必须派生不同互斥名");
    let owner_b = CoreOwnerLock::acquire(setup_for(&root_b.path().join("service.db"), "iso-b"))
        .expect("isolated instance B coexists with A");
    drop(owner_b);
    drop(guard_b);

    // 同一根的第二个实例仍被仲裁
    let duplicate = CoreOwnerLock::acquire(setup_for(&root_a.path().join("service.db"), "iso-a2"));
    assert!(
        duplicate.is_err(),
        "same isolated namespace must still arbitrate: {duplicate:?}"
    );
    drop(guard_a);
    drop(owner_a);
}

#[test]
fn service_db_redirect_alone_keeps_shared_owner_paths() {
    // 仅重定向 service DB(不设 MF_CORE_INSTANCE_DIR):装配关系仍是
    // 共享用户级 owner/discovery(即不构成隔离)。同样因日常 Core 持有
    // 默认互斥,此处断言装配路径关系而非真实 acquire。
    let _ns = EnvGuard::set("MF_CORE_INSTANCE_DIR", None);
    let (_home, home_tmp) = home_guard();
    let one = setup_for(&home_tmp.path().join("one.db"), "solo-a");
    let two = setup_for(&home_tmp.path().join("two.db"), "solo-b");
    assert_eq!(
        one.paths.lock_path, two.paths.lock_path,
        "不同 service DB 的 owner lock 路径必须共享(默认装配)"
    );
    assert_eq!(
        one.paths.discovery_path, two.paths.discovery_path,
        "不同 service DB 的 discovery 路径必须共享(默认装配)"
    );
}
