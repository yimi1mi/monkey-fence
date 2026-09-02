//! mf-kernel:无界面 Core Service 的内核 crate(canonical spec §2.9/§15.1)。
//!
//! T1d(Issue #19)交付 service-v1.db schema 与 Project Registry /
//! session.json 幂等导入;T1e(Issue #20)交付 CoreOwnerLock/owner epoch/
//! stale discovery fencing(§11.1,L-OWNER)与附录 A7 生命周期参数;
//! T1f(Issue #21)交付 command intent→target receipt/outbox 原子链;
//! T1g(Issue #22)交付 Operation saga、重启 reconcile 与 retention/GC
//! (§4/附录 A4)。
//!
//! T2a(Issue #23)交付 CoreKernel facade(§2.2 唯一深模块缝隙):封闭
//! `workflow.rename` 命令经 dispatch→Project Store→snapshot/event 贯通,
//! `crates/mf` 的 GPUI rename 经 in-process adapter 改走 facade。除该
//! tracer 外,本 crate 不接管 `crates/mf` AppCtx 的其它权威状态。
//! T2b(Issue #24)交付 ProjectionHub/L-PUBLISH、跨 Project 有界 journal、
//! resume/gap、独立 client queue、epoch rotate/recovery 与 A1/A9 契约。
//! standalone Core bin、WebGateway 与 attach_terminal 属后续 ticket。

pub mod command;
pub mod handles;
mod journal;
pub mod kernel;
pub mod lease;
pub mod limits;
pub mod operation;
mod platform_acl;
pub mod project_registry;
pub mod projection;
pub mod reconcile;
pub mod run_control;
pub mod run_lifecycle;
mod run_projection;
pub mod service_schema;
pub mod shutdown;
pub mod singleton;
pub mod workflow_start;
mod workspace_projection;

// Command contracts 需要 crate-private target effect seam；源文件仍位于
// `tests/contract/`，作为 lib unit module 编译，release test 同样覆盖。
#[cfg(test)]
#[path = "../tests/contract/barrier_consistency.rs"]
mod barrier_consistency;
#[cfg(test)]
#[path = "../tests/contract/command_idempotency.rs"]
mod command_idempotency;
#[cfg(test)]
#[path = "../tests/contract/command_support.rs"]
mod command_support;
#[cfg(test)]
#[path = "../tests/contract/intent_recovery.rs"]
mod intent_recovery;
#[cfg(test)]
#[path = "../tests/contract/journal_limits.rs"]
mod journal_limits;
#[cfg(test)]
#[path = "../tests/contract/journal_overflow.rs"]
mod journal_overflow;
#[cfg(test)]
#[path = "../tests/contract/journal_recovery.rs"]
mod journal_recovery;
#[cfg(test)]
#[path = "../tests/contract/kernel_first_tracer.rs"]
mod kernel_first_tracer;
#[cfg(test)]
#[path = "../tests/contract/multistore_crash_recovery.rs"]
mod multistore_crash_recovery;
#[cfg(test)]
#[path = "../tests/contract/no_mutation_bypass.rs"]
mod no_mutation_bypass;
#[cfg(test)]
#[path = "../tests/contract/operation_saga.rs"]
mod operation_saga;
#[cfg(test)]
#[path = "../tests/contract/operation_snapshot.rs"]
mod operation_snapshot;
#[cfg(test)]
#[path = "../tests/contract/project_workflow_commands.rs"]
mod project_workflow_commands;
#[cfg(test)]
#[path = "../tests/contract/projection_support.rs"]
mod projection_support;
#[cfg(test)]
#[path = "../tests/contract/retention_gc.rs"]
mod retention_gc;
#[cfg(test)]
#[path = "../tests/contract/run_control.rs"]
mod run_control_contract;
#[cfg(test)]
#[path = "../tests/contract/terminal_channel_shim.rs"]
mod terminal_channel_shim;
#[cfg(test)]
#[path = "../tests/contract/workflow_run_commands.rs"]
mod workflow_run_commands;
#[cfg(test)]
#[path = "../tests/contract/workflow_run_snapshot.rs"]
mod workflow_run_snapshot;
#[cfg(test)]
#[path = "../tests/contract/workflow_start_operation.rs"]
mod workflow_start_operation;
#[cfg(test)]
#[path = "../tests/contract/workspace_snapshot.rs"]
mod workspace_snapshot;
