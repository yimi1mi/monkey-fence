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

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            workers: default_workers(),
            max_iterations: default_max_iters(),
            max_failures: default_max_failures(),
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
}

fn default_font_family() -> String {
    "Consolas".into()
}
fn default_font_size() -> f32 {
    13.0
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            font_family: default_font_family(),
            font_size: default_font_size(),
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
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
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
}
