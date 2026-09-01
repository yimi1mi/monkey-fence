//! mf-kernel:无界面 Core Service 的内核 crate(canonical spec §2.9/§15.1)。
//!
//! T1d(Issue #19)交付最小骨架:service-v1.db schema 与 Project Registry /
//! session.json 幂等导入。kernel/command/operation/projection 等模块随后续
//! ticket 落位;本 crate 当前是 dark data——不接管 `crates/mf` AppCtx 的
//! 任何权威状态,也不提供 standalone Core bin。

mod platform_acl;
pub mod project_registry;
pub mod service_schema;
