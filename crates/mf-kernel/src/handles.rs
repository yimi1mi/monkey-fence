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
