//! Generic Command Adapter:最简 Agent Adapter 实现 ——
//! 任意可执行文件 + argv 直启,不经 Shell(设计 §4.1 首批内置类型)。
//!
//! 不支持进程级隔离配置:实例请求 `config_files` 时必须显式失败,
//! 不允许静默改写真实 CLI 全局配置。

use anyhow::Result;
use mf_agent::agent_adapter::{
    resolve_secret_env, AgentAdapter, CompletionDetector, CompletionObservation, ExecutionContract,
    HandoffDraft, InputInjection, LaunchContext, LaunchPlan, ProcessObservation,
};
use mf_agent::agent_instance::AgentInstanceSnapshot;
use std::path::PathBuf;

pub const ADAPTER_ID: &str = "generic-command";
const PROMPT_FILE_NAME: &str = "prompt.txt";

pub struct GenericCommandAdapter;

impl GenericCommandAdapter {
    pub fn new() -> GenericCommandAdapter {
        GenericCommandAdapter
    }
}

impl Default for GenericCommandAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for GenericCommandAdapter {
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
        let contract = ExecutionContract::parse(snapshot)?;
        if contract.use_shell && !ctx.grants_shell {
            anyhow::bail!("Shell 模式需要插件 capabilities.shell 授权,默认必须直接进程启动");
        }
        // 隔离配置是显式能力:generic-command 不支持,宁可不启动也不改写全局
        if snapshot.config.get("config_files").is_some() {
            anyhow::bail!(
                "generic-command 不支持进程级隔离配置(config_files);\
                 需要 CLI 专属配置请使用 claude-code / codex 适配器"
            );
        }

        let (secret_env, redactions) = resolve_secret_env(snapshot, ctx)?;

        let mut argv = snapshot.argv.clone();
        let mut temp_files = Vec::new();
        let input = match (&contract.input, ctx.prompt.as_deref()) {
            (_, None) => InputInjection::Argv(String::new()),
            (mode, Some(prompt)) => match mode {
                mf_agent::agent_adapter::InputMode::Argv => {
                    argv.push(prompt.to_string());
                    InputInjection::Argv(prompt.to_string())
                }
                mf_agent::agent_adapter::InputMode::Stdin => {
                    InputInjection::Stdin(prompt.as_bytes().to_vec())
                }
                mf_agent::agent_adapter::InputMode::PromptFile => {
                    let path = ctx.run_temp.join(PROMPT_FILE_NAME);
                    temp_files.push(mf_agent::agent_adapter::TempFileSpec {
                        path: path.clone(),
                        contents: prompt.as_bytes().to_vec(),
                    });
                    InputInjection::PromptFile(path)
                }
            },
        };

        let completion = match contract.completion {
            mf_agent::agent_adapter::CompletionMode::ProcessExit => CompletionDetector::ProcessExit,
            mf_agent::agent_adapter::CompletionMode::StdoutMarker => {
                if contract.stdout_marker.is_empty() {
                    anyhow::bail!("stdout-marker 完成检测缺少 stdout_marker 标记");
                }
                CompletionDetector::StdoutMarker(contract.stdout_marker.clone())
            }
            mf_agent::agent_adapter::CompletionMode::ResultFile => {
                if contract.result_file.is_empty() {
                    anyhow::bail!("result-file 完成检测缺少 result_file 文件名");
                }
                CompletionDetector::ResultFile(ctx.run_temp.join(&contract.result_file))
            }
            mf_agent::agent_adapter::CompletionMode::Manual => CompletionDetector::Manual,
        };

        Ok(LaunchPlan {
            executable: PathBuf::from(&snapshot.executable),
            argv,
            env: snapshot.env.clone(),
            secret_env,
            cwd: Some(ctx.workdir.clone()),
            temp_files,
            input,
            completion,
            redactions,
            uses_shell: contract.use_shell,
        })
    }

    fn observe(
        &self,
        snapshot: &AgentInstanceSnapshot,
        obs: &ProcessObservation,
    ) -> CompletionObservation {
        let Ok(contract) = ExecutionContract::parse(snapshot) else {
            return CompletionObservation::Failed("执行契约非法".into());
        };
        match contract.completion {
            mf_agent::agent_adapter::CompletionMode::ProcessExit => {
                if obs.exited {
                    CompletionObservation::Completed
                } else {
                    CompletionObservation::Running
                }
            }
            mf_agent::agent_adapter::CompletionMode::StdoutMarker => {
                if obs.stdout_tail.contains(&contract.stdout_marker) {
                    CompletionObservation::Completed
                } else if obs.exited {
                    CompletionObservation::Failed(format!(
                        "进程已退出但未出现完成标记 `{}`",
                        contract.stdout_marker
                    ))
                } else {
                    CompletionObservation::Running
                }
            }
            mf_agent::agent_adapter::CompletionMode::ResultFile => {
                if obs.result_file.is_some() {
                    CompletionObservation::Completed
                } else if obs.exited {
                    CompletionObservation::Failed("进程已退出但结果文件未出现".into())
                } else {
                    CompletionObservation::Running
                }
            }
            mf_agent::agent_adapter::CompletionMode::Manual => CompletionObservation::Running,
        }
    }

    fn extract_handoff(&self, obs: &ProcessObservation) -> Result<HandoffDraft> {
        // 结果文件优先:JSON 里的 summary/output 直接进入 Handoff
        if let Some(bytes) = &obs.result_file {
            let parsed: serde_json::Value = serde_json::from_slice(bytes)
                .map_err(|e| anyhow::anyhow!("结果文件不是合法 JSON: {e}"))?;
            let mut handoff = HandoffDraft {
                status: "completed".into(),
                ..Default::default()
            };
            if let Some(s) = parsed.get("summary").and_then(|v| v.as_str()) {
                handoff.summary = s.to_string();
            }
            if let Some(o) = parsed.get("output") {
                handoff.output = o.clone();
            }
            if let Some(a) = parsed.get("artifacts").and_then(|v| v.as_array()) {
                handoff.artifacts = a
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
            }
            return Ok(handoff);
        }
        // 无结果文件:stdout 尾部一行作为摘要,不复制完整输出
        let summary = obs
            .stdout_tail
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(200)
            .collect::<String>();
        Ok(HandoffDraft {
            status: if obs.exited { "completed" } else { "running" }.into(),
            summary,
            ..Default::default()
        })
    }
}
