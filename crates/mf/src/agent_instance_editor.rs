//! Agent Instance 编辑器状态(UI 计划 Task 1)。
//!
//! 纯状态模型(独立于 GPUI 渲染):字段编辑、结构校验、
//! Secret 引用管理(明文永不进入编辑器)与草案导出。

use mf_agent::agent_instance::{AgentInstanceDraft, AgentInstanceSnapshot};
use mf_agent::{InstanceScope, RunMode};

/// Agent Type 的 UI 投影(来自插件贡献 + PATH 检测)。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTypeInfo {
    pub id: String,
    /// 完整贡献 ID(`publisher.plugin.agent_type`);身份展示与 pin 解析使用。
    pub full_contribution_id: String,
    pub name: String,
    pub plugin_name: String,
    /// 贡献插件版本与内容哈希(实例页/插件页的活动 pin 展示)。
    pub plugin_version: String,
    pub content_hash: String,
    /// CLI 是否检测到(缺失时可见但不可保存实例)。
    pub detected: bool,
    pub supports_isolated_config: bool,
    pub default_command: String,
    pub adapter: String,
    pub modes: Vec<RunMode>,
}

/// 结构校验错误(机器码 + 人类可读信息)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorValidationError {
    pub code: &'static str,
    pub message: String,
}

fn err(code: &'static str, message: impl Into<String>) -> EditorValidationError {
    EditorValidationError {
        code,
        message: message.into(),
    }
}

/// 实例编辑器状态。
#[derive(Debug, Clone)]
pub struct AgentInstanceEditorState {
    pub info: AgentTypeInfo,
    pub name: String,
    pub executable: String,
    /// argv 按空白切分(支持引号?第一版简单切分,与 UI 提示一致)。
    pub argv_text: String,
    /// 每行一个 `KEY=VALUE`。
    pub env_text: String,
    /// Secret 引用(id 列表;明文由 Secret Store 管理,不进编辑器)。
    pub secret_refs: Vec<String>,
    pub run_mode: RunMode,
    /// 编辑已有实例时固定其 id。
    pub editing_instance_id: Option<String>,
}

impl AgentInstanceEditorState {
    pub fn new(info: AgentTypeInfo) -> AgentInstanceEditorState {
        AgentInstanceEditorState {
            executable: info.default_command.clone(),
            run_mode: info.modes.first().copied().unwrap_or(RunMode::Interactive),
            info,
            name: String::new(),
            argv_text: String::new(),
            env_text: String::new(),
            secret_refs: Vec::new(),
            editing_instance_id: None,
        }
    }

    /// 从既有实例装载(编辑路径)。
    pub fn from_instance(info: AgentTypeInfo, snapshot: &AgentInstanceSnapshot) -> Self {
        let mut state = Self::new(info);
        state.name = snapshot.name.clone();
        state.executable = snapshot.executable.clone();
        state.argv_text = snapshot.argv.join(" ");
        state.env_text = snapshot
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.secret_refs = snapshot.sealed_secret_ids.clone();
        state.run_mode = snapshot.run_mode;
        state.editing_instance_id = Some(snapshot.id.clone());
        state
    }

    pub fn set_name(&mut self, name: &str) {
        self.name = name.to_string();
    }

    pub fn set_executable(&mut self, exe: &str) {
        self.executable = exe.to_string();
    }

    pub fn set_argv(&mut self, argv: &str) {
        self.argv_text = argv.to_string();
    }

    pub fn set_env_lines(&mut self, env: &str) {
        self.env_text = env.to_string();
    }

    pub fn add_secret_ref(&mut self, secret_id: &str) {
        if !self.secret_refs.iter().any(|s| s == secret_id) {
            self.secret_refs.push(secret_id.to_string());
        }
    }

    pub fn remove_secret_ref(&mut self, secret_id: &str) {
        self.secret_refs.retain(|s| s != secret_id);
    }

    /// Secret 展示:引用 id + 掩码(无明文)。
    pub fn secret_display(&self) -> String {
        self.secret_refs
            .iter()
            .map(|id| format!("{id} = ••••"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 结构校验:类型可用性 + 核心字段 + env 格式。
    pub fn validation(&self) -> Vec<EditorValidationError> {
        let mut errors = Vec::new();
        if !self.info.detected {
            errors.push(err(
                "cli-not-detected",
                format!("未检测到 {} 命令,无法保存实例", self.info.default_command),
            ));
        }
        if self.name.trim().is_empty() {
            errors.push(err("name-required", "实例名称不能为空"));
        }
        if self.executable.trim().is_empty() {
            errors.push(err("executable-required", "可执行文件不能为空"));
        }
        for (_line, parsed) in parse_env(&self.env_text) {
            if parsed.is_err() {
                errors.push(err(
                    "env-invalid",
                    format!("环境变量行无效(需要 KEY=VALUE):{_line}"),
                ));
            }
        }
        errors
    }

    pub fn can_save(&self) -> bool {
        self.validation().is_empty()
    }

    /// 导出目录库草案。
    pub fn to_draft(
        &self,
        scope: InstanceScope,
        project_key: Option<String>,
    ) -> AgentInstanceDraft {
        AgentInstanceDraft {
            name: self.name.trim().to_string(),
            agent_type: self.info.id.clone(),
            scope,
            project_key,
            enabled: true,
            run_mode: self.run_mode,
            executable: self.executable.trim().to_string(),
            argv: self
                .argv_text
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            env: parse_env(&self.env_text)
                .into_iter()
                .filter_map(|(_bad, kv)| kv.ok().flatten())
                .collect(),
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({
                "input": "argv",
                "completion": if self.run_mode == RunMode::OneShot { "process-exit" } else { "manual" },
            }),
            sealed_secret_ids: self.secret_refs.clone(),
        }
    }

    /// 一次性启动快照(临时实例):不落目录库,直接以当前编辑字段启动。
    /// `instance_key` 是临时标识(不与目录库实例 ID 冲突)。
    pub fn to_launch_snapshot(&self, instance_key: &str) -> AgentInstanceSnapshot {
        let draft = self.to_draft(InstanceScope::User, None);
        AgentInstanceSnapshot {
            id: instance_key.to_string(),
            name: draft.name,
            agent_type: draft.agent_type,
            version: 1,
            enabled: true,
            run_mode: draft.run_mode,
            executable: draft.executable,
            argv: draft.argv,
            env: draft.env,
            config: draft.config,
            execution_contract: draft.execution_contract,
            sealed_secret_ids: draft.sealed_secret_ids,
        }
    }
}

/// 解析 env 行:返回 (原文, 解析结果);无效行 Err(原文)。
fn parse_env(text: &str) -> Vec<(String, Result<Option<(String, String)>, String>)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => (
                line.to_string(),
                Ok(Some((k.trim().to_string(), v.to_string()))),
            ),
            _ => (line.to_string(), Err(line.to_string())),
        })
        .collect()
}
