//! 不可变 Installation Receipt 与 executable identity pin(T4c,Issue #43;
//! spec §9.6/§9.7)。
//!
//! receipt 不可变:安装/更新/修复写入,后续只读;update/repair/uninstall
//! 只碰 receipt 拥有的内容(canonical root 内);Revision 冻结 executable
//! identity(CLI 被替换 → 新 Agent Run 拒绝 + Needs You)。不存
//! API Key/完整环境/未脱敏 argv/终端输入。

use std::path::{Path, PathBuf};

use crate::discovery::InstallationKind;

/// 不可变安装收据(§9.6 字段子集;elevation 身份分离字段属 #44)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationReceipt {
    pub receipt_id: String,
    pub plugin_full_id: String,
    pub agent_type_id: String,
    pub installer_id: String,
    pub recipe_digest: String,
    /// 请求版本(用户预览所见)。
    pub requested_version: String,
    /// 实际版本(post-probe 探测;installed 状态的必要条件)。
    pub actual_version: String,
    pub scope: String,
    /// canonical executable + 可执行身份(SHA256)。
    pub canonical_executable: PathBuf,
    pub executable_sha256: String,
    /// rollback/uninstall 方法(结构化;不含明文凭据)。
    pub rollback: RollbackMethod,
    /// 安装时间。
    pub installed_at: String,
}

/// 回滚方法(receipt-owned 内容的逆操作)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackMethod {
    /// 删除受管目录下的整个安装根(receipt 拥有)。
    RemoveOwnedRoot { owned_root: PathBuf },
    /// 包管理器卸载(结构化 argv)。
    PackageManagerUninstall { argv: Vec<String> },
    /// 无自动回滚(verified-download 已原子切换;旧版本按 side-by-side)。
    None,
}

/// ownership 判定:update/repair/uninstall 只允许操作 receipt 拥有的
/// 内容(target 在 owned root 内且为该 receipt 的 canonical root)。
pub fn is_receipt_owned(receipt: &InstallationReceipt, target: &Path) -> bool {
    match &receipt.rollback {
        RollbackMethod::RemoveOwnedRoot { owned_root } => target.starts_with(owned_root),
        RollbackMethod::PackageManagerUninstall { .. } => false, // 包管理器自管
        RollbackMethod::None => false,
    }
}

/// Revision 的 executable identity pin(§9.7)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableIdentityPin {
    pub installation_receipt_id: String,
    pub canonical_executable: PathBuf,
    pub executable_sha256: String,
    pub actual_cli_version: String,
}

/// 启动前重核(新 Agent Run 前必查):canonical executable 或其身份被
/// 替换 → 拒绝启动并 Needs You(不偷偷用 PATH 新版本)。
pub fn verify_pin(
    pin: &ExecutableIdentityPin,
    current_canonical: &Path,
    current_sha256: &str,
) -> Result<(), PinProblem> {
    if pin.canonical_executable != current_canonical {
        return Err(PinProblem::ExecutableReplaced {
            pinned: pin.canonical_executable.display().to_string(),
            current: current_canonical.display().to_string(),
        });
    }
    if pin.executable_sha256 != current_sha256 {
        return Err(PinProblem::IdentityDrift {
            pinned: pin.executable_sha256[..8].to_string(),
            current: current_sha256[..8].to_string(),
        });
    }
    Ok(())
}

/// pin 重核问题(Needs You 语义:调用方标记会话/运行)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PinProblem {
    #[error("可执行路径已替换(pinned={pinned}, current={current}):拒绝启动,进入 Needs You")]
    ExecutableReplaced { pinned: String, current: String },
    #[error("可执行身份漂移(pinned={pinned}.., current={current}..):拒绝静默运行")]
    IdentityDrift { pinned: String, current: String },
}

/// 安装域分类(receipt 呈现:受管安装 vs external 只 launch)。
pub fn installation_kind_of(receipt: Option<&InstallationReceipt>) -> InstallationKind {
    match receipt {
        Some(_) => InstallationKind::Managed,
        None => InstallationKind::External,
    }
}
