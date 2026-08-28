//! Workflow Editor 状态测试(UI Task 2):默认布局 B、偏好持久化、
//! 画布模型(拖入/连线/环拒绝/自动布局/选中)、检查器。

use crate::workflow_editor::{
    EditorNode, EditorPrefs, MemoryPrefs, WorkflowEditorState, WorkflowLayout,
};

fn node(key: &str, deps: &[&str]) -> EditorNode {
    EditorNode {
        key: key.into(),
        title: key.into(),
        instance_id: "inst_a".into(),
        deps: deps.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn default_layout_is_sidebar_and_user_choice_persists() {
    let mut prefs = MemoryPrefs::default();
    let mut state = WorkflowEditorState::load(&prefs);
    assert_eq!(state.layout(), WorkflowLayout::Sidebar);
    state.set_layout(WorkflowLayout::Stacked, &mut prefs);
    assert_eq!(
        WorkflowEditorState::load(&prefs).layout(),
        WorkflowLayout::Stacked
    );
    // 切回同样持久化
    let mut state = WorkflowEditorState::load(&prefs);
    state.set_layout(WorkflowLayout::Sidebar, &mut prefs);
    assert_eq!(
        WorkflowEditorState::load(&prefs).layout(),
        WorkflowLayout::Sidebar
    );
}

#[test]
fn drag_from_library_adds_node_with_unique_key() {
    let mut state = WorkflowEditorState::load(&MemoryPrefs::default());
    state.drag_from_library("inst_a");
    state.drag_from_library("inst_a");
    assert_eq!(state.nodes().len(), 2);
    let keys: Vec<&str> = state.nodes().iter().map(|n| n.key.as_str()).collect();
    assert_ne!(keys[0], keys[1], "同实例拖入两次应有不同节点键");
}

#[test]
fn dependency_add_and_remove_updates_graph() {
    let mut state = WorkflowEditorState::load(&MemoryPrefs::default());
    state.drag_from_library("inst_a");
    state.drag_from_library("inst_b");
    let a = state.nodes()[0].key.clone();
    let b = state.nodes()[1].key.clone();

    state.add_dependency(&b, &a).unwrap();
    assert_eq!(state.nodes()[1].deps, vec![a.clone()]);

    // 删除依赖
    state.remove_dependency(&b, &a);
    assert!(state.nodes()[1].deps.is_empty());
}

#[test]
fn cycle_rejected_by_editor() {
    let mut state = WorkflowEditorState::load(&MemoryPrefs::default());
    state.drag_from_library("inst_a");
    state.drag_from_library("inst_b");
    state.drag_from_library("inst_c");
    let a = state.nodes()[0].key.clone();
    let b = state.nodes()[1].key.clone();
    let c = state.nodes()[2].key.clone();

    state.add_dependency(&b, &a).unwrap();
    state.add_dependency(&c, &b).unwrap();
    // c→a→b→c 成环:拒绝
    assert!(state.add_dependency(&a, &c).is_err());
    assert!(state.nodes()[0].deps.is_empty());

    // 自依赖同样拒绝
    assert!(state.add_dependency(&a, &a).is_err());
}

#[test]
fn autolayout_layers_by_topology() {
    let mut state = WorkflowEditorState::load(&MemoryPrefs::default());
    state.drag_from_library("inst_a");
    state.drag_from_library("inst_b");
    state.drag_from_library("inst_c");
    let a = state.nodes()[0].key.clone();
    let b = state.nodes()[1].key.clone();
    let c = state.nodes()[2].key.clone();
    state.add_dependency(&b, &a).unwrap();
    state.add_dependency(&c, &b).unwrap();

    let layout = state.autolayout();
    // 层号:依赖全部在更早层
    let layer = |key: &str| {
        layout
            .iter()
            .position(|l| l.contains(&(key.to_string(), 0usize)) || l.iter().any(|(k, _)| k == key))
            .unwrap()
    };
    let (la, lb, lc) = (layer(&a), layer(&b), layer(&c));
    assert!(la < lb && lb < lc, "a={} b={} c={}", la, lb, lc);
}

#[test]
fn node_selection_and_inspector() {
    let mut state = WorkflowEditorState::load(&MemoryPrefs::default());
    state.drag_from_library("inst_a");
    assert!(state.selected().is_none());
    let a = state.nodes()[0].key.clone();
    state.select(&a);
    assert_eq!(state.selected().as_deref(), Some(a.as_str()));
    // 选中节点的检查器可改标题
    state.set_selected_title("新标题");
    assert_eq!(state.nodes()[0].title, "新标题");
    state.clear_selection();
    assert!(state.selected().is_none());
}

#[test]
fn delete_selected_node_removes_and_cleans_deps() {
    let mut state = WorkflowEditorState::load(&MemoryPrefs::default());
    state.drag_from_library("inst_a");
    state.drag_from_library("inst_b");
    let a = state.nodes()[0].key.clone();
    let b = state.nodes()[1].key.clone();
    state.add_dependency(&b, &a).unwrap();

    state.select(&a);
    state.delete_selected();
    assert_eq!(state.nodes().len(), 1);
    assert!(state.nodes()[0].deps.is_empty(), "悬空依赖应被清理");
}

#[test]
fn compiler_diagnostics_surface_in_editor() {
    let mut state = WorkflowEditorState::load(&MemoryPrefs::default());
    state.drag_from_library("inst_a");
    // 编辑器侧最小诊断:空图提示 + 环检测提示(完整编译在保存时执行)
    let mut empty = WorkflowEditorState::load(&MemoryPrefs::default());
    assert!(empty.diagnostics().iter().any(|d| d.contains("一个节点")));
    let _ = state;
}
