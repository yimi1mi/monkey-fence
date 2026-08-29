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
    /// manifest config_schema 声明的表单字段(编辑器真渲染 DeclarativeForm)。
    pub config_schema_fields: Vec<crate::declarative_form::FormField>,
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
    /// 作用域(User/Project)与项目键(Project 必填)。
    pub scope: InstanceScope,
    pub project_key: String,
    /// 启用开关(禁用的实例不参与分配/启动)。
    pub enabled: bool,
    /// 插件 config_schema 声明式表单(值并入实例 config)。
    config_form: crate::declarative_form::DeclarativeForm,
    /// 编辑已有实例时固定其 id。
    pub editing_instance_id: Option<String>,
}

impl AgentInstanceEditorState {
    pub fn new(info: AgentTypeInfo) -> AgentInstanceEditorState {
        let config_form =
            crate::declarative_form::DeclarativeForm::new(info.config_schema_fields.clone());
        AgentInstanceEditorState {
            executable: info.default_command.clone(),
            run_mode: info.modes.first().copied().unwrap_or(RunMode::Interactive),
            scope: InstanceScope::User,
            project_key: String::new(),
            enabled: true,
            info,
            name: String::new(),
            argv_text: String::new(),
            env_text: String::new(),
            secret_refs: Vec::new(),
            config_form,
            editing_instance_id: None,
        }
    }

    /// 从既有实例装载(编辑路径);scope/project_key/enabled 是行级
    /// 开关(不在版本快照里),由调用方从实例行带出。
    pub fn from_instance(
        info: AgentTypeInfo,
        snapshot: &AgentInstanceSnapshot,
        scope: InstanceScope,
        project_key: Option<&str>,
        enabled: bool,
    ) -> Self {
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
        state.scope = scope;
        state.project_key = project_key.unwrap_or_default().to_string();
        state.enabled = enabled;
        // 既有 config 值回填表单
        for field in state.config_form.fields().to_vec() {
            if let Some(value) = snapshot.config.get(&field.id).and_then(|v| v.as_str()) {
                state.config_form.set_value(&field.id, value);
            }
        }
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

    pub fn set_scope(&mut self, scope: InstanceScope) {
        self.scope = scope;
    }

    pub fn set_project_key(&mut self, key: &str) {
        self.project_key = key.to_string();
    }

    pub fn set_run_mode(&mut self, mode: RunMode) {
        self.run_mode = mode;
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            InstanceScope::User => InstanceScope::Project,
            InstanceScope::Project => InstanceScope::User,
        };
    }

    pub fn toggle_run_mode(&mut self) {
        self.run_mode = match self.run_mode {
            RunMode::Interactive => RunMode::OneShot,
            RunMode::OneShot => RunMode::Interactive,
        };
    }

    pub fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
    }

    /// 替换声明式表单(类型变化/Schema 加载完成后)。
    pub fn set_config_form(&mut self, form: crate::declarative_form::DeclarativeForm) {
        self.config_form = form;
    }

    pub fn config_form(&self) -> &crate::declarative_form::DeclarativeForm {
        &self.config_form
    }

    pub fn set_config_value(&mut self, id: &str, raw: &str) {
        self.config_form.set_value(id, raw);
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
        match (self.scope, self.project_key.trim()) {
            (InstanceScope::Project, "") => {
                errors.push(err(
                    "scope-project-key",
                    "Project 作用域必须填写 project_key",
                ));
            }
            (InstanceScope::User, k) if !k.is_empty() => {
                errors.push(err("scope-project-key", "User 作用域不能携带 project_key"));
            }
            _ => {}
        }
        for message in self.config_form.validation() {
            errors.push(err("config-schema", message));
        }
        errors
    }

    pub fn can_save(&self) -> bool {
        self.validation().is_empty()
    }

    /// 导出目录库草案(作用域/项目键/启用/运行模式取编辑器状态)。
    pub fn to_draft(&self) -> AgentInstanceDraft {
        let project_key = match self.scope {
            InstanceScope::Project => Some(self.project_key.trim().to_string()),
            InstanceScope::User => None,
        };
        AgentInstanceDraft {
            name: self.name.trim().to_string(),
            agent_type: self.info.id.clone(),
            scope: self.scope,
            project_key,
            enabled: self.enabled,
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
            config: self.config_form.to_json(),
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
        let draft = self.to_draft();
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
