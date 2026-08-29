//! 节点检查器独立模块(UI 计划 Task 2)。
//!
//! 检查器视图随画布渲染(`workflow_canvas::WorkflowNodeInspector`);
//! 本模块承载可独立测试的检查器纯逻辑(后续字段扩展落点)。

pub use crate::workflow_canvas::WorkflowNodeInspector;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow_editor::EditorNode;

    #[test]
    fn inspector_captures_node_identity_and_collapses() {
        let node = EditorNode {
            instructions: String::new(),
            key: "step-1".into(),
            title: "审查".into(),
            instance_id: "inst_a".into(),
            deps: vec![],
        };
        let mut inspector = WorkflowNodeInspector::new(&node);
        assert_eq!(inspector.node_key, "step-1");
        assert_eq!(inspector.title_buffer, "审查");
        assert!(!inspector.collapsed);
        inspector.collapsed = true;
        assert!(inspector.collapsed);
    }
}
