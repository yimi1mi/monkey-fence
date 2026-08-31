use anyhow::{anyhow, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Git 命令行环境。状态、diff 等仓库读取仍由 libgit2 完成；必须调用
/// 外部 Git 的操作统一经此对象，确保设置页选择的可执行文件真实生效。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCliConfig {
    pub executable: String,
}

impl Default for GitCliConfig {
    fn default() -> Self {
        Self {
            executable: "git".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GitCli {
    cwd: PathBuf,
    config: GitCliConfig,
}

impl GitCli {
    pub fn new(cwd: impl AsRef<Path>, config: GitCliConfig) -> Self {
        Self {
            cwd: cwd.as_ref().to_path_buf(),
            config,
        }
    }

    pub fn version(&self) -> Result<String> {
        self.run(&["--version"], None)
    }

    pub fn checkout(&self, rel_path: &str) -> Result<String> {
        self.run(&["checkout", "--", rel_path], None)
    }

    pub fn apply_reverse(&self, patch: &str) -> Result<String> {
        self.run(&["apply", "-R", "--recount"], Some(patch))
    }

    pub fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        let mut command = Command::new(&self.config.executable);
        command.args(args).current_dir(&self.cwd);
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                anyhow!(
                    "无法启动 Git `{}`: {e}(请在设置 → 版本控制中检查路径)",
                    self.config.executable
                )
            })?;
        if let (Some(input), Some(mut writer)) = (stdin, child.stdin.take()) {
            writer.write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            anyhow::bail!(
                "Git {} 失败: {}",
                args.first().copied().unwrap_or_default(),
                if stderr.is_empty() { stdout } else { stderr }
            );
        }
        Ok(stdout)
    }
}
