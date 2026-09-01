//! 附录 A7 生命周期参数(canonical spec `docs/superpowers/specs/2026-09-01-web-interaction-core-service.md`)。
//!
//! 本文件是 A7 的唯一数值来源:默认值、允许范围、hard cap 与派生规则。
//! 可配置项仅能通过 `~/.monkeyfence/config.toml` 的 `[limits]` 段在允许
//! 范围内覆盖默认值,且不得超过 hard cap;A7 各行的 hard cap 与允许范围
//! 上限一致。`discovery_heartbeat_ms` 派生 stale 判定(stale = 3×heartbeat,
//! §11.1),派生值不可独立配置。
//!
//! A7 表把常量落点标为 `limits.rs`、`singleton.rs`:A7 参数在本文,
//! 非参数工程常量(互斥名、文件名、acquire 超时)在 `singleton.rs`。
//! `shutdown_freeze_grace_ms`/`shutdown_drain_timeout_ms`/`forced_kill_grace_ms`
//! 由安全退出(§11.4,shutdown.rs)消费,`handoff_reacquire_window_ms` 由
//! owner handoff(§13.3)消费;T1e 先冻结数值契约,消费方随后续 ticket 落位。

/// 单个 A7 参数的三元组描述(默认 / 允许范围 / hard cap)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleParamSpec {
    /// A7 参数名(`config.toml [limits]` 键)。
    pub name: &'static str,
    pub default: u64,
    pub min: u64,
    pub max: u64,
    /// 安全上限:取值不得超过;A7 全部行 hard cap == 允许范围上限。
    pub hard_cap: u64,
}

/// A7 全量参数表(表驱动校验与 `limits_defaults` 契约测试的权威输入)。
pub const LIFECYCLE_PARAMS: [LifecycleParamSpec; 5] = [
    LifecycleParamSpec {
        name: "discovery_heartbeat_ms",
        default: 5_000,
        min: 1_000,
        max: 30_000,
        hard_cap: 30_000,
    },
    LifecycleParamSpec {
        name: "shutdown_freeze_grace_ms",
        default: 5_000,
        min: 1_000,
        max: 30_000,
        hard_cap: 30_000,
    },
    LifecycleParamSpec {
        name: "shutdown_drain_timeout_ms",
        default: 120_000,
        min: 30_000,
        max: 600_000,
        hard_cap: 600_000,
    },
    LifecycleParamSpec {
        name: "forced_kill_grace_ms",
        default: 10_000,
        min: 2_000,
        max: 60_000,
        hard_cap: 60_000,
    },
    LifecycleParamSpec {
        name: "handoff_reacquire_window_ms",
        default: 60_000,
        min: 15_000,
        max: 300_000,
        hard_cap: 300_000,
    },
];

/// §11.1:stale = 3×heartbeat(派生,不可独立配置)。
pub const DISCOVERY_STALE_HEARTBEATS: u64 = 3;

/// 超出允许范围的配置值(A7:任何取值不得超过 hard cap)。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid_limits:{name}={value} 超出允许范围 [{min}, {max}](hard cap {hard_cap})")]
pub struct LifecycleLimitsError {
    pub name: &'static str,
    pub value: u64,
    pub min: u64,
    pub max: u64,
    pub hard_cap: u64,
}

/// A7 生命周期参数的运行时取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleLimits {
    pub discovery_heartbeat_ms: u64,
    pub shutdown_freeze_grace_ms: u64,
    pub shutdown_drain_timeout_ms: u64,
    pub forced_kill_grace_ms: u64,
    pub handoff_reacquire_window_ms: u64,
}

impl Default for LifecycleLimits {
    fn default() -> Self {
        Self {
            discovery_heartbeat_ms: LIFECYCLE_PARAMS[0].default,
            shutdown_freeze_grace_ms: LIFECYCLE_PARAMS[1].default,
            shutdown_drain_timeout_ms: LIFECYCLE_PARAMS[2].default,
            forced_kill_grace_ms: LIFECYCLE_PARAMS[3].default,
            handoff_reacquire_window_ms: LIFECYCLE_PARAMS[4].default,
        }
    }
}

impl LifecycleLimits {
    /// 校验全部取值落在 A7 允许范围内(范围上限即 hard cap)。
    /// 越界一律拒绝,不静默钳制——调用方(未来的 config 装配)负责回错。
    pub fn validate(&self) -> Result<(), LifecycleLimitsError> {
        for (value, spec) in [
            (self.discovery_heartbeat_ms, &LIFECYCLE_PARAMS[0]),
            (self.shutdown_freeze_grace_ms, &LIFECYCLE_PARAMS[1]),
            (self.shutdown_drain_timeout_ms, &LIFECYCLE_PARAMS[2]),
            (self.forced_kill_grace_ms, &LIFECYCLE_PARAMS[3]),
            (self.handoff_reacquire_window_ms, &LIFECYCLE_PARAMS[4]),
        ] {
            if value < spec.min || value > spec.max {
                return Err(LifecycleLimitsError {
                    name: spec.name,
                    value,
                    min: spec.min,
                    max: spec.max,
                    hard_cap: spec.hard_cap,
                });
            }
        }
        Ok(())
    }

    /// 派生:discovery 记录从最后一次心跳起经过该时长即视为 stale
    /// (§11.1 stale = 3×heartbeat;不可独立配置)。
    pub fn discovery_stale_after_ms(&self) -> u64 {
        self.discovery_heartbeat_ms * DISCOVERY_STALE_HEARTBEATS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_params_table() {
        let defaults = LifecycleLimits::default();
        assert_eq!(defaults.discovery_heartbeat_ms, 5_000);
        assert_eq!(defaults.shutdown_freeze_grace_ms, 5_000);
        assert_eq!(defaults.shutdown_drain_timeout_ms, 120_000);
        assert_eq!(defaults.forced_kill_grace_ms, 10_000);
        assert_eq!(defaults.handoff_reacquire_window_ms, 60_000);
        defaults.validate().unwrap();
    }

    #[test]
    fn stale_after_is_three_heartbeats() {
        let limits = LifecycleLimits::default();
        assert_eq!(limits.discovery_stale_after_ms(), 15_000);
    }

    #[test]
    fn out_of_range_rejected() {
        let limits = LifecycleLimits {
            discovery_heartbeat_ms: LIFECYCLE_PARAMS[0].max + 1,
            ..Default::default()
        };
        let error = limits.validate().unwrap_err();
        assert_eq!(error.name, "discovery_heartbeat_ms");
    }
}
