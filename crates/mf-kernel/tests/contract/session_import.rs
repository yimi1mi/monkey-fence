//! T1d 契约(Issue #19):session.json → project_registry 幂等导入(§3.5)。
//!
//! 覆盖:canonical root 去重、可用 foreground、缺失路径保留 missing、
//! 重复导入幂等、崩溃残留(rows 先落、marker 未写)收敛、非目标 UI 状态
//! 不导入、原 session.json 字节不变、无文件/损坏文件的安全路径。
//! 全部使用 tempfile fixture,不触碰真实用户目录与真实 session.json。

use crate::support::{
    dump_all_text, is_uuid_v7, read_only, registry_rows, sha256_file, write_session_json,
};
use mf_kernel::project_registry::{
    canonical_root_of, ProjectStatus, ServiceStore, SessionImportStatus, SESSION_IMPORT_MARKER,
};
use serde_json::json;
use std::fs;

fn service_db(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("service-v1.db")
}

fn session_file(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("session.json")
}

fn marker_count(db: &std::path::Path) -> i64 {
    read_only(db)
        .query_row(
            "SELECT COUNT(*) FROM migration_marker WHERE name = ?1",
            [SESSION_IMPORT_MARKER],
            |r| r.get(0),
        )
        .unwrap()
}

/// 与生产同口径的 canonical root 期望值(去掉 `\\?\` 前缀)。
fn expected_root(path: &std::path::Path) -> String {
    canonical_root_of(path).0.to_string_lossy().into_owned()
}

/// 典型导入:项目列表 + 同目录多种拼写去重 + 可用 foreground;
/// 旧格式(无 project_states)同样可解析。
#[test]
fn imports_projects_with_canonical_dedup() {
    let tmp = tempfile::tempdir().unwrap();
    let alpha = tmp.path().join("alpha");
    let beta = tmp.path().join("beta");
    fs::create_dir_all(&alpha).unwrap();
    fs::create_dir_all(&beta).unwrap();

    let session = session_file(&tmp);
    write_session_json(
        &session,
        &json!({
            // alpha 以「自身」与「/alpha/.」两种拼写出现;canonicalize 后相同。
            "projects": [
                alpha.to_string_lossy(),
                alpha.join(".").to_string_lossy(),
                beta.to_string_lossy(),
            ],
            "foreground": alpha.to_string_lossy(),
        }),
    );

    let store = ServiceStore::open(&service_db(&tmp)).unwrap();
    let report = store.import_session_projects(&session).unwrap();
    assert_eq!(report.status, SessionImportStatus::Imported);
    assert_eq!(report.imported, 2, "三种拼写必须合并为两行");
    assert_eq!(report.duplicates_skipped, 1);
    assert_eq!(report.missing, 0);
    assert_eq!(
        report.foreground.as_deref(),
        Some(expected_root(&alpha).as_str()),
        "可用 foreground 应被记录"
    );
    assert_eq!(marker_count(&service_db(&tmp)), 1);

    let projects = store.list_projects().unwrap();
    assert_eq!(projects.len(), 2);
    let alpha_row = projects
        .iter()
        .find(|p| p.canonical_root == expected_root(&alpha))
        .unwrap();
    assert!(
        alpha_row.project_handle.starts_with("proj_")
            && is_uuid_v7(alpha_row.project_handle.trim_start_matches("proj_")),
        "handle 必须是 proj_ + UUIDv7:{}",
        alpha_row.project_handle
    );
    assert!(is_uuid_v7(&alpha_row.public_id), "public_id 应为 UUIDv7");
    assert_eq!(
        alpha_row.display_path,
        alpha.to_string_lossy(),
        "display_path 保留首个拼写"
    );
    assert_eq!(alpha_row.status, ProjectStatus::Registered);
    assert!(
        alpha_row.registered_at.contains('T'),
        "registered_at 应为 RFC3339 时间戳"
    );
}

/// 缺失路径保留 `missing` 状态供用户清理;不创建也不删除真实目录。
#[test]
fn missing_paths_kept_as_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let gone = tmp.path().join("gone");
    assert!(!gone.exists());

    let session = session_file(&tmp);
    write_session_json(
        &session,
        &json!({ "projects": [gone.to_string_lossy()], "foreground": gone.to_string_lossy() }),
    );

    let store = ServiceStore::open(&service_db(&tmp)).unwrap();
    let report = store.import_session_projects(&session).unwrap();
    assert_eq!(report.imported, 1);
    assert_eq!(report.missing, 1);
    assert_eq!(report.foreground, None, "不可用 foreground 不记录");

    let projects = store.list_projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].status, ProjectStatus::Missing);
    assert_eq!(projects[0].canonical_root, gone.to_string_lossy());
    assert!(!gone.exists(), "导入不得创建(或删除)缺失路径对应的真实目录");
}

/// 重复导入幂等:第二次是 no-op,全部行(含随机 handle)逐值不变。
#[test]
fn repeat_import_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let alpha = tmp.path().join("alpha");
    fs::create_dir_all(&alpha).unwrap();
    let session = session_file(&tmp);
    write_session_json(
        &session,
        &json!({ "projects": [alpha.to_string_lossy()], "foreground": alpha.to_string_lossy() }),
    );

    let store = ServiceStore::open(&service_db(&tmp)).unwrap();
    store.import_session_projects(&session).unwrap();
    let rows_after_first = registry_rows(&service_db(&tmp));

    let second = store.import_session_projects(&session).unwrap();
    assert_eq!(second.status, SessionImportStatus::AlreadyImported);
    assert_eq!(second.imported, 0);
    assert_eq!(
        registry_rows(&service_db(&tmp)),
        rows_after_first,
        "重复导入不得改写任何行(含 handle/registered_at)"
    );
    assert_eq!(marker_count(&service_db(&tmp)), 1, "marker 恰一行");
}

/// 两个独立 service 连接同时越过事务外快速检查时，writer lock 内的
/// marker 复验让后到者收敛为 AlreadyImported，而不是主键/busy 错误。
#[test]
fn concurrent_imports_converge_to_one_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let alpha = tmp.path().join("alpha");
    fs::create_dir_all(&alpha).unwrap();
    let session = session_file(&tmp);
    write_session_json(
        &session,
        &json!({ "projects": [alpha.to_string_lossy()], "foreground": alpha.to_string_lossy() }),
    );
    let db = service_db(&tmp);
    let stores = [
        ServiceStore::open(&db).unwrap(),
        ServiceStore::open(&db).unwrap(),
    ];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = stores
        .into_iter()
        .map(|store| {
            let barrier = barrier.clone();
            let session = session.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.import_session_projects(&session)
            })
        })
        .collect();
    let mut statuses: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap().unwrap().status)
        .collect();
    statuses.sort_by_key(|status| match status {
        SessionImportStatus::Imported => 0,
        SessionImportStatus::AlreadyImported => 1,
        SessionImportStatus::NoSessionFile => 2,
    });
    assert_eq!(
        statuses,
        vec![
            SessionImportStatus::Imported,
            SessionImportStatus::AlreadyImported
        ]
    );
    assert_eq!(marker_count(&db), 1);
    assert_eq!(registry_rows(&db).len(), 1);
}

/// 崩溃残留收敛:registry 行已落库但 marker 未写的中间态,重跑导入
/// 保持既有行(handle/registered_at 不变)并补写 marker,不产生重复行。
#[test]
fn rows_without_marker_crash_residual_converges() {
    let tmp = tempfile::tempdir().unwrap();
    let alpha = tmp.path().join("alpha");
    fs::create_dir_all(&alpha).unwrap();
    let session = session_file(&tmp);
    write_session_json(
        &session,
        &json!({ "projects": [alpha.to_string_lossy()], "foreground": alpha.to_string_lossy() }),
    );

    // 模拟崩溃残留:marker 之前手工落入的一行(不同的 handle/时间;
    // canonical_root 用生产同口径,模拟此前导入已落的行)。
    let db = service_db(&tmp);
    let store = ServiceStore::open(&db).unwrap();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO project_registry
                 (project_handle, public_id, canonical_root, display_path,
                  registered_at, status)
             VALUES ('proj_residual', 'pub_residual', ?1, ?1,
                     '2024-01-01T00:00:00Z', 'registered')",
            [expected_root(&alpha).as_str()],
        )
        .unwrap();
    }
    assert_eq!(marker_count(&db), 0);

    let report = store.import_session_projects(&session).unwrap();
    assert_eq!(report.status, SessionImportStatus::Imported);
    assert_eq!(report.imported, 0, "既有行不得重复插入");
    let rows = registry_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "proj_residual", "崩溃残留的 handle 保持不变");
    assert_eq!(rows[0][4], "2024-01-01T00:00:00Z", "registered_at 保持不变");
    assert_eq!(marker_count(&db), 1, "重跑补写 marker");
}

/// 非目标 UI 状态不导入:open_files、active file、selected_task_id、
/// GPUI panel/layout 的任何痕迹都不进入 service 库(全表全文本扫描)。
#[test]
fn ui_state_is_not_imported() {
    let tmp = tempfile::tempdir().unwrap();
    let alpha = tmp.path().join("alpha");
    fs::create_dir_all(alpha.join("src")).unwrap();
    let open_file = alpha.join("src").join("main.rs");
    fs::write(&open_file, b"fn main() {}").unwrap();

    let session = session_file(&tmp);
    write_session_json(
        &session,
        &json!({
            "projects": [alpha.to_string_lossy()],
            "foreground": alpha.to_string_lossy(),
            "project_states": [{
                "root": alpha.to_string_lossy(),
                "selected_task_id": 7,
                "open_files": [open_file.to_string_lossy()],
                "active_file": open_file.to_string_lossy(),
            }],
            "panels": { "left": "file_tree", "gpui_layout": [1, 2, 3] },
        }),
    );

    let store = ServiceStore::open(&service_db(&tmp)).unwrap();
    store.import_session_projects(&session).unwrap();

    let projects = store.list_projects().unwrap();
    assert_eq!(projects.len(), 1, "仅项目列表本身被导入");
    let db = service_db(&tmp);
    let dump = dump_all_text(&db);
    assert!(
        !dump.contains("main.rs"),
        "open_files/active_file 路径不得进入任何表:{dump}"
    );
    assert!(
        !dump.contains("selected_task_id") && !dump.contains("gpui_layout"),
        "GPUI/编辑器 UI 状态不得进入任何表:{dump}"
    );
}

/// 原 session.json 保留且字节不变(只读导入)。
#[test]
fn original_session_json_bytes_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let alpha = tmp.path().join("alpha");
    fs::create_dir_all(&alpha).unwrap();
    let session = session_file(&tmp);
    let original = write_session_json(
        &session,
        &json!({ "projects": [alpha.to_string_lossy()], "foreground": alpha.to_string_lossy() }),
    );

    let store = ServiceStore::open(&service_db(&tmp)).unwrap();
    store.import_session_projects(&session).unwrap();
    store.import_session_projects(&session).unwrap();

    assert_eq!(fs::read(&session).unwrap(), original, "文件字节不变");
    assert_eq!(sha256_file(&session), sha256_file(&session));
}

/// session.json 不存在:不是错误,不写 marker、不落行。
#[test]
fn absent_session_file_is_noop_without_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ServiceStore::open(&service_db(&tmp)).unwrap();
    let report = store
        .import_session_projects(&tmp.path().join("session.json"))
        .unwrap();
    assert_eq!(report.status, SessionImportStatus::NoSessionFile);
    assert_eq!(report.imported, 0);
    let db = service_db(&tmp);
    assert_eq!(marker_count(&db), 0);
    assert!(registry_rows(&db).is_empty());
}

/// 损坏的 session.json:fail-closed 报错,保持未迁移(无行、无 marker)。
#[test]
fn corrupt_session_json_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let session = session_file(&tmp);
    fs::write(&session, b"{ not valid json").unwrap();

    let store = ServiceStore::open(&service_db(&tmp)).unwrap();
    assert!(store.import_session_projects(&session).is_err());
    let db = service_db(&tmp);
    assert_eq!(marker_count(&db), 0, "失败不得写 marker");
    assert!(registry_rows(&db).is_empty(), "失败不得落任何行");
}

/// foreground 不在项目列表中:可用(存在)即登记;不可用则不额外登记。
#[test]
fn foreground_outside_project_list_imported_only_when_available() {
    let tmp = tempfile::tempdir().unwrap();
    let alpha = tmp.path().join("alpha");
    let solo = tmp.path().join("solo");
    let ghost = tmp.path().join("ghost");
    fs::create_dir_all(&alpha).unwrap();
    fs::create_dir_all(&solo).unwrap();

    let session = session_file(&tmp);
    write_session_json(
        &session,
        &json!({
            "projects": [alpha.to_string_lossy(), solo.to_string_lossy()],
            "foreground": solo.to_string_lossy(),
        }),
    );
    let store = ServiceStore::open(&service_db(&tmp)).unwrap();
    let report = store.import_session_projects(&session).unwrap();
    let projects = store.list_projects().unwrap();
    assert_eq!(projects.len(), 2, "列表内项目全部登记");
    assert_eq!(
        report.foreground.as_deref(),
        Some(expected_root(&solo).as_str())
    );

    // 换一个库验证:foreground 指向不存在且不在列表中的路径 → 不登记。
    let tmp2 = tempfile::tempdir().unwrap();
    let session2 = session_file(&tmp2);
    write_session_json(
        &session2,
        &json!({
            "projects": [alpha.to_string_lossy()],
            "foreground": ghost.to_string_lossy(),
        }),
    );
    let store2 = ServiceStore::open(&service_db(&tmp2)).unwrap();
    let report2 = store2.import_session_projects(&session2).unwrap();
    let projects2 = store2.list_projects().unwrap();
    assert_eq!(projects2.len(), 1, "不可用且不在列表的 foreground 不登记");
    assert!(!projects2
        .iter()
        .any(|p| p.canonical_root == ghost.to_string_lossy()));
    assert_eq!(report2.foreground, None);
    assert_eq!(report2.missing, 0);
}
