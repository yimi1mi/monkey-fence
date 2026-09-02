//! `monkeyfence-core` bin(T6b/T12,Issue #48/#65;spec §2.3/§11)。
//!
//! 每 OS 用户唯一、普通权限、无 UI 的 standalone Core——T12 后是唯一
//! 宿主。启动链:L-OWNER(owner lock + discovery)→ AppRuntime 装配
//! (SessionRegistry + transcript sink + runtime host)→ kernel tracer
//! + terminal host 注入 → owning 服务 Client。浏览器尚拿不到
//! bootstrap(T11 flag;Web 面已装配但入口隐藏)。

use mf_kernel::app_runtime::{bootstrap_kernel_with_terminal_host, AppRuntime};
use mf_kernel::core_lifecycle::{CoreLifecycle, CorePhase, DiscoveryRecord};
use mf_kernel::singleton::{FakeOwnerMutex, OwnerMutexSource};

fn main() {
    let started = std::time::Instant::now();
    let mut lifecycle = CoreLifecycle::new();
    // 影子→生产切换点:平台 owner mutex 接线随发布打包(T11 bundle);
    // 当前 fake 互斥保持单实例语义。
    let mutex = FakeOwnerMutex::new("monkeyfence-core");
    match lifecycle.acquire_owner_lock(&mutex) {
        Ok(()) => {
            // headless 装配:SessionRegistry + durable transcript sink +
            // runtime host(#65 AppRuntime)
            let runtime = match AppRuntime::assemble(mf_agent::Config::load().unwrap_or_default()) {
                Ok(runtime) => runtime,
                Err(error) => {
                    lifecycle.fail();
                    eprintln!("mf-core assemble: {error}");
                    std::process::exit(2);
                }
            };
            // kernel tracer + terminal host(L-OWNER 生产路径)
            if let Err(error) = bootstrap_kernel_with_terminal_host(runtime.registry.clone()) {
                lifecycle.fail();
                eprintln!("mf-core kernel: {error}");
                std::process::exit(3);
            }
            // cold-start budget(影子:零项目)
            if let Err(error) = lifecycle.check_cold_start_budget(0, started.elapsed(), 5_000) {
                lifecycle.fail();
                eprintln!("mf-core: {error}");
                std::process::exit(4);
            }
            let record: DiscoveryRecord =
                match lifecycle.update_discovery(None, std::process::id(), None) {
                    Ok(record) => record,
                    Err(error) => {
                        lifecycle.fail();
                        eprintln!("mf-core discovery: {error}");
                        std::process::exit(5);
                    }
                };
            println!(
                "mf-core: owning(epoch={}, pid={}, phase={:?}, sessions-host=ready, transcript=durable)",
                record.owner_epoch, record.pid, CorePhase::Owning
            );
        }
        Err(error) => {
            // 败者:转发 open intent 后退出
            eprintln!("mf-core: {error}");
            std::process::exit(1);
        }
    }
}
