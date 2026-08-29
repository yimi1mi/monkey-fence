//! Plugin Host 验收:内容寻址安装、按哈希解析、活动 pin 不被插件更新替换。

use mf_agent::CatalogStore;
use mf_plugins::host::{PluginHost, ResolvedPlugin};
use mf_plugins::install::InstallSource;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// 测试辅助:把 fixtures 中的具名插件装入宿主。
trait FixtureInstall {
    fn install_fixture(&self, name: &str) -> anyhow::Result<ResolvedPlugin>;
}

impl FixtureInstall for PluginHost {
    fn install_fixture(&self, name: &str) -> anyhow::Result<ResolvedPlugin> {
        let dir = fixtures_dir().join(name);
        self.install_package(
            &dir,
            InstallSource::Local {
                path: dir.display().to_string(),
            },
        )
    }
}

/// 隔离宿主:独立临时插件根,不扫描真实 ~/.monkeyfence。
struct HostEnv {
    host: Arc<PluginHost>,
    _tmp: tempfile::TempDir,
}

impl std::ops::Deref for HostEnv {
    type Target = PluginHost;
    fn deref(&self) -> &PluginHost {
        &self.host
    }
}

fn fixture_host() -> HostEnv {
    let tmp = tempfile::tempdir().unwrap();
    let host = PluginHost::empty_at(tmp.path().to_path_buf());
    HostEnv { host, _tmp: tmp }
}

#[test]
fn update_does_not_replace_active_pin() {
    let host = fixture_host();
    let v1 = host.install_fixture("demo-v1").unwrap();
    let pin = host.pin_for_run("run-1", &v1).unwrap();
    host.install_fixture("demo-v2").unwrap();
    assert_eq!(
        host.resolve_pin(&pin).unwrap().content_hash,
        v1.content_hash
    );
    // pin 固定的版本仍可完整解析(清单来自 v1 包)
    let resolved = host.resolve_pin(&pin).unwrap();
    assert_eq!(resolved.manifest.manifest.version_str, "0.1.0");
}

#[test]
fn resolve_requires_matching_identity_and_intact_content() {
    let host = fixture_host();
    let v1 = host.install_fixture("demo-v1").unwrap();
    // 版本不匹配
    assert!(host
        .resolve(&v1.full_id, "9.9.9", &v1.content_hash)
        .is_err());
    // 插件 id 不匹配
    assert!(host
        .resolve("other.plugin", &v1.version, &v1.content_hash)
        .is_err());
    // 未知哈希
    assert!(host
        .resolve(
            &v1.full_id,
            &v1.version,
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        )
        .is_err());
    // 包内容被篡改(重算哈希与目录名不符)
    std::fs::write(v1.root.join("TAMPER.txt"), "boom").unwrap();
    assert!(host
        .resolve(&v1.full_id, &v1.version, &v1.content_hash)
        .is_err());
}

#[test]
fn reinstall_same_content_is_idempotent() {
    let host = fixture_host();
    let a = host.install_fixture("demo-v1").unwrap();
    let b = host.install_fixture("demo-v1").unwrap();
    assert_eq!(a.content_hash, b.content_hash);
    assert_eq!(a.root, b.root, "相同内容必须复用同一内容寻址目录");
}

#[test]
fn release_run_pins_drops_references_idempotently() {
    let host = fixture_host();
    let v1 = host.install_fixture("demo-v1").unwrap();
    let p1 = host.pin_for_run("run-a", &v1).unwrap();
    let _p2 = host.pin_for_run("run-b", &v1).unwrap();
    assert_eq!(host.active_pin_count(&v1.content_hash), 2);
    host.release_run_pins("run-a").unwrap();
    assert_eq!(host.active_pin_count(&v1.content_hash), 1);
    // 重复释放幂等
    host.release_run_pins("run-a").unwrap();
    assert_eq!(host.active_pin_count(&v1.content_hash), 1);
    // 释放只解除引用,不删除包(pin 之外的解析仍可用)
    assert!(host.resolve_pin(&p1).is_ok());
}

#[test]
fn new_install_is_disabled_until_authorized() {
    let host = fixture_host();
    let v1 = host.install_fixture("demo-v1").unwrap();
    let summary = host
        .summaries()
        .into_iter()
        .find(|s| s.full_id == v1.full_id)
        .unwrap();
    assert!(!summary.enabled, "新装插件默认禁用");
    // 启用 = 授权
    host.enable(&v1.full_id, true).unwrap();
    assert!(
        host.summaries()
            .into_iter()
            .find(|s| s.full_id == v1.full_id)
            .unwrap()
            .enabled
    );
}

#[test]
fn contribution_lookup_by_full_id() {
    let host = fixture_host();
    let v1 = host.install_fixture("demo-v1").unwrap();
    // 未启用时贡献不可见
    assert!(host
        .contributions()
        .find_agent_type("test.demo.demo-agent")
        .is_none());
    host.enable(&v1.full_id, true).unwrap();
    let reg = host.contributions();
    let (src, agent) = reg.find_agent_type("test.demo.demo-agent").unwrap();
    assert_eq!(src.plugin_full_id, v1.full_id);
    assert_eq!(src.content_hash, v1.content_hash);
    assert_eq!(agent.adapter, "generic-command");
    assert!(reg.find_agent_type("test.demo.missing").is_none());
}

#[test]
fn persisted_pin_survives_host_rebuild_and_blocks_uninstall() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = CatalogStore::memory().unwrap();

    let first = PluginHost::empty_at_with_catalog(tmp.path().to_path_buf(), catalog.clone());
    let package = first
        .install_package(
            &fixtures_dir().join("demo-v1"),
            InstallSource::Local {
                path: "demo-v1".into(),
            },
        )
        .unwrap();
    first.enable(&package.full_id, true).unwrap();
    first.pin_for_run("run-persisted", &package).unwrap();
    drop(first);

    let rebuilt = PluginHost::load_at_with_catalog(
        tmp.path().to_path_buf(),
        catalog,
        &mf_agent::Config::default(),
        &[],
    );
    assert_eq!(rebuilt.active_pin_count(&package.content_hash), 1);
    assert!(
        rebuilt.uninstall(&package.full_id).is_err(),
        "a persisted active pin must prevent package removal"
    );
    assert!(
        package.root.is_dir(),
        "rejected uninstall must preserve package files"
    );

    rebuilt.release_run_pins("run-persisted").unwrap();
    rebuilt.uninstall(&package.full_id).unwrap();
    assert!(
        !package.root.exists(),
        "released package may be uninstalled"
    );
}

// ---------- 源 pin(工作流冻结用;内置合成插件无内容寻址包) ----------

#[test]
fn source_pin_roundtrip_for_builtin_synthetic_plugins() {
    // load_at_with_catalog 注册内置合成插件(empty_at 不含内置)
    let host = PluginHost::load_at_with_catalog(
        tempfile::tempdir().unwrap().path().to_path_buf(),
        CatalogStore::memory().unwrap(),
        &mf_agent::Config::default(),
        &[],
    );
    // 内置合成插件:content_hash 为空,无 packages 记录
    host.pin_source_for_run("run-k", "monkeyfence.codex", "0.1.0", "")
        .unwrap();
    assert_eq!(host.active_pin_count(""), 1);
    // 解析校验通过(内置插件始终在位)
    host.resolve_source_pin("monkeyfence.codex", "0.1.0", "")
        .unwrap();
    // 释放后引用归零
    host.release_run_pins("run-k").unwrap();
    assert_eq!(host.active_pin_count(""), 0);
}

#[test]
fn source_pin_rejects_unknown_builtin() {
    let host = PluginHost::empty_at(tempfile::tempdir().unwrap().path().to_path_buf());
    assert!(host
        .pin_source_for_run("run-k", "monkeyfence.ghost", "0.1.0", "")
        .is_err());
    assert!(host
        .resolve_source_pin("monkeyfence.ghost", "0.1.0", "")
        .is_err());
}

#[test]
fn source_pin_for_packaged_plugin_requires_resolvable_hash() {
    let host = PluginHost::empty_at(tempfile::tempdir().unwrap().path().to_path_buf());
    // 非空 hash 但包不存在:拒绝(不得 pin 不存在的包)
    assert!(host
        .pin_source_for_run("run-k", "test.demo", "1.0.0", "sha256:deadbeef")
        .is_err());
}

// ---------- 复审阻塞项 14:摘要携带真实哈希/兼容性/计数/活动 pin ----------

#[test]
fn summaries_expose_real_hash_compatibility_counts_and_pins() {
    let tmp = tempfile::tempdir().unwrap();
    let host = PluginHost::load_at_with_catalog(
        tmp.path().to_path_buf(),
        CatalogStore::memory().unwrap(),
        &mf_agent::Config::default(),
        &[],
    );
    let summaries = host.summaries();
    assert!(!summaries.is_empty(), "内置合成插件应在列");
    let claude = summaries
        .iter()
        .find(|s| s.full_id == "monkeyfence.claude")
        .expect("内置 claude 插件");
    // 真实贡献计数(不是 0 占位)
    assert_eq!(claude.agent_types_count, 1);
    // 兼容性是计算值(内置插件 min_app_version 为空 → 兼容)
    assert!(claude.compatible);
    // 内置合成插件哈希为空,活动 pin 计数来自目录库源 pin
    assert_eq!(claude.active_pins, 0);
    // pin 后活动计数上升
    host.pin_source_for_run("run-k", "monkeyfence.claude", "0.1.0", "")
        .unwrap();
    let after = host
        .summaries()
        .into_iter()
        .find(|s| s.full_id == "monkeyfence.claude")
        .unwrap();
    assert_eq!(after.active_pins, 1, "pin 后摘要必须反映活动引用");
}
