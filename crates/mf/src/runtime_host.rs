//! 迁移壳(T12,Issue #65):SessionRuntime 已整体迁至
//! `mf-terminal/src/session_runtime.rs`;本文件仅 re-export,保持
//! legacy 调用点路径不变。删除 crates/mf 时随 crate 消失。

pub use mf_terminal::session_runtime::*;
