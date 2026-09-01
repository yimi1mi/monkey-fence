//! T1b 契约(Issue #17):workflow collection / semantic / presentation
//! 三条 CAS 轴互不串增。
//!
//! - create/delete 只增 collection;语义编辑(图/名称)只增 semantic;
//!   viewport/collapse/layout/position 只增 presentation;
//! - stale 冲突返回稳定判别错误(RevisionConflict),不写入任何一半状态;
//! - 冲突后重试携带新 revision 成功。

use mf_agent::model::{RevisionAxis, RevisionConflict};
use mf_agent::store::Store;
use mf_agent::workflow::ProjectWorkflowDraft;
use std::sync::Arc;

fn node(key: &str, deps: &[&str]) -> mf_agent::workflow::WorkflowNodeDraft {
    mf_agent::workflow::WorkflowNodeDraft {
        key: key.to_string(),
        title: format!("节点 {key}"),
        instructions: "固定指令".to_string(),
        agent_instance_id: "inst".to_string(),
        deps: deps.iter().map(|d| d.to_string()).collect(),
    }
}

fn draft(
    key: &str,
    name: &str,
    nodes: Vec<mf_agent::workflow::WorkflowNodeDraft>,
) -> ProjectWorkflowDraft {
    ProjectWorkflowDraft {
        key: key.to_string(),
        name: name.to_string(),
        nodes,
        allow_unsafe_parallel: false,
    }
}

fn fresh_store() -> (tempfile::TempDir, Arc<Store>) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
    (tmp, store)
}

/// 断言错误是 RevisionConflict 且轴/期望/实际精确。
fn expect_conflict(err: &anyhow::Error, axis: RevisionAxis, expected: i64, actual: i64) {
    let conflict = err
        .downcast_ref::<RevisionConflict>()
        .unwrap_or_else(|| panic!("必须是 RevisionConflict 判别值: {err:#}"));
    assert_eq!(conflict.axis, axis);
    assert_eq!(conflict.expected, expected);
    assert_eq!(conflict.actual, actual);
}

#[test]
fn create_bumps_collection_only() {
    let (_tmp, store) = fresh_store();
    assert_eq!(
        store.workflow_collection_revision().unwrap(),
        1,
        "singleton 初始 1"
    );

    let record = store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();
    assert_eq!(record.semantic_revision, 1);
    assert_eq!(record.presentation_revision, 1);
    assert!(crate::support::is_uuid_v7(&record.public_handle));
    assert_eq!(store.workflow_collection_revision().unwrap(), 2);
    assert_eq!(store.workflow_node_identities("wf").unwrap().len(), 1);
}

#[test]
fn stale_create_conflict_writes_nothing() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();
    // 集合已推进到 2,持旧快照 (1) 的创建命令必须冲突
    let err = store
        .create_project_workflow_cas(&draft("wf2", "二", vec![node("a", &[])]), 1)
        .unwrap_err();
    expect_conflict(&err, RevisionAxis::Collection, 1, 2);

    assert!(
        store.load_project_workflow("wf2").unwrap().is_none(),
        "冲突不写任何一半"
    );
    assert!(
        store.workflow_node_identities("wf2").unwrap().is_empty(),
        "冲突不留 identity"
    );
    assert_eq!(store.workflow_collection_revision().unwrap(), 2);
    // 携带新 revision 重试成功
    store
        .create_project_workflow_cas(&draft("wf2", "二", vec![node("a", &[])]), 2)
        .unwrap();
    assert_eq!(store.workflow_collection_revision().unwrap(), 3);
}

#[test]
fn semantic_edit_bumps_semantic_only() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();

    let record = store
        .save_project_workflow_semantic_cas(
            &draft("wf", "一", vec![node("a", &[]), node("b", &["a"])]),
            1,
        )
        .unwrap();
    assert_eq!(record.semantic_revision, 2, "语义编辑只增 semantic");
    assert_eq!(record.presentation_revision, 1, "不串 presentation");
    assert_eq!(
        store.workflow_collection_revision().unwrap(),
        2,
        "不串 collection"
    );
    assert_eq!(
        store.workflow_node_identities("wf").unwrap().len(),
        2,
        "identity 同步"
    );
}

#[test]
fn stale_semantic_conflict_writes_nothing() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();
    store
        .save_project_workflow_semantic_cas(&draft("wf", "一改", vec![node("a", &[])]), 1)
        .unwrap();

    // 持旧 semantic=1 的编辑命令:冲突,图/名称/identity 全部不变
    let err = store
        .save_project_workflow_semantic_cas(
            &draft("wf", "二改", vec![node("a", &[]), node("b", &["a"])]),
            1,
        )
        .unwrap_err();
    expect_conflict(&err, RevisionAxis::Semantic, 1, 2);

    let current = store.load_project_workflow("wf").unwrap().unwrap();
    assert_eq!(current.name, "一改", "冲突不写名称");
    assert_eq!(current.nodes.len(), 1, "冲突不写图");
    assert_eq!(current.semantic_revision, 2);
    assert_eq!(
        store.workflow_node_identities("wf").unwrap().len(),
        1,
        "冲突不同步 identity"
    );
    // 携带新 revision 重试成功
    store
        .save_project_workflow_semantic_cas(
            &draft("wf", "二改", vec![node("a", &[]), node("b", &["a"])]),
            2,
        )
        .unwrap();
    assert_eq!(
        store
            .load_project_workflow("wf")
            .unwrap()
            .unwrap()
            .semantic_revision,
        3
    );
}

#[test]
fn presentation_writes_bump_presentation_only() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();

    // 初始无 presentation 行(迁移不伪造)
    assert!(store.workflow_presentation("wf").unwrap().is_none());

    let revision = store
        .set_workflow_presentation_cas("wf", 1, Some(r#"{"zoom":1.25,"x":10,"y":20}"#), None, None)
        .unwrap();
    assert_eq!(revision, 2, "presentation 写入只增 presentation");
    let state = store.workflow_presentation("wf").unwrap().unwrap();
    assert_eq!(
        state.viewport_json.as_deref(),
        Some(r#"{"zoom":1.25,"x":10,"y":20}"#)
    );
    assert_eq!(state.collapse_json, None);
    assert_eq!(state.layout_json, None);

    // collapse/layout 单独更新,viewport 不被改写
    store
        .set_workflow_presentation_cas("wf", 2, None, Some(r#"["b"]"#), Some(r#"{"mode":"dagre"}"#))
        .unwrap();
    let state = store.workflow_presentation("wf").unwrap().unwrap();
    assert_eq!(
        state.viewport_json.as_deref(),
        Some(r#"{"zoom":1.25,"x":10,"y":20}"#)
    );
    assert_eq!(state.collapse_json.as_deref(), Some(r#"["b"]"#));
    assert_eq!(state.layout_json.as_deref(), Some(r#"{"mode":"dagre"}"#));

    let record = store.load_project_workflow("wf").unwrap().unwrap();
    assert_eq!(record.presentation_revision, 3);
    assert_eq!(record.semantic_revision, 1, "presentation 不串 semantic");
    assert_eq!(
        store.workflow_collection_revision().unwrap(),
        2,
        "不串 collection"
    );
}

#[test]
fn position_write_bumps_presentation_only() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(
            &draft("wf", "一", vec![node("a", &[]), node("b", &["a"])]),
            1,
        )
        .unwrap();
    let a_handle = store
        .workflow_node_identities("wf")
        .unwrap()
        .iter()
        .find(|n| n.node_key == "a")
        .unwrap()
        .node_handle
        .clone();

    let revision = store
        .set_node_position_cas(&a_handle, 100.5, -42.25, 1)
        .unwrap();
    assert_eq!(revision, 2, "position 属 presentation 轴");
    let positions = store.node_positions_of_workflow("wf").unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].0, a_handle);
    assert_eq!(positions[0].1, 100.5);
    assert_eq!(positions[0].2, -42.25);

    let record = store.load_project_workflow("wf").unwrap().unwrap();
    assert_eq!(record.presentation_revision, 2);
    assert_eq!(record.semantic_revision, 1, "不串 semantic");
    assert_eq!(
        store.workflow_collection_revision().unwrap(),
        2,
        "不串 collection"
    );

    // 同节点重复写是 upsert,不是新行
    store.set_node_position_cas(&a_handle, 1.0, 2.0, 2).unwrap();
    let positions = store.node_positions_of_workflow("wf").unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].1, 1.0);
}

#[test]
fn stale_presentation_conflict_writes_nothing() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();
    let a_handle = store
        .workflow_node_identities("wf")
        .unwrap()
        .iter()
        .find(|n| n.node_key == "a")
        .unwrap()
        .node_handle
        .clone();
    store
        .set_workflow_presentation_cas("wf", 1, Some(r#"{"zoom":2}"#), None, None)
        .unwrap();

    let err = store
        .set_workflow_presentation_cas("wf", 1, Some(r#"{"zoom":9}"#), None, None)
        .unwrap_err();
    expect_conflict(&err, RevisionAxis::Presentation, 1, 2);
    let err = store
        .set_node_position_cas(&a_handle, 7.0, 8.0, 1)
        .unwrap_err();
    expect_conflict(&err, RevisionAxis::Presentation, 1, 2);

    let state = store.workflow_presentation("wf").unwrap().unwrap();
    assert_eq!(
        state.viewport_json.as_deref(),
        Some(r#"{"zoom":2}"#),
        "冲突不写 viewport"
    );
    assert!(
        store.node_positions_of_workflow("wf").unwrap().is_empty(),
        "冲突不写 position"
    );
    assert_eq!(
        store
            .load_project_workflow("wf")
            .unwrap()
            .unwrap()
            .presentation_revision,
        2
    );
}

#[test]
fn delete_bumps_collection_only_and_stale_delete_conflicts() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();

    let err = store.delete_project_workflow_cas("wf", 1).unwrap_err();
    expect_conflict(&err, RevisionAxis::Collection, 1, 2);
    assert!(
        store.load_project_workflow("wf").unwrap().is_some(),
        "冲突不删除"
    );

    assert!(store.delete_project_workflow_cas("wf", 2).unwrap());
    assert_eq!(store.workflow_collection_revision().unwrap(), 3);
    assert!(store.load_project_workflow("wf").unwrap().is_none());
    assert_eq!(
        store.workflow_node_identities("wf").unwrap().len(),
        0,
        "删除同事务清理 identity"
    );
}

#[test]
fn name_change_and_noop_rules() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();

    // 名称属于工作流自身内容:走 semantic 轴
    let record = store
        .save_project_workflow_semantic_cas(&draft("wf", "改名", vec![node("a", &[])]), 1)
        .unwrap();
    assert_eq!(record.semantic_revision, 2);
    assert_eq!(record.name, "改名");

    // 完全相同的 no-op 保存:revision 不增、handle 不变
    let before = store.load_project_workflow("wf").unwrap().unwrap();
    let after = store
        .save_project_workflow_semantic_cas(&draft("wf", "改名", vec![node("a", &[])]), 2)
        .unwrap();
    assert_eq!(after.semantic_revision, 2, "no-op 不增 semantic");
    assert_eq!(after.public_handle, before.public_handle, "handle 稳定");
}

/// 非法图(重复键/未知依赖)在写任何行之前 fail-closed。
#[test]
fn invalid_graph_rejected_before_writes() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();
    let err = store
        .save_project_workflow_semantic_cas(
            &draft("wf", "一", vec![node("a", &[]), node("a", &[])]),
            1,
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("重复"), "重复键: {err:#}");
    let err = store
        .save_project_workflow_semantic_cas(
            &draft("wf", "一", vec![node("a", &[]), node("b", &["ghost"])]),
            1,
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("未知"), "未知依赖: {err:#}");
    assert_eq!(
        store
            .load_project_workflow("wf")
            .unwrap()
            .unwrap()
            .semantic_revision,
        1,
        "非法图不推进 revision"
    );
    assert_eq!(store.workflow_node_identities("wf").unwrap().len(), 1);
}

/// presentation JSON 必须合法;非法值 fail-closed 不落库。
#[test]
fn invalid_presentation_json_rejected() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", "一", vec![node("a", &[])]), 1)
        .unwrap();
    let err = store
        .set_workflow_presentation_cas("wf", 1, Some("{not-json"), None, None)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("JSON"),
        "非法 viewport: {err:#}"
    );
    assert!(store.workflow_presentation("wf").unwrap().is_none());
    assert_eq!(
        store
            .load_project_workflow("wf")
            .unwrap()
            .unwrap()
            .presentation_revision,
        1
    );
}

/// GPUI 旧写路径(无 CAS)同样推进 revision 轴并同步 identity:
/// 新建走 collection,编辑走 semantic。
#[test]
fn legacy_save_paths_advance_axes_and_sync_identity() {
    let (_tmp, store) = fresh_store();
    // legacy save 新建 = collection 轴
    let created = store
        .save_project_workflow(&draft("wf", "一", vec![node("a", &[])]))
        .unwrap();
    assert_eq!(store.workflow_collection_revision().unwrap(), 2);
    assert_eq!(created.semantic_revision, 1);
    assert_eq!(store.workflow_node_identities("wf").unwrap().len(), 1);

    // legacy save 编辑 = semantic 轴
    let edited = store
        .save_project_workflow(&draft("wf", "一", vec![node("a", &[]), node("b", &["a"])]))
        .unwrap();
    assert_eq!(edited.semantic_revision, 2);
    assert_eq!(
        store.workflow_collection_revision().unwrap(),
        2,
        "编辑不串 collection"
    );
    assert_eq!(store.workflow_node_identities("wf").unwrap().len(), 2);

    // legacy delete = collection 轴 + identity 清理
    assert!(store.delete_project_workflow("wf").unwrap());
    assert_eq!(store.workflow_collection_revision().unwrap(), 3);
    assert_eq!(store.workflow_node_identities("wf").unwrap().len(), 0);
}

/// 未知工作流/节点的 CAS 写入必须显式报错(不得静默造行)。
#[test]
fn cas_on_unknown_targets_fails_explicitly() {
    let (_tmp, store) = fresh_store();
    let err = store
        .save_project_workflow_semantic_cas(&draft("ghost", "无", vec![node("a", &[])]), 1)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("ghost"),
        "编辑不存在的 workflow 必须报错: {err:#}"
    );

    let err = store
        .set_node_position_cas("01900000-0000-7000-8000-000000000000", 1.0, 1.0, 1)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("node_handle") || format!("{err:#}").contains("不存在"),
        "未知 node_handle 必须报错: {err:#}"
    );
}
