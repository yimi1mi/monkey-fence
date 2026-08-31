use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Mock,
    Openai,
    Anthropic,
}

impl ProviderKind {
    pub fn kind_str(&self) -> &'static str {
        match self {
            ProviderKind::Mock => "mock",
            ProviderKind::Openai => "openai",
            ProviderKind::Anthropic => "anthropic",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default = "default_workers")]
    pub workers: usize,
    /// worker 工具循环的最大轮数
    #[serde(default = "default_max_iters")]
    pub max_iterations: usize,
    /// 连续失败几次后熔断
    #[serde(default = "default_max_failures")]
    pub max_failures: i32,
    /// 全局(跨项目)Agent 并发上限
    #[serde(default = "default_global_concurrency")]
    pub global_concurrency: usize,
    /// 单项目 Agent 并发上限
    #[serde(default = "default_per_project_concurrency")]
    pub per_project_concurrency: usize,
}

fn default_workers() -> usize {
    2
}
fn default_max_iters() -> usize {
    24
}
fn default_max_failures() -> i32 {
    3
}
fn default_global_concurrency() -> usize {
    4
}
fn default_per_project_concurrency() -> usize {
    2
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            workers: default_workers(),
            max_iterations: default_max_iters(),
            max_failures: default_max_failures(),
            global_concurrency: default_global_concurrency(),
            per_project_concurrency: default_per_project_concurrency(),
        }
    }
}

/// 编辑器外观(全局字体等),设置界面可改
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditorConfig {
    #[serde(default = "default_font_family")]
    pub font_family: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// night-voyage | morning-mist；未知值回退 night-voyage。
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_font_family() -> String {
    "Consolas".into()
}
fn default_font_size() -> f32 {
    13.0
}
fn default_theme() -> String {
    "night-voyage".into()
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            font_family: default_font_family(),
            font_size: default_font_size(),
            theme: default_theme(),
        }
    }
}

/// 终端矩阵(驾驶舱)spawn 配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalConfig {
    /// 终端命令(如 "codex" / "powershell");留空 = 平台默认(cmd/COMSPEC)
    #[serde(default)]
    pub command: Option<String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self { command: None }
    }
}

/// 智能体设置页对应的全局开关。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentsConfig {
    /// yolo = 附加 permission_args(自动批准);manual = 不附加,由用户在终端里手动批准
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
    /// 状态钩子总开关(关闭时不写入任何本地 Agent 配置)
    #[serde(default = "default_true")]
    pub hooks_enabled: bool,
    /// 自动生成标签(会话)标题
    #[serde(default = "default_true")]
    pub auto_title: bool,
    /// Agent 工作时保持唤醒(SetThreadExecutionState)
    #[serde(default = "default_true")]
    pub keep_awake: bool,
    /// 默认智能体:Auto / blank-terminal / 已启用且检测到的 profile id
    #[serde(default)]
    pub default_agent: String,
}

/// 插件贡献实例的用户配置。核心不理解字段语义，只按完整贡献 ID
/// 保存插件声明的键值；默认值仍由插件清单提供，避免插件升级时把默认值
/// 复制进用户配置。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstanceConfig {
    #[serde(default)]
    pub values: HashMap<String, String>,
}

fn default_permission_mode() -> String {
    "yolo".into()
}
fn default_true() -> bool {
    true
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            permission_mode: default_permission_mode(),
            hooks_enabled: true,
            auto_title: true,
            keep_awake: true,
            default_agent: String::new(), // 空 = Auto
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// 角色 → 提供方名称;缺省为 mock
    #[serde(default)]
    pub roles: HashMap<String, String>,
    #[serde(default)]
    pub engine: EngineConfig,
    #[serde(default)]
    pub editor: EditorConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    /// 完整贡献 ID → 单一实例配置。当前不修改 Git/P4/Agent 的全局配置文件。
    #[serde(default)]
    pub plugin_instances: HashMap<String, PluginInstanceConfig>,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = HashMap::new();
        providers.insert(
            "mock".into(),
            ProviderConfig {
                kind: ProviderKind::Mock,
                base_url: String::new(),
                api_key: String::new(),
                model: String::new(),
            },
        );
        let mut roles = HashMap::new();
        roles.insert("planner".into(), "mock".into());
        roles.insert("worker".into(), "mock".into());
        roles.insert("reviewer".into(), "mock".into());
        Self {
            providers,
            roles,
            engine: EngineConfig::default(),
            editor: EditorConfig::default(),
            terminal: TerminalConfig::default(),
            agents: AgentsConfig::default(),
            plugin_instances: HashMap::new(),
        }
    }
}

impl Config {
    pub fn config_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".monkeyfence")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            let cfg = Self::default();
            cfg.save_example()?;
            return Ok(cfg);
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| "parse config.toml")?;
        Ok(cfg)
    }

    /// 首次运行写入示例配置(含注释模板)
    fn save_example(&self) -> Result<()> {
        let dir = Self::config_dir();
        std::fs::create_dir_all(&dir)?;
        let template = r#"# MonkeyFence 配置
# 提供方:kind = mock | openai(OpenAI 兼容) | anthropic
[providers.mock]
kind = "mock"

# 示例:智谱 GLM(OpenAI 兼容)
# [providers.glm]
# kind = "openai"
# base_url = "https://open.bigmodel.cn/api/paas/v4"
# api_key = "your-key"
# model = "glm-4.6"

# 示例:Anthropic
# [providers.claude]
# kind = "anthropic"
# base_url = "https://api.anthropic.com"
# api_key = "sk-..."
# model = "claude-sonnet-4-5"

# 角色 → 提供方
[roles]
planner = "mock"
worker = "mock"

[engine]
workers = 2
max_iterations = 24
max_failures = 3
"#;
        std::fs::write(Self::config_path(), template)?;
        Ok(())
    }

    /// 把当前配置写回 config.toml(设置界面“保存”用)
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(Self::config_dir())?;
        let text = toml::to_string_pretty(self).context("serialize config")?;
        std::fs::write(Self::config_path(), text)
            .with_context(|| format!("write {}", Self::config_path().display()))?;
        Ok(())
    }

    pub fn provider_for_role(&self, role: &str) -> ProviderConfig {
        let name = self
            .roles
            .get(role)
            .cloned()
            .unwrap_or_else(|| "mock".into());
        self.providers
            .get(&name)
            .cloned()
            .unwrap_or(ProviderConfig {
                kind: ProviderKind::Mock,
                base_url: String::new(),
                api_key: String::new(),
                model: String::new(),
            })
    }

    /// 读取插件实例字段；用户未覆盖时返回插件清单给出的默认值。
    pub fn plugin_value(&self, contribution_id: &str, field_id: &str, default: &str) -> String {
        self.plugin_instances
            .get(contribution_id)
            .and_then(|instance| instance.values.get(field_id))
            .cloned()
            .unwrap_or_else(|| default.to_string())
    }

    /// 写入单一插件实例字段。配置只属于 MonkeyFence，不触碰工具的全局配置。
    pub fn set_plugin_value(
        &mut self,
        contribution_id: impl Into<String>,
        field_id: impl Into<String>,
        value: impl Into<String>,
    ) {
        self.plugin_instances
            .entry(contribution_id.into())
            .or_default()
            .values
            .insert(field_id.into(), value.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_instance_values_round_trip_without_tool_global_state() {
        let mut config = Config::default();
        config.editor.theme = "morning-mist".into();
        config.set_plugin_value("monkeyfence.vcs.git", "executable", "C:/Git/bin/git.exe");
        let text = toml::to_string(&config).unwrap();
        let restored: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            restored.plugin_value("monkeyfence.vcs.git", "executable", "git"),
            "C:/Git/bin/git.exe"
        );
        assert_eq!(
            restored.plugin_value("monkeyfence.vcs.p4", "executable", "p4"),
            "p4"
        );
        assert_eq!(restored.editor.theme, "morning-mist");
    }
}
