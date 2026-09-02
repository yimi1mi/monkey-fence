//! L-CMD 事务内授权/租约/CAS 复验缝隙。

use crate::command::{CommandProblem, CommandType};
use crate::handles::{ClientId, CommandTarget, ExpectedRevision, Principal};
use rusqlite::Transaction;

#[derive(Debug)]
pub struct LeaseCheck<'a> {
    pub principal: &'a Principal,
    pub client_id: &'a ClientId,
    pub controller_epoch: u64,
    pub root_epoch: Option<u64>,
    pub command_type: CommandType,
    pub target: &'a CommandTarget,
    pub expected: &'a [ExpectedRevision],
}

/// Permit 生命周期覆盖目标事务 commit：实现可持有 Controller publication
/// barrier/epoch lock，防止复验通过后、L-CMD 前被 takeover 旋转。
pub trait CommandPermit {
    /// expected revision 只对尚未线性化的新 effect 检查，不能拿命令
    /// 自身已产生的 post-revision 拒绝合法 receipt replay。
    fn validate_expected(
        &self,
        tx: &Transaction<'_>,
        check: &LeaseCheck<'_>,
    ) -> Result<(), CommandProblem>;
}

pub trait CommandAuthorizer: Send + Sync {
    fn acquire<'a>(
        &'a self,
        tx: &Transaction<'_>,
        check: &LeaseCheck<'_>,
    ) -> Result<Box<dyn CommandPermit + 'a>, CommandProblem>;
}

// ---------------------------------------------------------------------------
// L-INPUT(§2.4/T3c,Issue #31):终端输入的原子复验缝隙
// ---------------------------------------------------------------------------

/// 字节进入 Agent Session 单线程有序 PTY 写队列前的复验输入。
/// 与 L-CMD 的 `LeaseCheck` 平行;区别在于 effect 是内存写队列 enqueue
/// (非 Store 事务),但复验同样必须发生在 effect 之前——takeover 旋转
/// epoch 后,旧 Controller 的在途字节不得再入队(已线性化字节不回收)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputLeaseCheck {
    /// 发起写的 Controller epoch(writer lease 授予时绑定)。
    pub controller_epoch: u64,
    /// mf-terminal `WriterLeaseManager` 签发的 lease(UUID bytes)。
    pub writer_lease_id: [u8; 16],
}

/// L-INPUT 复验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLeaseVerdict {
    /// epoch 与当前 Controller 一致:允许进入写队列。
    Current,
    /// epoch 已被 takeover 旋转:拒绝入队,撤销 writer lease。
    ControllerTakeover,
}
