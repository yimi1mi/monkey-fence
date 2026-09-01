//! mf-kernel:无界面 Core Service 的内核 crate(canonical spec §2.9/§15.1)。
//!
//! T1d(Issue #19)交付 service-v1.db schema 与 Project Registry /
//! session.json 幂等导入;T1e(Issue #20)交付 CoreOwnerLock/owner epoch/
//! stale discovery fencing(§11.1,L-OWNER)与附录 A7 生命周期参数。
//! kernel/command/operation/projection 等模块随后续 ticket 落位;本 crate
//! 当前是 dark data——不接管 `crates/mf` AppCtx 的任何权威状态,也不提供
//! standalone Core bin(`owner_lock_probe` 是跨进程契约测试的探针工具,
//! 不是 Core)。

pub mod limits;
mod platform_acl;
pub mod project_registry;
pub mod service_schema;
pub mod singleton;
