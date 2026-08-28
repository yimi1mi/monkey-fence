//! SessionState 兼容与恢复计划测试(M4)。

use crate::app_ctx::{
    choose_restore_project, plan_restore, AppCtx, ProjectSessionState, SessionState,
};
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mf-session-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 老格式 `{projects, foreground}` 仍可读取,project_states 默认为空。
#[test]
fn legacy_session_json_still_loads() {
    let dir = tmp("legacy");
    let path = dir.join("session.json");
    std::fs::write(&path, r#"{"projects":["D:/a","D:/b"],"foreground":"D:/b"}"#).unwrap();
    let state = AppCtx::load_session_at(&path);
    assert_eq!(state.projects.len(), 2);
    assert_eq!(
        state.foreground.as_deref(),
        Some(std::path::Path::new("D:/b"))
    );
    assert!(state.project_states.is_empty(), "老格式无 project_states");
}

/// 新格式:两项目、各自 selected task / open files / active file 往返。
#[test]
fn new_format_roundtrips_two_projects() {
    let dir = tmp("roundtrip");
    let path = dir.join("session.json");
    let a = dir.join("a");
    let b = dir.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let fa = a.join("main.rs");
    let fb = b.join("lib.rs");
    std::fs::write(&fa, "fn main(){}").unwrap();
    std::fs::write(&fb, "pub fn f(){}").unwrap();
    let state = SessionState {
        projects: vec![a.clone(), b.clone()],
        foreground: Some(b.clone()),
        project_states: vec![
            ProjectSessionState {
                root: a.clone(),
                selected_task_id: Some(3),
                open_files: vec![fa.clone()],
                active_file: Some(fa.clone()),
            },
            ProjectSessionState {
                root: b.clone(),
                selected_task_id: Some(1),
                open_files: vec![fb.clone()],
                active_file: Some(fb.clone()),
            },
        ],
    };
    AppCtx::save_session_at(&path, &state);
    let back = AppCtx::load_session_at(&path);
    assert_eq!(back.project_states.len(), 2);
    let sa = &back.project_states[0];
    let sb = &back.project_states[1];
    assert_eq!(sa.root, a);
    assert_eq!(sa.selected_task_id, Some(3));
    assert_eq!(sa.open_files, vec![fa.clone()]);
    assert_eq!(sa.active_file.as_deref(), Some(fa.as_path()));
    assert_eq!(sb.root, b);
    assert_eq!(sb.selected_task_id, Some(1));
    assert_eq!(back.foreground.as_deref(), Some(b.as_path()));
}

/// 文件删除/移出项目后,恢复计划忽略该文件;active_file 一并清除。
#[test]
fn plan_restore_skips_missing_or_foreign_files() {
    let dir = tmp("plan-files");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let exists = proj.join("ok.rs");
    std::fs::write(&exists, "x").unwrap();
    let deleted = proj.join("gone.rs"); // 不存在
    let foreign = dir.join("outside.rs"); // 存在但不属于该项目
    std::fs::write(&foreign, "x").unwrap();
    let session = SessionState {
        project_states: vec![ProjectSessionState {
            root: proj.clone(),
            selected_task_id: Some(7),
            open_files: vec![exists.clone(), deleted.clone(), foreign.clone()],
            active_file: Some(deleted.clone()),
        }],
        ..Default::default()
    };
    let plans = plan_restore(&session, |_, _| true);
    let expected = crate::project_context::normalize_project_path(&exists)
        .0
        .root();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].open_files, vec![expected]);
    assert_eq!(plans[0].active_file, None, "活动文件被删除时清空");
    assert_eq!(plans[0].selected_task_id, Some(7));
}

#[test]
fn plan_restore_reorders_active_file_to_open_last() {
    let dir = tmp("active-last");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let active = proj.join("active.rs");
    let other = proj.join("other.rs");
    std::fs::write(&active, "active").unwrap();
    std::fs::write(&other, "other").unwrap();
    let session = SessionState {
        project_states: vec![ProjectSessionState {
            root: proj,
            selected_task_id: None,
            open_files: vec![active.clone(), other],
            active_file: Some(active.clone()),
        }],
        ..Default::default()
    };

    let plans = plan_restore(&session, |_, _| true);
    let expected_active = crate::project_context::normalize_project_path(&active)
        .0
        .root();

    assert_eq!(
        plans[0].open_files.last(),
        Some(&expected_active),
        "Workspace 按顺序打开文件，active_file 必须最后打开"
    );
}

#[test]
fn plan_restore_rejects_parent_dir_escape() {
    let dir = tmp("parent-escape");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let outside = dir.join("outside.rs");
    std::fs::write(&outside, "outside").unwrap();
    let escaped = proj.join("..").join("outside.rs");
    let session = SessionState {
        project_states: vec![ProjectSessionState {
            root: proj,
            selected_task_id: None,
            open_files: vec![escaped],
            active_file: None,
        }],
        ..Default::default()
    };

    let plans = plan_restore(&session, |_, _| true);

    assert!(
        plans[0].open_files.is_empty(),
        "词法 starts_with 不能把 root\\..\\outside 当成项目内文件"
    );
}

/// Task 删除/归档后恢复时清空 selection(不导致启动失败)。
#[test]
fn plan_restore_clears_missing_task_selection() {
    let proj = PathBuf::from(r"C:\gone\proj");
    let session = SessionState {
        project_states: vec![ProjectSessionState {
            root: proj.clone(),
            selected_task_id: Some(9),
            open_files: vec![],
            active_file: None,
        }],
        ..Default::default()
    };
    let plans = plan_restore(&session, |_, _| false);
    assert_eq!(plans[0].selected_task_id, None);
}

/// JSON 损坏回退空状态。
#[test]
fn corrupt_session_falls_back_to_empty() {
    let dir = tmp("corrupt");
    let path = dir.join("session.json");
    std::fs::write(&path, "{broken").unwrap();
    let state = AppCtx::load_session_at(&path);
    assert!(state.projects.is_empty());
    assert!(state.project_states.is_empty());
    assert!(state.foreground.is_none());
}

/// 保存的项目顺序与前台 fallback 稳定:roundtrip 保持顺序。
#[test]
fn project_order_and_foreground_are_stable() {
    let dir = tmp("order");
    let path = dir.join("session.json");
    let state = SessionState {
        projects: vec!["D:/p1".into(), "D:/p2".into(), "D:/p3".into()],
        foreground: Some("D:/p3".into()),
        project_states: vec![
            ProjectSessionState {
                root: "D:/p1".into(),
                ..Default::default()
            },
            ProjectSessionState {
                root: "D:/p2".into(),
                ..Default::default()
            },
            ProjectSessionState {
                root: "D:/p3".into(),
                ..Default::default()
            },
        ],
    };
    AppCtx::save_session_at(&path, &state);
    let back = AppCtx::load_session_at(&path);
    assert_eq!(back.projects, state.projects);
    assert_eq!(
        back.project_states
            .iter()
            .map(|p| p.root.clone())
            .collect::<Vec<_>>(),
        vec![
            PathBuf::from("D:/p1"),
            PathBuf::from("D:/p2"),
            PathBuf::from("D:/p3")
        ],
    );
    assert_eq!(
        back.foreground.as_deref(),
        Some(std::path::Path::new("D:/p3"))
    );
}

#[test]
fn legacy_session_without_foreground_falls_back_to_last_open_project() {
    let projects = vec![PathBuf::from("D:/p1"), PathBuf::from("D:/p2")];

    let selected = choose_restore_project(None, None, &projects);

    assert_eq!(selected, Some(PathBuf::from("D:/p2")));
}
