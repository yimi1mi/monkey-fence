//! 单目标 command intent → target receipt/outbox 原子链。
//!
//! 本模块不是 transport，也不暴露任意 SQL 命令。未来 CoreKernel 的封闭
//! command enum 负责把已验证的领域命令编译为本层 effect closure；本层只
//! 拥有 canonical digest、service intent、L-CMD 事务与 crash reconcile。

use crate::handles::{
    ClientId, CommandId, CommandTarget, ExpectedRevision, Principal, TargetStoreKind,
};
use crate::lease::{CommandAuthorizer, LeaseCheck};
use crate::project_registry::ServiceStore;
use hmac::{Hmac, Mac};
use mf_agent::catalog_store::CatalogV2Store;
use mf_agent::store::Store;
use rand::RngCore;
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::Arc;
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;

pub const COMMAND_SCHEMA: &str = "mf.command.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandType {
    WorkflowCreate,
    WorkflowRename,
    WorkflowDelete,
    WorkflowAddNode,
    WorkflowUpdateNode,
    WorkflowRemoveNode,
    WorkflowMoveNode,
    WorkflowConnect,
    WorkflowDisconnect,
    WorkflowSetViewport,
    WorkflowSetUnsafeParallel,
    WorkflowRun,
    WorkflowRunCancel,
    WorkflowRetryStep,
    WorkflowRespond,
    WorkflowSettle,
    PreviewSessionStart,
    PreviewSessionStop,
    AdHocSessionStart,
    AdHocSessionStop,
    CatalogRefresh,
    ProviderModelProbe,
    ProviderProfileUpsert,
    AgentInstanceUpsert,
    InstallationPreview,
    InstallationExecute,
    InstallationCancel,
    RootEnable,
    RootDisable,
    ControllerTakeover,
}

impl CommandType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkflowCreate => "workflow.create",
            Self::WorkflowRename => "workflow.rename",
            Self::WorkflowDelete => "workflow.delete",
            Self::WorkflowAddNode => "workflow.add_node",
            Self::WorkflowUpdateNode => "workflow.update_node",
            Self::WorkflowRemoveNode => "workflow.remove_node",
            Self::WorkflowMoveNode => "workflow.move_node",
            Self::WorkflowConnect => "workflow.connect",
            Self::WorkflowDisconnect => "workflow.disconnect",
            Self::WorkflowSetViewport => "workflow.set_viewport",
            Self::WorkflowSetUnsafeParallel => "workflow.set_unsafe_parallel",
            Self::WorkflowRun => "workflow.run",
            Self::WorkflowRunCancel => "workflow.run_cancel",
            Self::WorkflowRetryStep => "workflow.retry_step",
            Self::WorkflowRespond => "workflow.respond",
            Self::WorkflowSettle => "workflow.settle",
            Self::PreviewSessionStart => "preview_session.start",
            Self::PreviewSessionStop => "preview_session.stop",
            Self::AdHocSessionStart => "ad_hoc_session.start",
            Self::AdHocSessionStop => "ad_hoc_session.stop",
            Self::CatalogRefresh => "catalog.refresh",
            Self::ProviderModelProbe => "provider.model_probe",
            Self::ProviderProfileUpsert => "provider_profile.upsert",
            Self::AgentInstanceUpsert => "agent_instance.upsert",
            Self::InstallationPreview => "installation.preview",
            Self::InstallationExecute => "installation.execute",
            Self::InstallationCancel => "installation.cancel",
            Self::RootEnable => "root.enable",
            Self::RootDisable => "root.disable",
            Self::ControllerTakeover => "controller.takeover",
        }
    }
}

pub enum CommandPayload {
    Plain(Value),
    /// 明文只活在 `Zeroizing` 中。canonical request 把它替换为 HMAC；
    /// response/receipt/outbox 永远拿不到此字段。
    WriteOnlySecret {
        public_fields: Value,
        secret: Zeroizing<String>,
    },
}

impl fmt::Debug for CommandPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(value) => f.debug_tuple("Plain").field(value).finish(),
            Self::WriteOnlySecret { public_fields, .. } => f
                .debug_struct("WriteOnlySecret")
                .field("public_fields", public_fields)
                .field("secret", &"[REDACTED]")
                .finish(),
        }
    }
}

pub struct CommandEnvelope {
    command_id: CommandId,
    client_id: ClientId,
    principal: Principal,
    controller_epoch: u64,
    root_epoch: Option<u64>,
    target: CommandTarget,
    expected: Vec<ExpectedRevision>,
    command_type: CommandType,
    payload: CommandPayload,
}

impl fmt::Debug for CommandEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandEnvelope")
            .field("command_id", &self.command_id)
            .field("target", &self.target)
            .field("command_type", &self.command_type)
            .field("payload", &self.payload)
            .finish_non_exhaustive()
    }
}

impl CommandEnvelope {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        command_id: CommandId,
        client_id: ClientId,
        principal: Principal,
        controller_epoch: u64,
        root_epoch: Option<u64>,
        target: CommandTarget,
        expected: Vec<ExpectedRevision>,
        command_type: CommandType,
        payload: CommandPayload,
    ) -> Result<Self, CommandProblem> {
        match target.store {
            TargetStoreKind::Project if !target.store_handle.starts_with("proj_") => {
                return Err(CommandProblem::InvalidEnvelope(
                    "Project target 缺少 proj_ store handle".into(),
                ));
            }
            TargetStoreKind::Catalog if target.store_handle != "catalog" => {
                return Err(CommandProblem::InvalidEnvelope(
                    "Catalog target store handle 必须为 catalog".into(),
                ));
            }
            _ => {}
        }
        match &payload {
            CommandPayload::WriteOnlySecret { public_fields, .. }
                if value_has_sensitive_field(public_fields) =>
            {
                return Err(CommandProblem::InvalidEnvelope(
                    "Secret public_fields 含凭据或可复用引用".into(),
                ));
            }
            CommandPayload::Plain(value) if value_has_sensitive_field(value) => {
                return Err(CommandProblem::InvalidEnvelope(
                    "凭据/Token 不得进入 Plain command payload".into(),
                ));
            }
            _ => {}
        }
        let mut keys: Vec<_> = expected
            .iter()
            .map(|item| format!("{}/{}", item.aggregate.kind.as_str(), item.aggregate.handle))
            .collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        if keys.len() != before {
            return Err(CommandProblem::InvalidEnvelope(
                "expected aggregate 重复".into(),
            ));
        }
        Ok(Self {
            command_id,
            client_id,
            principal,
            controller_epoch,
            root_epoch,
            target,
            expected,
            command_type,
            payload,
        })
    }

    pub fn command_id(&self) -> &CommandId {
        &self.command_id
    }

    pub fn target(&self) -> &CommandTarget {
        &self.target
    }

    pub const fn command_type(&self) -> CommandType {
        self.command_type
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    pub fn client_id(&self) -> &ClientId {
        &self.client_id
    }

    pub const fn controller_epoch(&self) -> u64 {
        self.controller_epoch
    }

    pub const fn root_epoch(&self) -> Option<u64> {
        self.root_epoch
    }

    pub fn semantic_digest(&self, key: &ServiceIdempotencyKey) -> Result<String, CommandProblem> {
        let payload = match &self.payload {
            CommandPayload::Plain(value) => sorted_json(value),
            CommandPayload::WriteOnlySecret {
                public_fields,
                secret,
            } => {
                let mut fields = match sorted_json(public_fields) {
                    Value::Object(fields) => fields,
                    _ => {
                        return Err(CommandProblem::InvalidEnvelope(
                            "Secret public_fields 必须是 object".into(),
                        ))
                    }
                };
                fields.insert(
                    "secret_hmac".into(),
                    Value::String(key.hmac(secret.as_bytes())?),
                );
                Value::Object(fields)
            }
        };
        let expected = canonical_expected_revisions(&self.expected);
        let canonical = sorted_json(&serde_json::json!({
            "schema": COMMAND_SCHEMA,
            "type": self.command_type.as_str(),
            "target": {
                "store": self.target.store_key(),
                "kind": self.target.aggregate.kind.as_str(),
                "handle": self.target.aggregate.handle,
            },
            "expected": expected,
            "payload": payload,
        }));
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|error| CommandProblem::Internal(error.to_string()))?;
        Ok(hex_digest(Sha256::digest(bytes)))
    }

    pub(crate) fn lease_check(&self) -> LeaseCheck<'_> {
        LeaseCheck {
            principal: &self.principal,
            client_id: &self.client_id,
            controller_epoch: self.controller_epoch,
            root_epoch: self.root_epoch,
            command_type: self.command_type,
            target: &self.target,
            expected: &self.expected,
        }
    }

    fn secret_plaintext(&self) -> Option<&str> {
        match &self.payload {
            CommandPayload::WriteOnlySecret { secret, .. } => Some(secret.as_str()),
            CommandPayload::Plain(_) => None,
        }
    }
}

pub struct ServiceIdempotencyKey(Zeroizing<Vec<u8>>);

impl ServiceIdempotencyKey {
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(bytes: Vec<u8>) -> Result<Self, CommandProblem> {
        Self::from_zeroizing(Zeroizing::new(bytes))
    }

    #[cfg(test)]
    pub(crate) fn new(bytes: Vec<u8>) -> Result<Self, CommandProblem> {
        Self::from_zeroizing(Zeroizing::new(bytes))
    }

    fn from_zeroizing(bytes: Zeroizing<Vec<u8>>) -> Result<Self, CommandProblem> {
        if bytes.len() < 32 {
            return Err(CommandProblem::InvalidEnvelope(
                "service idempotency key 至少 256 bit".into(),
            ));
        }
        Ok(Self(bytes))
    }

    fn hmac(&self, secret: &[u8]) -> Result<String, CommandProblem> {
        let mut mac = HmacSha256::new_from_slice(&self.0)
            .map_err(|error| CommandProblem::Internal(error.to_string()))?;
        mac.update(secret);
        Ok(hex_digest(mac.finalize().into_bytes()))
    }

    /// 生产入口：从 OS 当前用户 keyring 读取；首次生成 256-bit key。
    /// 调用方必须已持有 #20 CoreOwnerLock（跨进程唯一），本地 mutex 再
    /// 防同进程重复初始化。T1 不启动 Core，后续 assembly 只需调用此
    /// 入口，不可每次随机注入。
    pub fn load_or_create() -> Result<Self, CommandProblem> {
        const SERVICE: &str = "MonkeyFence";
        const ACCOUNT: &str = "service-idempotency-v1";
        static INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = INIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|error| {
            CommandProblem::Internal(format!("创建 idempotency keyring:{error}"))
        })?;
        let key = match entry.get_password() {
            Ok(hex) => parse_key_hex(&Zeroizing::new(hex))?,
            Err(keyring::Error::NoEntry) => {
                let mut key = Zeroizing::new(vec![0u8; 32]);
                rand::thread_rng().fill_bytes(&mut key);
                let hex = Zeroizing::new(hex_digest(&key));
                entry.set_password(&hex).map_err(|error| {
                    CommandProblem::Internal(format!("保存 idempotency keyring:{error}"))
                })?;
                key
            }
            Err(error) => {
                return Err(CommandProblem::Internal(format!(
                    "读取 idempotency keyring:{error}"
                )))
            }
        };
        Self::from_zeroizing(key)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectionEffect {
    /// None = command target aggregate；create/delete 可显式指定其它 aggregate。
    pub(crate) aggregate: Option<crate::handles::AggregateRef>,
    /// None = command type；wire override 用于 canonical event 名。
    pub(crate) event_type: Option<String>,
    pub(crate) projection_critical: bool,
    pub(crate) payload: Value,
}

impl ProjectionEffect {
    pub(crate) fn primary(payload: Value) -> Self {
        Self {
            aggregate: None,
            event_type: None,
            projection_critical: true,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EffectOutput {
    pub(crate) result_revisions: Value,
    /// 空 = 已线性化的 no-op；多项仍与业务效果/receipt 同一 target tx。
    pub(crate) projections: Vec<ProjectionEffect>,
}

impl EffectOutput {
    #[cfg(test)]
    pub(crate) fn for_contract(result_revisions: Value, projection: Value) -> Self {
        Self {
            result_revisions,
            projections: vec![ProjectionEffect::primary(projection)],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandOutcome {
    Applied {
        result_revisions: Value,
        replayed: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentState {
    Reserved,
    Applied,
    Failed,
    Cancelled,
    Revoked,
}

impl IntentState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Revoked => "revoked",
        }
    }

    fn parse(value: &str) -> Result<Self, CommandProblem> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "applied" => Ok(Self::Applied),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "revoked" => Ok(Self::Revoked),
            other => Err(CommandProblem::Internal(format!(
                "未知 command_intent.state:{other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    AfterIntentReserve,
    AfterTargetCommit,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandProblem {
    #[error("invalid_envelope:{0}")]
    InvalidEnvelope(String),
    #[error("command_id_reused")]
    CommandIdReused,
    #[error("command_in_progress")]
    CommandInProgress,
    #[error("controller_lease_expired")]
    ControllerLeaseExpired,
    #[error("root_epoch_expired")]
    RootEpochExpired,
    #[error("revision_conflict")]
    RevisionConflict,
    #[error("validation_failed:{0}")]
    ValidationFailed(String),
    #[error("workflow_cycle:{0}")]
    WorkflowCycle(String),
    #[error("unknown_dependency:{0}")]
    UnknownDependency(String),
    #[error("resource_not_found")]
    ResourceNotFound,
    #[error("target_store_mismatch")]
    TargetStoreMismatch,
    #[error("previous_command_failed")]
    PreviousCommandFailed,
    #[error("previous_command_cancelled")]
    PreviousCommandCancelled,
    #[error("fault_injected:{0}")]
    FaultInjected(&'static str),
    #[error("internal_error:{0}")]
    Internal(String),
}

impl CommandProblem {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidEnvelope(_) => "invalid_envelope",
            Self::CommandIdReused => "command_id_reused",
            Self::CommandInProgress => "command_in_progress",
            Self::ControllerLeaseExpired => "controller_lease_expired",
            Self::RootEpochExpired => "root_epoch_expired",
            Self::RevisionConflict => "revision_conflict",
            Self::ValidationFailed(_) => "validation_failed",
            Self::WorkflowCycle(_) => "workflow_cycle",
            Self::UnknownDependency(_) => "unknown_dependency",
            Self::ResourceNotFound => "resource_not_found",
            Self::TargetStoreMismatch => "internal_error",
            Self::PreviousCommandFailed => "internal_error",
            Self::PreviousCommandCancelled => "internal_error",
            Self::FaultInjected(_) => "internal_error",
            Self::Internal(_) => "internal_error",
        }
    }
}

#[derive(Clone)]
pub(crate) struct TargetDatabase {
    store_key: String,
    connection: TargetConnection,
}

#[derive(Clone)]
#[allow(dead_code)] // T2 CoreKernel facade wires the frozen T1 adapter.
enum TargetConnection {
    Project(Arc<Store>),
    Catalog(Arc<CatalogV2Store>),
}

#[allow(dead_code)] // T2 CoreKernel facade wires the frozen T1 adapter.
impl TargetDatabase {
    pub(crate) fn project(
        project_handle: impl Into<String>,
        store: Arc<Store>,
    ) -> Result<Self, CommandProblem> {
        let project_handle = project_handle.into();
        if !project_handle.starts_with("proj_") {
            return Err(CommandProblem::InvalidEnvelope(
                "Project target store 必须是 proj_ handle".into(),
            ));
        }
        Ok(Self {
            store_key: format!("project:{project_handle}"),
            connection: TargetConnection::Project(store),
        })
    }

    pub(crate) fn catalog(store: Arc<CatalogV2Store>) -> Self {
        Self {
            store_key: "catalog".into(),
            connection: TargetConnection::Catalog(store),
        }
    }

    fn kind(&self) -> TargetStoreKind {
        match &self.connection {
            TargetConnection::Project(_) => TargetStoreKind::Project,
            TargetConnection::Catalog(_) => TargetStoreKind::Catalog,
        }
    }

    pub(crate) fn store_key(&self) -> &str {
        &self.store_key
    }

    pub(crate) fn with_tx<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, CommandProblem>,
    ) -> Result<T, CommandProblem> {
        let mut f = Some(f);
        let result = match &self.connection {
            TargetConnection::Project(store) => store.with_tx(|tx| {
                f.take().expect("target closure 只调用一次")(tx).map_err(anyhow::Error::new)
            }),
            TargetConnection::Catalog(store) => store.with_tx(|tx| {
                f.take().expect("target closure 只调用一次")(tx).map_err(anyhow::Error::new)
            }),
        };
        result.map_err(problem_from_anyhow)
    }

    pub(crate) fn with_conn<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, CommandProblem>,
    ) -> Result<T, CommandProblem> {
        let mut f = Some(f);
        let result = match &self.connection {
            TargetConnection::Project(store) => store.with_conn(|conn| {
                f.take().expect("target closure 只调用一次")(conn).map_err(anyhow::Error::new)
            }),
            TargetConnection::Catalog(store) => store.with_conn(|conn| {
                f.take().expect("target closure 只调用一次")(conn).map_err(anyhow::Error::new)
            }),
        };
        result.map_err(problem_from_anyhow)
    }
}

#[allow(dead_code)] // Dark data until the T2 CoreKernel tracer.
pub(crate) struct CommandCoordinator {
    service: Arc<ServiceStore>,
    idempotency_key: ServiceIdempotencyKey,
}

#[allow(dead_code)] // Dark data until the T2 CoreKernel tracer.
impl CommandCoordinator {
    pub(crate) fn new(service: Arc<ServiceStore>, idempotency_key: ServiceIdempotencyKey) -> Self {
        Self {
            service,
            idempotency_key,
        }
    }

    /// Integration contract seam; production release 不暴露任意 effect。
    #[cfg(test)]
    pub(crate) fn dispatch_contract<F>(
        &self,
        envelope: &CommandEnvelope,
        target: &TargetDatabase,
        authorizer: &dyn CommandAuthorizer,
        effect: F,
    ) -> Result<CommandOutcome, CommandProblem>
    where
        F: FnOnce(&Transaction<'_>) -> Result<EffectOutput, CommandProblem>,
    {
        self.dispatch_internal(envelope, target, authorizer, effect, None, || {})
    }

    #[cfg(test)]
    pub(crate) fn dispatch_with_fault<F>(
        &self,
        envelope: &CommandEnvelope,
        target: &TargetDatabase,
        authorizer: &dyn CommandAuthorizer,
        effect: F,
        fault: Option<FaultPoint>,
    ) -> Result<CommandOutcome, CommandProblem>
    where
        F: FnOnce(&Transaction<'_>) -> Result<EffectOutput, CommandProblem>,
    {
        self.dispatch_internal(envelope, target, authorizer, effect, fault, || {})
    }

    #[cfg(test)]
    pub(crate) fn dispatch_with_reserve_hook<F, H>(
        &self,
        envelope: &CommandEnvelope,
        target: &TargetDatabase,
        authorizer: &dyn CommandAuthorizer,
        effect: F,
        after_reserve: H,
    ) -> Result<CommandOutcome, CommandProblem>
    where
        F: FnOnce(&Transaction<'_>) -> Result<EffectOutput, CommandProblem>,
        H: FnOnce(),
    {
        self.dispatch_internal(envelope, target, authorizer, effect, None, after_reserve)
    }

    /// 仅 CoreKernel 内封闭 command enum 可调用；transport/legacy/plugin
    /// 无法提交任意 SQL effect 或自造 outbox event type。
    #[allow(dead_code)]
    pub(crate) fn dispatch_internal<F, H>(
        &self,
        envelope: &CommandEnvelope,
        target: &TargetDatabase,
        authorizer: &dyn CommandAuthorizer,
        effect: F,
        fault: Option<FaultPoint>,
        after_reserve: H,
    ) -> Result<CommandOutcome, CommandProblem>
    where
        F: FnOnce(&Transaction<'_>) -> Result<EffectOutput, CommandProblem>,
        H: FnOnce(),
    {
        // 覆盖 reserve intent 提交到第二个 service coordinator transaction
        // 的全部窗口；同一 Store 的 startup reconcile 必须走同一 gate。
        let _command_gate = self.service.command_gate();
        if envelope.target.store != target.kind()
            || envelope.target.store_key() != target.store_key()
        {
            return Err(CommandProblem::TargetStoreMismatch);
        }
        let digest = envelope.semantic_digest(&self.idempotency_key)?;
        let reservation = match self.reserve_intent(envelope, &digest) {
            Ok(state) => state,
            Err(CommandProblem::CommandIdReused) => {
                // 不能让异 digest/target 成为绕过当前 lease 的探测旁路。
                let (_unit, _permit) = target.with_tx(|tx| {
                    let permit = authorizer.acquire(tx, &envelope.lease_check())?;
                    Ok(((), permit))
                })?;
                return Err(CommandProblem::CommandIdReused);
            }
            Err(error) => return Err(error),
        };
        after_reserve();
        if matches!(
            reservation,
            IntentState::Failed | IntentState::Cancelled | IntentState::Revoked
        ) {
            // 即使返回旧终态，也先在目标事务中复验当前安全 lease。
            let (_unit, _permit) = target.with_tx(|tx| {
                let permit = authorizer.acquire(tx, &envelope.lease_check())?;
                Ok(((), permit))
            })?;
            return Err(self
                .intent_problem(envelope.command_id())?
                .unwrap_or_else(|| match reservation {
                    IntentState::Failed => CommandProblem::PreviousCommandFailed,
                    IntentState::Cancelled => CommandProblem::PreviousCommandCancelled,
                    IntentState::Revoked => CommandProblem::ControllerLeaseExpired,
                    _ => unreachable!(),
                }));
        }
        if fault == Some(FaultPoint::AfterIntentReserve) {
            return Err(CommandProblem::FaultInjected("after_intent_reserve"));
        }

        // 第二个 service IMMEDIATE transaction 是 coordinator guard：跨进程
        // 串行 dispatch/reconcile。target commit 后、service finalize 前崩溃
        // 会回滚此 guard transaction，保留 reserved 供 receipt reconcile。
        self.service
            .with_tx(|service_tx| {
                let current = intent_tx(service_tx, envelope.command_id.as_str())?
                    .ok_or_else(|| anyhow::anyhow!("command intent 消失"))?;
                anyhow::ensure!(
                    matches!(current.state, IntentState::Reserved | IntentState::Applied),
                    "command intent 在 target 前进入冲突终态"
                );
                let guard_state = current.state;
                let target_result = target.with_tx(|tx| {
                    // 安全 lease/capability 先于 replay；CAS 只约束新 effect。
                    let permit = authorizer.acquire(tx, &envelope.lease_check())?;
                    if let Some(outcome) = receipt_outcome(tx, envelope, &digest)? {
                        return Ok((outcome, permit));
                    }
                    if guard_state == IntentState::Applied {
                        return Err(CommandProblem::Internal(
                            "applied intent 缺少 target receipt".into(),
                        ));
                    }
                    permit.validate_expected(tx, &envelope.lease_check())?;
                    let output = effect(tx)?;
                    let sensitive_output = value_has_sensitive_field(&output.result_revisions)
                        || output
                            .projections
                            .iter()
                            .any(|projection| value_has_sensitive_field(&projection.payload));
                    let plaintext_output = envelope.secret_plaintext().is_some_and(|secret| {
                        value_contains(&output.result_revisions, secret)
                            || output
                                .projections
                                .iter()
                                .any(|projection| value_contains(&projection.payload, secret))
                    });
                    if sensitive_output || plaintext_output {
                        return Err(CommandProblem::InvalidEnvelope(
                            "Secret 明文或可复用引用进入 result/event".into(),
                        ));
                    }
                    let revisions = canonical_json(&output.result_revisions)?;
                    let now = chrono::Utc::now().to_rfc3339();
                    tx.execute(
                        "INSERT INTO command_receipt
                         (command_id, semantic_digest, aggregate_handle, result_revisions,
                          state, created_at, finalized_at)
                         VALUES (?1, ?2, ?3, ?4, 'applied', ?5, ?5)",
                        params![
                            envelope.command_id.as_str(),
                            digest,
                            envelope.target.aggregate.handle,
                            revisions,
                            now,
                        ],
                    )
                    .map_err(internal)?;
                    for projection in output.projections {
                        let aggregate = projection
                            .aggregate
                            .unwrap_or_else(|| envelope.target.aggregate.clone());
                        let event_type = projection
                            .event_type
                            .unwrap_or_else(|| envelope.command_type.as_str().to_string());
                        let event = canonical_json(&serde_json::json!({
                            "type": format!("{event_type}.applied"),
                            "aggregate": {
                                "kind": aggregate.kind.as_str(),
                                "handle": aggregate.handle,
                            },
                            "caused_by_command_id": envelope.command_id.as_str(),
                            "projection_critical": projection.projection_critical,
                            "projection": projection.payload,
                        }))?;
                        tx.execute(
                            "INSERT INTO projection_outbox(event_json, published_at)
                             VALUES (?1, NULL)",
                            [event],
                        )
                        .map_err(internal)?;
                    }
                    Ok((
                        CommandOutcome::Applied {
                            result_revisions: output.result_revisions,
                            replayed: false,
                        },
                        permit,
                    ))
                });

                let (outcome, _permit) = match target_result {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let terminal = match &error {
                            CommandProblem::ControllerLeaseExpired
                            | CommandProblem::RootEpochExpired => Some(IntentState::Revoked),
                            CommandProblem::RevisionConflict
                            | CommandProblem::InvalidEnvelope(_)
                            | CommandProblem::ResourceNotFound => Some(IntentState::Failed),
                            _ => None,
                        };
                        if guard_state == IntentState::Reserved {
                            if let Some(state) = terminal {
                                finish_intent_tx(
                                    service_tx,
                                    envelope.command_id.as_str(),
                                    state,
                                    Some(error.code()),
                                )
                                .map_err(anyhow::Error::new)?;
                            }
                        }
                        return Ok(Err(error));
                    }
                };
                if fault == Some(FaultPoint::AfterTargetCommit) {
                    return Ok(Err(CommandProblem::FaultInjected("after_target_commit")));
                }
                finish_intent_tx(
                    service_tx,
                    envelope.command_id.as_str(),
                    IntentState::Applied,
                    None,
                )
                .map_err(anyhow::Error::new)?;
                Ok(Ok(outcome))
            })
            .map_err(problem_from_anyhow)?
    }

    /// 恢复只查询 target receipt，绝不重放 effect。无 receipt 的旧
    /// reserved intent 终结为 revoked；有 receipt 只补 service 结果。
    pub(crate) fn reconcile(
        &self,
        command_id: &CommandId,
        target: &TargetDatabase,
    ) -> Result<ReconcileOutcome, CommandProblem> {
        let _command_gate = self.service.command_gate();
        self.service
            .with_tx(|service_tx| {
                let intent = intent_tx(service_tx, command_id.as_str())?
                    .ok_or_else(|| anyhow::anyhow!("command intent 不存在:{command_id}"))?;
                if intent.target_store != target.store_key() {
                    return Err(anyhow::Error::new(CommandProblem::TargetStoreMismatch));
                }
                if intent.state != IntentState::Reserved && intent.state != IntentState::Applied {
                    return Ok(ReconcileOutcome::Terminal(intent.state));
                }
                let receipt = target
                    .with_conn(|conn| {
                        validated_receipt_result(
                            conn,
                            command_id.as_str(),
                            &intent.semantic_digest,
                            &intent.aggregate_handle,
                        )
                    })
                    .map_err(anyhow::Error::new)?;
                let Some(result_revisions) = receipt else {
                    finish_intent_tx(
                        service_tx,
                        command_id.as_str(),
                        IntentState::Revoked,
                        Some(CommandProblem::ControllerLeaseExpired.code()),
                    )
                    .map_err(anyhow::Error::new)?;
                    return Ok(ReconcileOutcome::Terminal(IntentState::Revoked));
                };
                finish_intent_tx(service_tx, command_id.as_str(), IntentState::Applied, None)
                    .map_err(anyhow::Error::new)?;
                Ok(ReconcileOutcome::Applied(CommandOutcome::Applied {
                    result_revisions,
                    replayed: true,
                }))
            })
            .map_err(problem_from_anyhow)
    }

    pub(crate) fn intent_state(
        &self,
        command_id: &CommandId,
    ) -> Result<Option<IntentState>, CommandProblem> {
        Ok(self.intent(command_id)?.map(|intent| intent.state))
    }

    fn reserve_intent(
        &self,
        envelope: &CommandEnvelope,
        digest: &str,
    ) -> Result<IntentState, CommandProblem> {
        let controller_epoch = i64::try_from(envelope.controller_epoch)
            .map_err(|_| CommandProblem::InvalidEnvelope("controller epoch 越界".into()))?;
        let root_epoch = envelope
            .root_epoch
            .map(i64::try_from)
            .transpose()
            .map_err(|_| CommandProblem::InvalidEnvelope("root epoch 越界".into()))?;
        self.service
            .with_tx(|tx| {
                let existing: Option<(String, String, String, String)> = tx
                    .query_row(
                        "SELECT semantic_digest, target_store, aggregate, state
                         FROM command_intent WHERE command_id=?1",
                        [envelope.command_id.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                if let Some((old_digest, store, aggregate, state)) = existing {
                    if old_digest != digest
                        || store != envelope.target.store_key()
                        || aggregate != envelope.target.aggregate.handle
                    {
                        return Err(anyhow::Error::new(CommandProblem::CommandIdReused));
                    }
                    return IntentState::parse(&state).map_err(anyhow::Error::new);
                }
                tx.execute(
                    "INSERT INTO command_intent
                     (command_id, semantic_digest, target_store, aggregate, principal,
                      client_id, controller_epoch, root_epoch, state, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'reserved', ?9)",
                    params![
                        envelope.command_id.as_str(),
                        digest,
                        envelope.target.store_key(),
                        envelope.target.aggregate.handle,
                        envelope.principal.as_str(),
                        envelope.client_id.as_str(),
                        controller_epoch,
                        root_epoch,
                        chrono::Utc::now().to_rfc3339(),
                    ],
                )?;
                Ok(IntentState::Reserved)
            })
            .map_err(problem_from_anyhow)
    }

    fn intent(&self, command_id: &CommandId) -> Result<Option<IntentRecord>, CommandProblem> {
        self.service
            .with_conn(|conn| intent_tx(conn, command_id.as_str()).map_err(anyhow::Error::new))
            .map_err(problem_from_anyhow)
    }

    fn intent_problem(
        &self,
        command_id: &CommandId,
    ) -> Result<Option<CommandProblem>, CommandProblem> {
        Ok(self
            .intent(command_id)?
            .and_then(|intent| intent.problem_code)
            .as_deref()
            .and_then(problem_for_terminal_code))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReconcileOutcome {
    Applied(CommandOutcome),
    Terminal(IntentState),
}

#[allow(dead_code)]
pub(crate) struct IntentRecord {
    pub(crate) semantic_digest: String,
    pub(crate) target_store: String,
    pub(crate) aggregate_handle: String,
    pub(crate) state: IntentState,
    pub(crate) problem_code: Option<String>,
}

/// service `command_intent` 行读取(operation/reconcile 复用)。
pub(crate) fn intent_tx(
    conn: &rusqlite::Connection,
    command_id: &str,
) -> Result<Option<IntentRecord>, CommandProblem> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT semantic_digest, target_store, aggregate, state, problem_code
             FROM command_intent WHERE command_id=?1",
            [command_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(internal)?;
    row.map(
        |(semantic_digest, target_store, aggregate_handle, state, problem_code)| {
            Ok(IntentRecord {
                semantic_digest,
                target_store,
                aggregate_handle,
                state: IntentState::parse(&state)?,
                problem_code,
            })
        },
    )
    .transpose()
}

/// 终结 command intent(operation saga 与 reconcile 复用)。`problem_code`
/// 直接取 [`CommandProblem::code`] 的稳定码,幂等重放同码不冲突。
pub(crate) fn finish_intent_tx(
    tx: &Transaction<'_>,
    command_id: &str,
    state: IntentState,
    problem_code: Option<&str>,
) -> Result<(), CommandProblem> {
    let changed = tx
        .execute(
            "UPDATE command_intent SET state=?2, resolved_at=?3, problem_code=?4
             WHERE command_id=?1 AND state='reserved'",
            params![
                command_id,
                state.as_str(),
                chrono::Utc::now().to_rfc3339(),
                problem_code,
            ],
        )
        .map_err(internal)?;
    if changed == 1 {
        return Ok(());
    }
    let current = intent_tx(tx, command_id)?
        .ok_or_else(|| CommandProblem::Internal("command intent 消失".into()))?;
    if current.state == state && current.problem_code.as_deref() == problem_code {
        Ok(())
    } else {
        Err(CommandProblem::Internal(format!(
            "command intent 终态冲突:current={} attempted={}",
            current.state.as_str(),
            state.as_str()
        )))
    }
}

pub(crate) fn problem_for_terminal_code(code: &str) -> Option<CommandProblem> {
    match code {
        "controller_lease_expired" => Some(CommandProblem::ControllerLeaseExpired),
        "root_epoch_expired" => Some(CommandProblem::RootEpochExpired),
        "revision_conflict" => Some(CommandProblem::RevisionConflict),
        "resource_not_found" => Some(CommandProblem::ResourceNotFound),
        "invalid_envelope" => Some(CommandProblem::InvalidEnvelope(
            "previous command rejected".into(),
        )),
        _ => None,
    }
}

fn receipt_outcome(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    digest: &str,
) -> Result<Option<CommandOutcome>, CommandProblem> {
    let Some(result_revisions) = validated_receipt_result(
        tx,
        envelope.command_id.as_str(),
        digest,
        &envelope.target.aggregate.handle,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(CommandOutcome::Applied {
        result_revisions,
        replayed: true,
    }))
}

/// command 与 Operation 共用的 target-local receipt reader。身份必须匹配，
/// 且只有 applied+finalized 的行可作为已线性化凭证。
pub(crate) fn validated_receipt_result(
    conn: &rusqlite::Connection,
    receipt_id: &str,
    semantic_digest: &str,
    aggregate_handle: &str,
) -> Result<Option<Value>, CommandProblem> {
    let receipt: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT semantic_digest, aggregate_handle, result_revisions, state, finalized_at
             FROM command_receipt WHERE command_id=?1",
            [receipt_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(internal)?;
    let Some((stored_digest, aggregate, revisions, state, finalized_at)) = receipt else {
        return Ok(None);
    };
    if stored_digest != semantic_digest || aggregate != aggregate_handle {
        return Err(CommandProblem::CommandIdReused);
    }
    if state != "applied" || finalized_at.is_none() {
        return Err(CommandProblem::CommandInProgress);
    }
    serde_json::from_str(&revisions).map(Some).map_err(internal)
}

fn sorted_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), sorted_json(&object[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.iter().map(sorted_json).collect()),
        other => other.clone(),
    }
}

pub(crate) fn canonical_json(value: &Value) -> Result<String, CommandProblem> {
    serde_json::to_string(&sorted_json(value))
        .map_err(|error| CommandProblem::Internal(error.to_string()))
}

/// command 与 Operation step 共用的 expected revision canonical 编码。
/// 轴由 BTreeMap 保序，aggregate 列表按 kind/handle 排序。
pub(crate) fn canonical_expected_revisions(expected: &[ExpectedRevision]) -> Vec<Value> {
    let mut expected = expected.to_vec();
    expected.sort_by(|a, b| {
        (a.aggregate.kind.as_str(), a.aggregate.handle.as_str())
            .cmp(&(b.aggregate.kind.as_str(), b.aggregate.handle.as_str()))
    });
    expected
        .into_iter()
        .map(|item| {
            let revisions: serde_json::Map<String, Value> = item
                .revisions
                .into_iter()
                .map(|(axis, revision)| (axis, Value::String(revision.to_string())))
                .collect();
            serde_json::json!({
                "aggregate": {
                    "kind": item.aggregate.kind.as_str(),
                    "handle": item.aggregate.handle,
                },
                "revisions": revisions,
            })
        })
        .collect()
}

fn value_contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(value) => value.contains(needle),
        Value::Array(values) => values.iter().any(|value| value_contains(value, needle)),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.contains(needle) || value_contains(value, needle)),
        _ => false,
    }
}

pub(crate) fn value_has_sensitive_field(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(value_has_sensitive_field),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| is_sensitive_key(key) || value_has_sensitive_field(value)),
        _ => false,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    // 与 worker protocol redaction 的分隔符归一口径一致；token 用精确/
    // 后缀规则，避免误伤合法 `max_tokens` / `token_count`。
    let key = key.to_lowercase().replace(['-', ' ', '.'], "_");
    key.contains("secret")
        || key.contains("password")
        || key.contains("api_key")
        || key.contains("apikey")
        || key.contains("credential")
        || key == "token"
        || key == "token_ref"
        || key.ends_with("_token")
        || key.ends_with("_token_ref")
}

pub(crate) fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn parse_key_hex(hex: &str) -> Result<Zeroizing<Vec<u8>>, CommandProblem> {
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CommandProblem::Internal(
            "idempotency keyring 格式非法".into(),
        ));
    }
    let mut key = Zeroizing::new(Vec::with_capacity(32));
    for index in (0..64).step_by(2) {
        key.push(
            u8::from_str_radix(&hex[index..index + 2], 16)
                .map_err(|_| CommandProblem::Internal("idempotency keyring 格式非法".into()))?,
        );
    }
    Ok(key)
}

fn internal(error: impl fmt::Display) -> CommandProblem {
    CommandProblem::Internal(error.to_string())
}

pub(crate) fn problem_from_anyhow(error: anyhow::Error) -> CommandProblem {
    error
        .downcast_ref::<CommandProblem>()
        .cloned()
        .unwrap_or_else(|| CommandProblem::Internal(format!("{error:#}")))
}
