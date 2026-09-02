//! standalone Core 生命周期(T6b,Issue #48;spec §2.3/§11)。
//!
//! `starting → acquiring_owner_lock → owning(服务 Client)`:
//! 只有 **L-OWNER 成功且 discovery 更新**后才服务 Client;singleton
//! race 的败者只转发 open intent 后退出(不触碰 Store);stale discovery
//! fencing(旧 epoch 被拒);Core crash → restart 冷启动(≤10 Project
//! cold-start budget)。影子/测试模式启动——未取得 owner lock 不动
//! Store,失败保持 Bridge A owner。

use std::time::Instant;

use crate::singleton::OwnerMutexSource;

/// 冷启动阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CorePhase {
    Starting,
    AcquiringOwnerLock,
    Owning,
    /// 败者:转发 open intent 后退出。
    LostSingleton,
    /// 启动失败(保持 Bridge A owner)。
    Failed,
}

/// discovery 记录(用户级 discovery 文件形态)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiscoveryRecord {
    pub owner_epoch: u64,
    pub pid: u32,
    pub started_at: String,
    /// 随机 loopback listener(影子模式未暴露 Web;仅记录)。
    pub loopback_port: Option<u16>,
}

/// 生命周期问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleProblem {
    #[error("owner lock 已被存活进程持有(pid={pid}):败者只转发 open intent 后退出")]
    LostSingleton { pid: u32 },
    #[error("discovery 记录为陈旧 epoch({stale} ≤ 当前 {current}):fencing 拒绝")]
    StaleDiscovery { stale: u64, current: u64 },
    #[error("cold-start 超预算({projects} projects, {elapsed_ms}ms > {budget_ms}ms)")]
    ColdStartBudget {
        projects: usize,
        elapsed_ms: u128,
        budget_ms: u64,
    },
}

/// 冷启动状态机(与 OwnerMutexSource/ServiceStore 组合;IO 注入)。
pub struct CoreLifecycle {
    phase: CorePhase,
    owner_epoch: u64,
}

impl CoreLifecycle {
    pub fn new() -> Self {
        Self {
            phase: CorePhase::Starting,
            owner_epoch: 0,
        }
    }

    pub fn phase(&self) -> CorePhase {
        self.phase
    }

    pub fn owner_epoch(&self) -> u64 {
        self.owner_epoch
    }

    /// L-OWNER:获取 owner lock(或确认败者)。锁由注入的 mutex 承载;
    /// 败者 → LostSingleton(**不触碰 Store**;只转发 open intent 后退出)。
    pub fn acquire_owner_lock(
        &mut self,
        mutex: &dyn OwnerMutexSource,
    ) -> Result<(), LifecycleProblem> {
        self.phase = CorePhase::AcquiringOwnerLock;
        match mutex.acquire(std::time::Duration::from_secs(1)) {
            Ok(_guard) => {
                self.owner_epoch += 1;
                Ok(())
            }
            Err(_) => {
                self.phase = CorePhase::LostSingleton;
                Err(LifecycleProblem::LostSingleton { pid: 0 })
            }
        }
    }

    /// discovery 更新(L-OWNER 成功后;stale fencing——记录的 epoch 必须
    /// 严格更旧)。
    pub fn update_discovery(
        &mut self,
        previous: Option<&DiscoveryRecord>,
        pid: u32,
        loopback_port: Option<u16>,
    ) -> Result<DiscoveryRecord, LifecycleProblem> {
        if let Some(previous) = previous {
            if previous.owner_epoch >= self.owner_epoch {
                return Err(LifecycleProblem::StaleDiscovery {
                    stale: previous.owner_epoch,
                    current: self.owner_epoch,
                });
            }
        }
        let record = DiscoveryRecord {
            owner_epoch: self.owner_epoch,
            pid,
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_default(),
            loopback_port,
        };
        // 只有 L-OWNER 成功且 discovery 更新后才服务 Client
        self.phase = CorePhase::Owning;
        Ok(record)
    }

    /// Project Registry 冷启动预算(≤10 Project,5000ms;附录 A9)。
    pub fn check_cold_start_budget(
        &self,
        projects: usize,
        elapsed: std::time::Duration,
        budget_ms: u64,
    ) -> Result<(), LifecycleProblem> {
        if elapsed.as_millis() > budget_ms as u128 {
            return Err(LifecycleProblem::ColdStartBudget {
                projects,
                elapsed_ms: elapsed.as_millis(),
                budget_ms,
            });
        }
        Ok(())
    }

    /// 启动失败(影子模式:保持 Bridge A owner,不动生产)。
    pub fn fail(&mut self) {
        self.phase = CorePhase::Failed;
    }
}

impl Default for CoreLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::singleton::FakeOwnerMutex;

    #[test]
    fn loser_exits_without_serving() {
        let winner = FakeOwnerMutex::new("mf-core-singleton");
        let _guard = winner.acquire(std::time::Duration::from_secs(1)).unwrap();
        let mut loser = CoreLifecycle::new();
        let result = loser.acquire_owner_lock(&winner);
        assert!(matches!(
            result,
            Err(LifecycleProblem::LostSingleton { .. })
        ));
        assert_eq!(loser.phase(), CorePhase::LostSingleton);
        // 败者永不 Owning(不服务 Client)
        assert_ne!(loser.phase(), CorePhase::Owning);
    }

    #[test]
    fn owning_requires_owner_lock_then_discovery() {
        let mutex = FakeOwnerMutex::new("mf-core-2");
        let mut core = CoreLifecycle::new();
        core.acquire_owner_lock(&mutex).unwrap();
        // 锁已取得但 discovery 未更新 → 尚未 Owning
        assert_ne!(core.phase(), CorePhase::Owning);
        let record = core.update_discovery(None, 4242, Some(51234)).unwrap();
        assert_eq!(core.phase(), CorePhase::Owning);
        assert_eq!(record.owner_epoch, 1);
        assert_eq!(record.loopback_port, Some(51234));
    }

    #[test]
    fn stale_discovery_epoch_is_fenced() {
        let mutex = FakeOwnerMutex::new("mf-core-3");
        let mut core = CoreLifecycle::new();
        core.acquire_owner_lock(&mutex).unwrap();
        // 记录里已是同 epoch/更高 → 陈旧 fencing 拒绝
        let stale = DiscoveryRecord {
            owner_epoch: 1,
            pid: 9999,
            started_at: "0".into(),
            loopback_port: None,
        };
        assert!(matches!(
            core.update_discovery(Some(&stale), 4242, None),
            Err(LifecycleProblem::StaleDiscovery {
                stale: 1,
                current: 1
            })
        ));
        // 更旧记录(0 < 1)正常接管
        let older = DiscoveryRecord {
            owner_epoch: 0,
            ..stale
        };
        assert!(core.update_discovery(Some(&older), 4242, None).is_ok());
    }

    #[test]
    fn cold_start_budget_enforced() {
        let mutex = FakeOwnerMutex::new("mf-core-4");
        let mut core = CoreLifecycle::new();
        core.acquire_owner_lock(&mutex).unwrap();
        // 10 项目 5000ms 内 → ok
        core.check_cold_start_budget(10, std::time::Duration::from_millis(4_999), 5_000)
            .unwrap();
        assert!(matches!(
            core.check_cold_start_budget(10, std::time::Duration::from_millis(5_001), 5_000),
            Err(LifecycleProblem::ColdStartBudget { .. })
        ));
    }
}
