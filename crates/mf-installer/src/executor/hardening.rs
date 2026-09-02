//! 三类 executor 与安装状态机(T4c,Issue #43;spec §9.3/§9.5)。
//!
//! 执行流程:queued→resolving→downloading|executing→verifying→
//! **installed(仅当 post-probe 成功;退出码 0 不算)**;cancel 是命令
//! 非 UI 状态;部分外部状态 → repair-needed 展示诊断(不谎报回滚)。
//! 下载硬边界:HTTPS、redirect 仅预览冻结域名且 ≤5(fixed)、大小/停滞
//! 上限、archive traversal/symlink/junction 拒绝、原子发布(L-SWITCH)。
//! L-SWITCH 前失败只清 staging。shell 注入失败关闭:结构化 argv 直启,
//! shell 字符串拒绝伪装。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::plan::FrozenInstallPlan;
use crate::receipt::InstallationReceipt;

/// 安装任务状态(单调:状态只沿表前进,回退=内部错误)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobPhase {
    Queued,
    Resolving,
    Downloading,
    Executing,
    Verifying,
    Installed,
    Failed,
    Cancelled,
    RepairNeeded,
}

/// 执行进度事件(单调 seq;Snapshot+resume 恢复)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEvent {
    pub seq: u64,
    pub phase: JobPhase,
    pub detail: String,
}

/// 执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteOutcome {
    Installed(InstallationReceipt),
    Failed { phase: JobPhase, reason: String },
    RepairNeeded { reason: String },
    Cancelled,
}

/// executor 环境注入(测试 fake;生产真实进程/HTTP)。
pub trait ExecutorEnv {
    /// package-manager/custom-command:以结构化 argv 启动进程,收集
    /// (脱敏后)输出与退出码。**永不经 shell**。
    fn run_structured(
        &mut self,
        program: &str,
        argv: &[String],
        cwd: Option<&Path>,
    ) -> Result<(i32, Vec<u8>), String>;

    /// verified-download:下载冻结 URL(HTTPS)到字节数组,遵循硬边界
    /// (redirect≤5 且同冻结域、大小上限、停滞超时)。
    fn download(&mut self, url: &str, frozen_host: &str) -> Result<Vec<u8>, String>;

    /// 发布文件(staging → 最终位置的原子写;由环境决定物理原子性)。
    fn publish(&mut self, target: &Path, bytes: &[u8]) -> Result<(), String>;

    /// post-install probe:探测安装后的版本(探测失败 ≠ 安装成功)。
    fn probe_version(
        &mut self,
        executable: &Path,
        version_argv: &[String],
    ) -> Result<String, String>;

    /// 清理 staging(L-SWITCH 前失败;不触碰已安装内容)。
    fn cleanup_staging(&mut self, staging: &Path);

    /// 目标文件的 SHA256(receipt 的 executable identity)。
    fn file_sha256(&mut self, path: &Path) -> Result<String, String>;
}

/// 下载硬边界(A5)。
pub struct DownloadPolicy {
    pub max_bytes: u64,
    pub stall_timeout: Duration,
    pub redirect_max: u32,
}

impl Default for DownloadPolicy {
    fn default() -> Self {
        Self {
            max_bytes: 2 * 1024 * 1024 * 1024,
            stall_timeout: Duration::from_millis(60_000),
            redirect_max: crate::limits::INSTALL_REDIRECT_MAX,
        }
    }
}

/// URL 硬化:仅 HTTPS、仅预览冻结域、redirect 检查。
pub fn validate_download_url(
    url: &str,
    frozen_host: &str,
    redirect_count: u32,
) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err(format!("仅允许 HTTPS(拒绝明文/文件协议):{url}"));
    }
    let host = url_host(url)?;
    if host != frozen_host {
        return Err(format!(
            "域名漂移(冻结={frozen_host}, 实际={host}):仅预览冻结域允许"
        ));
    }
    if redirect_count > crate::limits::INSTALL_REDIRECT_MAX {
        return Err(format!(
            "redirect 超上限({redirect_count} > {})",
            crate::limits::INSTALL_REDIRECT_MAX
        ));
    }
    Ok(())
}

fn url_host(url: &str) -> Result<String, String> {
    url.split_once("https://")
        .and_then(|(_, rest)| rest.split('/').next().map(str::to_string))
        .ok_or_else(|| format!("URL 无 host:{url}"))
}

/// archive 条目硬化:绝对路径/`..`/symlink/junction/设备文件全拒绝。
pub fn validate_archive_entry(entry_name: &str, is_symlink: bool) -> Result<(), String> {
    if is_symlink {
        return Err(format!("archive 拒绝 symlink/junction 条目:{entry_name}"));
    }
    let path = Path::new(entry_name);
    let windows_root =
        entry_name.starts_with('\\') || entry_name.contains("..\\") || entry_name.starts_with('/');
    if path.is_absolute() || windows_root {
        return Err(format!("archive 拒绝绝对路径条目:{entry_name}"));
    }
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(format!("archive traversal 拒绝 `..`:{entry_name}"));
        }
    }
    if entry_name.contains(':') && entry_name.len() > 1 && entry_name.as_bytes()[1] == b':' {
        return Err(format!("archive 拒绝设备/驱动器路径:{entry_name}"));
    }
    Ok(())
}

/// shell 注入硬化:执行输入必须是结构化 argv;含 shell 元字符的
/// "整条命令字符串"不得伪装成单个 argv 元素(检测明显拼接形态并
/// 拒绝,由调用方拆分)。
pub fn validate_structured_argv(program: &str, argv: &[String]) -> Result<(), String> {
    if program.trim().is_empty() {
        return Err("程序名为空".into());
    }
    if program.contains(|c: char| c.is_whitespace()) {
        return Err(format!("程序名含空白(疑似 shell 拼接):{program}"));
    }
    for arg in argv {
        // 参数本身可以含空格(引用值);但以 shell 连接符开头的独立
        // 元素(如 `&& rm -rf`)是拼接泄漏的明确信号
        if arg.trim_start().starts_with("&& ")
            || arg.trim_start().starts_with("|| ")
            || arg.trim_start().starts_with("; ")
        {
            return Err(format!("argv 含 shell 连接符(注入形态):{arg}"));
        }
    }
    Ok(())
}

/// 执行冻结计划(三类 executor;L-SWITCH 语义由 publish 环境承载)。
/// `cancel` 旗标由调用方轮询设置(协作式取消)。
pub fn execute_plan(
    env: &mut dyn ExecutorEnv,
    plan: &FrozenInstallPlan,
    staging: &Path,
    target_executable: &Path,
    version_argv: &[String],
    policy: &DownloadPolicy,
    is_cancelled: &dyn Fn() -> bool,
) -> ExecuteOutcome {
    let preview = &plan.preview;
    if is_cancelled() {
        env.cleanup_staging(staging);
        return ExecuteOutcome::Cancelled;
    }
    // 解析(exact 冻结值;不重解析 latest)
    if is_cancelled() {
        env.cleanup_staging(staging);
        return ExecuteOutcome::Cancelled;
    }
    match preview.kind.as_str() {
        "package-manager" => {
            if let Err(reason) = validate_structured_argv(&preview.exact_package, &preview.argv) {
                return ExecuteOutcome::Failed {
                    phase: JobPhase::Executing,
                    reason,
                };
            }
            // 结构化执行(不经 shell)
            match env.run_structured(&preview.exact_package, &preview.argv, None) {
                Ok((code, _output)) if code == 0 => {}
                Ok((code, output)) => {
                    // 包管理器部分外部状态 → repair-needed(不谎报回滚)
                    let reason = format!(
                        "包管理器退出码 {code}:{}",
                        String::from_utf8_lossy(&output[..output.len().min(200)])
                    );
                    return if looks_partial(&reason) {
                        ExecuteOutcome::RepairNeeded { reason }
                    } else {
                        ExecuteOutcome::Failed {
                            phase: JobPhase::Executing,
                            reason,
                        }
                    };
                }
                Err(reason) => {
                    return ExecuteOutcome::Failed {
                        phase: JobPhase::Executing,
                        reason,
                    }
                }
            }
        }
        "verified-download" => {
            let download = preview
                .download
                .as_ref()
                .expect("verified-download 必有冻结下载");
            if let Err(reason) = validate_download_url(&download.url, &download.frozen_host, 0) {
                env.cleanup_staging(staging);
                return ExecuteOutcome::Failed {
                    phase: JobPhase::Downloading,
                    reason,
                };
            }
            let bytes = match env.download(&download.url, &download.frozen_host) {
                Ok(bytes) => bytes,
                Err(reason) => {
                    env.cleanup_staging(staging);
                    return ExecuteOutcome::Failed {
                        phase: JobPhase::Downloading,
                        reason,
                    };
                }
            };
            if bytes.len() as u64 > policy.max_bytes {
                env.cleanup_staging(staging);
                return ExecuteOutcome::Failed {
                    phase: JobPhase::Downloading,
                    reason: format!("下载超限({} > {})", bytes.len(), policy.max_bytes),
                };
            }
            use sha2::{Digest, Sha256};
            let actual = format!("{:x}", Sha256::digest(&bytes));
            if actual != download.sha256 {
                env.cleanup_staging(staging);
                return ExecuteOutcome::Failed {
                    phase: JobPhase::Verifying,
                    reason: format!(
                        "下载校验失败(expected {}.., actual {}..)",
                        &download.sha256[..8],
                        &actual[..8]
                    ),
                };
            }
            // 原子发布(staging→受管目录;L-SWITCH)
            if let Err(reason) = env.publish(target_executable, &bytes) {
                env.cleanup_staging(staging);
                return ExecuteOutcome::Failed {
                    phase: JobPhase::Verifying,
                    reason,
                };
            }
        }
        "custom-command" => {
            // 结构化 executable+argv;shell 字符串在 validate 阶段拒绝
            if let Err(reason) = validate_structured_argv(&preview.exact_package, &preview.argv) {
                return ExecuteOutcome::Failed {
                    phase: JobPhase::Executing,
                    reason,
                };
            }
            match env.run_structured(&preview.exact_package, &preview.argv, None) {
                Ok((code, _)) if code == 0 => {}
                Ok((code, _)) => {
                    return ExecuteOutcome::Failed {
                        phase: JobPhase::Executing,
                        reason: format!("custom-command 执行失败(退出码 {code})"),
                    }
                }
                Err(reason) => {
                    return ExecuteOutcome::Failed {
                        phase: JobPhase::Executing,
                        reason,
                    }
                }
            }
        }
        other => {
            return ExecuteOutcome::Failed {
                phase: JobPhase::Resolving,
                reason: format!("未知 executor kind:{other}"),
            }
        }
    }
    if is_cancelled() {
        env.cleanup_staging(staging);
        return ExecuteOutcome::Cancelled;
    }
    // **安装成功必经 post-probe**(退出码 0 不算成功)
    match env.probe_version(target_executable, version_argv) {
        Ok(version) => ExecuteOutcome::Installed(InstallationReceipt {
            receipt_id: format!("irec_{}", uuid::Uuid::now_v7().simple()),
            plugin_full_id: format!("builtin.{}", preview.agent_type_id),
            agent_type_id: preview.agent_type_id.clone(),
            installer_id: preview.installer_id.clone(),
            recipe_digest: plan.recipe_digest.clone(),
            requested_version: preview.exact_version.clone(),
            actual_version: version,
            scope: "user".into(),
            canonical_executable: target_executable.to_path_buf(),
            executable_sha256: env.file_sha256(target_executable).unwrap_or_default(),
            rollback: crate::receipt::RollbackMethod::RemoveOwnedRoot {
                owned_root: target_executable
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("/unmanaged")),
            },
            installed_at: chrono_now(),
        }),
        Err(reason) => ExecuteOutcome::Failed {
            phase: JobPhase::Verifying,
            reason: format!("post-install probe 失败(退出码 0 不算安装成功):{reason}"),
        },
    }
}

fn looks_partial(reason: &str) -> bool {
    // 包管理器部分外部状态(如 npm EEXIST/pip 已存在目录):repair 而非失败
    reason.contains("EEXIST") || reason.contains("already exists") || reason.contains("已存在")
}

fn chrono_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}
