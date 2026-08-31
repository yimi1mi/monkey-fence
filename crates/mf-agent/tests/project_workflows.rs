//! 项目工作流存储(ADR 0004 / Task 2):独立于 task_workflows 的
//! 项目级工作流对象 —— CRUD、内容身份幂等保存、损坏数据报错、
//! 旧库升级后任务本地草稿保留。

use mf_agent::schema::upgrade_project;
use mf_agent::store::Store;
use mf_agent::workflow::{workflow_content_digest, ProjectWorkflowDraft, WorkflowNodeDraft};
use rusqlite::Connection;
use std::time::Duration;

fn node(key: &str, title: &str) -> WorkflowNodeDraft {
    WorkflowNodeDraft {
        key: key.into(),
        title: title.into(),
        instructions: format!("做 {title}"),
        agent_instance_id: "inst-a".into(),
        deps: vec![],
    }
}

fn draft(key: &str, name: &str, titles: &[&str]) -> ProjectWorkflowDraft {
    ProjectWorkflowDraft {
        key: key.into(),
        name: name.into(),
        nodes: titles.iter().map(|t| node(&t.to_lowercase(), t)).collect(),
        allow_unsafe_parallel: false,
    }
}

#[test]
fn project_workflow_crud_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("db.sqlite")).unwrap();

    let saved = store
        .save_project_workflow(&draft("wf-a", "发布前检查", &["Lint", "Test"]))
        .unwrap();
    assert_eq!(saved.key, "wf-a");
    assert_eq!(saved.nodes.len(), 2);
    assert!(!saved.content_digest.is_empty());
    assert_eq!(saved.created_at, saved.updated_at);

    // 读取
    let loaded = store.load_project_workflow("wf-a").unwrap().unwrap();
    assert_eq!(loaded.name, "发布前检查");
    assert_eq!(loaded.nodes[0].key, "lint");
    assert_eq!(loaded.nodes[1].deps, Vec::<String>::new());
    assert!(store.load_project_workflow("missing").unwrap().is_none());

    // 覆盖:同 key 新内容
    let v2 = ProjectWorkflowDraft {
        nodes: vec![
            node("lint", "Lint"),
            node("test", "Test"),
            node("report", "Report"),
        ],
        ..draft("wf-a", "发布前检查 v2", &[])
    };
    store.save_project_workflow(&v2).unwrap();
    let loaded = store.load_project_workflow("wf-a").unwrap().unwrap();
    assert_eq!(loaded.name, "发布前检查 v2");
    assert_eq!(loaded.nodes.len(), 3);

    // 列表稳定排序:名称 NOCASE,再按 key
    store
        .save_project_workflow(&draft("wf-b", "beta", &["B"]))
        .unwrap();
    store
        .save_project_workflow(&draft("wf-c", "Alpha", &["C"]))
        .unwrap();
    let list = store.list_project_workflows().unwrap();
    let names: Vec<&str> = list.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Alpha", "beta", "发布前检查 v2"],
        "列表按名称 NOCASE 稳定排序"
    );
    // 同名时按 key 稳定
    store
        .save_project_workflow(&draft("wf-z", "beta", &["Z"]))
        .unwrap();
    let list = store.list_project_workflows().unwrap();
    let betas: Vec<&str> = list
        .iter()
        .filter(|w| w.name == "beta")
        .map(|w| w.key.as_str())
        .collect();
    assert_eq!(betas, vec!["wf-b", "wf-z"], "同名按 key 稳定排序");

    // 删除
    assert!(store.delete_project_workflow("wf-b").unwrap());
    assert!(
        !store.delete_project_workflow("wf-b").unwrap(),
        "重复删除返回 false"
    );
    assert!(store.load_project_workflow("wf-b").unwrap().is_none());
    assert_eq!(store.list_project_workflows().unwrap().len(), 3);
}

#[test]
fn validation_rejects_empty_key_name_and_nodes() {
    let store = Store::memory().unwrap();
    let err = store
        .save_project_workflow(&ProjectWorkflowDraft {
            key: "  ".into(),
            name: "n".into(),
            nodes: vec![node("a", "A")],
            allow_unsafe_parallel: false,
        })
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("key"));
    let err = store
        .save_project_workflow(&ProjectWorkflowDraft {
            key: "k".into(),
            name: "".into(),
            nodes: vec![node("a", "A")],
            allow_unsafe_parallel: false,
        })
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("名称"));
    let err = store
        .save_project_workflow(&ProjectWorkflowDraft {
            key: "k".into(),
            name: "n".into(),
            nodes: vec![],
            allow_unsafe_parallel: false,
        })
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("节点"));
}

#[test]
fn saving_same_content_does_not_refresh_updated_at() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("db.sqlite")).unwrap();
    store
        .save_project_workflow(&draft("wf", "同名", &["A"]))
        .unwrap();
    let t0 = store
        .load_project_workflow("wf")
        .unwrap()
        .unwrap()
        .updated_at;

    // 时间戳秒级:跨秒后保存同内容 → updated_at 不变
    std::thread::sleep(Duration::from_millis(1100));
    store
        .save_project_workflow(&draft("wf", "同名", &["A"]))
        .unwrap();
    let t1 = store
        .load_project_workflow("wf")
        .unwrap()
        .unwrap()
        .updated_at;
    assert_eq!(t0, t1, "同内容保存不得刷新 updated_at");

    // 内容变化 → 推进
    std::thread::sleep(Duration::from_millis(1100));
    store
        .save_project_workflow(&ProjectWorkflowDraft {
            nodes: vec![node("a", "A"), node("b", "B")],
            ..draft("wf", "同名", &[])
        })
        .unwrap();
    let t2 = store
        .load_project_workflow("wf")
        .unwrap()
        .unwrap()
        .updated_at;
    assert_ne!(t1, t2, "内容变化必须刷新 updated_at");

    // 纯重命名:摘要不变,名称仍要落库(不刷新 updated_at 属内容语义)
    store
        .save_project_workflow(&draft("wf", "新名字", &["A", "B"]))
        .unwrap();
    let rec = store.load_project_workflow("wf").unwrap().unwrap();
    assert_eq!(rec.name, "新名字");
}

#[test]
fn allow_unsafe_parallel_participates_in_digest() {
    let nodes = vec![node("a", "A")];
    let off = workflow_content_digest(&nodes, false);
    let on = workflow_content_digest(&nodes, true);
    assert_ne!(off, on, "并行风险开关必须参与内容摘要");

    // 存储层:仅切换开关也算内容变化
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("db.sqlite")).unwrap();
    store
        .save_project_workflow(&draft("wf", "并行", &["A"]))
        .unwrap();
    let d0 = store
        .load_project_workflow("wf")
        .unwrap()
        .unwrap()
        .content_digest;
    std::thread::sleep(Duration::from_millis(1100));
    store
        .save_project_workflow(&ProjectWorkflowDraft {
            allow_unsafe_parallel: true,
            ..draft("wf", "并行", &["A"])
        })
        .unwrap();
    let rec = store.load_project_workflow("wf").unwrap().unwrap();
    assert_ne!(rec.content_digest, d0);
    assert!(rec.allow_unsafe_parallel);
}

#[test]
fn corrupted_graph_json_returns_error_not_silent_empty() {
    let store = Store::memory().unwrap();
    store
        .save_project_workflow(&draft("wf", "好数据", &["A"]))
        .unwrap();
    store
        .with_conn(|c| {
            c.execute(
                "UPDATE project_workflows SET graph_json = '{not-json' WHERE workflow_key = 'wf'",
                rusqlite::params![],
            )?;
            Ok(())
        })
        .unwrap();
    let err = store
        .load_project_workflow("wf")
        .err()
        .expect("损坏 JSON 必须报错");
    assert!(
        format!("{err:#}").contains("损坏"),
        "错误必须指明数据损坏: {err:#}"
    );
    // 列表同样不得静默吞掉损坏行
    let err = store.list_project_workflows().err().expect("列表必须报错");
    assert!(format!("{err:#}").contains("损坏"));
}

/// 旧项目库(v5)升级到 v6 后:task_workflows 数据原样保留,
/// project_workflows 表可用且互不影响。
#[test]
fn legacy_v5_database_upgrade_preserves_task_workflows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    {
        let mut conn = Connection::open(&db).unwrap();
        upgrade_project(&mut conn, 5).unwrap();
        let pre_v6_tables: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        assert!(
            !pre_v6_tables.contains(&"project_workflows".to_string()),
            "target v5 不得提前执行 v6 DDL"
        );
        conn.execute(
            "INSERT INTO task_workflows
                (project_key, task_id, graph_json, allow_unsafe_parallel, updated_at, content_digest)
             VALUES ('proj', 1, '[]', 0, '2024-01-01T00:00:00+00:00', 'd')",
            rusqlite::params![],
        )
        .unwrap();
    }
    let store = Store::open(&db).unwrap();
    assert_eq!(
        store.schema_version().unwrap(),
        mf_agent::schema::PROJECT_SCHEMA_VERSION
    );
    // 任务本地草稿仍在(语义不变,不迁移)
    let task_draft = store.load_task_workflow("proj", 1).unwrap();
    assert!(
        task_draft.is_some(),
        "升级不得删除/改写 task_workflows 数据"
    );
    // 新表可用
    store
        .save_project_workflow(&draft("wf", "升级后", &["A"]))
        .unwrap();
    assert_eq!(store.list_project_workflows().unwrap().len(), 1);
    assert!(
        store
            .table_names()
            .unwrap()
            .contains(&"project_workflows".to_string()),
        "v6 迁移必须创建 project_workflows 表"
    );
}
