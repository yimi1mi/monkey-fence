//! 全新 v1 存储命名空间验收:项目库与目录库相互独立,且不含任何旧 MonkeyFence 表。

use mf_agent::catalog_store::CatalogStore;
use mf_agent::store::Store;

#[test]
fn project_schema_starts_at_v1_without_legacy_tables() {
    let store = Store::memory().unwrap();
    let tables = store.table_names().unwrap();
    assert!(tables.contains(&"agent_tasks".to_string()));
    assert!(!tables.contains(&"runs".to_string()));
    assert_eq!(store.schema_version().unwrap(), 1);
}

#[test]
fn catalog_schema_is_independent() {
    let catalog = CatalogStore::memory().unwrap();
    assert_eq!(catalog.schema_version().unwrap(), 1);
    let tables = catalog.table_names().unwrap();
    assert!(tables.contains(&"plugin_packages".to_string()));
    assert!(tables.contains(&"plugin_pins".to_string()));
    assert!(!tables.contains(&"agent_tasks".to_string()));
}
