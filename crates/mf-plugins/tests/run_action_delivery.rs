use mf_agent::execution_directory::{
    ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome, RunActionDeliveryKey,
};
use mf_agent::workflow::PluginSourcePin;
use mf_plugins::worker_directory_provider::{DirectoryWorkerTransport, WorkerDirectoryProvider};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 模拟独立 worker 的生产持久收口：receipt 落在独立目录，首次真实效果
/// 成功后故意丢失 RPC reply。重建 provider/transport 后仍从 receipt 判重，
/// 因而不是宿主进程内 HashSet 掩盖幂等缺口。
struct DurableWorker {
    root: PathBuf,
}

impl DurableWorker {
    fn receipt(&self, method: &str, key: &RunActionDeliveryKey) -> PathBuf {
        self.root.join("receipts").join(format!(
            "{}-{}-{}-{}",
            method.replace('.', "-"),
            key.outbox_id(),
            key.action_index(),
            key.scope().replace(['/', '\\', ':', ','], "_")
        ))
    }

    fn external_effect(&self, method: &str, params: &Value) -> anyhow::Result<Value> {
        let key: RunActionDeliveryKey = serde_json::from_value(
            params
                .get("delivery_key")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("durable worker 缺 delivery_key"))?,
        )?;
        let receipt = self.receipt(method, &key);
        if receipt.exists() {
            return Ok(match method {
                "dir.merge" => serde_json::json!({ "type": "merged" }),
                _ => Value::Null,
            });
        }
        std::fs::create_dir_all(receipt.parent().unwrap())?;
        let mut effects = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("external-effects.log"))?;
        use std::io::Write as _;
        writeln!(
            effects,
            "{method}:{}:{}",
            key.outbox_id(),
            key.action_index()
        )?;
        std::fs::write(&receipt, b"committed")?;
        anyhow::bail!("故障注入:外部动作成功后、Kernel ack 前连接丢失")
    }
}

impl DirectoryWorkerTransport for DurableWorker {
    fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        match method {
            "dir.acquire" => {
                let step_id = params["step_id"].as_i64().unwrap();
                let path = self.root.join(format!("lease-{step_id}"));
                std::fs::create_dir_all(&path)?;
                Ok(serde_json::json!({
                    "id": format!("lease-{step_id}"),
                    "path": path,
                    "isolated": true,
                    "provider": "delivery.test.wt",
                    "metadata": { "step_key": params["step_key"] }
                }))
            }
            "dir.release" | "dir.merge" => self.external_effect(method, &params),
            other => anyhow::bail!("unexpected method {other}"),
        }
    }
}

fn provider(root: &Path) -> WorkerDirectoryProvider {
    WorkerDirectoryProvider::new_production(
        "delivery.test.wt",
        "worktree",
        true,
        Box::new(DurableWorker {
            root: root.to_path_buf(),
        }),
        PluginSourcePin {
            full_id: "delivery.test".into(),
            version: "1.0.0".into(),
            content_hash: "sha256-delivery-test".into(),
            contribution_id: "delivery.test.wt".into(),
        },
        root.to_path_buf(),
    )
    .unwrap()
}

fn lease(provider: &WorkerDirectoryProvider, root: &Path, step_id: i64) -> ExecutionLease {
    provider
        .acquire(&LeaseContext {
            task_id: 1,
            step_id,
            revision_id: 1,
            attempt: 1,
            project_root: root.to_path_buf(),
            step_key: format!("s{step_id}"),
            deps: vec![],
        })
        .unwrap()
}

fn effects(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("external-effects.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn release_replay_after_lost_ack_has_one_external_effect_and_different_key_is_not_collapsed() {
    let temp = tempfile::tempdir().unwrap();
    let first = provider(temp.path());
    let lease_a = lease(&first, temp.path(), 11);
    let key_a = RunActionDeliveryKey::new(71, 2).scoped("release-lease:lease-11");
    assert!(first.release_for_delivery(&key_a, &lease_a).is_err());

    // 模拟进程重启：重建 transport/provider，同 key 重放从外部 receipt 收口。
    let restarted = provider(temp.path());
    restarted.release_for_delivery(&key_a, &lease_a).unwrap();
    assert_eq!(effects(temp.path()), ["dir.release:71:2"]);

    let lease_b = lease(&restarted, temp.path(), 12);
    let key_b = RunActionDeliveryKey::new(71, 3).scoped("release-lease:lease-12");
    assert!(restarted.release_for_delivery(&key_b, &lease_b).is_err());
    provider(temp.path())
        .release_for_delivery(&key_b, &lease_b)
        .unwrap();
    assert_eq!(effects(temp.path()).len(), 2, "不同 key 不得误判为同一交付");
}

#[test]
fn merge_replay_after_lost_ack_uses_the_same_durable_delivery_key() {
    let temp = tempfile::tempdir().unwrap();
    let initial = provider(temp.path());
    let lease = lease(&initial, temp.path(), 21);
    let key = RunActionDeliveryKey::new(88, 4).scoped("merge-leases:lease-21");
    assert!(initial.merge_for_delivery(&key, &[lease.clone()]).is_err());

    let restarted = provider(temp.path());
    assert_eq!(
        restarted.merge_for_delivery(&key, &[lease]).unwrap(),
        MergeOutcome::Merged
    );
    assert_eq!(effects(temp.path()), ["dir.merge:88:4"]);
}
