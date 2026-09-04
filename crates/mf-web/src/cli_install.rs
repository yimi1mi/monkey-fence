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
    /// npm | pip(aider 走 pip;其余 npm)。
    pub prefer: &'static str,
}

pub const RECIPES: &[CliRecipe] = &[
    CliRecipe {
        agent_type: "agoragentic-acp",
        package: "agoragentic-mcp",
        display: "agoragentic-acp",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "auggie",
        package: "@augmentcode/auggie",
        display: "auggie",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "autohand",
        package: "@autohandai/autohand-acp",
        display: "autohand",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "claude-acp",
        package: "@agentclientprotocol/claude-agent-acp",
        display: "claude-acp",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "cline",
        package: "cline",
        display: "cline",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "codebuddy-code",
        package: "@tencent-ai/codebuddy-code",
        display: "codebuddy-code",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "codex-acp",
        package: "@agentclientprotocol/codex-acp",
        display: "codex-acp",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "deepagents",
        package: "deepagents-acp",
        display: "deepagents",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "dimcode",
        package: "dimcode",
        display: "dimcode",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "dirac",
        package: "dirac-cli",
        display: "dirac",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "factory-droid",
        package: "droid",
        display: "factory-droid",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "fast-agent",
        package: "fast-agent-acp",
        display: "fast-agent",
        prefer: "pip",
    },
    CliRecipe {
        agent_type: "gemini",
        package: "@google/gemini-cli",
        display: "gemini",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "github-copilot-cli",
        package: "@github/copilot",
        display: "github-copilot-cli",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "glm-acp-agent",
        package: "glm-acp-agent",
        display: "glm-acp-agent",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "grok-build",
        package: "@xai-official/grok",
        display: "grok-build",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "kilo",
        package: "@kilocode/cli",
        display: "kilo",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "minion-code",
        package: "minion-code",
        display: "minion-code",
        prefer: "pip",
    },
    CliRecipe {
        agent_type: "nova",
        package: "@compass-ai/nova",
        display: "nova",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "pi-acp",
        package: "pi-acp",
        display: "pi-acp",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "qoder",
        package: "@qoder-ai/qodercli",
        display: "qoder",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "qwen-code",
        package: "@qwen-code/qwen-code",
        display: "qwen-code",
        prefer: "npm",
    },
    CliRecipe {
        agent_type: "sigit",
        package: "@smbcloud/sigit",
        display: "sigit",
        prefer: "npm",
    },
];

/// 探测可用的包管理器(npm 优先;winget 备选)。Windows 上 npm 是
/// .cmd(Rust Command 不解析 PATHEXT,须显式 npm.cmd)。
pub fn detect_package_manager() -> Option<(&'static str, Vec<String>)> {
    detect_manager_named("npm")
        .or_else(|| detect_manager_named("pip"))
        .or_else(|| detect_manager_named("winget"))
}

/// 按 manager 名探测:返回 (名, 安装基参数)。
/// npm → install -g;pip → install;winget → install --silent。
fn detect_manager_named(name: &str) -> Option<(&'static str, Vec<String>)> {
    let program = match name {
        "npm" => {
            if cfg!(windows) {
                "npm.cmd"
            } else {
                "npm"
            }
        }
        "pip" => {
            if cfg!(windows) {
                "pip.exe"
            } else {
                "pip3"
            }
        }
        other => other,
    };
    let ok = Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        return None;
    }
    let argv = match name {
        "npm" => vec!["install".to_string(), "-g".to_string()],
        "pip" => vec!["install".to_string()],
        _ => vec!["install".to_string(), "--silent".to_string()],
    };
    Some((
        match name {
            "npm" => "npm",
            "pip" => "pip",
            _ => "winget",
        },
        argv,
    ))
}
