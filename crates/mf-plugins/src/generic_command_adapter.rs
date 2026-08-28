//! Generic Command Adapter:最简 Agent Adapter 实现 ——
//! 任意可执行文件 + argv 直启,不经 Shell(设计 §4.1 首批内置类型)。
//!
//! 同时承载 CLI 适配器的共享编译逻辑(`compile_cli_launch` / 契约观察),
//! Claude Code / Codex 隔离适配器在其上叠加 run-temp 配置目录注入。

use anyhow::Result;
use mf_agent::agent_adapter::{
    resolve_secret_env, AgentAdapter, CompletionDetector, CompletionObservation, ExecutionContract,
    HandoffDraft, InputInjection, LaunchContext, LaunchPlan, ProcessObservation, TempFileSpec,
};
use mf_agent::agent_instance::AgentInstanceSnapshot;
use std::path::{Path, PathBuf};

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

/// 进程级隔离配置声明:适配器把 `env_name` 指向 run-temp 下的
/// `subdir` 目录,并在启动前物化快照声明的 `config_files`。
pub(crate) struct IsolationSpec {
    pub env_name: &'static str,
    pub subdir: &'static str,
}

/// CLI 适配器共享的编译逻辑。
/// `isolation = None`:不支持隔离配置,实例请求 `config_files` 时显式失败,
/// 绝不静默改写真实 CLI 全局配置。
pub(crate) fn compile_cli_launch(
    snapshot: &AgentInstanceSnapshot,
    ctx: &LaunchContext,
    isolation: Option<&IsolationSpec>,
) -> Result<LaunchPlan> {
    let contract = ExecutionContract::parse(snapshot)?;
    if contract.use_shell && !ctx.grants_shell {
        anyhow::bail!("Shell 模式需要插件 capabilities.shell 授权,默认必须直接进程启动");
    }

    let mut env = snapshot.env.clone();
    let mut temp_files = Vec::new();
    match isolation {
        Some(iso) => {
            let dir = ctx.run_temp.join(iso.subdir);
            temp_files.extend(config_file_specs(snapshot, &dir)?);
            env.push((iso.env_name.to_string(), dir.to_string_lossy().to_string()));
        }
        None => {
            if snapshot.config.get("config_files").is_some() {
                anyhow::bail!(
                    "该 Agent Type 不支持进程级隔离配置(config_files);\
                     需要 CLI 专属配置请使用 claude-code / codex 适配器"
                );
            }
        }
    }

    let secret_env = resolve_secret_env(snapshot, ctx)?;

    let mut argv = snapshot.argv.clone();
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
                temp_files.push(TempFileSpec {
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
            let result_file = safe_relative_path(&contract.result_file, "结果文件")?;
            CompletionDetector::ResultFile(ctx.run_temp.join(result_file))
        }
        mf_agent::agent_adapter::CompletionMode::Manual => CompletionDetector::Manual,
    };

    Ok(LaunchPlan {
        executable: PathBuf::from(&snapshot.executable),
        argv,
        env,
        secret_env,
        cwd: Some(ctx.workdir.clone()),
        temp_files,
        input,
        completion,
        uses_shell: contract.use_shell,
    })
}

/// 编译快照声明的 `config_files` 为待物化规格。
/// Adapter 保持纯函数;Runtime Host 负责统一写入。
fn config_file_specs(snapshot: &AgentInstanceSnapshot, dir: &Path) -> Result<Vec<TempFileSpec>> {
    let mut specs = Vec::new();
    let Some(files) = snapshot
        .config
        .get("config_files")
        .and_then(|v| v.as_object())
    else {
        return Ok(specs);
    };
    for (rel, value) in files {
        let rel_path = safe_relative_path(rel, "配置文件")?;
        let target = dir.join(rel_path);
        let contents = match value {
            serde_json::Value::String(s) => s.as_bytes().to_vec(),
            other => serde_json::to_vec_pretty(other)?,
        };
        specs.push(TempFileSpec {
            path: target,
            contents,
        });
    }
    Ok(specs)
}

fn safe_relative_path<'a>(value: &'a str, label: &str) -> Result<&'a Path> {
    let path = Path::new(value);
    if path.is_absolute() {
        anyhow::bail!("{label}路径必须是相对路径(不允许逃逸运行目录): {value}");
    }
    if path
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        anyhow::bail!("{label}路径不允许 `..`(路径逃逸): {value}");
    }
    Ok(path)
}

/// CLI 适配器共享的完成观察(按执行契约判定)。
pub(crate) fn observe_contract(
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

/// CLI 适配器共享的 Handoff 提取(结果文件 JSON 优先,stdout 尾行兜底)。
pub(crate) fn extract_handoff_from_obs(obs: &ProcessObservation) -> Result<HandoffDraft> {
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
        compile_cli_launch(snapshot, ctx, None)
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
