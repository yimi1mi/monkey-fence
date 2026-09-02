//! mf-terminal:Agent Session 终端管线 crate(canonical spec §8/§15.1)。
//!
//! T2f(Issue #28)只交付 `TerminalChannel` 兼容 shim 与 `TerminalHost`
//! 宿主缝隙:`attach_terminal` 暂时委托现有 SessionRegistry,调用者拿不到
//! `PtyMaster`/raw writer,`send_prompt_raw` 类旁路不再是外部入口。
//!
//! T3a(Issue #29)迁入 PTY 平台实现(`pty/`,自 `crates/mf/src/pty_spawn.rs`)
//! 并为 `PtyMaster` 增加真实 resize;终端模拟器与统一脱敏入口随后迁入
//! (`term_screen.rs`/`redactor.rs`)。T3 其余语义(epoch/seq/replay ring、
//! ACK 反压、writer lease、durable transcript、history gap)在后续 ticket
//! 于相同 `TerminalChannel` 接口后落地。
//!
//! 依赖方向:`mf-kernel` 依赖本 crate(kernel 的 `attach_terminal` 返回
//! `TerminalChannel`);拥有 SessionRuntime 的装配件实现 `TerminalHost`
//! 并注入 kernel。本 crate 不依赖 `mf-kernel`,也不接触 Store/Orchestrator。

pub mod channel;
pub mod journal;
pub mod limits;
pub mod pty;
pub mod redactor;
pub mod session;
pub mod term_screen;
pub mod writer_lease;

pub use channel::{TerminalChannel, TerminalHost, TerminalProblem, TerminalSessionRef};
