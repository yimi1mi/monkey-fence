//! `monkeyfence-core` bin(T6b,Issue #48;spec §2.3/§11)。
//!
//! 每 OS 用户唯一、普通权限、无 UI 的 standalone Core。**影子/测试
//! 模式**:本阶段不切 GPUI owner——未取得 owner lock 不触碰 Store,
//! 启动失败保持 Bridge A owner;浏览器尚拿不到 bootstrap(默认只打
//! 印生命周期事件,不绑定生产 listener)。RunControl IPC 与 Project
//! Registry 复用已交付组件(pipe_server/legacy_transport 接线随
//! Gate T6 后续 ticket)。

use mf_kernel::core_lifecycle::{CoreLifecycle, CorePhase, DiscoveryRecord};
use mf_kernel::singleton::{FakeOwnerMutex, OwnerMutexSource};

fn main() {
    let started = std::time::Instant::now();
    let mut lifecycle = CoreLifecycle::new();
    // 影子模式:使用 fake 互斥(生产为平台 owner mutex;切换随 Gate T6)
    let mutex = FakeOwnerMutex::new("monkeyfence-core-shadow");
    match lifecycle.acquire_owner_lock(&mutex) {
        Ok(()) => {
            // Project Registry 冷启动(影子模式零项目)
            if let Err(error) = lifecycle.check_cold_start_budget(0, started.elapsed(), 5_000) {
                lifecycle.fail();
                eprintln!("mf-core: {error}");
                std::process::exit(2);
            }
            let record: DiscoveryRecord =
                match lifecycle.update_discovery(None, std::process::id(), None) {
                    Ok(record) => record,
                    Err(error) => {
                        lifecycle.fail();
                        eprintln!("mf-core discovery: {error}");
                        std::process::exit(3);
                    }
                };
            println!(
                "mf-core: owning(epoch={}, pid={}, phase={:?})",
                record.owner_epoch,
                record.pid,
                CorePhase::Owning
            );
        }
        Err(error) => {
            // 败者:只转发 open intent 后退出(影子模式无转发目标,
            // 直接退出)
            eprintln!("mf-core: {error}");
            std::process::exit(1);
        }
    }
}
