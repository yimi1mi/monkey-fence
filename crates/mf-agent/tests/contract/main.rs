//! mf-agent 契约测试入口:`tests/contract/` 下每个文件是一个模块。
//! Cargo 只把 `tests/*.rs` 与 `tests/*/main.rs` 识别为集成测试 crate,
//! 因此本文件聚合全部契约模块。

mod backup_before_migration;
mod catalog_schema_guards;
mod catalog_v2_migration;
mod identity_backfill;
mod identity_gc;
mod project_v7_migration;
mod revision_cas;
mod schema_guards;
mod support;
