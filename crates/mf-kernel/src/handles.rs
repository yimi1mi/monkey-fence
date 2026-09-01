//! Command 层持久标识与目标引用。

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct CommandId(String);

impl CommandId {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        let uuid = uuid::Uuid::parse_str(&value)?;
        anyhow::ensure!(uuid.get_version_num() == 7, "command_id 必须是 UUIDv7");
        Ok(Self(uuid.to_string()))
    }

    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for CommandId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CommandId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(String);

impl ClientId {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        anyhow::ensure!(!value.trim().is_empty(), "client_id 不能为空");
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Principal(String);

impl Principal {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        anyhow::ensure!(!value.trim().is_empty(), "principal 不能为空");
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn parse_uuidv7(value: &str) -> anyhow::Result<()> {
    let uuid = uuid::Uuid::parse_str(value)?;
    anyhow::ensure!(uuid.get_version_num() == 7, "handle 必须是 UUIDv7:{value}");
    Ok(())
}

/// Project Store 的持久 opaque handle(`proj_` + UUIDv7,service-v1
/// `project_registry` 发放,永不复用、不得由 rowid/路径派生)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ProjectStoreHandle(String);

impl ProjectStoreHandle {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        anyhow::ensure!(
            value.starts_with("proj_"),
            "Project store handle 必须以 proj_ 开头"
        );
        parse_uuidv7(value.strip_prefix("proj_").unwrap_or_default())?;
        Ok(Self(value))
    }

    pub fn generate() -> Self {
        Self(format!("proj_{}", uuid::Uuid::now_v7()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectStoreHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProjectStoreHandle {
    type Error = anyhow::Error;
    fn try_from(value: String) -> anyhow::Result<Self> {
        Self::parse(value)
    }
}

/// Project Workflow 聚合的持久 opaque handle(Project v7
/// `project_workflows.public_handle`,UUIDv7,永不复用)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct WorkflowHandle(String);

impl WorkflowHandle {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        parse_uuidv7(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkflowHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for WorkflowHandle {
    type Error = anyhow::Error;
    fn try_from(value: String) -> anyhow::Result<Self> {
        Self::parse(value)
    }
}

/// Agent Session opaque handle(`sess_` + UUIDv7)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct SessionHandle(String);

impl SessionHandle {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        let uuid = value
            .strip_prefix("sess_")
            .ok_or_else(|| anyhow::anyhow!("session handle 必须以 sess_ 开头"))?;
        parse_uuidv7(uuid)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SessionHandle {
    type Error = anyhow::Error;
    fn try_from(value: String) -> anyhow::Result<Self> {
        Self::parse(value)
    }
}

/// Core 进程实例标识(每次构造新值;重启后旧值失效,附录 B)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerInstanceId(String);

impl ServerInstanceId {
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ServerInstanceId {
    fn default() -> Self {
        Self::new()
    }
}

/// 事件流 epoch(publication 失败/容量 fail-closed 时旋转;客户端必须
/// 重新 Snapshot,附录 B)。opaque,不承诺格式。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamEpoch(String);

impl StreamEpoch {
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for StreamEpoch {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStoreKind {
    Project,
    Catalog,
}

impl TargetStoreKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Catalog => "catalog",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateKind {
    Project,
    ProjectWorkflow,
    WorkflowRun,
    Step,
    AgentSession,
    AgentInstance,
    ProviderProfile,
    Installation,
    RootState,
}

impl AggregateKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::ProjectWorkflow => "project_workflow",
            Self::WorkflowRun => "workflow_run",
            Self::Step => "step",
            Self::AgentSession => "agent_session",
            Self::AgentInstance => "agent_instance",
            Self::ProviderProfile => "provider_profile",
            Self::Installation => "installation",
            Self::RootState => "root_state",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AggregateRef {
    pub kind: AggregateKind,
    pub handle: String,
}

impl AggregateRef {
    pub fn new(kind: AggregateKind, handle: impl Into<String>) -> anyhow::Result<Self> {
        let handle = handle.into();
        anyhow::ensure!(!handle.trim().is_empty(), "aggregate handle 不能为空");
        Ok(Self { kind, handle })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommandTarget {
    pub store: TargetStoreKind,
    /// Project target = `proj_...`；Catalog target 固定 `catalog`。
    pub store_handle: String,
    pub aggregate: AggregateRef,
}

impl CommandTarget {
    pub fn store_key(&self) -> String {
        match self.store {
            TargetStoreKind::Project => format!("project:{}", self.store_handle),
            TargetStoreKind::Catalog => "catalog".into(),
        }
    }

    pub fn stable_key(&self) -> String {
        format!(
            "{}:{}/{}",
            self.store_key(),
            self.aggregate.kind.as_str(),
            self.aggregate.handle
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedRevision {
    pub aggregate: AggregateRef,
    /// 轴名 → revision。使用有序 map，canonical digest 不受插入顺序影响。
    pub revisions: std::collections::BTreeMap<String, u64>,
}
