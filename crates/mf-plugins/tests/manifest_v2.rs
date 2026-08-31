//! Manifest v2 贡献词汇表验收:解析全部贡献类型、能力指纹覆盖新权限。

use mf_plugins::manifest::{Capabilities, PluginManifest};

#[test]
fn parses_every_v2_contribution() {
    let manifest = PluginManifest::parse(include_str!("fixtures/manifest-v2.toml")).unwrap();
    assert_eq!(manifest.agent_types.len(), 1);
    assert_eq!(manifest.node_types.len(), 1);
    assert_eq!(manifest.execution_directory_providers.len(), 1);
    assert_eq!(manifest.vcs_providers.len(), 1);
    assert_eq!(manifest.secret_stores.len(), 1);
    assert_eq!(manifest.ui_schemas.len(), 1);
    let agent = &manifest.agent_types[0];
    assert_eq!(agent.adapter, "generic-command");
    assert_eq!(agent.detect_commands, vec!["demo".to_string()]);
    assert!(agent.modes.contains(&"interactive".to_string()));
    assert_eq!(manifest.vcs_providers[0].settings.len(), 2);
}

#[test]
fn shell_and_secret_permissions_change_fingerprint() {
    let mut caps = Capabilities::default();
    let before = caps.fingerprint_part();
    caps.shell = true;
    caps.secrets = true;
    assert_ne!(before, caps.fingerprint_part());
}

#[test]
fn v1_manifest_rejected() {
    assert!(PluginManifest::parse(
        r#"
[manifest]
version = 1
publisher = "zhipu"
id = "demo"
name = "Demo"
version_str = "0.1.0"

[[agents]]
id = "demo"
name = "Demo"
runtime = "pty"
command = "demo"
"#
    )
    .is_err());
}

#[test]
fn duplicate_contribution_ids_rejected_per_class() {
    let base = include_str!("fixtures/manifest-v2.toml");
    // agent_types id 重复
    let dup_agent = base.replace(
        r#"[[node_types]]"#,
        r#"[[agent_types]]
id = "demo-agent"
name = "Duplicate"
adapter = "generic-command"
config_schema = ""

[[node_types]]"#,
    );
    assert!(
        PluginManifest::parse(&dup_agent).is_err(),
        "agent_types id 重复必须拒绝"
    );
    // node_types id 重复
    let dup_node = base.replace(
        r#"[[execution_directory_providers]]"#,
        r#"[[node_types]]
id = "agent-node"
name = "Duplicate"
kind = "agent"

[[execution_directory_providers]]"#,
    );
    assert!(
        PluginManifest::parse(&dup_node).is_err(),
        "node_types id 重复必须拒绝"
    );
    // 不同类别允许同 id(类别内独立校验)
    assert!(PluginManifest::parse(base).is_ok());
}

#[test]
fn referenced_schema_paths_must_stay_inside_plugin_root() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = PluginManifest::parse(include_str!("fixtures/manifest-v2.toml")).unwrap();
    let escaping = PluginManifest {
        ui_schemas: vec![mf_plugins::manifest::UiSchemaContribution {
            id: "settings-form".into(),
            surface: "settings-form".into(),
            file: "../outside.json".into(),
        }],
        ..manifest.clone()
    };
    assert!(
        mf_plugins::manifest::validate_manifest_paths(tmp.path(), &escaping).is_err(),
        "ui_schemas 路径逃逸必须拒绝"
    );
    // 完整 fixture 的引用文件在临时根内建好后应通过
    let manifest = PluginManifest::parse(include_str!("fixtures/manifest-v2.toml")).unwrap();
    for rel in [
        "schemas/agent.json",
        "schemas/node.json",
        "templates/t1.json",
        "schemas/settings.json",
    ] {
        let p = tmp.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{}").unwrap();
    }
    std::fs::create_dir_all(tmp.path().join("skills/demo")).unwrap();
    assert!(mf_plugins::manifest::validate_manifest_paths(tmp.path(), &manifest).is_ok());
}
