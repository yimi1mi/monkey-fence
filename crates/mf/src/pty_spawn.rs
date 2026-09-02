//! 原生 PTY 启动封装的迁移壳(T3a,Issue #29)。
//!
//! 运行逻辑已迁至 `mf-terminal/src/pty/`(公共契约在 `mod.rs`,Windows
//! ConPTY/Job Object 与 Unix openpty/fork/execve 在各自平台文件,并新增
//! 真实 `PtyMaster::resize`)。本文件仅 re-export,保持 legacy 调用点
//! (`runtime_host.rs`/`console.rs` 的 `crate::pty_spawn`)路径不变。

pub use mf_terminal::pty::{
    openpty, ExitStatus, JobGuard, PtyChild, PtyChildKiller, PtyMaster, PtyPair, PtyReader,
    PtySize, PtyWriter, SpawnCommand, SpawnEnvBlock,
};
