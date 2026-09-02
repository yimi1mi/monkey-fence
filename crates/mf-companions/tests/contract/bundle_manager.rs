//! T5a 契约(Issue #35):半写 pointer、缺文件/hash 不符、仅 UI 更新
//! 拒绝、schema 假兼容拒绝、previous retention、活动 pin 阻止清理。

use mf_companions::bundle::{
    retention_keep_set, BundleCompatibility, BundleManager, BundleManifest, ComponentEntry,
    ComponentKind, StorageEligibility,
};
use sha2::{Digest, Sha256};

fn storage() -> StorageEligibility {
    StorageEligibility {
        project_schema: 11,
        catalog_schema: 1,
        service_schema: 4,
        durable_features_written: vec![],
    }
}

fn manifest_with(id: &str, core_sha: &str, assets_sha: &str) -> BundleManifest {
    BundleManifest {
        bundle_id: id.into(),
        version: format!("1.0-{id}"),
        components: vec![
            ComponentEntry {
                kind: ComponentKind::Core,
                path: "core/bin.exe".into(),
                sha256: core_sha.into(),
            },
            ComponentEntry {
                kind: ComponentKind::Assets,
                path: "assets/index.html".into(),
                sha256: assets_sha.into(),
            },
            ComponentEntry {
                kind: ComponentKind::Mfctl,
                path: "mfctl/mfctl.exe".into(),
                sha256: "c".repeat(64),
            },
        ],
        compatibility: BundleCompatibility {
            max_project_schema: 11,
            max_catalog_schema: 1,
            max_service_schema: 4,
            durable_features: vec![],
            host_protocol_version: 1,
        },
    }
}

fn sha(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

fn healthy_manifest(id: &str) -> (BundleManifest, Vec<(String, Vec<u8>)>) {
    let core = format!("core-binary-{id}");
    let assets = format!("<html>{id}</html>");
    let mfctl = format!("mfctl-{id}");
    let mut manifest = manifest_with(id, &sha(core.as_bytes()), &sha(assets.as_bytes()));
    for component in &mut manifest.components {
        if component.path == "mfctl/mfctl.exe" {
            component.sha256 = sha(mfctl.as_bytes());
        }
    }
    let components = vec![
        ("core/bin.exe".to_string(), core.into_bytes()),
        ("assets/index.html".to_string(), assets.into_bytes()),
        ("mfctl/mfctl.exe".to_string(), mfctl.into_bytes()),
    ];
    (manifest, components)
}

#[test]
fn install_switches_pointer_only_after_health_check() {
    let root = tempfile::tempdir().unwrap();
    let manager = BundleManager::new(root.path());
    let (manifest, components) = healthy_manifest("v1");
    let installed = manager.install(&manifest, &components, &storage()).unwrap();
    assert!(installed.previous.is_none());
    assert_eq!(manager.current().unwrap().unwrap().0, "v1");
    assert!(installed.bundle_dir.join("bundle-manifest.json").exists());
}

#[test]
fn hash_mismatch_keeps_pointer_on_old_bundle() {
    let root = tempfile::tempdir().unwrap();
    let manager = BundleManager::new(root.path());
    // v1 健康
    let (v1, components) = healthy_manifest("v1");
    manager.install(&v1, &components, &storage()).unwrap();
    // v2 hash 不符(manifest 声称的 core hash 与落盘内容不同)
    let (mut v2, mut components2) = healthy_manifest("v2");
    // 篡改 core 内容但保留 manifest hash
    for (path, content) in components2.iter_mut() {
        if path == "core/bin.exe" {
            *content = b"tampered".to_vec();
        }
    }
    let _ = &mut v2;
    let error = manager.install(&v2, &components2, &storage()).unwrap_err();
    assert!(
        error.to_string().contains("健康检查失败") || error.to_string().contains("hash"),
        "hash 不符必须失败:{error:#}"
    );
    // pointer 仍在 v1
    assert_eq!(manager.current().unwrap().unwrap().0, "v1");
}

#[test]
fn missing_component_fails_install() {
    let root = tempfile::tempdir().unwrap();
    let manager = BundleManager::new(root.path());
    let (manifest, mut components) = healthy_manifest("v1");
    // 抽走 assets(缺文件)
    components.retain(|(path, _)| path != "assets/index.html");
    let error = manager
        .install(&manifest, &components, &storage())
        .unwrap_err();
    assert!(error.to_string().contains("健康检查失败") || error.to_string().contains("组件缺失"));
    assert!(manager.current().unwrap().is_none(), "pointer 未切换");
}

#[test]
fn ui_only_bundle_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let manager = BundleManager::new(root.path());
    let mut manifest = healthy_manifest("ui-only").0;
    // 只保留 assets(仅 UI 更新)
    manifest
        .components
        .retain(|c| c.kind == ComponentKind::Assets);
    let components = vec![("assets/index.html".to_string(), b"x".to_vec())];
    let error = manager
        .install(&manifest, &components, &storage())
        .unwrap_err();
    assert!(
        error.to_string().contains("仅 UI") || error.to_string().contains("全包一致"),
        "仅 UI 更新拒绝:{error:#}"
    );
}

#[test]
fn schema_rollforward_is_rejected_as_fake_compatibility() {
    let root = tempfile::tempdir().unwrap();
    let manager = BundleManager::new(root.path());
    let (manifest, components) = healthy_manifest("old-bundle");
    let future = StorageEligibility {
        project_schema: 12,
        ..storage()
    };
    let error = manager
        .install(&manifest, &components, &future)
        .unwrap_err();
    assert!(
        error.to_string().contains("假兼容") || error.to_string().contains("schema"),
        "schema 前滚拒绝:{error:#}"
    );
}

#[test]
fn rollback_keeps_previous_bundle_and_refuses_on_durable_write() {
    let root = tempfile::tempdir().unwrap();
    let manager = BundleManager::new(root.path());
    let (v1, c1) = healthy_manifest("v1");
    manager.install(&v1, &c1, &storage()).unwrap();
    let (v2, c2) = healthy_manifest("v2");
    manager.install(&v2, &c2, &storage()).unwrap();
    // 回滚:v1 仍在磁盘,pointer 切回
    let rolled = manager.rollback(&storage()).unwrap();
    assert_eq!(rolled, "v1");
    assert!(manager.bundle_dir("v2").exists(), "previous(v2)目录保留");
    assert_eq!(manager.current().unwrap().unwrap().0, "v1");
    // durable feature 已写入(v2 引入且数据已落)→ 禁止恢复
    let (v3, c3) = healthy_manifest("v3");
    let mut v3 = v3;
    v3.compatibility.durable_features = vec!["new-durable".into()];
    manager.install(&v3, &c3, &storage()).unwrap();
    let rolled_forward = StorageEligibility {
        durable_features_written: vec!["new-durable".into()],
        ..storage()
    };
    let error = manager.rollback(&rolled_forward).unwrap_err();
    assert!(
        error.to_string().contains("禁止自动恢复") || error.to_string().contains("不兼容"),
        "durable 前滚禁止恢复:{error:#}"
    );
}

#[test]
fn half_written_pointer_is_invisible() {
    let root = tempfile::tempdir().unwrap();
    let manager = BundleManager::new(root.path());
    let (v1, c1) = healthy_manifest("v1");
    manager.install(&v1, &c1, &storage()).unwrap();
    // 模拟崩溃残留:temp 指针文件(半写)
    let temp = root.path().join("current.json.tmp");
    std::fs::write(&temp, b"{\"bundle_id\": \"half-wri").unwrap();
    // current 读取不受残留 temp 影响
    assert_eq!(manager.current().unwrap().unwrap().0, "v1");
    // 再次安装:rename 覆盖 temp(不留累积)
    let (v2, c2) = healthy_manifest("v2");
    manager.install(&v2, &c2, &storage()).unwrap();
    assert_eq!(manager.current().unwrap().unwrap().0, "v2");
}

#[test]
fn retention_keeps_current_previous_and_active_pins() {
    let root = tempfile::tempdir().unwrap();
    let manager = BundleManager::new(root.path());
    for id in ["v1", "v2", "v3"] {
        let (manifest, components) = healthy_manifest(id);
        manager.install(&manifest, &components, &storage()).unwrap();
    }
    // current=v3;previous=v2;v1 可清理
    let keep = retention_keep_set(&manager, &[]).unwrap();
    assert!(keep.contains(&"v3".to_string()), "current 保留");
    assert!(keep.contains(&"v2".to_string()), "previous 保留");
    assert!(!keep.contains(&"v1".to_string()), "更旧的可清理");
    // 活动 pin(v1 被活动 Revision 引用)→ 也保留
    let keep_pinned = retention_keep_set(&manager, &["v1".to_string()]).unwrap();
    assert!(keep_pinned.contains(&"v1".to_string()), "活动 pin 阻止清理");
}

#[test]
fn side_by_side_never_overwrites() {
    let root = tempfile::tempdir().unwrap();
    let manager = BundleManager::new(root.path());
    let (v1, c1) = healthy_manifest("v1");
    manager.install(&v1, &c1, &storage()).unwrap();
    // 同 id 重装(目录非空)→ 拒绝
    let error = manager.install(&v1, &c1, &storage()).unwrap_err();
    assert!(error.to_string().contains("不覆盖"));
}
