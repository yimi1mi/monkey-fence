//! 已持有 Project Store transaction 内的封闭 Workflow mutation seam。
//! mf-kernel 用它把领域写、receipt 与 outbox 保持在同一 L-CMD 事务。

use crate::workflow::{ProjectWorkflowDraft, ProjectWorkflowRecord};

#[derive(Debug, thiserror::Error)]
pub enum WorkflowMutationError {
    #[error("workflow_scope_mismatch")]
    ScopeMismatch,
    #[error("workflow_validation:{0}")]
    Validation(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectWorkflowMutation {
    Create {
        draft: ProjectWorkflowDraft,
        expected_collection_revision: i64,
    },
    ReplaceSemantic {
        draft: ProjectWorkflowDraft,
        expected_semantic_revision: i64,
    },
    Delete {
        workflow_handle: String,
        expected_collection_revision: i64,
        expected_semantic_revision: i64,
        expected_presentation_revision: i64,
    },
    SetPresentation {
        workflow_handle: String,
        expected_presentation_revision: i64,
        viewport_json: Option<String>,
        collapse_json: Option<String>,
        layout_json: Option<String>,
    },
    SetNodePosition {
        workflow_handle: String,
        node_handle: String,
        expected_presentation_revision: i64,
        x: f64,
        y: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowMutationResult {
    pub before: Option<ProjectWorkflowRecord>,
    pub after: Option<ProjectWorkflowRecord>,
    pub collection_revision: i64,
    pub no_op: bool,
    pub affected_node_handle: Option<String>,
}
