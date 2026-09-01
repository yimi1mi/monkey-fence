//! 附录 A1/A4/A7 参数(canonical spec `docs/superpowers/specs/2026-09-01-web-interaction-core-service.md`)。
//!
//! 本文件是 A1(Workflow 事件与 API)、A4(命令/审计 retention 与 GC)
//! 与 A7(生命周期)的唯一数值
//! 来源:默认值、允许范围、hard cap 与派生规则。可配置项仅能通过
//! `~/.monkeyfence/config.toml` 的 `[limits]` 段在允许范围内覆盖默认值,
//! 且不得超过 hard cap;A1/A4/A7 各行的 hard cap 与允许范围上限一致。
//! `discovery_heartbeat_ms` 派生 stale 判定(stale = 3×heartbeat,
//! §11.1),派生值不可独立配置。
//!
//! A7 表把常量落点标为 `limits.rs`、`singleton.rs`:A7 参数在本文,
//! 非参数工程常量(互斥名、文件名、acquire 超时)在 `singleton.rs`。
//! `shutdown_freeze_grace_ms`/`shutdown_drain_timeout_ms`/`forced_kill_grace_ms`
//! 由安全退出(§11.4,shutdown.rs)消费,`handoff_reacquire_window_ms` 由
//! owner handoff(§13.3)消费;T1e 先冻结数值契约,消费方随后续 ticket 落位。
//! A4 retention/GC 参数由 command receipt / Operation / audit 的 GC
//! (§4.6,`reconcile.rs`)与进度事件节奏消费;`gc_interval_ms` 是周期调度
//! 与启动时 GC 的间隔,`operation_progress_interval_ms` 是 Operation 进度
//! 事件的最低间隔(节流阈值,不是禁止更早的终态事件)。

// ─────────────────────── 附录 A1:Workflow 事件与 API ───────────────────────

/// 兼容 #23 tracer 的附录 A1 默认 journal 水位常量。
pub const JOURNAL_MAX_EVENTS_DEFAULT: usize = 20_000;
pub const JOURNAL_MAX_BYTES_DEFAULT: usize = 64 * 1024 * 1024;

/// §5.2:命令 token bucket 的 burst 是速率的 3 倍,不可独立配置。
pub const COMMAND_RATE_BURST_MULTIPLIER: u64 = 3;

/// 单个 A1 参数的三元组描述(默认 / 允许范围 / hard cap)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalParamSpec {
    /// A1 参数名(`config.toml [limits]` 键)。
    pub name: &'static str,
    pub default: u64,
    pub min: u64,
    pub max: u64,
    /// 安全上限:取值不得超过;A1 全部行 hard cap == 允许范围上限。
    pub hard_cap: u64,
}

/// A1 全量参数表(表驱动校验与 `limits_defaults` 契约测试的权威输入)。
/// 字节值均以 byte 表示,速率值为每秒数量。
pub const JOURNAL_PARAMS: [JournalParamSpec; 9] = [
    JournalParamSpec {
        name: "journal_max_events",
        default: 20_000,
        min: 1_000,
        max: 100_000,
        hard_cap: 100_000,
    },
    JournalParamSpec {
        name: "journal_max_bytes",
        default: 64 * 1024 * 1024,
        min: 4 * 1024 * 1024,
        max: 256 * 1024 * 1024,
        hard_cap: 256 * 1024 * 1024,
    },
    JournalParamSpec {
        name: "journal_min_age_secs",
        default: 1_800,
        min: 0,
        max: 86_400,
        hard_cap: 86_400,
    },
    JournalParamSpec {
        name: "journal_event_max_bytes",
        default: 1024 * 1024,
        min: 64 * 1024,
        max: 2 * 1024 * 1024,
        hard_cap: 2 * 1024 * 1024,
    },
    JournalParamSpec {
        name: "client_event_queue_max_events",
        default: 2_000,
        min: 100,
        max: 20_000,
        hard_cap: 20_000,
    },
    JournalParamSpec {
        name: "client_event_queue_max_bytes",
        default: 8 * 1024 * 1024,
        min: 1024 * 1024,
        max: 64 * 1024 * 1024,
        hard_cap: 64 * 1024 * 1024,
    },
    JournalParamSpec {
        name: "events_ws_ping_interval_ms",
        default: 20_000,
        min: 5_000,
        max: 60_000,
        hard_cap: 60_000,
    },
    JournalParamSpec {
        name: "events_ws_idle_timeout_ms",
        default: 90_000,
        min: 30_000,
        max: 300_000,
        hard_cap: 300_000,
    },
    JournalParamSpec {
        name: "command_rate_per_client",
        default: 40,
        min: 5,
        max: 200,
        hard_cap: 200,
    },
];

/// 超出允许范围的 A1 配置值。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid_limits:{name}={value} 超出允许范围 [{min}, {max}](hard cap {hard_cap})")]
pub struct JournalLimitsError {
    pub name: &'static str,
    pub value: u64,
    pub min: u64,
    pub max: u64,
    pub hard_cap: u64,
}

/// A1 Workflow 事件与 API 参数的运行时取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalLimits {
    pub journal_max_events: usize,
    pub journal_max_bytes: usize,
    pub journal_min_age_secs: u64,
    pub journal_event_max_bytes: usize,
    pub client_event_queue_max_events: usize,
    pub client_event_queue_max_bytes: usize,
    pub events_ws_ping_interval_ms: u64,
    pub events_ws_idle_timeout_ms: u64,
    pub command_rate_per_client: u64,
}

impl Default for JournalLimits {
    fn default() -> Self {
        Self {
            journal_max_events: JOURNAL_MAX_EVENTS_DEFAULT,
            journal_max_bytes: JOURNAL_MAX_BYTES_DEFAULT,
            journal_min_age_secs: JOURNAL_PARAMS[2].default,
            journal_event_max_bytes: JOURNAL_PARAMS[3].default as usize,
            client_event_queue_max_events: JOURNAL_PARAMS[4].default as usize,
            client_event_queue_max_bytes: JOURNAL_PARAMS[5].default as usize,
            events_ws_ping_interval_ms: JOURNAL_PARAMS[6].default,
            events_ws_idle_timeout_ms: JOURNAL_PARAMS[7].default,
            command_rate_per_client: JOURNAL_PARAMS[8].default,
        }
    }
}

impl JournalLimits {
    /// 校验全部取值落在 A1 允许范围内。越界一律拒绝,不静默钳制。
    pub fn validate(&self) -> Result<(), JournalLimitsError> {
        for (value, spec) in [
            (usize_as_u64(self.journal_max_events), &JOURNAL_PARAMS[0]),
            (usize_as_u64(self.journal_max_bytes), &JOURNAL_PARAMS[1]),
            (self.journal_min_age_secs, &JOURNAL_PARAMS[2]),
            (
                usize_as_u64(self.journal_event_max_bytes),
                &JOURNAL_PARAMS[3],
            ),
            (
                usize_as_u64(self.client_event_queue_max_events),
                &JOURNAL_PARAMS[4],
            ),
            (
                usize_as_u64(self.client_event_queue_max_bytes),
                &JOURNAL_PARAMS[5],
            ),
            (self.events_ws_ping_interval_ms, &JOURNAL_PARAMS[6]),
            (self.events_ws_idle_timeout_ms, &JOURNAL_PARAMS[7]),
            (self.command_rate_per_client, &JOURNAL_PARAMS[8]),
        ] {
            if value < spec.min || value > spec.max || value > spec.hard_cap {
                return Err(JournalLimitsError {
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

    /// 派生 token bucket burst(3×每客户端命令速率),不可独立配置。
    pub fn command_burst_per_client(&self) -> u64 {
        self.command_rate_per_client * COMMAND_RATE_BURST_MULTIPLIER
    }

    /// 从 canonical `config.toml [limits]` 读取 A1 覆盖；缺文件使用默认值，
    /// 语法错误或越界一律 fail-closed。
    pub fn load_from_path(path: &std::path::Path) -> Result<Self, JournalLimitsLoadError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(JournalLimitsLoadError::Io(error)),
        };
        let config: LimitsConfigFile = toml::from_str(&text)?;
        let mut limits = Self::default();
        if let Some(values) = config.limits {
            values.apply(&mut limits);
        }
        limits.validate()?;
        Ok(limits)
    }

    pub fn load_default_path() -> Result<Self, JournalLimitsLoadError> {
        let home = dirs::home_dir().ok_or(JournalLimitsLoadError::HomeUnavailable)?;
        Self::load_from_path(&home.join(".monkeyfence").join("config.toml"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JournalLimitsLoadError {
    #[error("limits_config_io:{0}")]
    Io(#[from] std::io::Error),
    #[error("limits_config_parse:{0}")]
    Parse(#[from] toml::de::Error),
    #[error(transparent)]
    Invalid(#[from] JournalLimitsError),
    #[error("limits_config_home_unavailable")]
    HomeUnavailable,
}

#[derive(Debug, Default, serde::Deserialize)]
struct LimitsConfigFile {
    limits: Option<JournalLimitOverrides>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct JournalLimitOverrides {
    journal_max_events: Option<usize>,
    journal_max_bytes: Option<usize>,
    journal_min_age_secs: Option<u64>,
    journal_event_max_bytes: Option<usize>,
    client_event_queue_max_events: Option<usize>,
    client_event_queue_max_bytes: Option<usize>,
    events_ws_ping_interval_ms: Option<u64>,
    events_ws_idle_timeout_ms: Option<u64>,
    command_rate_per_client: Option<u64>,
}

impl JournalLimitOverrides {
    fn apply(self, limits: &mut JournalLimits) {
        macro_rules! apply {
            ($field:ident) => {
                if let Some(value) = self.$field {
                    limits.$field = value;
                }
            };
        }
        apply!(journal_max_events);
        apply!(journal_max_bytes);
        apply!(journal_min_age_secs);
        apply!(journal_event_max_bytes);
        apply!(client_event_queue_max_events);
        apply!(client_event_queue_max_bytes);
        apply!(events_ws_ping_interval_ms);
        apply!(events_ws_idle_timeout_ms);
        apply!(command_rate_per_client);
    }
}

fn usize_as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

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

// ─────────────────────── 附录 A4:命令/审计 retention 与 GC ───────────────────────

/// 单个 A4 参数的三元组描述(默认 / 允许范围 / hard cap)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionParamSpec {
    /// A4 参数名(`config.toml [limits]` 键)。
    pub name: &'static str,
    pub default: u64,
    pub min: u64,
    pub max: u64,
    /// 安全上限:取值不得超过;A4 全部行 hard cap == 允许范围上限。
    pub hard_cap: u64,
}

/// A4 全量参数表(表驱动校验与 `limits_defaults` 契约测试的权威输入)。
pub const RETENTION_PARAMS: [RetentionParamSpec; 6] = [
    RetentionParamSpec {
        name: "receipt_retention_days",
        default: 30,
        min: 7,
        max: 365,
        hard_cap: 365,
    },
    RetentionParamSpec {
        name: "receipt_max_rows_per_store",
        default: 200_000,
        min: 10_000,
        max: 1_000_000,
        hard_cap: 1_000_000,
    },
    RetentionParamSpec {
        name: "operation_retention_days",
        default: 90,
        min: 7,
        max: 365,
        hard_cap: 365,
    },
    RetentionParamSpec {
        name: "audit_retention_days",
        default: 365,
        min: 30,
        max: 3_650,
        hard_cap: 3_650,
    },
    RetentionParamSpec {
        name: "gc_interval_ms",
        default: 3_600_000,
        min: 300_000,
        max: 86_400_000,
        hard_cap: 86_400_000,
    },
    RetentionParamSpec {
        name: "operation_progress_interval_ms",
        default: 1_000,
        min: 250,
        max: 10_000,
        hard_cap: 10_000,
    },
];

/// 超出允许范围的配置值(A4:任何取值不得超过 hard cap)。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid_limits:{name}={value} 超出允许范围 [{min}, {max}](hard cap {hard_cap})")]
pub struct RetentionLimitsError {
    pub name: &'static str,
    pub value: u64,
    pub min: u64,
    pub max: u64,
    pub hard_cap: u64,
}

/// A4 retention/GC 参数的运行时取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionLimits {
    pub receipt_retention_days: u64,
    pub receipt_max_rows_per_store: u64,
    pub operation_retention_days: u64,
    pub audit_retention_days: u64,
    pub gc_interval_ms: u64,
    pub operation_progress_interval_ms: u64,
}

impl Default for RetentionLimits {
    fn default() -> Self {
        Self {
            receipt_retention_days: RETENTION_PARAMS[0].default,
            receipt_max_rows_per_store: RETENTION_PARAMS[1].default,
            operation_retention_days: RETENTION_PARAMS[2].default,
            audit_retention_days: RETENTION_PARAMS[3].default,
            gc_interval_ms: RETENTION_PARAMS[4].default,
            operation_progress_interval_ms: RETENTION_PARAMS[5].default,
        }
    }
}

impl RetentionLimits {
    /// 校验全部取值落在 A4 允许范围内(范围上限即 hard cap)。
    /// 越界一律拒绝,不静默钳制——调用方(未来的 config 装配)负责回错。
    pub fn validate(&self) -> Result<(), RetentionLimitsError> {
        for (value, spec) in [
            (self.receipt_retention_days, &RETENTION_PARAMS[0]),
            (self.receipt_max_rows_per_store, &RETENTION_PARAMS[1]),
            (self.operation_retention_days, &RETENTION_PARAMS[2]),
            (self.audit_retention_days, &RETENTION_PARAMS[3]),
            (self.gc_interval_ms, &RETENTION_PARAMS[4]),
            (self.operation_progress_interval_ms, &RETENTION_PARAMS[5]),
        ] {
            if value < spec.min || value > spec.max {
                return Err(RetentionLimitsError {
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
