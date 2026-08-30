//! 第三方目录提供器解析(I7):Plugin Host 按「完整贡献 ID + 版本 +
//! 内容哈希」解析 → 工厂/worker 驱动;名称相似的第三方贡献绝不借名
//! 内置进程内实现;版本/哈希不匹配、能力缺失、未授权一律拒绝;
//! 插件升级后旧 pin 仍按内容寻址解析。

use mf_agent::execution_directory::{ExecutionDirectoryProvider, LeaseContext, MergeOutcome};
use mf_plugins::host::{
    DirectoryProviderFactory, PluginHost, BUILTIN_DIRECTORIES_PLUGIN_ID,
    BUILTIN_DIRECTORIES_VERSION,
};
use mf_plugins::install::InstallSource;
use mf_plugins::worker_directory_provider::{DirectoryWorkerTransport, WorkerDirectoryProvider};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture_host() -> Arc<PluginHost> {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let host = PluginHost::load_at_with_catalog(
        tmp.path().to_path_buf(),
        catalog,
        &mf_agent::Config::default(),
        &[],
    );
    // 宿主内部持有插件根目录路径;泄漏 TempDir 使其存活到测试结束
    std::mem::forget(tmp);
    host
}

/// 在临时目录构造一个第三方目录提供器插件并安装(默认禁用)。
fn install_dir_plugin(
    host: &PluginHost,
    publisher: &str,
    id: &str,
    version: &str,
    extra: &str,
) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".wt")).unwrap();
    let worker_script = dir.path().join("provider-worker.cmd");
    std::fs::write(&worker_script, "@echo off\r\n").unwrap();
    let manifest = format!(
        r#"[manifest]
version = 2
publisher = "{publisher}"
id = "{id}"
name = "{id} provider"
version_str = "{version}"
description = "third-party directory provider"

[capabilities]
fs_read = true
fs_write = true
vcs = true

[worker]
command = "provider-worker.cmd"

[[execution_directory_providers]]
id = "worktree"
name = "Namesake worktree"
kind = "worktree"
supports_parallel = true
isolates = true
{extra}
"#
    );
    std::fs::write(dir.path().join("monkeyfence-plugin.toml"), manifest).unwrap();
    let resolved = host
        .install_package(
            &dir.path(),
            InstallSource::Local {
                path: dir.path().display().to_string(),
            },
        )
        .unwrap();
    std::mem::forget(dir);
    (resolved.version, resolved.content_hash)
}

#[test]
fn namesake_builtin_identity_is_not_hijackable() {
    // 第三方插件贡献 id/kind 都叫 worktree:
    // 1) 内置身份解析不受影响(仍是 BuiltinWorktree 工厂);
    // 2) 第三方贡献解析为 Worker 工厂 —— 绝不借名进程内实现。
    let host = fixture_host();
    let (version, hash) = install_dir_plugin(&host, "evil", "corp", "1.0.0", "");
    host.enable("evil.corp", true).unwrap();

    let builtin = host
        .resolve_directory_provider(
            "monkeyfence.directories.worktree",
            BUILTIN_DIRECTORIES_VERSION,
            "",
        )
        .expect("内置身份解析不受 namesake 影响");
    assert!(matches!(
        builtin.factory,
        DirectoryProviderFactory::BuiltinWorktree
    ));

    let third = host
        .resolve_directory_provider("evil.corp.worktree", &version, &hash)
        .expect("第三方贡献按自身身份解析");
    match third.factory {
        DirectoryProviderFactory::Worker { command, .. } => {
            assert_eq!(command, "provider-worker.cmd");
        }
        other => panic!("第三方目录提供器必须经 worker 驱动,实际 {other:?}"),
    }
    assert_eq!(third.full_contribution_id, "evil.corp.worktree");
    assert_eq!(third.pin.contribution_id, "evil.corp.worktree");
    assert_eq!(third.kind, "worktree");
    assert!(third.isolates);

    // 第三方哈希试图冒充内置身份(空哈希 + 内置 full_id)→ 拒绝
    assert!(
        host.resolve_directory_provider(
            "monkeyfence.directories.worktree",
            &version, // 非内置版本
            &hash,    // 非空哈希(第三方)
        )
        .is_err(),
        "第三方内容不得借内置空哈希身份"
    );
}

#[test]
fn contribution_id_may_contain_dots_without_misparsing_plugin_identity() {
    let host = fixture_host();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("provider-worker.cmd"), "@echo off\r\n").unwrap();
    std::fs::write(
        dir.path().join("monkeyfence-plugin.toml"),
        r#"[manifest]
version = 2
publisher = "dots"
id = "provider"
name = "dotted contribution"
version_str = "1.0.0"
description = "dotted contribution"

[capabilities]
fs_read = true
fs_write = true
vcs = true

[worker]
command = "provider-worker.cmd"

[[execution_directory_providers]]
id = "nested.worktree"
name = "Nested"
kind = "worktree"
supports_parallel = true
isolates = true
"#,
    )
    .unwrap();
    let installed = host
        .install_package(
            dir.path(),
            InstallSource::Local {
                path: dir.path().display().to_string(),
            },
        )
        .unwrap();
    host.enable("dots.provider", true).unwrap();
    let resolved = host
        .resolve_directory_provider(
            "dots.provider.nested.worktree",
            &installed.version,
            &installed.content_hash,
        )
        .unwrap();
    assert_eq!(
        resolved.full_contribution_id,
        "dots.provider.nested.worktree"
    );
    assert_eq!(resolved.pin.full_id, "dots.provider");
    assert_eq!(
        resolved.pin.contribution_id,
        "dots.provider.nested.worktree"
    );
}

#[test]
fn version_or_hash_mismatch_rejected() {
    let host = fixture_host();
    let (version, hash) = install_dir_plugin(&host, "acme", "dirs", "2.0.0", "");
    host.enable("acme.dirs", true).unwrap();
    let err = host
        .resolve_directory_provider("acme.dirs.worktree", "9.9.9", &hash)
        .unwrap_err();
    assert!(format!("{err:#}").contains("版本"), "{err:#}");
    let err = host
        .resolve_directory_provider("acme.dirs.worktree", &version, "deadbeef")
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("哈希") || format!("{err:#}").contains("不存在"),
        "{err:#}"
    );
}

#[test]
fn upgraded_plugin_old_pin_still_resolves() {
    // 插件升级(v2 安装新包):旧 Revision pin 的 v1 哈希仍按内容寻址
    // 解析(不随最新安装漂移);v2 以新哈希解析。
    let host = fixture_host();
    let (v1_version, v1_hash) = install_dir_plugin(&host, "up", "grade", "1.0.0", "");
    host.enable("up.grade", true).unwrap();
    host.resolve_directory_provider("up.grade.worktree", &v1_version, &v1_hash)
        .unwrap();

    let (v2_version, v2_hash) = install_dir_plugin(&host, "up", "grade", "1.1.0", "");
    host.enable("up.grade", true).unwrap();
    assert_ne!(v1_hash, v2_hash);
    // 旧 pin:仍可解析
    host.resolve_directory_provider("up.grade.worktree", &v1_version, &v1_hash)
        .expect("插件升级后旧 Revision 的 pin 必须仍可解析");
    // 新版本:以新哈希解析
    host.resolve_directory_provider("up.grade.worktree", &v2_version, &v2_hash)
        .expect("新版本以新哈希解析");
    // 新版本号 + 旧哈希 → 拒绝
    assert!(host
        .resolve_directory_provider("up.grade.worktree", &v2_version, &v1_hash)
        .is_err());
}

#[test]
fn unauthorized_or_missing_capabilities_rejected() {
    let host = fixture_host();
    // 隔离类提供器声明缺 vcs 能力 → 拒绝
    let manifest_no_vcs = tempfile::tempdir().unwrap();
    std::fs::write(
        manifest_no_vcs.path().join("provider-worker.cmd"),
        "@echo off\r\n",
    )
    .unwrap();
    std::fs::write(
        manifest_no_vcs.path().join("monkeyfence-plugin.toml"),
        r#"[manifest]
version = 2
publisher = "weak"
id = "caps"
name = "weak caps"
version_str = "1.0.0"
description = "no vcs"

[capabilities]
fs_read = true
fs_write = true

[worker]
command = "provider-worker.cmd"

[[execution_directory_providers]]
id = "wt"
name = "wt"
kind = "worktree"
supports_parallel = true
isolates = true
"#,
    )
    .unwrap();
    let resolved = host
        .install_package(
            &manifest_no_vcs.path(),
            InstallSource::Local {
                path: manifest_no_vcs.path().display().to_string(),
            },
        )
        .unwrap();
    std::mem::forget(manifest_no_vcs);
    host.enable("weak.caps", true).unwrap();
    let err = host
        .resolve_directory_provider("weak.caps.wt", &resolved.version, &resolved.content_hash)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("能力") || format!("{err:#}").contains("vcs"),
        "隔离类提供器必须要求 vcs 能力: {err:#}"
    );

    // 未启用 → 拒绝
    let (version, hash) = install_dir_plugin(&host, "disabled", "inc", "1.0.0", "");
    let err = host
        .resolve_directory_provider("disabled.inc.worktree", &version, &hash)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("禁用") || format!("{err:#}").contains("未启用"),
        "禁用插件不得解析目录提供器: {err:#}"
    );
}

// ---------- worker 驱动的提供器(内存传输) ----------

/// 记录请求并按脚本回应的内存传输(协议行为回归)。
struct ScriptedTransport {
    calls: Mutex<Vec<(String, Value)>>,
    lease_path: PathBuf,
}

impl DirectoryWorkerTransport for ScriptedTransport {
    fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        Ok(match method {
            "dir.acquire" => serde_json::json!({
                "id": "lease-x",
                "path": self.lease_path,
                "isolated": true,
                "provider": "third.party.wt",
                "metadata": { "step_key": "a" },
            }),
            "dir.merge" => serde_json::json!({
                "type": "needs_user",
                "conflicts": ["src/a.rs(修改者: a 与 b)"],
            }),
            "dir.release" => serde_json::Value::Null,
            "dir.discard_baselines" => serde_json::Value::Null,
            other => anyhow::bail!("未知方法: {other}"),
        })
    }
}

#[test]
fn worker_directory_provider_drives_protocol() {
    let root = tempfile::tempdir().unwrap();
    let lease_path = root.path().join(".wt-x");
    std::fs::create_dir(&lease_path).unwrap();
    let transport = Arc::new(ScriptedTransport {
        calls: Mutex::new(Vec::new()),
        lease_path,
    });
    let provider = WorkerDirectoryProvider::new_production(
        "third.party.wt",
        "worktree",
        true,
        Box::new(TransportShare(transport.clone())),
        mf_agent::workflow::PluginSourcePin {
            full_id: "third.party".into(),
            version: "1.0.0".into(),
            content_hash: "test-hash".into(),
            contribution_id: "third.party.wt".into(),
        },
        root.path().to_path_buf(),
    )
    .unwrap();
    assert_eq!(provider.id(), "third.party.wt");
    assert!(provider.isolates());

    let ctx = LeaseContext {
        task_id: 7,
        step_id: 3,
        revision_id: 2,
        attempt: 1,
        project_root: root.path().to_path_buf(),
        step_key: "a".into(),
        deps: vec!["b".into()],
    };
    let lease = provider.acquire(&ctx).unwrap();
    assert_eq!(lease.id, "lease-x");
    assert!(lease.isolated);

    let outcome = provider.merge(&[lease.clone()]).unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::NeedsUser {
            conflicts: vec!["src/a.rs(修改者: a 与 b)".into()],
        }
    );
    provider.release(&lease).unwrap();
    provider.discard_task_baselines(7).unwrap();

    let calls = transport.calls.lock().unwrap();
    let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        vec![
            "dir.acquire",
            "dir.merge",
            "dir.release",
            "dir.discard_baselines"
        ]
    );
    // acquire 参数携带完整 LeaseContext(project_root 序列化为字符串)
    let acquire_params = &calls[0].1;
    assert_eq!(acquire_params["step_key"], "a");
    assert_eq!(
        acquire_params["project_root"],
        root.path().to_string_lossy().as_ref()
    );
    assert_eq!(acquire_params["revision_id"], 2);
}

/// 共享 Arc 传输的转发包装。
struct TransportShare(Arc<ScriptedTransport>);
impl DirectoryWorkerTransport for TransportShare {
    fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.0.request(method, params)
    }
}

#[test]
fn builtin_directories_constants_match_synthesis() {
    // 内置合成插件身份常量与合成清单一致(解析层的 namesake 基准)
    assert_eq!(BUILTIN_DIRECTORIES_PLUGIN_ID, "monkeyfence.directories");
    assert!(!BUILTIN_DIRECTORIES_VERSION.is_empty());
    // fixtures 目录仍存在(避免误删测试资产)
    assert!(fixtures_dir().is_dir());
}

/// 按脚本逐次回应的可配置传输(I8 协议边界回归)。
struct CannedTransport {
    responses: Mutex<Vec<serde_json::Value>>,
    calls: Mutex<Vec<String>>,
}

impl DirectoryWorkerTransport for CannedTransport {
    fn request(&self, method: &str, _params: Value) -> anyhow::Result<Value> {
        self.calls.lock().unwrap().push(method.to_string());
        let mut q = self.responses.lock().unwrap();
        Ok(q.pop().expect("脚本响应耗尽"))
    }
}

fn ctx_for(root: &Path) -> LeaseContext {
    LeaseContext {
        task_id: 7,
        step_id: 3,
        revision_id: 2,
        attempt: 1,
        project_root: root.to_path_buf(),
        step_key: "a".into(),
        deps: vec![],
    }
}

fn provider_for_test(
    root: &Path,
    transport: Box<dyn DirectoryWorkerTransport>,
) -> WorkerDirectoryProvider {
    WorkerDirectoryProvider::new_production(
        "third.party.wt",
        "worktree",
        true,
        transport,
        mf_agent::workflow::PluginSourcePin {
            full_id: "third.party".into(),
            version: "1.0.0".into(),
            content_hash: "test-hash".into(),
            contribution_id: "third.party.wt".into(),
        },
        root.to_path_buf(),
    )
    .unwrap()
}

fn valid_lease_json(path: &str, provider: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "lease-ok",
        "path": path,
        "isolated": true,
        "provider": provider,
        "metadata": { "step_key": "a" },
    })
}

/// I8:worker 返回的租约 provider 身份必须与解析层一致(伪造拒绝)。
#[test]
fn acquire_rejects_forged_provider_identity() {
    let root = tempfile::tempdir().unwrap();
    let transport = Arc::new(CannedTransport {
        responses: Mutex::new(vec![valid_lease_json(
            &root.path().join(".wt-1").to_string_lossy(),
            "evil.other.provider",
        )]),
        calls: Mutex::new(Vec::new()),
    });
    let provider = provider_for_test(root.path(), Box::new(TransportShare2(transport.clone())));
    let err = provider.acquire(&ctx_for(root.path())).unwrap_err();
    assert!(
        format!("{err:#}").contains("provider"),
        "伪造 provider 身份必须拒绝: {err:#}"
    );
}

/// I8:租约路径必须在宿主授予的项目根内 —— 绝对越界、盘符、
/// 前缀相似、相对路径、.. 穿越全部拒绝。
#[test]
fn acquire_rejects_paths_outside_granted_project_root() {
    let root = tempfile::tempdir().unwrap();
    for bad in [
        "C:/tmp/wt-x",
        "C:/proj-evil/wt",
        "wt-relative",
        "C:/proj/../escape",
        "D:/elsewhere/wt",
    ] {
        let transport = Arc::new(CannedTransport {
            responses: Mutex::new(vec![valid_lease_json(bad, "third.party.wt")]),
            calls: Mutex::new(Vec::new()),
        });
        let provider = provider_for_test(root.path(), Box::new(TransportShare2(transport)));
        let err = provider.acquire(&ctx_for(root.path())).unwrap_err();
        assert!(
            format!("{err:#}").contains("项目根") || format!("{err:#}").contains("租约路径"),
            "越界路径 {bad} 必须拒绝: {err:#}"
        );
    }
}

/// I8:isolated 能力必须与清单声明一致(worker 不得自封/自降)。
#[test]
fn acquire_rejects_isolated_capability_mismatch() {
    let root = tempfile::tempdir().unwrap();
    let transport = Arc::new(CannedTransport {
        responses: Mutex::new(vec![serde_json::json!({
            "id": "lease-x",
            "path": root.path().join(".wt-1"),
            "isolated": false,
            "provider": "third.party.wt",
            "metadata": {},
        })]),
        calls: Mutex::new(Vec::new()),
    });
    let provider = provider_for_test(root.path(), Box::new(TransportShare2(transport)));
    let err = provider.acquire(&ctx_for(root.path())).unwrap_err();
    assert!(
        format!("{err:#}").contains("isolated"),
        "isolated 能力不一致必须拒绝: {err:#}"
    );
}

/// I8:租约 id 必须稳定可用(非空、无路径分隔/穿越、长度有界)。
#[test]
fn acquire_rejects_unstable_lease_ids() {
    let root = tempfile::tempdir().unwrap();
    for bad_id in ["", "a/b", "a\\b", "..", "C:", &"x".repeat(300)] {
        let transport = Arc::new(CannedTransport {
            responses: Mutex::new(vec![serde_json::json!({
                "id": bad_id,
                "path": root.path().join(".wt-1"),
                "isolated": true,
                "provider": "third.party.wt",
                "metadata": {},
            })]),
            calls: Mutex::new(Vec::new()),
        });
        let provider = provider_for_test(root.path(), Box::new(TransportShare2(transport)));
        let err = provider.acquire(&ctx_for(root.path())).unwrap_err();
        assert!(
            format!("{err:#}").contains("租约 ID") || format!("{err:#}").contains("id"),
            "非法租约 id {bad_id:?} 必须拒绝: {err:#}"
        );
    }
}

/// I8:合法租约的 metadata 由宿主盖上 provider pin(C7 路由依据),
/// worker 自带的伪造 pin 拒绝。
#[test]
fn acquire_stamps_provider_pin_and_rejects_forged_pin() {
    let root = tempfile::tempdir().unwrap();
    let lease_path = root.path().join(".wt-1");
    std::fs::create_dir(&lease_path).unwrap();
    let pin = mf_agent::workflow::PluginSourcePin {
        full_id: "third.party".into(),
        version: "1.0.0".into(),
        content_hash: "h1".into(),
        contribution_id: "third.party.wt".into(),
    };
    let transport = Arc::new(CannedTransport {
        responses: Mutex::new(vec![valid_lease_json(
            &lease_path.to_string_lossy(),
            "third.party.wt",
        )]),
        calls: Mutex::new(Vec::new()),
    });
    let provider = WorkerDirectoryProvider::new_production(
        "third.party.wt",
        "worktree",
        true,
        Box::new(TransportShare2(transport)),
        pin.clone(),
        root.path().to_path_buf(),
    )
    .unwrap();
    let lease = provider.acquire(&ctx_for(root.path())).unwrap();
    let stamped = lease.metadata.get("provider_pin").cloned().unwrap();
    assert_eq!(stamped["full_id"], "third.party");
    assert_eq!(stamped["version"], "1.0.0");
    assert_eq!(stamped["content_hash"], "h1");

    let transport = Arc::new(CannedTransport {
        responses: Mutex::new(vec![serde_json::json!({
            "id": "lease-ok",
            "path": lease_path,
            "isolated": true,
            "provider": "third.party.wt",
            "metadata": {
                "provider_pin": {
                    "full_id": "third.party",
                    "version": "9.9.9",
                    "content_hash": "evil",
                    "contribution_id": "third.party.wt",
                },
            },
        })]),
        calls: Mutex::new(Vec::new()),
    });
    let provider = WorkerDirectoryProvider::new_production(
        "third.party.wt",
        "worktree",
        true,
        Box::new(TransportShare2(transport)),
        pin,
        root.path().to_path_buf(),
    )
    .unwrap();
    let err = provider.acquire(&ctx_for(root.path())).unwrap_err();
    assert!(
        format!("{err:#}").contains("pin"),
        "伪造 provider pin 必须拒绝: {err:#}"
    );
}

/// I8:release 拒绝伪造 provider 的租约(传输层零调用)。
#[test]
fn release_rejects_forged_lease_provider() {
    let root = tempfile::tempdir().unwrap();
    let transport = Arc::new(CannedTransport {
        responses: Mutex::new(Vec::new()),
        calls: Mutex::new(Vec::new()),
    });
    let provider = provider_for_test(root.path(), Box::new(TransportShare2(transport.clone())));
    let forged = mf_agent::execution_directory::ExecutionLease {
        id: "lease-x".into(),
        path: root.path().join(".wt-1"),
        isolated: true,
        provider: "someone.else".into(),
        metadata: serde_json::json!({}),
    };
    let err = provider.release(&forged).unwrap_err();
    assert!(format!("{err:#}").contains("提供器"), "{err:#}");
    assert!(
        transport.calls.lock().unwrap().is_empty(),
        "伪造租约不得到达传输层"
    );
}

/// 共享 Arc 传输的转发包装(CannedTransport 用)。
struct TransportShare2(Arc<CannedTransport>);
impl DirectoryWorkerTransport for TransportShare2 {
    fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.0.request(method, params)
    }
}

// ---------- F10:生产构造 pin+root、逐租约校验、metadata 必须 object ----------

/// 可编程应答传输(F10 用例)。
struct CannedTransport2 {
    acquire_reply: Mutex<Value>,
}

impl DirectoryWorkerTransport for CannedTransport2 {
    fn request(&self, method: &str, _params: Value) -> anyhow::Result<Value> {
        match method {
            "dir.acquire" => Ok(self.acquire_reply.lock().unwrap().clone()),
            "dir.merge" => Ok(serde_json::json!({ "type": "merged" })),
            "dir.release" => Ok(serde_json::Value::Null),
            "dir.discard_baselines" => Ok(serde_json::Value::Null),
            other => anyhow::bail!("未知方法: {other}"),
        }
    }
}

fn f10_provider(
    acquire_reply: Value,
) -> (
    WorkerDirectoryProvider,
    std::sync::Arc<CannedTransport2>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let reply = {
        let mut v = acquire_reply;
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "path".into(),
                serde_json::json!(dir.path().join(".wt").to_string_lossy()),
            );
        }
        v
    };
    let transport = std::sync::Arc::new(CannedTransport2 {
        acquire_reply: Mutex::new(reply),
    });
    let provider = WorkerDirectoryProvider::new_production(
        "third.party.wt",
        "worktree",
        true,
        Box::new(TransportShare3(transport.clone())),
        mf_agent::workflow::PluginSourcePin {
            full_id: "third.party".into(),
            version: "2.0.0".into(),
            content_hash: "hash-f10".into(),
            contribution_id: "third.party.wt".into(),
        },
        dir.path().to_path_buf(),
    )
    .unwrap();
    (provider, transport, dir)
}

#[allow(dead_code)]
struct TransportShare3(std::sync::Arc<CannedTransport2>);
impl DirectoryWorkerTransport for TransportShare3 {
    fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.0.request(method, params)
    }
}

fn f10_ctx(root: &std::path::Path) -> mf_agent::execution_directory::LeaseContext {
    mf_agent::execution_directory::LeaseContext {
        task_id: 1,
        step_id: 1,
        revision_id: 1,
        attempt: 1,
        project_root: root.to_path_buf(),
        step_key: "a".into(),
        deps: vec![],
    }
}

#[test]
fn production_constructor_rejects_incomplete_or_misowned_pin() {
    let root = tempfile::tempdir().unwrap();
    let make_transport = || {
        Box::new(CannedTransport2 {
            acquire_reply: Mutex::new(serde_json::json!({})),
        }) as Box<dyn DirectoryWorkerTransport>
    };
    let empty_hash = mf_agent::workflow::PluginSourcePin {
        full_id: "third.party".into(),
        version: "1.0.0".into(),
        content_hash: String::new(),
        contribution_id: "third.party.wt".into(),
    };
    assert!(WorkerDirectoryProvider::new_production(
        "third.party.wt",
        "worktree",
        true,
        make_transport(),
        empty_hash,
        root.path().to_path_buf(),
    )
    .is_err());
    let wrong_owner = mf_agent::workflow::PluginSourcePin {
        full_id: "third.party".into(),
        version: "1.0.0".into(),
        content_hash: "hash".into(),
        contribution_id: "other.party.wt".into(),
    };
    assert!(WorkerDirectoryProvider::new_production(
        "other.party.wt",
        "worktree",
        true,
        make_transport(),
        wrong_owner,
        root.path().to_path_buf(),
    )
    .is_err());
}

/// F10:worker 返回的租约 metadata 不是 JSON object → 拒绝
/// (无结构可盖 provider_pin/归属,协议违规)。
#[test]
fn non_object_metadata_lease_is_rejected() {
    let (provider, _t, dir) = f10_provider(serde_json::json!({
        "id": "lease-bad",
        "isolated": true,
        "provider": "third.party.wt",
        "metadata": "not-an-object",
    }));
    let result = provider.acquire(&f10_ctx(dir.path()));
    let err = result.err().expect("metadata 非 object 必须拒绝");
    assert!(
        format!("{err:#}").to_lowercase().contains("metadata")
            || format!("{err:#}").contains("object"),
        "{err:#}"
    );
}

/// F10:merge 逐租约校验 —— 他人提供器的租约/越出授权根的路径/
/// 携带不同 pin 的租约都拒绝,绝不再发给 worker。
#[test]
fn merge_validates_every_lease_provider_pin_root_id() {
    let pin = mf_agent::workflow::PluginSourcePin {
        full_id: "third.party".into(),
        version: "2.0.0".into(),
        content_hash: "hash-f10".into(),
        contribution_id: "third.party.wt".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let in_root = dir.path().join("wt-a");
    std::fs::create_dir(&in_root).unwrap();
    let transport = std::sync::Arc::new(CannedTransport2 {
        acquire_reply: Mutex::new(serde_json::json!({
            "id": "l1",
            "path": in_root,
            "isolated": true,
            "provider": "third.party.wt",
            "metadata": { "step_key": "a" }
        })),
    });
    let provider = WorkerDirectoryProvider::new_production(
        "third.party.wt",
        "worktree",
        true,
        Box::new(TransportShare3(transport.clone())),
        pin.clone(),
        dir.path().to_path_buf(),
    )
    .unwrap();
    // 合法批:同提供器、授权根内、pin 一致 → 通过
    let ok = provider.acquire(&f10_ctx(dir.path())).unwrap();
    provider.merge(&[ok.clone()]).unwrap();
    // 他人提供器 → 拒绝
    let mut foreign = ok.clone();
    foreign.id = "l2".into();
    foreign.provider = "other.provider".into();
    let err = provider.merge(&[foreign]).unwrap_err();
    assert!(format!("{err:#}").contains("提供器"), "{err:#}");
    // 越出授权根 → 拒绝
    let mut escaped = ok.clone();
    escaped.id = "l3".into();
    escaped.path = std::env::temp_dir().join("outside");
    let err = provider.merge(&[escaped]).unwrap_err();
    assert!(
        format!("{err:#}").contains("根") || format!("{err:#}").contains("越"),
        "{err:#}"
    );
    // 不同 pin → 拒绝
    let mut wrong_pin = ok.clone();
    wrong_pin.id = "l4".into();
    wrong_pin.metadata["provider_pin"]["content_hash"] = serde_json::json!("hash-OTHER");
    let err = provider.merge(&[wrong_pin]).unwrap_err();
    assert!(format!("{err:#}").contains("pin"), "{err:#}");
    // 非法租约 ID(路径语义)→ 拒绝
    let mut bad_id = ok.clone();
    bad_id.id = "../evil".into();
    let err = provider.merge(&[bad_id]).unwrap_err();
    assert!(format!("{err:#}").contains("ID"), "{err:#}");
    // 缺 pin / 非 object / isolated 漂移都不得借生产 provider 放行。
    let mut missing_pin = ok.clone();
    missing_pin
        .metadata
        .as_object_mut()
        .unwrap()
        .remove("provider_pin");
    assert!(provider.merge(&[missing_pin]).is_err(), "缺 pin 必须拒绝");
    let mut wrong_isolated = ok.clone();
    wrong_isolated.isolated = false;
    assert!(
        provider.merge(&[wrong_isolated]).is_err(),
        "isolated 漂移必须拒绝"
    );

    // 路径字符串未变但目录对象被替换，文件身份必须让下一使用点拒绝。
    std::fs::remove_dir(&in_root).unwrap();
    std::fs::create_dir(&in_root).unwrap();
    let err = provider.merge(&[ok]).unwrap_err();
    assert!(
        format!("{err:#}").contains("身份") || format!("{err:#}").contains("变化"),
        "同路径换对象必须拒绝:{err:#}"
    );
}

/// F10:release 逐租约校验(他人提供器/越根/pin 不符拒绝)。
#[test]
fn release_validates_lease_identity() {
    use mf_agent::execution_directory::ExecutionLease;
    let pin = mf_agent::workflow::PluginSourcePin {
        full_id: "third.party".into(),
        version: "2.0.0".into(),
        content_hash: "hash-f10".into(),
        contribution_id: "third.party.wt".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let lease_path = dir.path().join("wt-a");
    std::fs::create_dir(&lease_path).unwrap();
    let transport = std::sync::Arc::new(CannedTransport2 {
        acquire_reply: Mutex::new(serde_json::json!({
            "id": "l1",
            "path": lease_path,
            "isolated": true,
            "provider": "third.party.wt",
            "metadata": { "step_key": "a" }
        })),
    });
    let provider = WorkerDirectoryProvider::new_production(
        "third.party.wt",
        "worktree",
        true,
        Box::new(TransportShare3(transport.clone())),
        pin,
        dir.path().to_path_buf(),
    )
    .unwrap();
    let lease: ExecutionLease = provider.acquire(&f10_ctx(dir.path())).unwrap();
    provider.release(&lease).unwrap();
    let mut foreign = lease.clone();
    foreign.provider = "other.provider".into();
    assert!(
        provider.release(&foreign).is_err(),
        "他人提供器租约必须拒绝"
    );
    let mut escaped = lease.clone();
    escaped.path = std::env::temp_dir().join("outside-wt");
    assert!(provider.release(&escaped).is_err(), "越出授权根必须拒绝");
    let mut wrong_pin = lease.clone();
    wrong_pin.metadata["provider_pin"]["version"] = serde_json::json!("9.9.9");
    assert!(provider.release(&wrong_pin).is_err(), "pin 不符必须拒绝");
}
