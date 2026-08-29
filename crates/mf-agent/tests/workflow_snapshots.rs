//! Workflow 模板/快照/Handoff 持久化(Orchestration Task 1):
//! 模板编辑不改变已编译快照、任务本地模板私有、实例配置冻结、
//! Revision 保存序列化快照、Handoff 固定字段往返。

use mf_agent::agent_instance::AgentInstanceDraft;
use mf_agent::catalog_store::CatalogStore;
use mf_agent::handoff::Handoff;
use mf_agent::store::Store;
use mf_agent::workflow::{freeze_workflow, WorkflowNodeDraft, WorkflowTemplateDraft};
use mf_agent::{InstanceScope, RunMode};
use std::sync::Arc;

// ---------- Fixture ----------

struct Fixture {
    catalog: Arc<CatalogStore>,
    instance_id: String,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            catalog: CatalogStore::memory().unwrap(),
            instance_id: String::new(),
        }
    }

    fn with_instance(name: &str) -> Fixture {
        let mut fixture = Fixture::new();
        let instance = fixture
            .catalog
            .create_agent_instance(instance_draft(name, "agent.exe"))
            .unwrap();
        fixture.instance_id = instance.id;
        fixture
    }

    fn compile(&self, template_version_id: i64) -> mf_agent::workflow::WorkflowSnapshot {
        let version = self
            .catalog
            .template_version(template_version_id)
            .unwrap()
            .unwrap();
        freeze_workflow(&self.catalog, &version).unwrap()
    }
}

fn instance_draft(name: &str, executable: &str) -> AgentInstanceDraft {
    AgentInstanceDraft {
        name: name.into(),
        agent_type: "generic-command".into(),
        scope: InstanceScope::User,
        project_key: None,
        enabled: true,
        run_mode: RunMode::OneShot,
        executable: executable.into(),
        argv: vec![],
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({ "completion": "process-exit" }),
        sealed_secret_ids: vec![],
    }
}

/// 单节点模板:instructions 用于验证版本冻结;实例取 fixture 里创建的。
fn template(fx: &Fixture, key: &str, instructions: &str) -> WorkflowTemplateDraft {
    WorkflowTemplateDraft {
        key: key.into(),
        name: format!("模板 {key}"),
        task_local: false,
        nodes: vec![WorkflowNodeDraft {
            key: "review".into(),
            title: "审查".into(),
            instructions: instructions.into(),
            agent_instance_id: fx.instance_id.clone(),
            deps: vec![],
        }],
    }
}

fn template_with_missing_instance(key: &str, instructions: &str) -> WorkflowTemplateDraft {
    let mut draft = template_key_only(key, instructions);
    draft.nodes[0].agent_instance_id = "missing".into();
    draft
}

fn template_key_only(key: &str, instructions: &str) -> WorkflowTemplateDraft {
    WorkflowTemplateDraft {
        key: key.into(),
        name: format!("模板 {key}"),
        task_local: false,
        nodes: vec![WorkflowNodeDraft {
            key: "review".into(),
            title: "审查".into(),
            instructions: instructions.into(),
            agent_instance_id: String::new(),
            deps: vec![],
        }],
    }
}

// ---------- 模板版本与快照冻结 ----------

#[test]
fn template_edit_does_not_change_revision_snapshot() {
    let fixture = Fixture::with_instance("review");
    let version = fixture
        .catalog
        .save_template(&template(&fixture, "t", "v1"))
        .unwrap();
    let snapshot = fixture.compile(version.version_id);
    fixture
        .catalog
        .save_template(&template(&fixture, "t", "v2"))
        .unwrap();
    assert_eq!(snapshot.nodes[0].instructions, "v1");
}

#[test]
fn saving_template_bumps_version_and_keeps_history() {
    let fixture = Fixture::with_instance("review");
    let first = fixture
        .catalog
        .save_template(&template(&fixture, "t", "v1"))
        .unwrap();
    let second = fixture
        .catalog
        .save_template(&template(&fixture, "t", "v2"))
        .unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(second.version, 2);
    assert_eq!(second.template_key, "t");

    let old = fixture
        .catalog
        .template_version(first.version_id)
        .unwrap()
        .unwrap();
    assert_eq!(old.nodes[0].instructions, "v1");
    let current = fixture
        .catalog
        .template_version(second.version_id)
        .unwrap()
        .unwrap();
    assert_eq!(current.nodes[0].instructions, "v2");

    let history = fixture.catalog.template_versions("t").unwrap();
    assert_eq!(history.len(), 2);
}

#[test]
fn task_local_template_stays_private_until_promoted() {
    let fixture = Fixture::with_instance("review");
    let mut draft = template(&fixture, "task-42", "local");
    draft.task_local = true;
    let version = fixture.catalog.save_template(&draft).unwrap();

    // 全局列表看不到任务本地模板
    let global = fixture.catalog.list_templates(false).unwrap();
    assert!(global.is_empty());
    // 任务作用域可见
    let local = fixture.catalog.list_templates(true).unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].key, "task-42");
    let _ = version;

    // 另存为全局模板后可见
    fixture
        .catalog
        .promote_template_to_global("task-42")
        .unwrap();
    assert_eq!(fixture.catalog.list_templates(false).unwrap().len(), 1);
}

#[test]
fn snapshot_freezes_instance_configuration() {
    let fixture = Fixture::with_instance("review");
    let draft = template(&fixture, "t", "v1");
    let version = fixture.catalog.save_template(&draft).unwrap();

    let snapshot = fixture.compile(version.version_id);
    assert_eq!(snapshot.nodes[0].instance.executable, "agent.exe");

    // 编辑实例(新版本)不改变已冻结快照;模板不变则 freeze 仍取当前实例版本
    let updated = AgentInstanceDraft {
        executable: "agent-v2.exe".into(),
        ..instance_draft("review", "agent-v2.exe")
    };
    fixture
        .catalog
        .update_agent_instance(&fixture.instance_id, updated)
        .unwrap();
    let refrozen = fixture.compile(version.version_id);
    assert_eq!(refrozen.nodes[0].instance.executable, "agent-v2.exe");

    // 真正的冻结语义:已编译出的快照对象不受实例编辑影响
    assert_eq!(snapshot.nodes[0].instance.executable, "agent.exe");
}

#[test]
fn freeze_rejects_unknown_instance() {
    let fixture = Fixture::new();
    let version = fixture
        .catalog
        .save_template(&template_with_missing_instance("t", "x"))
        .unwrap();
    let err = freeze_workflow(&fixture.catalog, &version).unwrap_err();
    assert!(err.to_string().contains("missing"), "{err}");
}

#[test]
fn snapshot_pins_template_and_version() {
    let fixture = Fixture::with_instance("review");
    let draft = template(&fixture, "t", "v1");
    let version = fixture.catalog.save_template(&draft).unwrap();

    let snapshot = fixture.compile(version.version_id);
    assert_eq!(snapshot.template_key, "t");
    assert_eq!(snapshot.template_version, 1);
    assert_eq!(snapshot.nodes[0].key, "review");
    assert_eq!(snapshot.nodes[0].title, "审查");
    assert_eq!(snapshot.nodes[0].instance.id, fixture.instance_id);
}

// ---------- Revision 快照持久化 ----------

#[test]
fn revision_stores_serialized_snapshot() {
    let store = Store::memory().unwrap();
    let fixture = Fixture::with_instance("review");
    let draft = template(&fixture, "t", "v1");
    let version = fixture.catalog.save_template(&draft).unwrap();
    let snapshot = fixture.compile(version.version_id);

    let task = store.create_task("标题", "目标").unwrap();
    let revision = store.create_workflow_revision(task.id, &snapshot).unwrap();
    let loaded = store.revision_snapshot(revision.id).unwrap().unwrap();
    assert_eq!(loaded, snapshot);
    // 快照不可变:再次读取一致
    assert_eq!(
        store.revision_snapshot(revision.id).unwrap().unwrap(),
        snapshot
    );
}

// ---------- Handoff ----------

#[test]
fn handoff_roundtrip_fixed_fields_and_custom_output() {
    let handoff = Handoff {
        status: "completed".into(),
        summary: "完成审查".into(),
        changed_files: vec!["src/a.rs".into()],
        artifacts: vec!["out/report.md".into()],
        verification: Some(serde_json::json!({ "tests": "passed" })),
        blockers: vec!["缺验收标准".into()],
        recommendations: vec!["补充集成测试".into()],
        output: serde_json::json!({ "report_path": "out/report.md" }),
        raw_log_ref: Some("runs/42.log".into()),
    };
    let json = serde_json::to_string(&handoff).unwrap();
    assert!(
        !json.to_lowercase().contains("transcript"),
        "不复制完整转录"
    );
    let back: Handoff = serde_json::from_str(&json).unwrap();
    assert_eq!(back, handoff);
}

#[test]
fn handoff_from_adapter_draft_keeps_fields() {
    let draft = mf_agent::HandoffDraft {
        status: "completed".into(),
        summary: "s".into(),
        output: serde_json::json!({"k": 1}),
        ..Default::default()
    };
    let handoff = Handoff::from(draft);
    assert_eq!(handoff.summary, "s");
    assert_eq!(handoff.output["k"], 1);
}

#[test]
fn handoff_persists_with_run_reference() {
    let store = Store::memory().unwrap();
    let task = store.create_task("t", "g").unwrap();
    let handoff = Handoff {
        status: "completed".into(),
        summary: "done".into(),
        ..Default::default()
    };
    let stored = store.insert_handoff(task.id, None, None, &handoff).unwrap();
    assert!(stored > 0);
    let list = store.list_handoffs(task.id).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].1.summary, "done");
}

// ---------- 任务本地工作流存项目 Store(按 project+task 键) ----------

fn draft_with_nodes(key: &str, nodes: Vec<WorkflowNodeDraft>) -> WorkflowTemplateDraft {
    WorkflowTemplateDraft {
        key: key.into(),
        name: format!("模板 {key}"),
        task_local: true,
        nodes,
    }
}

#[test]
fn task_workflow_roundtrips_per_project_and_task() {
    let store = Store::memory().unwrap();
    let fixture = Fixture::with_instance("review");
    let nodes = vec![WorkflowNodeDraft {
        key: "a".into(),
        title: "节点 A".into(),
        instructions: "做 A".into(),
        agent_instance_id: fixture.instance_id.clone(),
        deps: vec![],
    }];

    store
        .save_task_workflow(
            "proj-alpha",
            7,
            &draft_with_nodes("task-7", nodes.clone()),
            false,
        )
        .unwrap();
    // 同一 task id 在不同项目互不覆盖
    store
        .save_task_workflow(
            "proj-beta",
            7,
            &draft_with_nodes(
                "task-7",
                vec![WorkflowNodeDraft {
                    key: "b".into(),
                    title: "B 项目节点".into(),
                    instructions: String::new(),
                    agent_instance_id: fixture.instance_id.clone(),
                    deps: vec![],
                }],
            ),
            false,
        )
        .unwrap();

    let alpha = store.load_task_workflow("proj-alpha", 7).unwrap().unwrap();
    assert_eq!(alpha.nodes.len(), 1);
    assert_eq!(alpha.nodes[0].key, "a");
    assert_eq!(alpha.nodes[0].instructions, "做 A");
    let beta = store.load_task_workflow("proj-beta", 7).unwrap().unwrap();
    assert_eq!(beta.nodes[0].key, "b");
    assert!(store.load_task_workflow("proj-alpha", 8).unwrap().is_none());

    // 覆盖保存(编辑后)
    store
        .save_task_workflow(
            "proj-alpha",
            7,
            &draft_with_nodes("task-7", nodes.clone()),
            false,
        )
        .unwrap();
    assert_eq!(
        store
            .load_task_workflow("proj-alpha", 7)
            .unwrap()
            .unwrap()
            .nodes
            .len(),
        1
    );
}
