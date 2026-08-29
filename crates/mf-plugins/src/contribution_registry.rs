//! 类型化贡献查找:按完整贡献 ID(`publisher.plugin.contribution_id`)
//! 在启用插件中定位各类贡献。完整 ID 的插件前缀优先精确匹配,
//! 因此贡献 id 本身允许包含 `.`。

use crate::manifest::{
    NodeTypeContribution, PluginManifest, SecretStoreContribution, UiSchemaContribution,
    WorkflowTemplateContribution,
};
use crate::PluginEntry;

pub use crate::manifest::AgentTypeContribution;
pub use crate::manifest::ExecutionDirectoryContribution;

/// 贡献所属的插件来源(运行期固定版本用)。
#[derive(Debug, Clone)]
pub struct ContributionSource {
    pub plugin_full_id: String,
    pub plugin_version: String,
    pub content_hash: String,
}

#[derive(Debug, Default)]
pub struct ContributionRegistry {
    records: Vec<(ContributionSource, PluginManifest)>,
}

impl ContributionRegistry {
    /// 从插件条目构建索引(仅启用插件;内置合成插件天然启用)。
    pub fn from_enabled(entries: &[PluginEntry]) -> Self {
        let records = entries
            .iter()
            .filter(|p| p.enabled)
            .map(|p| {
                (
                    ContributionSource {
                        plugin_full_id: p.full_id.clone(),
                        plugin_version: p.manifest.manifest.version_str.clone(),
                        content_hash: p.content_hash.clone(),
                    },
                    p.manifest.clone(),
                )
            })
            .collect();
        ContributionRegistry { records }
    }

    /// 剥离插件前缀,返回 (来源, 清单, 贡献 id)。
    fn locate<'a, 'b>(
        &'a self,
        full_contribution_id: &'b str,
    ) -> Option<(&'a ContributionSource, &'a PluginManifest, &'b str)> {
        for (src, manifest) in &self.records {
            let prefix = format!("{}.", src.plugin_full_id);
            if let Some(rest) = full_contribution_id.strip_prefix(&prefix) {
                return Some((src, manifest, rest));
            }
        }
        None
    }

    pub fn find_agent_type(
        &self,
        full_contribution_id: &str,
    ) -> Option<(ContributionSource, AgentTypeContribution)> {
        let (src, m, id) = self.locate(full_contribution_id)?;
        m.agent_types
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .map(|a| (src.clone(), a))
    }

    pub fn find_node_type(
        &self,
        full_contribution_id: &str,
    ) -> Option<(ContributionSource, NodeTypeContribution)> {
        let (src, m, id) = self.locate(full_contribution_id)?;
        m.node_types
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .map(|n| (src.clone(), n))
    }

    /// 全部执行目录贡献:(完整贡献 ID, 来源, 贡献),按完整 ID 排序。
    pub fn execution_directories(
        &self,
    ) -> Vec<(String, ContributionSource, ExecutionDirectoryContribution)> {
        let mut out = Vec::new();
        for (src, manifest) in &self.records {
            for directory in &manifest.execution_directory_providers {
                out.push((
                    format!("{}.{}", src.plugin_full_id, directory.id),
                    src.clone(),
                    directory.clone(),
                ));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn find_execution_directory(
        &self,
        full_contribution_id: &str,
    ) -> Option<(ContributionSource, ExecutionDirectoryContribution)> {
        let (src, m, id) = self.locate(full_contribution_id)?;
        m.execution_directory_providers
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .map(|e| (src.clone(), e))
    }

    pub fn find_secret_store(
        &self,
        full_contribution_id: &str,
    ) -> Option<(ContributionSource, SecretStoreContribution)> {
        let (src, m, id) = self.locate(full_contribution_id)?;
        m.secret_stores
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .map(|s| (src.clone(), s))
    }

    pub fn find_workflow_template(
        &self,
        full_contribution_id: &str,
    ) -> Option<(ContributionSource, WorkflowTemplateContribution)> {
        let (src, m, id) = self.locate(full_contribution_id)?;
        m.workflow_templates
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .map(|t| (src.clone(), t))
    }

    pub fn find_ui_schema(
        &self,
        full_contribution_id: &str,
    ) -> Option<(ContributionSource, UiSchemaContribution)> {
        let (src, m, id) = self.locate(full_contribution_id)?;
        m.ui_schemas
            .iter()
            .find(|u| u.id == id)
            .cloned()
            .map(|u| (src.clone(), u))
    }

    /// 列出全部 Agent Type 贡献:(完整贡献 ID, 来源, 贡献),按完整 ID 稳定排序。
    /// 供工作流编译输入(agent_type → 插件包 pin)与实例页列表使用。
    pub fn agent_types(&self) -> Vec<(String, ContributionSource, AgentTypeContribution)> {
        let mut out: Vec<(String, ContributionSource, AgentTypeContribution)> = Vec::new();
        for (src, manifest) in &self.records {
            for agent_type in &manifest.agent_types {
                out.push((
                    format!("{}.{}", src.plugin_full_id, agent_type.id),
                    src.clone(),
                    agent_type.clone(),
                ));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Capabilities, ManifestHeader};

    fn manifest(publisher: &str, id: &str, agent_ids: &[&str]) -> PluginManifest {
        PluginManifest {
            manifest: ManifestHeader {
                version: crate::manifest::MANIFEST_VERSION,
                publisher: publisher.into(),
                id: id.into(),
                name: format!("{publisher}.{id}"),
                version_str: "0.1.0".into(),
                min_app_version: String::new(),
                description: String::new(),
                homepage: String::new(),
                icon: String::new(),
            },
            capabilities: Capabilities::default(),
            worker: None,
            agent_types: agent_ids
                .iter()
                .map(|a| AgentTypeContribution {
                    id: a.to_string(),
                    name: a.to_string(),
                    adapter: "generic-command".into(),
                    config_schema: String::new(),
                    command: String::new(),
                    detect_commands: vec![],
                    modes: vec![],
                    supports_isolated_config: false,
                })
                .collect(),
            node_types: vec![],
            execution_directory_providers: vec![],
            secret_stores: vec![],
            workflow_templates: vec![],
            skills: vec![],
            tools: vec![],
            ui_schemas: vec![],
        }
    }

    fn entry(m: PluginManifest, enabled: bool) -> PluginEntry {
        PluginEntry {
            full_id: m.full_id(),
            content_hash: "sha256:x".into(),
            permission_fingerprint: "f".into(),
            manifest: m,
            root: None,
            source: crate::install::InstallSource::Bundled,
            enabled,
            authorized_at: None,
            builtin: false,
            detected: Default::default(),
        }
    }

    #[test]
    fn lookup_strips_plugin_prefix_exact() {
        let entries = vec![
            entry(manifest("zhipu", "demo", &["agent.one"]), true),
            entry(manifest("zhipu", "demo-two", &["agent.two"]), false),
        ];
        let reg = ContributionRegistry::from_enabled(&entries);
        // 启用插件的贡献可查(贡献 id 含 `.` 也能精确剥离前缀)
        let (src, a) = reg.find_agent_type("zhipu.demo.agent.one").unwrap();
        assert_eq!(src.plugin_full_id, "zhipu.demo");
        assert_eq!(a.id, "agent.one");
        // 禁用插件贡献不可见
        assert!(reg.find_agent_type("zhipu.demo-two.agent.two").is_none());
        // 前缀是精确插件 full_id,不是字符串前缀
        assert!(reg.find_agent_type("zhipu.dem.agent.one").is_none());
        assert_eq!(reg.agent_types().len(), 1);
    }
}
