//! C7:held lease 重启后的提供器路由 —— 必须按租约 metadata 冻结的
//! 完整 pin(full_id+version+content_hash)解析提供器,绝不能用当前
//! `self.directory`(升级后的新版本)顶替:
//! - pin 可解析(v1 仍安装)→ merge/release 调用 v1;
//! - pin 不可解析(v1 已被升级移除)→ 持久 NeedsYou、租约保持持有、
//!   v2 一次都不被调用。

mod common;

use common::*;
use mf_agent::execution_directory::{
    ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use mf_agent::orchestrator::{DirectoryRouting, GlobalLimiter, Orchestrator, WorkflowKernel};
use mf_agent::store::Store;
use mf_agent::workflow::PluginSourcePin;
use mf_agent::Settlement;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn pin(v: &str) -> PluginSourcePin {
    PluginSourcePin {
        full_id: "third.party.dirs".into(),
        version: v.into(),
        content_hash: format!("hash-{v}"),
        contribution_id: "pinned".into(),
    }
}

/// 带固定 pin 的脚本提供器:租约 metadata 携带 provider_pin;
/// 记录 merge/release 次数(路由断言用)。
struct PinnedDirectory {
    root: std::path::PathBuf,
    pin: PluginSourcePin,
    pub merges: AtomicUsize,
    pub releases: AtomicUsize,
}

impl PinnedDirectory {
    fn new(root: &std::path::Path, p: PluginSourcePin) -> Arc<PinnedDirectory> {
        Arc::new(PinnedDirectory {
            root: root.to_path_buf(),
            pin: p,
            merges: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
        })
    }
}

impl ExecutionDirectoryProvider for PinnedDirectory {
    fn id(&self) -> &str {
        "pinned"
    }
    fn isolates(&self) -> bool {
        true
    }
    fn acquire(&self, ctx: &LeaseContext) -> anyhow::Result<ExecutionLease> {
        Ok(ExecutionLease {
            id: format!("lease-{}-{}", ctx.task_id, ctx.step_key),
            path: self.root.clone(),
            isolated: true,
            provider: "pinned".into(),
            metadata: serde_json::json!({
                "step_key": ctx.step_key,
                "provider_pin": {
                    "full_id": self.pin.full_id,
                    "version": self.pin.version,
                    "content_hash": self.pin.content_hash,
                    "contribution_id": self.pin.contribution_id,
                },
            }),
        })
    }
    fn merge(&self, leases: &[ExecutionLease]) -> anyhow::Result<MergeOutcome> {
        self.merges.fetch_add(1, Ordering::SeqCst);
        assert!(
            leases
                .iter()
                .all(|l| l.metadata.get("provider_pin").is_some()),
            "租约必须携带 provider_pin"
        );
        Ok(MergeOutcome::Merged)
    }
    fn release(&self, _lease: &ExecutionLease) -> anyhow::Result<()> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// 固定路由表解析器:pin → 提供器;缺失返回 None。
struct RegistryResolver {
    map: HashMap<String, Arc<dyn ExecutionDirectoryProvider>>,
}

impl mf_agent::execution_directory::DirectoryProviderResolver for RegistryResolver {
    fn resolve(&self, p: &PluginSourcePin) -> Option<Arc<dyn ExecutionDirectoryProvider>> {
        self.map
            .get(&format!("{}@{}+{}", p.full_id, p.version, p.content_hash))
            .cloned()
    }
}

fn registry(pairs: &[(PluginSourcePin, Arc<PinnedDirectory>)]) -> RegistryResolver {
    let mut map = HashMap::new();
    for (p, provider) in pairs {
        map.insert(
            format!("{}@{}+{}", p.full_id, p.version, p.content_hash),
            provider.clone() as Arc<dyn ExecutionDirectoryProvider>,
        );
    }
    RegistryResolver { map }
}

/// 用指定目录提供器 + 路由构建 Orchestrator(同一 Store 文件 = 重启)。
struct Phase {
    catalog: Arc<mf_agent::catalog_store::CatalogStore>,
    pins: Arc<FakePins>,
    host: Arc<RecordingHost>,
    orch: Arc<Orchestrator>,
    instance_id: String,
}

impl Phase {
    fn template(
        &self,
        key: &str,
        nodes: Vec<mf_agent::workflow::WorkflowNodeDraft>,
    ) -> mf_agent::workflow::WorkflowTemplateVersion {
        self.catalog
            .save_template(&mf_agent::workflow::WorkflowTemplateDraft {
                key: key.into(),
                name: format!("模板 {key}"),
                task_local: false,
                nodes,
            })
            .unwrap()
    }

    fn assign_and_run(&self, task_id: i64, version: &mf_agent::workflow::WorkflowTemplateVersion) {
        self.orch
            .assign_workflow(task_id, version, &plugin_index(), false)
            .unwrap();
        self.orch.confirm_and_run(task_id).unwrap();
    }
}

fn start_phase(
    tmp: &std::path::Path,
    directory: Arc<dyn ExecutionDirectoryProvider>,
    current_pin: Option<PluginSourcePin>,
    resolver: Option<Arc<dyn mf_agent::execution_directory::DirectoryProviderResolver>>,
) -> Phase {
    let catalog = catalog_with_worker_instance();
    let instance_id = fx_instance_id(&catalog);
    let pins = Arc::new(FakePins::default());
    let host = Arc::new(RecordingHost::default());
    let store = Store::open(&tmp.join("workflow-v1.db")).unwrap();
    let orch = Orchestrator::start_with_routing(
        store,
        tmp.to_path_buf(),
        mf_agent::config::Config::default(),
        host.clone(),
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        directory,
        WorkflowKernel {
            catalog: catalog.clone(),
            pins: Some(pins.clone()),
            instance_resolver: None,
        },
        DirectoryRouting {
            current_pin,
            resolver,
        },
    )
    .unwrap();
    Phase {
        catalog,
        pins,
        host,
        orch,
        instance_id,
    }
}

/// v1 运行中租约 → 升级到 v2 重启:租约 pin=v1,解析器仍含 v1 →
/// 汇合必须调用 v1,v2(当前 self.directory)零调用。
#[test]
fn held_lease_after_upgrade_routes_to_pinned_v1_not_current_v2() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let v1 = PinnedDirectory::new(root, pin("1.0.0"));
    let v2 = PinnedDirectory::new(root, pin("2.0.0"));

    // 阶段 1:v1 作为当前提供器运行 A(租约 pin=v1 持有)
    let fx1 = start_phase(
        root,
        v1.clone() as Arc<dyn ExecutionDirectoryProvider>,
        Some(pin("1.0.0")),
        None,
    );
    fx1.pins.resolve_ok(true);
    let version = fx1.template(
        "routing",
        vec![
            node("a", &[], "做 A", &fx1.instance_id),
            node("b", &[], "做 B", &fx1.instance_id),
            node("j", &["a", "b"], "汇合", &fx1.instance_id),
        ],
    );
    let task = fx1.orch.create_task("路由", "g").unwrap();
    fx1.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx1
        .host
        .workflow
        .lock()
        .len()
        == 2));
    fx1.orch
        .settle_by_token(
            &token_of_node(&fx1.orch, task.id, "a"),
            Settlement::complete("a"),
        )
        .unwrap();
    assert_eq!(v1.merges.load(Ordering::SeqCst), 0, "组未完整:不汇合");
    fx1.orch.stop();

    // 阶段 2("升级+重启"):当前提供器 = v2;解析器仍可解析 v1
    let fx2 = start_phase(
        root,
        v2.clone() as Arc<dyn ExecutionDirectoryProvider>,
        Some(pin("2.0.0")),
        Some(Arc::new(registry(&[
            (pin("1.0.0"), v1.clone()),
            (pin("2.0.0"), v2.clone()),
        ]))),
    );
    // B 结算:批 [A,B] 完整 → 必须路由到 v1
    fx2.orch
        .settle_by_token(
            &token_of_node(&fx2.orch, task.id, "b"),
            Settlement::complete("b"),
        )
        .unwrap();
    assert_eq!(
        v1.merges.load(Ordering::SeqCst),
        1,
        "held lease 的汇合必须调用租约 pin 的 v1 提供器"
    );
    assert_eq!(
        v2.merges.load(Ordering::SeqCst),
        0,
        "当前 self.directory(v2)不得被用来汇合 v1 的租约"
    );
    assert_eq!(v1.releases.load(Ordering::SeqCst), 2, "释放也走 v1");
    let rows = fx2.orch.store.list_execution_leases(task.id).unwrap();
    assert!(rows.iter().all(|r| r.status == "released"));
    fx2.orch.stop();
}

/// v1 已缺失(升级后卸载):解析失败 → 持久 NeedsYou、租约保持持有,
/// v2 一次都不被调用。
#[test]
fn unresolvable_pin_keeps_leases_held_and_needs_user_without_v2_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let v1 = PinnedDirectory::new(root, pin("1.0.0"));
    let v2 = PinnedDirectory::new(root, pin("2.0.0"));

    let fx1 = start_phase(
        root,
        v1.clone() as Arc<dyn ExecutionDirectoryProvider>,
        Some(pin("1.0.0")),
        None,
    );
    fx1.pins.resolve_ok(true);
    let version = fx1.template(
        "missing",
        vec![
            node("a", &[], "做 A", &fx1.instance_id),
            node("b", &[], "做 B", &fx1.instance_id),
            node("j", &["a", "b"], "汇合", &fx1.instance_id),
        ],
    );
    let task = fx1.orch.create_task("缺失", "g").unwrap();
    fx1.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx1
        .host
        .workflow
        .lock()
        .len()
        == 2));
    fx1.orch
        .settle_by_token(
            &token_of_node(&fx1.orch, task.id, "a"),
            Settlement::complete("a"),
        )
        .unwrap();
    fx1.orch.stop();

    // 阶段 2:v1 已卸载,解析器只含 v2
    let fx2 = start_phase(
        root,
        v2.clone() as Arc<dyn ExecutionDirectoryProvider>,
        Some(pin("2.0.0")),
        Some(Arc::new(registry(&[(pin("2.0.0"), v2.clone())]))),
    );
    fx2.orch
        .settle_by_token(
            &token_of_node(&fx2.orch, task.id, "b"),
            Settlement::complete("b"),
        )
        .unwrap();
    assert_eq!(
        v1.merges.load(Ordering::SeqCst) + v2.merges.load(Ordering::SeqCst),
        0,
        "pin 解析失败:任何提供器都不得被调用"
    );
    let conflicts = fx2.orch.pending_merge_conflicts(task.id);
    assert!(
        conflicts.iter().any(|c| c.contains("解析")),
        "解析失败必须持久化为待决汇合: {conflicts:?}"
    );
    let rows = fx2.orch.store.list_execution_leases(task.id).unwrap();
    assert!(
        rows.iter().all(|r| r.status == "held"),
        "解析失败时租约保持持有(可人工处理后重试): {rows:?}"
    );
    let t = fx2.orch.store.task_view(task.id).unwrap().unwrap();
    assert!(matches!(t.status, mf_agent::model::TaskStatus::NeedsYou));
    fx2.orch.stop();
}
