//! 外部安装 adoption(T4b,Issue #40;spec §9.4)。
//!
//! 外部安装默认 `external`、只 launch;仅当可执行 hash 与插件固定的
//! 可信 artifact **完全匹配**才可 adopt(创建 adoption receipt),否则
//! 只能重装到受管目录。启动前 target 被替换 → identity 不符拒绝(§9.7
//! 的 Revision 重核消费同一 identity)。

use std::path::PathBuf;

use crate::discovery::{DiscoveredInstallation, InstallationKind};

/// 插件固定的可信 artifact。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedArtifact {
    pub agent_type_id: String,
    pub installer_id: String,
    /// 可执行文件 SHA256(指纹即信任;签名校验属 #43 verified-download)。
    pub executable_sha256: String,
}

/// adoption 判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptionDecision {
    /// hash 完全匹配 → 允许 adopt(生成 receipt)。
    Adoptable(AdoptionReceipt),
    /// 不匹配/不可哈希 → 只能重装到受管目录。
    ReinstallRequired { reason: String },
}

/// adoption receipt(§9.6 的最小子集;完整 receipt 由 #43 扩展)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptionReceipt {
    pub receipt_id: String,
    pub agent_type_id: String,
    pub installer_id: String,
    pub canonical_executable: PathBuf,
    pub executable_sha256: String,
    pub adopted_at: String,
}

/// 评估外部安装是否可 adopt。
pub fn evaluate_adoption(
    installation: &DiscoveredInstallation,
    trusted: &TrustedArtifact,
) -> AdoptionDecision {
    if installation.kind != InstallationKind::External {
        return AdoptionDecision::ReinstallRequired {
            reason: "受管安装无需 adoption(已有收据链)".into(),
        };
    }
    if installation.executable_identity == trusted.executable_sha256 {
        return AdoptionDecision::Adoptable(AdoptionReceipt {
            receipt_id: format!("adr_{}", uuid::Uuid::now_v7().simple()),
            agent_type_id: trusted.agent_type_id.clone(),
            installer_id: trusted.installer_id.clone(),
            canonical_executable: installation.canonical_path.clone(),
            executable_sha256: installation.executable_identity.clone(),
            adopted_at: chrono::Utc::now().to_rfc3339(),
        });
    }
    AdoptionDecision::ReinstallRequired {
        reason: format!(
            "可执行身份不匹配(trusted={}, actual={});仅可信 artifact 完全匹配才可 adopt",
            &trusted.executable_sha256[..trusted.executable_sha256.len().min(8)],
            &installation.executable_identity[..installation.executable_identity.len().min(8)],
        ),
    }
}

/// 启动前 identity 重核(§9.7:Revision 冻结的 executable identity 与
/// 当前磁盘身份不符 → 拒绝静默运行,进入 Needs You)。
pub fn verify_launch_identity(
    installation: &DiscoveredInstallation,
    frozen_executable_sha256: &str,
) -> Result<(), String> {
    if installation.executable_identity != frozen_executable_sha256 {
        return Err(format!(
            "可执行身份已被替换(frozen={}, actual={}):拒绝静默运行",
            &frozen_executable_sha256[..frozen_executable_sha256.len().min(8)],
            &installation.executable_identity[..installation.executable_identity.len().min(8)],
        ));
    }
    Ok(())
}
