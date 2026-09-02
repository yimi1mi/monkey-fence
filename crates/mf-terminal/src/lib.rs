//! mf-terminal:Agent Session 终端管线 crate(canonical spec §8/§15.1)。
//!
//! T2f(Issue #28)只交付 `TerminalChannel` 兼容 shim 与 `TerminalHost`
//! 宿主缝隙:`attach_terminal` 暂时委托现有 SessionRegistry,调用者拿不到
//! `PtyMaster`/raw writer,`send_prompt_raw` 类旁路不再是外部入口。
//!
//! T3(§8)在相同 `TerminalChannel` 接口之后替换内部实现:统一 PTY/resize、
//! 全量 streaming redaction、epoch/seq/replay ring、ACK 反压、writer
//! lease、durable transcript 与 history gap。shim 不提前实现这些语义。
//!
//! 依赖方向:`mf-kernel` 依赖本 crate(kernel 的 `attach_terminal` 返回
//! `TerminalChannel`);拥有 SessionRuntime 的装配件实现 `TerminalHost`
//! 并注入 kernel。本 crate 不依赖 `mf-kernel`,也不接触 Store/Orchestrator。

pub mod channel;

pub use channel::{TerminalChannel, TerminalHost, TerminalProblem, TerminalSessionRef};
