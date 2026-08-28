//! 鼠标可达操作菜单的覆盖测试：快捷键不能是唯一入口。

use std::collections::HashSet;

use crate::project_context::normalize_project_path;
use crate::workspace::{project_switcher_items, workspace_command_entries};

#[test]
fn global_workspace_operations_are_always_mouse_reachable() {
    let ids: HashSet<_> = workspace_command_entries(false, false)
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    for required in [
        "open_folder",
        "toggle_left",
        "toggle_explorer",
        "toggle_tasks",
        "toggle_vcs",
        "show_agents",
        "show_pipeline",
        "toggle_console",
        "project_search",
        "open_settings",
    ] {
        assert!(ids.contains(required), "操作菜单缺少 {required}");
    }
}

#[test]
fn project_and_editor_operations_appear_when_context_exists() {
    let ids: HashSet<_> = workspace_command_entries(true, true)
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    for required in [
        "quick_open",
        "refresh_tree",
        "close_tab",
        "next_tab",
        "prev_tab",
        "save_file",
        "undo",
        "redo",
        "select_all",
        "duplicate_line",
        "move_line_up",
        "move_line_down",
    ] {
        assert!(ids.contains(required), "上下文操作菜单缺少 {required}");
    }
}

#[test]
fn project_switcher_keeps_every_open_project_and_marks_one_active() {
    let projects: Vec<_> = (0..50)
        .map(|index| {
            normalize_project_path(std::path::Path::new(&format!(
                "C:/projects/project-{index:02}"
            )))
            .0
        })
        .collect();
    let active = projects[37].clone();

    let items = project_switcher_items(&projects, Some(&active));

    assert_eq!(items.len(), 50, "项目多时选择器不得截断");
    assert_eq!(items.iter().filter(|item| item.active).count(), 1);
    assert_eq!(items[37].id, active);
    assert_eq!(items[0].name, "project-00");
}
