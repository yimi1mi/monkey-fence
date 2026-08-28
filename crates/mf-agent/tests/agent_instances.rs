//! Agent Instance 领域与目录库持久化:CRUD、不可变版本、项目覆盖解析。
//!
//! 设计文档 §4.2:编辑实例只产生新版本行,已取出的快照不受影响;
//! 项目覆盖只合并显式声明的键,生成解析后的不可变快照。

use mf_agent::agent_instance::{AgentInstanceDraft, AgentInstanceOverrides, AgentInstanceSnapshot};
use mf_agent::catalog_store::CatalogStore;
use mf_agent::{InstanceScope, RunMode};

fn draft(name: &str) -> AgentInstanceDraft {
    AgentInstanceDraft {
        name: name.into(),
        agent_type: "generic-command".into(),
        scope: InstanceScope::User,
        enabled: true,
        run_mode: RunMode::OneShot,
        executable: "agent.exe".into(),
        argv: vec!["--prompt".into(), "do work".into()],
        env: vec![("LANG".into(), "C".into()), ("ROWS".into(), "26".into())],
        config: serde_json::json!({ "model": "test-model", "tier": 1 }),
        execution_contract: serde_json::json!({
            "input": "argv",
            "completion": "process-exit"
        }),
        sealed_secret_ids: vec!["api-key".into()],
    }
}

#[test]
fn editing_instance_creates_version_without_mutating_snapshot() {
    let store = CatalogStore::memory().unwrap();
    let first = store.create_agent_instance(draft("review")).unwrap();
    let snapshot = store.snapshot_agent_instance(&first.id, None).unwrap();
    store
        .update_agent_instance(&first.id, draft("implementation"))
        .unwrap();
    assert_eq!(snapshot.name, "review");
}

#[test]
fn create_persists_all_version_fields() {
    let store = CatalogStore::memory().unwrap();
    let instance = store.create_agent_instance(draft("review")).unwrap();
    assert_eq!(instance.name, "review");
    assert_eq!(instance.agent_type, "generic-command");
    assert_eq!(instance.scope, InstanceScope::User);
    assert_eq!(instance.current_version, 1);
    assert!(instance.enabled);

    let snapshot = store.snapshot_agent_instance(&instance.id, None).unwrap();
    assert_eq!(snapshot.id, instance.id);
    assert_eq!(snapshot.executable, "agent.exe");
    assert_eq!(
        snapshot.argv,
        vec!["--prompt".to_string(), "do work".to_string()]
    );
    assert_eq!(
        snapshot.env,
        vec![
            ("LANG".to_string(), "C".to_string()),
            ("ROWS".to_string(), "26".to_string())
        ]
    );
    assert_eq!(snapshot.config["model"], "test-model");
    assert_eq!(snapshot.execution_contract["completion"], "process-exit");
    assert_eq!(snapshot.sealed_secret_ids, vec!["api-key".to_string()]);
    assert_eq!(snapshot.run_mode, RunMode::OneShot);
    assert_eq!(snapshot.version, 1);
}

#[test]
fn ids_are_stable_and_unique() {
    let store = CatalogStore::memory().unwrap();
    let a = store.create_agent_instance(draft("a")).unwrap();
    let b = store.create_agent_instance(draft("b")).unwrap();
    assert_ne!(a.id, b.id);
    // id 稳定:重新读取不变
    assert_eq!(store.get_agent_instance(&a.id).unwrap().unwrap().id, a.id);
}

#[test]
fn update_bumps_version_and_keeps_history_readable() {
    let store = CatalogStore::memory().unwrap();
    let instance = store.create_agent_instance(draft("v1-name")).unwrap();
    let updated = store
        .update_agent_instance(&instance.id, draft("v2-name"))
        .unwrap();
    assert_eq!(updated.current_version, 2);

    // 当前快照反映新草案
    let current = store.snapshot_agent_instance(&instance.id, None).unwrap();
    assert_eq!(current.name, "v2-name");
    assert_eq!(current.version, 2);

    // 旧版本仍可固定读取(Revision 冻结语义的基础)
    let pinned = store
        .snapshot_agent_instance_version(&instance.id, Some(1), None)
        .unwrap();
    assert_eq!(pinned.name, "v1-name");
    assert_eq!(pinned.version, 1);

    let history = store.agent_instance_versions(&instance.id).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].version, 1);
    assert_eq!(history[1].version, 2);
}

#[test]
fn update_rejects_unknown_instance() {
    let store = CatalogStore::memory().unwrap();
    assert!(store.update_agent_instance("nope", draft("x")).is_err());
}

#[test]
fn snapshot_of_missing_instance_errors() {
    let store = CatalogStore::memory().unwrap();
    assert!(store.snapshot_agent_instance("nope", None).is_err());
}

#[test]
fn project_overrides_merge_only_declared_keys() {
    let store = CatalogStore::memory().unwrap();
    let instance = store.create_agent_instance(draft("base")).unwrap();

    // 只声明 argv 与 env 中的一个键:config 与其余 env 保持用户配置
    let overrides = AgentInstanceOverrides {
        argv: Some(vec!["--other".into()]),
        env: Some(vec![("ROWS".into(), "40".into())]),
        config: Some(serde_json::json!({ "model": "project-model" })),
    };
    let merged: AgentInstanceSnapshot = store
        .snapshot_agent_instance(&instance.id, Some(&overrides))
        .unwrap();
    assert_eq!(merged.argv, vec!["--other".to_string()]);
    // env 按键覆盖:ROWS 被替换,LANG 保留
    assert_eq!(
        merged.env,
        vec![
            ("LANG".to_string(), "C".to_string()),
            ("ROWS".to_string(), "40".to_string())
        ]
    );
    // config 对象浅合并:声明的键覆盖,未声明的键保留
    assert_eq!(merged.config["model"], "project-model");
    assert_eq!(merged.config["tier"], 1);
    // 名称/可执行文件/Secret 引用不受项目覆盖影响
    assert_eq!(merged.name, "base");
    assert_eq!(merged.executable, "agent.exe");
    assert_eq!(merged.sealed_secret_ids, vec!["api-key".to_string()]);

    // 不声明任何键 → 与用户配置一致
    let none = AgentInstanceOverrides::default();
    let merged = store
        .snapshot_agent_instance(&instance.id, Some(&none))
        .unwrap();
    assert_eq!(
        merged.argv,
        vec!["--prompt".to_string(), "do work".to_string()]
    );
    assert_eq!(merged.config["model"], "test-model");
}

#[test]
fn list_filter_and_toggle_enabled() {
    let store = CatalogStore::memory().unwrap();
    let a = store.create_agent_instance(draft("a")).unwrap();
    let b = store
        .create_agent_instance(AgentInstanceDraft {
            scope: InstanceScope::Project,
            ..draft("b")
        })
        .unwrap();

    let all = store.list_agent_instances(None).unwrap();
    assert_eq!(all.len(), 1, "无项目上下文时只返回用户作用域实例");
    assert_eq!(all[0].id, a.id);
    let with_project = store.list_agent_instances(Some("proj")).unwrap();
    assert_eq!(with_project.len(), 2);

    let toggled = store
        .set_agent_instance_enabled(&a.id, false)
        .unwrap()
        .unwrap();
    assert!(!toggled.enabled);
    assert!(!store.get_agent_instance(&a.id).unwrap().unwrap().enabled);

    assert!(store.delete_agent_instance(&b.id).unwrap());
    assert!(store.get_agent_instance(&b.id).unwrap().is_none());
    assert!(!store.delete_agent_instance(&b.id).unwrap());
}

#[test]
fn draft_validation_rejects_empty_core_fields() {
    let store = CatalogStore::memory().unwrap();
    let bad = AgentInstanceDraft {
        name: "  ".into(),
        ..draft("x")
    };
    assert!(store.create_agent_instance(bad).is_err());
    let bad = AgentInstanceDraft {
        agent_type: String::new(),
        ..draft("x")
    };
    assert!(store.create_agent_instance(bad).is_err());
    let bad = AgentInstanceDraft {
        executable: String::new(),
        ..draft("x")
    };
    assert!(store.create_agent_instance(bad).is_err());
}
