//! mf-elevated:Root 逻辑/物理 owner 分离契约(T3e,Issue #33;spec §10)。
//!
//! 本 crate 交付 **fake seam**:Core/Broker/Root Host 三方的协议状态机、
//! epoch 复验、ACL spool、heartbeat/orphan grace 与窄化 read-only
//! reattach——不实现真实 Windows UAC Broker / Install Host / Root UI,
//! 也不执行真实 elevated Agent(Non-goals)。禁用 Root seam 即回到普通
//! Agent Session(无接入点、无回滚数据)。
//!
//! owner 语义(§10.3):Core SessionRegistry 是逻辑 owner;Root PTY/
//! 进程组的物理 owner 是 session-scoped root host,只服务一个 Agent
//! Session,不能创建会话或通用命令。Core channel 断开后 host 拒绝新
//! input/resize/control,把已脱敏输出写入 spool 进入 bounded orphan
//! grace;新 Core 只能以持久 receipt + OS identity 做 read-only
//! reattach(会话进 Needs You);grace 到期终止 Root process group。

pub mod limits;
pub mod platform;
pub mod protocol;
pub mod root_execution;
pub mod root_host;
pub mod spool;
