//! 进程内存储指标快照。
//!
//! T1 先冻结字段与更新点；后续 Core metrics exporter 只读取本模块快照，
//! 不再从日志反解析。所有计数均为进程生命周期内单调累计，depth/version
//! 为最后一次观测到的 gauge。

use crate::migration::StoreKind;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreStorageMetrics {
    pub store_kind: String,
    pub schema_version: i64,
    pub migration_count: u64,
    pub last_migration_duration_ms: u64,
    pub aggregate_handles_backfilled: u64,
    pub identity_nodes_backfilled: u64,
    pub identity_edges_backfilled: u64,
    pub migration_rows_imported: u64,
    pub outbox_depth: i64,
    pub identity_nodes_gc: u64,
    pub identity_edges_gc: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageMetricsSnapshot {
    /// key 是不泄露绝对路径的进程内 store identity；每个 Project DB
    /// 独立一行，禁止 last-open-wins 折叠。
    pub stores: BTreeMap<String, StoreStorageMetrics>,
}

fn metrics() -> &'static Mutex<StorageMetricsSnapshot> {
    static METRICS: OnceLock<Mutex<StorageMetricsSnapshot>> = OnceLock::new();
    METRICS.get_or_init(|| Mutex::new(StorageMetricsSnapshot::default()))
}

pub fn store_metric_key(conn: &rusqlite::Connection, store: StoreKind) -> String {
    let identity = match conn.path().filter(|path| !path.is_empty()) {
        Some(path) => {
            let digest = Sha256::digest(path.as_bytes());
            digest[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        }
        None => format!("memory-{:x}", conn as *const _ as usize),
    };
    format!("{}:{identity}", store.as_str())
}

fn with_store(store_key: &str, store: StoreKind, update: impl FnOnce(&mut StoreStorageMetrics)) {
    let mut metrics = metrics()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let entry = metrics.stores.entry(store_key.to_string()).or_default();
    entry.store_kind = store.as_str().to_string();
    update(entry);
}

pub fn storage_metrics_snapshot() -> StorageMetricsSnapshot {
    metrics()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

pub(crate) fn record_store_open(
    store_key: &str,
    store: StoreKind,
    schema_version: i64,
    outbox_depth: i64,
) {
    with_store(store_key, store, |metrics| {
        metrics.schema_version = schema_version;
        metrics.outbox_depth = outbox_depth;
    });
}

pub(crate) fn record_migration(store_key: &str, store: StoreKind, duration_ms: u128) {
    with_store(store_key, store, |metrics| {
        metrics.migration_count = metrics.migration_count.saturating_add(1);
        metrics.last_migration_duration_ms = duration_ms.min(u64::MAX as u128) as u64;
    });
}

pub(crate) fn record_identity_backfill(
    store_key: &str,
    aggregate_handles: usize,
    nodes: usize,
    edges: usize,
) {
    with_store(store_key, StoreKind::Project, |metrics| {
        metrics.aggregate_handles_backfilled = metrics
            .aggregate_handles_backfilled
            .saturating_add(aggregate_handles as u64);
        metrics.identity_nodes_backfilled = metrics
            .identity_nodes_backfilled
            .saturating_add(nodes as u64);
        metrics.identity_edges_backfilled = metrics
            .identity_edges_backfilled
            .saturating_add(edges as u64);
    });
}

pub(crate) fn record_identity_gc(store_key: &str, nodes: usize, edges: usize) {
    with_store(store_key, StoreKind::Project, |metrics| {
        metrics.identity_nodes_gc = metrics.identity_nodes_gc.saturating_add(nodes as u64);
        metrics.identity_edges_gc = metrics.identity_edges_gc.saturating_add(edges as u64);
    });
}

pub(crate) fn record_catalog_import(store_key: &str, duration_ms: u128, rows: usize) {
    with_store(store_key, StoreKind::Catalog, |metrics| {
        metrics.migration_count = metrics.migration_count.saturating_add(1);
        metrics.last_migration_duration_ms = duration_ms.min(u64::MAX as u128) as u64;
        metrics.migration_rows_imported =
            metrics.migration_rows_imported.saturating_add(rows as u64);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_metrics_are_per_store_and_exportable_without_parsing_logs() {
        let key = "project:test-observability";
        let before = storage_metrics_snapshot()
            .stores
            .get(key)
            .cloned()
            .unwrap_or_default();
        record_store_open(key, StoreKind::Project, 7, 3);
        record_migration(key, StoreKind::Project, 42);
        record_identity_backfill(key, 5, 4, 3);
        record_identity_gc(key, 2, 1);
        let other_key = "project:test-observability-other";
        record_store_open(other_key, StoreKind::Project, 7, 9);
        let catalog_key = "catalog:test-observability";
        record_catalog_import(catalog_key, 12, 6);
        let after = storage_metrics_snapshot().stores.get(key).cloned().unwrap();

        assert_eq!(after.store_kind, "project");
        assert_eq!(after.schema_version, 7);
        assert_eq!(after.outbox_depth, 3);
        assert_eq!(
            storage_metrics_snapshot().stores[catalog_key].migration_rows_imported,
            6
        );
        assert!(after.migration_count >= before.migration_count + 1);
        assert!(after.aggregate_handles_backfilled >= before.aggregate_handles_backfilled + 5);
        assert!(after.identity_nodes_backfilled >= before.identity_nodes_backfilled + 4);
        assert!(after.identity_edges_backfilled >= before.identity_edges_backfilled + 3);
        assert!(after.identity_nodes_gc >= before.identity_nodes_gc + 2);
        assert!(after.identity_edges_gc >= before.identity_edges_gc + 1);
        assert_eq!(
            storage_metrics_snapshot().stores[other_key].outbox_depth,
            9,
            "多个 Project Store 必须保持独立 gauge"
        );
        assert_eq!(after.outbox_depth, 3);
    }
}
