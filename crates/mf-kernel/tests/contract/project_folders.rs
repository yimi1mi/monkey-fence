//! #multi-folder 项目文件夹契约(ADR 0007):一主多附。
//! - 注册项目 ⇔ primary 文件夹行(canonical_root)落位;v5 及更早库
//!   升级后自动 backfill。
//! - 附加文件夹可增可删;同一项目重复添加幂等;跨项目占用拒绝。
//! - 主文件夹不可移除(项目库/执行语义锚点)。
//! - find_project_by_path 对 primary 与 additional 都命中(attach 幂等)。

use mf_kernel::project_registry::{canonical_root_of, FolderKind, ServiceStore};

/// 与注册表相同的规范化口径(canonicalize + 去 `\?\` 前缀)。
fn canonical(path: &std::path::Path) -> std::path::PathBuf {
    canonical_root_of(path).0
}
use mf_kernel::service_schema::{
    SERVICE_SCHEMA_V1, SERVICE_SCHEMA_V2_DELTA, SERVICE_SCHEMA_V3_DELTA, SERVICE_SCHEMA_V4_DELTA,
    SERVICE_SCHEMA_VERSION,
};

fn store_of(tmp: &tempfile::TempDir) -> std::sync::Arc<ServiceStore> {
    ServiceStore::open(&tmp.path().join("service.db")).unwrap()
}

#[test]
fn register_project_lands_primary_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store_of(&tmp);
    let root = tmp.path().join("proj-a");
    std::fs::create_dir_all(&root).unwrap();
    let project = store.register_project_path(&root).unwrap();
    assert_eq!(project.folders.len(), 1);
    assert_eq!(project.folders[0].kind, FolderKind::Primary);
    assert_eq!(project.folders[0].canonical_path, canonical(&root));
    // list_projects 同样携带(primary 恒在首位)
    let listed = store.list_projects().unwrap();
    assert_eq!(listed[0].folders, project.folders);
}

#[test]
fn add_and_remove_additional_folder_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store_of(&tmp);
    let root = tmp.path().join("proj-a");
    let extra = tmp.path().join("extra-folder");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&extra).unwrap();
    let project = store.register_project_path(&root).unwrap();
    let handle = project.project_handle.clone();

    let folders = store.add_project_folder(&handle, &extra).unwrap();
    assert_eq!(folders.len(), 2);
    assert_eq!(folders[0].kind, FolderKind::Primary);
    assert_eq!(folders[1].kind, FolderKind::Additional);
    assert_eq!(folders[1].canonical_path, canonical(&extra));

    // 同项目重复添加幂等(不报错、不重复)
    assert_eq!(store.add_project_folder(&handle, &extra).unwrap().len(), 2);

    // 附加文件夹路径反查命中同一项目
    let found = store.find_project_by_path(&extra).unwrap().unwrap();
    assert_eq!(found.project_handle, handle);

    // 移除后:反查不再命中、列表回到仅 primary
    let folders = store.remove_project_folder(&handle, &extra).unwrap();
    assert_eq!(folders.len(), 1);
    assert!(store.find_project_by_path(&extra).unwrap().is_none());
}

#[test]
fn folder_ownership_is_exclusive_across_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store_of(&tmp);
    let root_a = tmp.path().join("proj-a");
    let root_b = tmp.path().join("proj-b");
    let extra = tmp.path().join("shared");
    for dir in [&root_a, &root_b, &extra] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let a = store.register_project_path(&root_a).unwrap();
    let b = store.register_project_path(&root_b).unwrap();
    store.add_project_folder(&a.project_handle, &extra).unwrap();

    // 已被 A 占用:B 添加报错并指明占用者
    let conflict = store
        .add_project_folder(&b.project_handle, &extra)
        .unwrap_err();
    assert!(
        conflict.to_string().contains("project_folder_conflict"),
        "conflict must name the code: {conflict}"
    );
    assert!(conflict.to_string().contains(&a.project_handle));

    // A 的主文件夹同样不能被 B 添加
    assert!(store
        .add_project_folder(&b.project_handle, &root_a)
        .is_err());
}

#[test]
fn primary_folder_cannot_be_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store_of(&tmp);
    let root = tmp.path().join("proj-a");
    std::fs::create_dir_all(&root).unwrap();
    let project = store.register_project_path(&root).unwrap();
    let error = store
        .remove_project_folder(&project.project_handle, &root)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("project_folder_primary_immutable"),
        "primary removal must be rejected explicitly: {error}"
    );
}

#[test]
fn add_folder_validates_directory_and_project() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store_of(&tmp);
    let root = tmp.path().join("proj-a");
    std::fs::create_dir_all(&root).unwrap();
    let project = store.register_project_path(&root).unwrap();

    // 不存在的路径
    let missing = store
        .add_project_folder(&project.project_handle, &tmp.path().join("nope"))
        .unwrap_err()
        .to_string();
    assert!(missing.contains("project_folder"), "{missing}");
    // 文件不是目录
    let file = tmp.path().join("file.txt");
    std::fs::write(&file, "x").unwrap();
    assert!(store
        .add_project_folder(&project.project_handle, &file)
        .unwrap_err()
        .to_string()
        .contains("project_folder_not_a_directory"));
    // 未知项目
    assert!(store
        .add_project_folder("proj_unknown", &root)
        .unwrap_err()
        .to_string()
        .contains("project_unknown"));
    // 不属于该项目的路径移除
    let stranger = tmp.path().join("stranger");
    std::fs::create_dir_all(&stranger).unwrap();
    assert!(store
        .remove_project_folder(&project.project_handle, &stranger)
        .unwrap_err()
        .to_string()
        .contains("project_folder_unknown"));
}

#[test]
fn legacy_v5_database_backfills_primary_folders_on_upgrade() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("legacy-service.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V1).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V2_DELTA).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V3_DELTA).unwrap();
        conn.execute_batch(SERVICE_SCHEMA_V4_DELTA).unwrap();
        conn.execute_batch(mf_kernel::service_schema::MIGRATION_V5_DISPLAY_NAME)
            .unwrap();
        conn.execute(
            "INSERT INTO meta(id, instance_id, schema_version, owner_epoch)
             VALUES(1, '018f0000-0000-7000-8000-000000000005', 5, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO root_state(id, mode, root_epoch) VALUES(1, 'off', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_registry
             (project_handle, public_id, canonical_root, display_path, registered_at, status)
             VALUES('proj_legacy', 'pub_legacy', '/legacy-root', '/legacy-root', '2026-09-01', 'registered')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
    }
    let store = ServiceStore::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), SERVICE_SCHEMA_VERSION);
    let project = store
        .list_projects()
        .unwrap()
        .into_iter()
        .find(|project| project.project_handle == "proj_legacy")
        .unwrap();
    assert_eq!(project.folders.len(), 1);
    assert_eq!(project.folders[0].kind, FolderKind::Primary);
    assert_eq!(project.folders[0].canonical_path, "/legacy-root");
    // backfill 后新语义立即可用:经旧根路径反查命中
    assert_eq!(
        store
            .find_project_by_path(std::path::Path::new("/legacy-root"))
            .unwrap()
            .map(|found| found.project_handle),
        Some("proj_legacy".to_string())
    );
}

#[test]
fn remove_folder_works_after_directory_deleted_from_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store_of(&tmp);
    let root = tmp.path().join("proj-a");
    let extra = tmp.path().join("gone-soon");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&extra).unwrap();
    let project = store.register_project_path(&root).unwrap();
    store
        .add_project_folder(&project.project_handle, &extra)
        .unwrap();
    // UI 回传的是注册表存储拼写(snapshot folders[].path 原文)
    let stored = store
        .list_projects()
        .unwrap()
        .into_iter()
        .find(|listed| listed.project_handle == project.project_handle)
        .unwrap()
        .folders
        .into_iter()
        .find(|folder| folder.kind == FolderKind::Additional)
        .unwrap()
        .canonical_path;
    std::fs::remove_dir_all(&extra).unwrap();
    // 目录已删:按存储拼写移除仍成功(不依赖文件系统)
    let folders = store
        .remove_project_folder(&project.project_handle, std::path::Path::new(&stored))
        .unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].kind, FolderKind::Primary);
}
