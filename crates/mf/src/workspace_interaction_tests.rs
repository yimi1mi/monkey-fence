//! 鼠标可达操作菜单的覆盖测试：快捷键不能是唯一入口。

use std::collections::HashSet;

use crate::workspace::workspace_command_entries;

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
