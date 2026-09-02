//! mf-terminal 契约测试入口(T3b,Issue #30)。
//!
//! cargo 的集成测试目标发现要求 `tests/<dir>/main.rs`;各契约模块
//! (`seq_ack`/`terminal_limits`/`limits_defaults`)在此声明。

mod crash_incomplete;
mod gap_and_exit;
mod input_dedupe;
mod limits_defaults;
mod resize;
mod seq_ack;
mod terminal_limits;
mod transcript_gc;
mod writer_lease;
