//! Claude Code Adapter:每次运行独立的 `CLAUDE_CONFIG_DIR`。
//!
//! Claude Code 官方以 `CLAUDE_CONFIG_DIR` 环境变量指定配置目录
//! (未设置时默认 `~/.claude`;见官方文档 settings 页的
//! "Environment variables - CLAUDE_CONFIG_DIR")。
//! 本适配器只把该变量指向本次 Agent Run 的 run-temp 子目录,
//! 并物化实例快照声明的配置;绝不读写用户真实 `~/.claude`。

use crate::generic_command_adapter::{
    compile_cli_launch, extract_handoff_from_obs, observe_contract, IsolationSpec,
};
use anyhow::Result;
use mf_agent::agent_adapter::{
    AgentAdapter, CompletionObservation, ExecutionContract, HandoffDraft, LaunchContext,
    LaunchPlan, ProcessObservation,
};
use mf_agent::agent_instance::AgentInstanceSnapshot;

pub const ADAPTER_ID: &str = "claude-code";
const CONFIG_ENV: &str = "CLAUDE_CONFIG_DIR";
const CONFIG_SUBDIR: &str = "claude";

pub struct ClaudeCodeAdapter;

impl ClaudeCodeAdapter {
    pub fn new() -> ClaudeCodeAdapter {
        ClaudeCodeAdapter
    }
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        ADAPTER_ID
    }

    fn validate(&self, snapshot: &AgentInstanceSnapshot) -> Vec<String> {
        let mut errors = Vec::new();
        if snapshot.executable.trim().is_empty() {
            errors.push("可执行文件不能为空".into());
        }
        if let Err(e) = ExecutionContract::parse(snapshot) {
            errors.push(e.to_string());
        }
        errors
    }

    fn compile_launch(
        &self,
        snapshot: &AgentInstanceSnapshot,
        ctx: &LaunchContext,
    ) -> Result<LaunchPlan> {
        compile_cli_launch(
            snapshot,
            ctx,
            Some(&IsolationSpec {
                env_name: CONFIG_ENV,
                subdir: CONFIG_SUBDIR,
            }),
        )
    }

    fn observe(
        &self,
        snapshot: &AgentInstanceSnapshot,
        obs: &ProcessObservation,
    ) -> CompletionObservation {
        observe_contract(snapshot, obs)
    }

    fn extract_handoff(&self, obs: &ProcessObservation) -> Result<HandoffDraft> {
        extract_handoff_from_obs(obs)
    }
}
