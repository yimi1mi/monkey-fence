//! 生产 ExecutorEnv(#93):mf-installer executor 的真实进程注入。
//!
//! `run_structured`/`probe_version` 经 `std::process::Command` 直启
//! (永不过 shell);下载类 v1 未接(明确报错,verified-download 留 v2);
//! catalog 恒零写入(安装幂等,检测以 PATH 事实为准)。

use std::io::Read;
use std::path::Path;
use std::process::Command;

use mf_installer::executor::ExecutorEnv;

pub struct OsExecutorEnv;

impl OsExecutorEnv {
    fn spawn_and_capture(
        program: &str,
        argv: &[String],
        cwd: Option<&Path>,
    ) -> Result<(i32, Vec<u8>), String> {
        let mut command = Command::new(program);
        command.args(argv);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        let output = command
            .output()
            .map_err(|error| format!("启动 {program} 失败:{error}"))?;
        // stdout+stderr 合并(截断到 64KB,防巨量输出拖垮内存)
        let mut combined = output.stdout;
        combined.extend_from_slice(&output.stderr);
        combined.truncate(64 * 1024);
        Ok((output.status.code().unwrap_or(-1), combined))
    }
}

impl ExecutorEnv for OsExecutorEnv {
    fn run_structured(
        &mut self,
        program: &str,
        argv: &[String],
        cwd: Option<&Path>,
    ) -> Result<(i32, Vec<u8>), String> {
        Self::spawn_and_capture(program, argv, cwd)
    }

    fn download(&mut self, _url: &str, _frozen_host: &str) -> Result<Vec<u8>, String> {
        Err("verified-download 尚未接入 web 安装面(v2;当前用包管理器安装)".into())
    }

    fn publish(&mut self, target: &Path, bytes: &[u8]) -> Result<(), String> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败:{e}"))?;
        }
        let temp = target.with_extension("part");
        std::fs::write(&temp, bytes).map_err(|e| format!("写入失败:{e}"))?;
        std::fs::rename(&temp, target).map_err(|e| format!("原子发布失败:{e}"))
    }

    fn probe_version(
        &mut self,
        executable: &Path,
        version_argv: &[String],
    ) -> Result<String, String> {
        let program = executable
            .to_str()
            .ok_or_else(|| "可执行路径非 UTF-8".to_string())?;
        let (code, output) = Self::spawn_and_capture(program, version_argv, None)?;
        if code != 0 {
            return Err(format!("版本探测退出码 {code}"));
        }
        Ok(String::from_utf8_lossy(&output).trim().to_string())
    }

    fn cleanup_staging(&mut self, staging: &Path) {
        let _ = std::fs::remove_dir_all(staging);
    }

    fn file_sha256(&mut self, path: &Path) -> Result<String, String> {
        let mut file = std::fs::File::open(path).map_err(|e| format!("打开失败:{e}"))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .map_err(|e| format!("读取失败:{e}"))?;
        use sha2::{Digest, Sha256};
        Ok(format!("{:x}", Sha256::digest(&buffer)))
    }
}

/// 内置安装 recipe(#93:公开包名;catalog 恒零写入)。
pub struct CliRecipe {
    pub agent_type: &'static str,
    pub package: &'static str,
    pub display: &'static str,
}

pub const RECIPES: &[CliRecipe] = &[
    CliRecipe {
        agent_type: "codex",
        package: "@openai/codex",
        display: "OpenAI Codex CLI",
    },
    CliRecipe {
        agent_type: "claude",
        package: "@anthropic-ai/claude-code",
        display: "Anthropic Claude Code",
    },
    CliRecipe {
        agent_type: "gemini",
        package: "@google/gemini-cli",
        display: "Google Gemini CLI",
    },
    CliRecipe {
        agent_type: "qwen",
        package: "@qwen-code/qwen-code",
        display: "Qwen Code CLI",
    },
];

/// 探测可用的包管理器(npm 优先;winget 备选)。Windows 上 npm 是
/// .cmd(Rust Command 不解析 PATHEXT,须显式 npm.cmd)。
pub fn detect_package_manager() -> Option<(&'static str, Vec<String>)> {
    // npm install -g <pkg>
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    if Command::new(npm)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return Some(("npm", vec!["install".into(), "-g".into()]));
    }
    if Command::new("winget")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return Some(("winget", vec!["install".into(), "--silent".into()]));
    }
    None
}
