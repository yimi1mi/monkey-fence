//! mf-kernel:无界面 Core Service 的内核 crate(canonical spec §2.9/§15.1)。
//!
//! T1d(Issue #19)交付 service-v1.db schema 与 Project Registry /
//! session.json 幂等导入;T1e(Issue #20)交付 CoreOwnerLock/owner epoch/
//! stale discovery fencing(§11.1,L-OWNER)与附录 A7 生命周期参数;
//! T1f(Issue #21)交付 command intent→target receipt/outbox 原子链;
//! T1g(Issue #22)交付 Operation saga、重启 reconcile 与 retention/GC
//! (§4/附录 A4),并把 T2 所需 handle/revision/intent/operation durable
//! DTO 冻结在本 crate。kernel facade/projection 等模块随后续 ticket 落位;
//! 本 crate 当前是 dark data——不接管 `crates/mf` AppCtx 的任何权威状态,
//! 也不提供 standalone Core bin(`owner_lock_probe` 是跨进程契约测试的
//! 探针工具,不是 Core)。

pub mod command;
pub mod handles;
pub mod lease;
pub mod limits;
pub mod operation;
mod platform_acl;
pub mod project_registry;
pub mod reconcile;
pub mod service_schema;
pub mod singleton;

// Command contracts 需要 crate-private target effect seam；源文件仍位于
// `tests/contract/`，作为 lib unit module 编译，release test 同样覆盖。
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
#[path = "../tests/contract/multistore_crash_recovery.rs"]
mod multistore_crash_recovery;
#[cfg(test)]
#[path = "../tests/contract/operation_saga.rs"]
mod operation_saga;
#[cfg(test)]
#[path = "../tests/contract/retention_gc.rs"]
mod retention_gc;
