//! 平台缝隙(T4d,Issue #44):进程身份与管道/文件 ACL。
//!
//! 真实 UAC/ServiceManagement/polkit 提权属发行层(Non-goals 中"不执行
//! 需 elevation 的 recipe 的真实提权"由 Broker 进程承载);Core 侧只消费
//! 身份判定与 ACL 校验缝隙,fake 实现驱动契约。

pub mod fake;

/// OS 身份指纹(当前进程/对端)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsIdentity {
    pub user_sid: String,
    pub pid: u32,
}

pub trait PlatformIdentity: Send + Sync {
    /// 当前进程身份(Core 恒 asInvoker;Broker/host 为提权身份)。
    fn current(&self) -> OsIdentity;
    /// 判定对端身份是否为预期(错误 SID/PID 拒绝;§10.2)。
    fn peer_matches(&self, expected: &OsIdentity, actual: &OsIdentity) -> bool;
    /// 是否具备管理员组(仅 Broker/host true;Core false)。
    fn is_elevated(&self) -> bool;
}
