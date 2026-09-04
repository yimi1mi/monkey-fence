//! `mf.command.v1` envelope 与命令族(T7b,Issue #39;spec §7.4)。
//!
//! 命令族是封闭枚举(wire 值稳定);`expected` 恒为 aggregate 列表;
//! 同 `command_id + canonical digest` 幂等返回原结果,异 digest →
//! `command_id_reused`;write-only Secret 在 digest 中替换为 HMAC。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{handle, u64_str};
use crate::problem::ProblemCode;

/// 封闭命令族(§7.4;v1 冻结——新增命令必须 additive optional,改语义
/// 升 v2)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandType {
    // Project Workflow
    #[serde(rename = "workflow.create")]
    WorkflowCreate,
    #[serde(rename = "workflow.rename")]
    WorkflowRename,
    #[serde(rename = "workflow.delete")]
    WorkflowDelete,
    #[serde(rename = "workflow.add_node")]
    WorkflowAddNode,
    #[serde(rename = "workflow.update_node")]
    WorkflowUpdateNode,
    #[serde(rename = "workflow.remove_node")]
    WorkflowRemoveNode,
    #[serde(rename = "workflow.move_node")]
    WorkflowMoveNode,
    #[serde(rename = "workflow.connect")]
    WorkflowConnect,
    #[serde(rename = "workflow.disconnect")]
    WorkflowDisconnect,
    #[serde(rename = "workflow.viewport")]
    WorkflowViewport,
    #[serde(rename = "workflow.set_unsafe_parallel_policy")]
    WorkflowSetUnsafeParallelPolicy,
    // Workflow Run
    #[serde(rename = "workflow.run.start")]
    WorkflowRunStart,
    #[serde(rename = "workflow.run.cancel")]
    WorkflowRunCancel,
    #[serde(rename = "workflow.run.retry_step")]
    WorkflowRunRetryStep,
    #[serde(rename = "workflow.run.respond")]
    WorkflowRunRespond,
    #[serde(rename = "workflow.run.settle")]
    WorkflowRunSettle,
    /// 激活 agent 提案的 draft revision(#89 additive)。
    #[serde(rename = "workflow.confirm_proposal")]
    WorkflowConfirmProposal,
    // Agent Session(terminal attach/input 不走此入口)
    #[serde(rename = "session.start_preview")]
    SessionStartPreview,
    #[serde(rename = "session.stop_preview")]
    SessionStopPreview,
    #[serde(rename = "session.start_adhoc")]
    SessionStartAdhoc,
    #[serde(rename = "session.stop_adhoc")]
    SessionStopAdhoc,
    // Catalog
    #[serde(rename = "catalog.refresh_discovery")]
    CatalogRefreshDiscovery,
    #[serde(rename = "catalog.provider_model_probe")]
    CatalogProviderModelProbe,
    #[serde(rename = "catalog.provider_profile_upsert")]
    CatalogProviderProfileUpsert,
    #[serde(rename = "catalog.agent_instance_upsert")]
    CatalogAgentInstanceUpsert,
    // CLI
    #[serde(rename = "cli.install_preview")]
    CliInstallPreview,
    #[serde(rename = "cli.install")]
    CliInstall,
    #[serde(rename = "cli.update")]
    CliUpdate,
    #[serde(rename = "cli.repair")]
    CliRepair,
    #[serde(rename = "cli.uninstall")]
    CliUninstall,
    #[serde(rename = "cli.cancel")]
    CliCancel,
    // Root Mode
    #[serde(rename = "root.enable")]
    RootEnable,
    #[serde(rename = "root.disable")]
    RootDisable,
}

impl CommandType {
    pub fn as_str(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".into())
    }
}

/// aggregate 引用(kind + opaque handle)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateRef {
    pub kind: String,
    pub handle: String,
}

/// expected revision(单 aggregate 的双轴 CAS;轴缺省 = 不 CAS 该轴)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedRevision {
    pub aggregate: AggregateRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation_revision: Option<String>,
}

/// `mf.command.v1` envelope(u64 全部字符串化)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub schema: String,
    pub command_id: String,
    pub client_id: String,
    #[serde(with = "u64_str")]
    pub controller_lease_epoch: u64,
    pub target: AggregateRef,
    #[serde(default)]
    pub expected: Vec<ExpectedRevision>,
    #[serde(rename = "type")]
    pub command_type: CommandType,
    pub payload: serde_json::Value,
}

impl CommandEnvelope {
    pub fn new(
        command_id: &str,
        client_id: &str,
        controller_lease_epoch: u64,
        target: AggregateRef,
        command_type: CommandType,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            schema: "mf.command.v1".to_string(),
            command_id: command_id.to_string(),
            client_id: client_id.to_string(),
            controller_lease_epoch,
            target,
            expected: Vec::new(),
            command_type,
            payload,
        }
    }

    /// envelope 校验:target/expected handle 必须 opaque(任意
    /// path/PID/argv 在此 fail-closed → resource_not_found)。
    pub fn validate(&self) -> Result<(), ProblemCode> {
        handle::parse(&self.target.handle)?;
        for expected in &self.expected {
            handle::parse(&expected.aggregate.handle)?;
        }
        Ok(())
    }

    /// canonical digest(幂等键的一半):对 wire 规范编码
    /// schema/type/target/expected/payload 计算;同一 command 的重试
    /// 生成相同 digest。**write-only Secret 特例**:payload 中字段名含
    /// `secret` 且为字符串明文时,digest 输入替换为其 HMAC(持久
    /// service idempotency key 由调用方提供;§7.4)。
    pub fn canonical_digest(&self, secret_hmac_key: &[u8; 32]) -> String {
        let mut payload = self.payload.clone();
        redact_secrets_in_place(&mut payload, secret_hmac_key);
        let canonical = serde_json::json!({
            "schema": self.schema,
            "type": self.command_type,
            "target": { "kind": self.target.kind, "handle": self.target.handle },
            "expected": self.expected,
            "payload": payload,
        });
        let text = serde_json::to_string(&canonical).expect("canonical JSON");
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }
}

/// 递归把对象中 `*secret*` 字段的字符串明文替换为 HMAC 摘要(摘要
/// 进入 digest/receipt;明文永不落 wire/日志/收据,§7.4)。
fn redact_secrets_in_place(value: &mut serde_json::Value, key: &[u8; 32]) {
    use hmac::{Hmac, Mac};
    match value {
        serde_json::Value::Object(map) => {
            for (field, entry) in map.iter_mut() {
                if field.to_ascii_lowercase().contains("secret") {
                    if let serde_json::Value::String(plaintext) = entry {
                        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
                        mac.update(plaintext.as_bytes());
                        let digest = mac.finalize().into_bytes();
                        *entry = serde_json::Value::String(format!("hmac_{:x}", digest));
                        continue;
                    }
                }
                redact_secrets_in_place(entry, key);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_secrets_in_place(item, key);
            }
        }
        _ => {}
    }
}

/// 幂等判定:同 command_id 同 digest → 幂等重放;同 id 异 digest →
/// `command_id_reused`(整条拒绝)。
pub fn check_idempotency(previous: Option<&str>, current: &str) -> Result<(), ProblemCode> {
    match previous {
        Some(prev) if prev == current => Ok(()),
        Some(_) => Err(ProblemCode::CommandIdReused),
        None => Ok(()),
    }
}

/// 命令响应(200 applied / 202 accepted;§7.4)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommandOutcomeWire {
    Applied {
        revisions: Vec<ExpectedRevision>,
        /// true = 命中既有 receipt 的幂等重放。
        replayed: bool,
    },
    Accepted {
        operation_handle: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> CommandEnvelope {
        CommandEnvelope::new(
            "018f3e2a-1b2c-7d3e-9f4a-5b6c7d8e9f0a",
            "cl_test",
            17,
            AggregateRef {
                kind: "project_workflow".into(),
                handle: "wf_0123456789abcdef0123456789abcdef".into(),
            },
            CommandType::WorkflowMoveNode,
            serde_json::json!({
                "node_handle": "step_0123456789abcdef0123456789abcdef",
                "x": 420,
                "y": 180
            }),
        )
    }

    #[test]
    fn command_wire_shape_matches_spec_sample() {
        let json = serde_json::to_value(envelope()).unwrap();
        assert_eq!(json["schema"], "mf.command.v1");
        assert_eq!(json["controller_lease_epoch"], "17", "u64 字符串化");
        assert_eq!(json["type"], "workflow.move_node");
        assert_eq!(json["payload"]["x"], 420);
        let back: CommandEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back, envelope());
    }

    #[test]
    fn non_opaque_target_is_rejected_as_404() {
        let mut command = envelope();
        command.target.handle = "C:/evil/path.exe".into();
        assert_eq!(command.validate(), Err(ProblemCode::ResourceNotFound));
        command.target.handle = "123".into();
        assert_eq!(command.validate(), Err(ProblemCode::ResourceNotFound));
    }

    #[test]
    fn idempotency_digest_reuse_semantics() {
        let key = [7u8; 32];
        let command = envelope();
        let digest = command.canonical_digest(&key);
        // 相同 envelope → 相同 digest(幂等重放放行)
        assert_eq!(envelope().canonical_digest(&key), digest);
        check_idempotency(Some(&digest), &digest).unwrap();
        // 异 digest → command_id_reused
        assert_eq!(
            check_idempotency(Some("other"), &digest),
            Err(ProblemCode::CommandIdReused)
        );
        // 首见(None)放行
        check_idempotency(None, &digest).unwrap();
    }

    #[test]
    fn secrets_are_hmac_redacted_in_digest_but_payload_intact() {
        let key = [9u8; 32];
        let mut command = envelope();
        command.command_type = CommandType::CatalogProviderProfileUpsert;
        command.payload = serde_json::json!({
            "api_secret": "sk-super-secret-plaintext",
            "nested": { "client_secret": "another-plaintext" },
            "name": "prod"
        });
        let digest = command.canonical_digest(&key);
        // digest 不含明文
        assert!(!digest.contains("sk-super-secret"));
        // 原 envelope payload 保留明文(仅 digest 脱敏;明文只进 Secret Store)
        assert_eq!(command.payload["api_secret"], "sk-super-secret-plaintext");
        // 同明文 → 同 digest;异明文 → 异 digest
        let same = command.canonical_digest(&key);
        assert_eq!(digest, same);
        command.payload["api_secret"] = "sk-different".into();
        assert_ne!(command.canonical_digest(&key), digest);
    }
}
