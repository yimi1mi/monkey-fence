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
