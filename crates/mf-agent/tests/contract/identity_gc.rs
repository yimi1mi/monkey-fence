//! T1b 契约(Issue #17):identity/presentation/position 的同事务 GC。
//!
//! - 节点删除、边断开、工作流删除在同一 Store 事务清理 identity/
//!   presentation/position,不留孤儿 handle;
//! - 断开 edge 删除 identity,重新连接生成不同 handle(永不复用);
//! - 语义保存中途失败(trigger 注入)回滚保持旧 identity。

use crate::support::{graph_json, read_only};
use mf_agent::store::Store;
use mf_agent::workflow::ProjectWorkflowDraft;
use rusqlite::Connection;

fn node(key: &str, deps: &[&str]) -> mf_agent::workflow::WorkflowNodeDraft {
    mf_agent::workflow::WorkflowNodeDraft {
        key: key.to_string(),
        title: format!("节点 {key}"),
        instructions: "固定指令".to_string(),
        agent_instance_id: "inst".to_string(),
        deps: deps.iter().map(|d| d.to_string()).collect(),
    }
}

fn draft(key: &str, nodes: Vec<mf_agent::workflow::WorkflowNodeDraft>) -> ProjectWorkflowDraft {
    ProjectWorkflowDraft {
        key: key.to_string(),
        name: format!("工作流 {key}"),
        nodes,
        allow_unsafe_parallel: false,
    }
}

fn fresh_store() -> (tempfile::TempDir, std::sync::Arc<Store>) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
    (tmp, store)
}

fn counts(db: &std::path::Path) -> (i64, i64, i64, i64) {
    read_only(db)
        .query_row(
            "SELECT (SELECT COUNT(*) FROM workflow_node_identity),
                    (SELECT COUNT(*) FROM workflow_edge_identity),
                    (SELECT COUNT(*) FROM workflow_presentation),
                    (SELECT COUNT(*) FROM node_position)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

/// 工作流删除:identity/presentation/position 全部同事务清理,
/// 且 collection revision 递增。
#[test]
fn deleting_workflow_cleans_identity_presentation_position() {
    let (tmp, store) = fresh_store();
    let db = tmp.path().join("workflow-v1.db");
    let record = store
        .create_project_workflow_cas(
            &draft(
                "wf",
                vec![node("a", &[]), node("b", &["a"]), node("c", &["a"])],
            ),
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
    store
        .set_workflow_presentation_cas("wf", 1, Some("{\"zoom\":1}"), None, None)
        .unwrap();
    store
        .set_node_position_cas(&a_handle, 12.5, -3.0, 2)
        .unwrap();
    assert_eq!(counts(&db), (3, 2, 1, 1));

    let deleted = store.delete_project_workflow_cas("wf", 2).unwrap();
    assert!(deleted);
    assert_eq!(
        counts(&db),
        (0, 0, 0, 0),
        "删除工作流后不得残留任何 identity/presentation/position 行"
    );
    assert_eq!(
        store.workflow_collection_revision().unwrap(),
        3,
        "delete 只增 collection"
    );
    // FK 级联的原始层证明:直接删 project_workflows 行也清 identity
    let record2 = store
        .create_project_workflow_cas(&draft("wf2", vec![node("x", &[])]), 3)
        .unwrap();
    {
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute(
            "DELETE FROM project_workflows WHERE workflow_key = 'wf2'",
            [],
        )
        .unwrap();
    }
    assert_eq!(counts(&db), (0, 0, 0, 0), "FK CASCADE 兜底同样无孤儿");
    drop(record);
    drop(record2);
}

/// 节点删除:其 identity + position 同事务清理,触及它的边也清理,
/// 兄弟节点 handle 保持稳定。
#[test]
fn node_delete_cleans_identity_position_and_touching_edges() {
    let (tmp, store) = fresh_store();
    let db = tmp.path().join("workflow-v1.db");
    store
        .create_project_workflow_cas(
            &draft(
                "wf",
                vec![node("a", &[]), node("b", &["a"]), node("c", &["a"])],
            ),
            1,
        )
        .unwrap();
    let identities = store.workflow_node_identities("wf").unwrap();
    let a_handle = identities
        .iter()
        .find(|n| n.node_key == "a")
        .unwrap()
        .clone();
    let b_handle = identities
        .iter()
        .find(|n| n.node_key == "b")
        .unwrap()
        .clone();
    store
        .set_node_position_cas(&a_handle.node_handle, 1.0, 2.0, 1)
        .unwrap();

    // 删除节点 a(语义编辑):b/c 保留但去掉对 a 的依赖
    store
        .save_project_workflow_semantic_cas(&draft("wf", vec![node("b", &[]), node("c", &[])]), 1)
        .unwrap();

    let remaining = store.workflow_node_identities("wf").unwrap();
    assert_eq!(remaining.len(), 2, "a 的 identity 已删");
    assert!(
        remaining
            .iter()
            .any(|n| n.node_key == "b" && n.node_handle == b_handle.node_handle),
        "兄弟节点 handle 稳定"
    );
    assert_eq!(
        store.workflow_edge_identities("wf").unwrap().len(),
        0,
        "触及 a 的边全部清理"
    );
    assert_eq!(counts(&db), (2, 0, 0, 0), "position 随节点删除清理");
    assert!(store
        .node_position(&a_handle.node_handle)
        .unwrap()
        .is_none());
}

/// 边断开删除 identity;重新连接必须生成新的 edge handle(永不复用),
/// 未涉及的节点 handle 保持不变。
#[test]
fn edge_disconnect_then_reconnect_gets_new_handle() {
    let (_tmp, store) = fresh_store();
    store
        .create_project_workflow_cas(&draft("wf", vec![node("a", &[]), node("b", &["a"])]), 1)
        .unwrap();
    let identities = store.workflow_node_identities("wf").unwrap();
    let a_handle = identities
        .iter()
        .find(|n| n.node_key == "a")
        .unwrap()
        .clone();
    let b_handle = identities
        .iter()
        .find(|n| n.node_key == "b")
        .unwrap()
        .clone();
    let first_edge = store.workflow_edge_identities("wf").unwrap()[0]
        .edge_handle
        .clone();

    // 断开:b 不再依赖 a
    store
        .save_project_workflow_semantic_cas(&draft("wf", vec![node("a", &[]), node("b", &[])]), 1)
        .unwrap();
    assert!(
        store.workflow_edge_identities("wf").unwrap().is_empty(),
        "断开后边 identity 必须删除"
    );

    // 重连:生成新 handle,且与旧 handle 不同
    store
        .save_project_workflow_semantic_cas(
            &draft("wf", vec![node("a", &[]), node("b", &["a"])]),
            2,
        )
        .unwrap();
    let edges = store.workflow_edge_identities("wf").unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(
        (
            edges[0].upstream_node_key.as_str(),
            edges[0].downstream_node_key.as_str()
        ),
        ("a", "b"),
        "端点键对正确"
    );
    assert_ne!(
        edges[0].edge_handle, first_edge,
        "重连必须生成不同 handle(旧 handle 永不复用)"
    );
    let identities_after = store.workflow_node_identities("wf").unwrap();
    assert!(
        identities_after
            .iter()
            .any(|n| n.node_key == "a" && n.node_handle == a_handle.node_handle),
        "重连不得改节点 handle"
    );
    assert!(
        identities_after
            .iter()
            .any(|n| n.node_key == "b" && n.node_handle == b_handle.node_handle),
        "重连不得改节点 handle"
    );
    drop(store);
}

/// 语义保存中途失败(trigger 注入 edge identity 插入):
/// 事务回滚,旧 identity 与 revision 完整保留。
#[test]
fn failed_semantic_save_keeps_old_identity() {
    let (tmp, store) = fresh_store();
    let db = tmp.path().join("workflow-v1.db");
    store
        .create_project_workflow_cas(&draft("wf", vec![node("a", &[]), node("b", &["a"])]), 1)
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fault_edge_insert BEFORE INSERT ON workflow_edge_identity
             BEGIN SELECT RAISE(ABORT, 'fault:edge-insert'); END;",
        )
        .unwrap();

    let before_identities = store.workflow_node_identities("wf").unwrap();
    let before_edges = store.workflow_edge_identities("wf").unwrap();
    let metric_key = {
        let conn = Connection::open(&db).unwrap();
        mf_agent::observability::store_metric_key(&conn, mf_agent::migration::StoreKind::Project)
    };
    let metrics_before = mf_agent::observability::storage_metrics_snapshot()
        .stores
        .get(&metric_key)
        .cloned()
        .unwrap_or_default();
    let err = store
        .save_project_workflow_semantic_cas(
            &draft("wf", vec![node("a", &[]), node("c", &["a"])]),
            1,
        )
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("fault:edge-insert"),
        "失败必须来自注入点: {err:#}"
    );

    // 回滚:graph 未变、identity 未变、semantic revision 未增
    let after = store.load_project_workflow("wf").unwrap().unwrap();
    assert_eq!(after.nodes.len(), 2, "冲突/失败的图不落任何一半状态");
    assert_eq!(after.semantic_revision, 1);
    let identities = store.workflow_node_identities("wf").unwrap();
    assert_eq!(identities, before_identities, "旧 identity 完整保留");
    assert_eq!(
        store.workflow_edge_identities("wf").unwrap(),
        before_edges,
        "半 identity 不得残留"
    );
    assert_eq!(
        mf_agent::observability::storage_metrics_snapshot()
            .stores
            .get(&metric_key)
            .cloned()
            .unwrap_or_default(),
        metrics_before,
        "回滚的 identity 删除不得累计 GC 指标"
    );
}

/// 迁移回填后的既有工作流经语义保存同样同步 identity
/// (删除 + 新增 + 保留),并且 revision 轴继续正确推进。
#[test]
fn migrated_workflow_semantic_save_syncs_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    crate::support::build_v6_project_db(
        &db,
        &[(
            "legacy",
            "旧",
            &graph_json(&[("old-a", &[]), ("old-b", &["old-a"])]),
        )],
    );
    let store = Store::open(&db).unwrap();
    let before = store.workflow_node_identities("legacy").unwrap();
    assert_eq!(before.len(), 2);

    store
        .save_project_workflow_semantic_cas(
            &draft(
                "legacy",
                vec![node("old-a", &[]), node("new-c", &["old-a"])],
            ),
            1,
        )
        .unwrap();
    let after = store.workflow_node_identities("legacy").unwrap();
    assert_eq!(after.len(), 2, "old-b 删、new-c 增");
    assert!(
        after.iter().any(|n| n.node_key == "old-a"
            && n.node_handle
                == before
                    .iter()
                    .find(|n| n.node_key == "old-a")
                    .unwrap()
                    .node_handle),
        "保留节点的 handle 稳定"
    );
    assert!(after.iter().any(|n| n.node_key == "new-c"));
    assert!(!after.iter().any(|n| n.node_key == "old-b"));
    let edges = store.workflow_edge_identities("legacy").unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(
        (
            edges[0].upstream_node_key.as_str(),
            edges[0].downstream_node_key.as_str()
        ),
        ("old-a", "new-c")
    );
}
