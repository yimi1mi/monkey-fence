//! T1b 契约(Issue #17):v6 既有工作流图的 node/edge identity 回填。
//!
//! - 多 workflow、多 node、多 deps(join/扇出)回填数量与映射精确;
//! - node_key 用 graph_json 既有 `WorkflowNodeDraft.key`,
//!   edge 按 downstream `deps` 的 upstream/downstream 键对回填;
//! - 全部 handle 是真实 UUIDv7、全局唯一、与 rowid/key 无派生关系;
//! - 相同 node_key 在不同工作流得到不同 node_handle。
//!
//! 全部基于 tempfile fixture。

use crate::support::{build_v6_project_db, graph_json, is_uuid_v7, read_only};
use mf_agent::schema::PROJECT_SCHEMA_VERSION;
use mf_agent::store::Store;
use rusqlite::Connection;
use std::collections::BTreeMap;

struct Backfilled {
    workflow_handles: BTreeMap<String, String>,
    nodes: Vec<(String, String, String)>,
    edges: Vec<(String, String, String, String)>,
}

/// 打开(迁移)后读取全部 identity 行,按稳定顺序返回。
fn backfilled(db: &std::path::Path) -> Backfilled {
    let conn = read_only(db);
    let mut workflow_handles = BTreeMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT workflow_key, public_handle FROM project_workflows")
            .unwrap();
        for (key, handle) in stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
        {
            workflow_handles.insert(key, handle);
        }
    }
    let nodes = {
        let mut stmt = conn
            .prepare(
                "SELECT workflow_handle, node_key, node_handle
                 FROM workflow_node_identity
                 ORDER BY workflow_handle, node_key",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    let edges = {
        let mut stmt = conn
            .prepare(
                "SELECT workflow_handle, upstream_node_key, downstream_node_key, edge_handle
                 FROM workflow_edge_identity
                 ORDER BY workflow_handle, upstream_node_key, downstream_node_key",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    Backfilled {
        workflow_handles,
        nodes,
        edges,
    }
}

/// 多 workflow / join / 扇出:数量、键对与句柄映射精确。
#[test]
fn backfill_counts_and_mappings_are_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    // w1:fetch → build(链)
    // w2:a → c,a → d,c → d(扇出 + 汇合)
    // w3:单节点
    build_v6_project_db(
        &db,
        &[
            (
                "w1",
                "链",
                &graph_json(&[("fetch", &[]), ("build", &["fetch"])]),
            ),
            (
                "w2",
                "汇合",
                &graph_json(&[("a", &[]), ("b", &[]), ("c", &["a"]), ("d", &["a", "c"])]),
            ),
            ("w3", "单点", &graph_json(&[("solo", &[])])),
        ],
    );

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    drop(store);

    let identity = backfilled(&db);
    let w1 = identity.workflow_handles["w1"].clone();
    let w2 = identity.workflow_handles["w2"].clone();
    let w3 = identity.workflow_handles["w3"].clone();
    assert!(is_uuid_v7(&w1) && is_uuid_v7(&w2) && is_uuid_v7(&w3));

    // 节点:键集合与所属工作流精确对应(归属 + node_key,顺序稳定)
    let node_pairs: Vec<(String, String)> = identity
        .nodes
        .iter()
        .map(|(h, k, _)| (h.clone(), k.clone()))
        .collect();
    assert_eq!(
        node_pairs,
        vec![
            (w1.clone(), "build".into()),
            (w1.clone(), "fetch".into()),
            (w2.clone(), "a".into()),
            (w2.clone(), "b".into()),
            (w2.clone(), "c".into()),
            (w2.clone(), "d".into()),
            (w3.clone(), "solo".into()),
        ],
        "node identity 恰 7 行,归属与 node_key 精确"
    );
    // node_handle 全部非空且互不相同
    let mut node_handles: Vec<&String> = identity.nodes.iter().map(|(_, _, h)| h).collect();
    node_handles.sort();
    node_handles.dedup();
    assert_eq!(node_handles.len(), identity.nodes.len());
    // 用 node_key→handle 映射做后续边断言
    let node_handle = |wf: &str, key: &str| -> String {
        identity
            .nodes
            .iter()
            .find(|(h, k, _)| h == wf && k == key)
            .unwrap()
            .2
            .clone()
    };
    assert_eq!(node_handle(&w1, "fetch").len(), 36, "标准 UUID 文本形态");

    // 边:严格按 downstream deps 的 (upstream, downstream) 键对
    let edge = |wf: &str, up: &str, down: &str| -> String {
        identity
            .edges
            .iter()
            .find(|(h, u, d, _)| h == wf && u == up && d == down)
            .unwrap_or_else(|| panic!("缺边 {up}→{down}"))
            .3
            .clone()
    };
    assert_eq!(identity.edges.len(), 4, "w1×1 + w2×3 + w3×0");
    assert!(is_uuid_v7(&edge(&w1, "fetch", "build")));
    assert!(is_uuid_v7(&edge(&w2, "a", "c")));
    assert!(is_uuid_v7(&edge(&w2, "a", "d")));
    assert!(is_uuid_v7(&edge(&w2, "c", "d")));
    // 不存在的键对不回填
    assert!(
        identity
            .edges
            .iter()
            .all(|(h, _, d, _)| !(h == &w2 && d == "b")),
        "无入边节点 b 不产生边 identity"
    );
    assert!(identity.edges.iter().all(|(h, _, _, _)| h != &w3));
}

/// 全部持久 handle:真实 UUIDv7、全局唯一、与 rowid/键无派生关系。
#[test]
fn handles_are_real_v7_unique_and_not_derived() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    // 键取长而独特的名字:若 handle 由键派生必然包含键文本
    build_v6_project_db(
        &db,
        &[
            (
                "wf-alpha",
                "一",
                &graph_json(&[("node-alpha", &[]), ("node-beta", &["node-alpha"])]),
            ),
            (
                "wf-beta",
                "二",
                &graph_json(&[("node-alpha", &[]), ("node-gamma", &["node-alpha"])]),
            ),
        ],
    );
    let store = Store::open(&db).unwrap();
    drop(store);

    let handles = crate::support::all_persistent_handles(&db);
    // 6 聚合业务行 + 2 工作流 + 4 节点 + 2 边
    assert_eq!(handles.len(), 14, "全部持久对象都有 handle: {handles:?}");
    assert!(
        handles.iter().all(|h| is_uuid_v7(h)),
        "所有 handle 必须是真实 UUIDv7"
    );
    let mut sorted = handles.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), handles.len(), "handle 全局唯一(跨表跨类型)");

    // 无 rowid/键派生:handle 不包含任何 workflow_key/node_key/rowid 十进制文本
    let conn = read_only(&db);
    let rowids: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT CAST(id AS TEXT) FROM agent_tasks
                 UNION ALL SELECT CAST(id AS TEXT) FROM pipeline_revisions
                 UNION ALL SELECT CAST(id AS TEXT) FROM steps
                 UNION ALL SELECT CAST(id AS TEXT) FROM agent_sessions
                 UNION ALL SELECT CAST(id AS TEXT) FROM agent_runs
                 UNION ALL SELECT CAST(id AS TEXT) FROM ad_hoc_sessions",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    for handle in &handles {
        for key in [
            "wf-alpha",
            "wf-beta",
            "node-alpha",
            "node-beta",
            "node-gamma",
        ] {
            assert!(
                !handle.contains(key),
                "handle 不得由对象键派生(包含 `{key}`): {handle}"
            );
        }
        for rowid in &rowids {
            assert_ne!(
                handle, rowid,
                "handle 不得等于 rowid 文本(禁止 table+rowid 映射)"
            );
        }
    }
}

/// 相同 node_key 在不同工作流是不同节点:node_handle 不同,
/// UNIQUE 约束按 (workflow_handle, node_key) 组合生效。
#[test]
fn same_node_key_across_workflows_gets_distinct_handles() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_v6_project_db(
        &db,
        &[
            ("w1", "一", &graph_json(&[("a", &[]), ("b", &["a"])])),
            ("w2", "二", &graph_json(&[("a", &[]), ("c", &["a"])])),
        ],
    );
    let store = Store::open(&db).unwrap();
    drop(store);

    let identity = backfilled(&db);
    let w1 = &identity.workflow_handles["w1"];
    let w2 = &identity.workflow_handles["w2"];
    assert_ne!(w1, w2);
    let a_in_w1 = identity
        .nodes
        .iter()
        .find(|(h, k, _)| h == w1 && k == "a")
        .unwrap();
    let a_in_w2 = identity
        .nodes
        .iter()
        .find(|(h, k, _)| h == w2 && k == "a")
        .unwrap();
    assert_ne!(a_in_w1.2, a_in_w2.2, "跨工作流同名节点必须不同 handle");
    assert!(is_uuid_v7(&a_in_w1.2) && is_uuid_v7(&a_in_w2.2));
}

/// 迁移后的空白库路径:新库直接落 v7(含 identity 表与 singleton meta)。
#[test]
fn fresh_database_lands_on_v7_with_meta_singleton() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);
    let conn = Connection::open(&db).unwrap();
    let (meta, nodes, edges): (i64, i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM project_meta),
                    (SELECT COUNT(*) FROM workflow_node_identity),
                    (SELECT COUNT(*) FROM workflow_edge_identity)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((meta, nodes, edges), (1, 0, 0), "新库恰一行 singleton meta");
}

/// 旧编辑器/Compiler 没有禁止重复 deps。expand-only 迁移必须把同一键对
/// 归一为一个 edge identity,不能因 UNIQUE 冲突拒绝存量 v6 库。
#[test]
fn duplicate_legacy_dependencies_backfill_one_edge() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    build_v6_project_db(
        &db,
        &[(
            "dup-deps",
            "重复依赖",
            &graph_json(&[("a", &[]), ("b", &["a", "a"])]),
        )],
    );
    drop(Store::open(&db).unwrap());
    let identity = backfilled(&db);
    assert_eq!(identity.nodes.len(), 2);
    assert_eq!(identity.edges.len(), 1);
    assert_eq!(identity.edges[0].1, "a");
    assert_eq!(identity.edges[0].2, "b");
}
