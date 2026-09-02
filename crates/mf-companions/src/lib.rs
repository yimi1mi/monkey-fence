//! mf-companions:bundle manager 与随行进程(T5a 起,Issue #35)。
//!
//! T5a 交付 side-by-side whole-bundle manager:versioned 目录、
//! `current.json` 原子指针、bundle manifest(Core/assets/companions/
//! mfctl/Broker/hosts 全包一致性)、健康检查通过前不切 pointer、
//! previous bundle 保留、有新 durable 写入禁止自动恢复旧备份。
//! launcher/tray/picker bins 属 T6c;owner handoff/MSI/默认 Web 切换
//! 属后续 ticket。

pub mod bundle;
pub mod commands;
pub mod journeys;
