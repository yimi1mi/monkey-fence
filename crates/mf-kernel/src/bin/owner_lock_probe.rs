//! `owner-lock-probe`:T1e 跨进程契约测试探针(非产品二进制)。
//!
//! 由 `tests/contract/owner_lock.rs` 以真实子进程驱动,验证 Windows 首发
//! 路径的 OS 级 owner 互斥、crash 后接管与干净释放(§11.1)。探针使用与
//! 生产一致的真实缝隙(平台互斥/系统时钟/OS 存活探针),状态全部落在
//! 调用方指定的 `--state-dir`(tempdir),绝不触碰 `~/.monkeyfence`。
//!
//! 模式:
//! - `probe`:acquire → heartbeat → shutdown_flow 证明 → 干净 release,
//!   stdout 输出一行 JSON 结果。
//! - `hold`:acquire → 写 ack 文件 → 阻塞等 stdin EOF → 按
//!   `--crash-after-hold` 决定 `abort()`(模拟 crash,文件留 stale)或
//!   干净 release。

use mf_kernel::singleton::{CoreOwnerLock, OwnerLockPaths, OwnerLockSetup, StoresClosedProof};
use std::io::{Read as _, Write as _};
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(state_dir) = arg_value(&args, "--state-dir") else {
        eprintln!("缺少 --state-dir");
        return ExitCode::FAILURE;
    };
    let mode = arg_value(&args, "--mode").unwrap_or_else(|| "probe".to_string());
    let mutex_name =
        arg_value(&args, "--mutex-name").unwrap_or_else(|| derive_mutex_name(&state_dir));
    let build = arg_value(&args, "--build").unwrap_or_else(|| "probe".to_string());
    let port: u16 = arg_value(&args, "--port")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let timeout_ms: u64 = arg_value(&args, "--timeout-ms")
        .and_then(|value| value.parse().ok())
        .unwrap_or(2_000);
    let ack_file =
        arg_value(&args, "--ack-file").unwrap_or_else(|| format!("{state_dir}/probe-ack.json"));
    let crash_after_hold = args.iter().any(|arg| arg == "--crash-after-hold");

    let paths = OwnerLockPaths::in_dir(std::path::Path::new(&state_dir));
    let setup = OwnerLockSetup::new(
        paths.clone(),
        mf_kernel::singleton::platform_owner_mutex(&mutex_name, &paths.flock_path()),
        std::sync::Arc::new(mf_kernel::singleton::SystemOwnerClock),
        std::sync::Arc::new(mf_kernel::singleton::OsProcessLivenessProbe),
    )
    .with_build(build)
    .with_port(port)
    .with_acquire_timeout(Duration::from_millis(timeout_ms));

    match mode.as_str() {
        "probe" => run_probe(setup),
        "hold" => run_hold(setup, &ack_file, crash_after_hold),
        other => {
            eprintln!("未知 --mode:{other}");
            ExitCode::FAILURE
        }
    }
}

fn run_probe(setup: OwnerLockSetup) -> ExitCode {
    match CoreOwnerLock::acquire(setup) {
        Ok(lock) => {
            let epoch = lock.owner_epoch();
            let released = clean_release(lock);
            emit(&serde_json::json!({
                "acquired": true,
                "epoch": epoch,
                "released": released,
            }));
            ExitCode::SUCCESS
        }
        Err(error) => {
            emit(&serde_json::json!({
                "acquired": false,
                "code": error.code(),
                "message": error.to_string(),
            }));
            ExitCode::SUCCESS
        }
    }
}

fn run_hold(setup: OwnerLockSetup, ack_file: &str, crash_after_hold: bool) -> ExitCode {
    let lock = match CoreOwnerLock::acquire(setup) {
        Ok(lock) => lock,
        Err(error) => {
            emit(&serde_json::json!({
                "acquired": false,
                "code": error.code(),
                "message": error.to_string(),
            }));
            return ExitCode::SUCCESS;
        }
    };
    let epoch = lock.owner_epoch();
    emit(&serde_json::json!({ "acquired": true, "epoch": epoch }));
    let ack = serde_json::json!({
        "epoch": epoch,
        "pid": std::process::id(),
        "instance_id": lock.instance_id(),
    });
    if let Err(error) = std::fs::write(ack_file, ack.to_string()) {
        eprintln!("写 ack 文件失败 {ack_file}:{error}");
        return ExitCode::FAILURE;
    }
    // 阻塞直到父进程关闭 stdin(单向「继续」信号)。
    let mut stdin = std::io::stdin();
    let mut sink = [0u8; 64];
    loop {
        match stdin.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    if crash_after_hold {
        // 模拟 crash:不写任何文件直接终止,OS 互斥被回收,
        // lock/discovery 留给父进程做 stale 接管判定。
        std::process::abort();
    }
    let released = clean_release(lock);
    emit(&serde_json::json!({ "event": "released", "released": released }));
    ExitCode::SUCCESS
}

fn clean_release(lock: CoreOwnerLock) -> bool {
    match stores_closed_proof(&lock) {
        Ok(proof) => lock.release(proof).is_ok(),
        Err(_) => false,
    }
}

fn stores_closed_proof(
    lock: &CoreOwnerLock,
) -> Result<StoresClosedProof, mf_kernel::singleton::OwnerLockError> {
    lock.shutdown_flow()
        .freeze()
        .and_then(|flow| flow.drain())
        .and_then(|flow| flow.stores_closed())
}

fn emit(value: &serde_json::Value) {
    println!("{value}");
    std::io::stdout().flush().ok();
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == key)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

/// 从 state dir 派生互斥名(FNV-1a;Windows 对象名仅限安全字符,用 hex)。
fn derive_mutex_name(state_dir: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in state_dir.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!(r"Local\MonkeyFence.Core.Probe.{hash:016x}")
}
