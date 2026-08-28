//! Agent Instance 领域类型:用户保存的一套 Agent Type 配置(设计 §4.2)。
//!
//! - 编辑实例只追加新的不可变版本行(`agent_instance_versions`),
//!   已取出的快照与已冻结的 Revision 不随后续编辑变化。
//! - 版本行保存可执行文件、argv、非敏感 env、配置 JSON、执行契约与
//!   sealed Secret 引用;明文 Secret 永不进入版本行(见 `secrets`)。
//! - 项目覆盖只合并显式声明的键,产出解析后的不可变快照。

use crate::model::{InstanceScope, RunMode};
use serde::{Deserialize, Serialize};

/// 目录库 `agent_instances` 行:规范化字段(敏感内容在版本行密文引用里)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInstance {
    /// 稳定字符串 ID(instance_key)。
    pub id: String,
    pub name: String,
    pub agent_type: String,
    pub scope: InstanceScope,
    pub current_version: i64,
    pub enabled: bool,
}

/// 创建/更新用的草案;更新会生成新的不可变版本行。
#[derive(Debug, Clone)]
pub struct AgentInstanceDraft {
    pub name: String,
    pub agent_type: String,
    pub scope: InstanceScope,
    pub enabled: bool,
    pub run_mode: RunMode,
    pub executable: String,
    pub argv: Vec<String>,
    /// 非敏感环境变量;Secret 只通过 `sealed_secret_ids` 引用。
    pub env: Vec<(String, String)>,
    /// Agent Type 专属配置(JSON,内容由插件 Schema 约定)。
    pub config: serde_json::Value,
    /// 执行契约:输入注入、完成检测与结果提取配置(JSON;
    /// 结构由 `agent_adapter::ExecutionContract` 解析)。
    pub execution_contract: serde_json::Value,
    /// sealed Secret 引用(密文 ID,不含明文)。
    pub sealed_secret_ids: Vec<String>,
}

impl AgentInstanceDraft {
    /// 基础校验:核心字段非空。结构化契约校验由 Agent Adapter 的
    /// `validate` 负责(插件知道自己声明了哪些键)。
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("实例名称不能为空".into());
        }
        if self.agent_type.trim().is_empty() {
            return Err("agent_type 不能为空".into());
        }
        if self.executable.trim().is_empty() {
            return Err("可执行文件不能为空".into());
        }
        Ok(())
    }
}

/// 不可变版本行内容(整体序列化进 `agent_instance_versions.config_json`)。
/// `instance_id`/`version`/`created_at` 是行级元数据,读取时由存储层回填,
/// 不参与 payload 序列化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInstanceVersion {
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub version: i64,
    /// 版本固定时的显示名/类型:固定历史版本快照必须还原当时的值,
    /// 不受实例行后续编辑影响。
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub agent_type: String,
    pub run_mode: RunMode,
    pub executable: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub config: serde_json::Value,
    pub execution_contract: serde_json::Value,
    pub sealed_secret_ids: Vec<String>,
    #[serde(default)]
    pub created_at: String,
}

/// 解析后的不可变快照:用户配置(+ 可选项目覆盖)合并的结果。
/// 启动、Revision 冻结与离散会话都只消费快照,不再回读可变行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInstanceSnapshot {
    pub id: String,
    pub name: String,
    pub agent_type: String,
    pub version: i64,
    pub enabled: bool,
    pub run_mode: RunMode,
    pub executable: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub config: serde_json::Value,
    pub execution_contract: serde_json::Value,
    pub sealed_secret_ids: Vec<String>,
}

impl AgentInstanceSnapshot {
    /// 从行 + 版本构造(不含项目覆盖)。
    /// 名称/类型取版本行固定值(编辑实例只影响下一次启动,
    /// 旧版本快照不随实例行变化);`enabled` 是行级开关,取当前值。
    pub fn resolve(
        instance: &AgentInstance,
        version: &AgentInstanceVersion,
    ) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: instance.id.clone(),
            name: if version.name.is_empty() {
                instance.name.clone()
            } else {
                version.name.clone()
            },
            agent_type: if version.agent_type.is_empty() {
                instance.agent_type.clone()
            } else {
                version.agent_type.clone()
            },
            version: version.version,
            enabled: instance.enabled,
            run_mode: version.run_mode,
            executable: version.executable.clone(),
            argv: version.argv.clone(),
            env: version.env.clone(),
            config: version.config.clone(),
            execution_contract: version.execution_contract.clone(),
            sealed_secret_ids: version.sealed_secret_ids.clone(),
        }
    }

    /// 应用项目覆盖:只有显式声明的键参与合并。
    /// - `argv`:声明则整体替换;
    /// - `env`:按键覆盖,未覆盖的用户键保留;
    /// - `config`:对象浅合并,声明的顶层键覆盖;
    ///
    /// 其余字段(名称、可执行文件、Secret 引用等)永不覆盖。
    pub fn apply_overrides(mut self, overrides: &AgentInstanceOverrides) -> AgentInstanceSnapshot {
        if let Some(argv) = &overrides.argv {
            self.argv = argv.clone();
        }
        if let Some(env) = &overrides.env {
            for (k, v) in env {
                self.env.retain(|(ek, _)| ek != k);
                self.env.push((k.clone(), v.clone()));
            }
        }
        if let Some(config) = &overrides.config {
            self.config = merge_json_objects(self.config.clone(), config);
        }
        self
    }
}

/// 对象浅合并(override 的顶层键覆盖,其余保留);任一侧不是对象则整体替换。
fn merge_json_objects(base: serde_json::Value, overrides: &serde_json::Value) -> serde_json::Value {
    match (base, overrides) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                b.insert(k.clone(), v.clone());
            }
            serde_json::Value::Object(b)
        }
        (_, o) => o.clone(),
    }
}

/// 项目覆盖声明:只有 `Some` 的键会覆盖用户配置。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentInstanceOverrides {
    pub argv: Option<Vec<String>>,
    pub env: Option<Vec<(String, String)>>,
    pub config: Option<serde_json::Value>,
}

impl AgentInstanceOverrides {
    pub fn is_empty(&self) -> bool {
        self.argv.is_none() && self.env.is_none() && self.config.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: "inst_x".into(),
            name: "n".into(),
            agent_type: "generic-command".into(),
            version: 1,
            enabled: true,
            run_mode: RunMode::OneShot,
            executable: "agent.exe".into(),
            argv: vec!["--a".into()],
            env: vec![("A".into(), "1".into()), ("B".into(), "2".into())],
            config: serde_json::json!({ "x": 1, "y": 2 }),
            execution_contract: serde_json::json!({}),
            sealed_secret_ids: vec![],
        }
    }

    #[test]
    fn overrides_touch_only_declared_keys() {
        let s = sample().apply_overrides(&AgentInstanceOverrides {
            env: Some(vec![("B".into(), "9".into())]),
            config: Some(serde_json::json!({ "y": 8 })),
            argv: None,
        });
        assert_eq!(s.argv, vec!["--a".to_string()]);
        assert_eq!(
            s.env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "9".to_string())
            ]
        );
        assert_eq!(s.config["x"], 1);
        assert_eq!(s.config["y"], 8);
        assert_eq!(s.name, "n");
    }

    #[test]
    fn empty_overrides_are_noop() {
        let s = sample();
        let s2 = s
            .clone()
            .apply_overrides(&AgentInstanceOverrides::default());
        assert_eq!(s, s2);
    }
}
