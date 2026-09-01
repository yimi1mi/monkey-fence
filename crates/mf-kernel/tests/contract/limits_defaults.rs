//! T1e/T1g/T2b 契约(Issue #20/#22/#24):附录 A1 Workflow 事件与 API、
//! A7 生命周期与 A4 命令/审计 retention 参数的默认值/允许范围/hard cap。
//!
//! 表驱动断言 `JOURNAL_PARAMS`/`LIFECYCLE_PARAMS`/`RETENTION_PARAMS`
//! 与 spec 附录逐行一致;
//! 边界值(min/max)接受、越界(min-1/max+1,即超过 hard cap)拒绝;
//! stale 派生 = 3×heartbeat,command burst 派生 = 3×rate。

use mf_kernel::limits::{
    JournalLimits, JournalParamSpec, LifecycleLimits, LifecycleParamSpec, RetentionLimits,
    RetentionParamSpec, COMMAND_RATE_BURST_MULTIPLIER, DISCOVERY_STALE_HEARTBEATS,
    JOURNAL_MAX_BYTES_DEFAULT, JOURNAL_MAX_EVENTS_DEFAULT,
};

/// 附录 A1 原文行(唯一数值来源):name/default/min/max/hard cap。
fn appendix_a1_rows() -> Vec<JournalParamSpec> {
    vec![
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
    ]
}

#[test]
fn workflow_event_params_match_appendix_a1() {
    let expected = appendix_a1_rows();
    let actual = mf_kernel::limits::JOURNAL_PARAMS.to_vec();
    assert_eq!(actual.len(), expected.len(), "A1 恰好 9 个参数");
    for (spec, want) in actual.iter().zip(&expected) {
        assert_eq!(spec, want, "{} 参数三元组", want.name);
        assert!(
            spec.min <= spec.default && spec.default <= spec.max,
            "{} 默认值必须在允许范围内",
            want.name
        );
        assert!(
            spec.max <= spec.hard_cap,
            "{} 范围不得超过 hard cap",
            want.name
        );
    }
}

#[test]
fn journal_defaults_match_appendix_a1_and_legacy_constants() {
    let defaults = JournalLimits::default();
    let rows = appendix_a1_rows();
    assert_eq!(defaults.journal_max_events, rows[0].default as usize);
    assert_eq!(defaults.journal_max_bytes, rows[1].default as usize);
    assert_eq!(defaults.journal_min_age_secs, rows[2].default);
    assert_eq!(defaults.journal_event_max_bytes, rows[3].default as usize);
    assert_eq!(
        defaults.client_event_queue_max_events,
        rows[4].default as usize
    );
    assert_eq!(
        defaults.client_event_queue_max_bytes,
        rows[5].default as usize
    );
    assert_eq!(defaults.events_ws_ping_interval_ms, rows[6].default);
    assert_eq!(defaults.events_ws_idle_timeout_ms, rows[7].default);
    assert_eq!(defaults.command_rate_per_client, rows[8].default);
    assert_eq!(JOURNAL_MAX_EVENTS_DEFAULT, 20_000);
    assert_eq!(JOURNAL_MAX_BYTES_DEFAULT, 64 * 1024 * 1024);
    assert_eq!(defaults.journal_max_events, JOURNAL_MAX_EVENTS_DEFAULT);
    assert_eq!(defaults.journal_max_bytes, JOURNAL_MAX_BYTES_DEFAULT);
    defaults.validate().unwrap();
}

#[test]
fn journal_range_boundaries_are_accepted() {
    JournalLimits {
        journal_max_events: 1_000,
        journal_max_bytes: 4 * 1024 * 1024,
        journal_min_age_secs: 0,
        journal_event_max_bytes: 64 * 1024,
        client_event_queue_max_events: 100,
        client_event_queue_max_bytes: 1024 * 1024,
        events_ws_ping_interval_ms: 5_000,
        events_ws_idle_timeout_ms: 30_000,
        command_rate_per_client: 5,
    }
    .validate()
    .unwrap();

    JournalLimits {
        journal_max_events: 100_000,
        journal_max_bytes: 256 * 1024 * 1024,
        journal_min_age_secs: 86_400,
        journal_event_max_bytes: 2 * 1024 * 1024,
        client_event_queue_max_events: 20_000,
        client_event_queue_max_bytes: 64 * 1024 * 1024,
        events_ws_ping_interval_ms: 60_000,
        events_ws_idle_timeout_ms: 300_000,
        command_rate_per_client: 200,
    }
    .validate()
    .unwrap();
}

#[test]
fn journal_out_of_range_is_rejected_with_param_name() {
    for index in 0..mf_kernel::limits::JOURNAL_PARAMS.len() {
        let spec = mf_kernel::limits::JOURNAL_PARAMS[index];
        let below = spec.min.checked_sub(1);
        for bad in below.into_iter().chain(std::iter::once(spec.hard_cap + 1)) {
            let mut limits = JournalLimits::default();
            match index {
                0 => limits.journal_max_events = bad as usize,
                1 => limits.journal_max_bytes = bad as usize,
                2 => limits.journal_min_age_secs = bad,
                3 => limits.journal_event_max_bytes = bad as usize,
                4 => limits.client_event_queue_max_events = bad as usize,
                5 => limits.client_event_queue_max_bytes = bad as usize,
                6 => limits.events_ws_ping_interval_ms = bad,
                7 => limits.events_ws_idle_timeout_ms = bad,
                _ => limits.command_rate_per_client = bad,
            }
            let error = limits.validate().unwrap_err();
            assert_eq!(error.name, spec.name);
            assert_eq!(error.value, bad);
            assert_eq!(error.min, spec.min);
            assert_eq!(error.max, spec.max);
            assert_eq!(error.hard_cap, spec.hard_cap);
        }
    }
}

#[test]
fn command_burst_is_derived_as_three_times_rate() {
    assert_eq!(COMMAND_RATE_BURST_MULTIPLIER, 3);
    assert_eq!(JournalLimits::default().command_burst_per_client(), 120);
    assert_eq!(
        JournalLimits {
            command_rate_per_client: 5,
            ..Default::default()
        }
        .command_burst_per_client(),
        15
    );
    assert_eq!(
        JournalLimits {
            command_rate_per_client: 200,
            ..Default::default()
        }
        .command_burst_per_client(),
        600
    );
}

/// 附录 A7 原文行(唯一数值来源):name/default/min/max/hard cap。
fn appendix_a7_rows() -> Vec<LifecycleParamSpec> {
    vec![
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
    ]
}

/// LIFECYCLE_PARAMS 与附录 A7 逐行一致(参数名、默认、范围、hard cap)。
#[test]
fn lifecycle_params_match_appendix_a7() {
    let expected = appendix_a7_rows();
    let actual = mf_kernel::limits::LIFECYCLE_PARAMS.to_vec();
    assert_eq!(actual.len(), expected.len(), "A7 恰好 5 个参数");
    for (spec, want) in actual.iter().zip(&expected) {
        assert_eq!(spec.name, want.name);
        assert_eq!(spec.default, want.default, "{} 默认值", want.name);
        assert_eq!(spec.min, want.min, "{} 允许范围下限", want.name);
        assert_eq!(spec.max, want.max, "{} 允许范围上限", want.name);
        assert_eq!(spec.hard_cap, want.hard_cap, "{} hard cap", want.name);
        assert!(
            spec.min <= spec.default && spec.default <= spec.max,
            "{} 默认值必须在允许范围内",
            want.name
        );
    }
}

/// `LifecycleLimits::default()` 等于 A7 默认值且自身合法。
#[test]
fn default_limits_equal_appendix_defaults_and_validate() {
    let defaults = LifecycleLimits::default();
    for (value, spec) in [
        (defaults.discovery_heartbeat_ms, "discovery_heartbeat_ms"),
        (
            defaults.shutdown_freeze_grace_ms,
            "shutdown_freeze_grace_ms",
        ),
        (
            defaults.shutdown_drain_timeout_ms,
            "shutdown_drain_timeout_ms",
        ),
        (defaults.forced_kill_grace_ms, "forced_kill_grace_ms"),
        (
            defaults.handoff_reacquire_window_ms,
            "handoff_reacquire_window_ms",
        ),
    ] {
        let want = appendix_a7_rows()
            .into_iter()
            .find(|row| row.name == spec)
            .unwrap();
        assert_eq!(value, want.default, "{spec} 默认值");
    }
    defaults.validate().unwrap();
}

/// 边界值全部接受:min 与 max(hard cap)是合法配置。
#[test]
fn range_boundaries_accepted() {
    LifecycleLimits {
        discovery_heartbeat_ms: 1_000,
        shutdown_freeze_grace_ms: 30_000,
        shutdown_drain_timeout_ms: 30_000,
        forced_kill_grace_ms: 2_000,
        handoff_reacquire_window_ms: 15_000,
    }
    .validate()
    .unwrap();

    LifecycleLimits {
        discovery_heartbeat_ms: 30_000,
        shutdown_freeze_grace_ms: 30_000,
        shutdown_drain_timeout_ms: 600_000,
        forced_kill_grace_ms: 60_000,
        handoff_reacquire_window_ms: 300_000,
    }
    .validate()
    .unwrap();
}

/// 越界拒绝:低于 min、超过 max/hard cap 都报带参数名的稳定错误。
#[test]
fn out_of_range_rejected_with_param_name() {
    for index in 0..mf_kernel::limits::LIFECYCLE_PARAMS.len() {
        for bad in [
            mf_kernel::limits::LIFECYCLE_PARAMS[index].min - 1,
            mf_kernel::limits::LIFECYCLE_PARAMS[index].max + 1,
        ] {
            let mut limits = LifecycleLimits::default();
            match index {
                0 => limits.discovery_heartbeat_ms = bad,
                1 => limits.shutdown_freeze_grace_ms = bad,
                2 => limits.shutdown_drain_timeout_ms = bad,
                3 => limits.forced_kill_grace_ms = bad,
                _ => limits.handoff_reacquire_window_ms = bad,
            }
            let error = limits.validate().unwrap_err();
            assert_eq!(error.name, mf_kernel::limits::LIFECYCLE_PARAMS[index].name);
            assert_eq!(error.value, bad);
            assert_eq!(
                error.hard_cap,
                mf_kernel::limits::LIFECYCLE_PARAMS[index].hard_cap
            );
        }
    }
}

/// 派生规则(§11.1):stale = 3×heartbeat,随 heartbeat 取值变化,
/// 不可独立配置(结构体上没有独立字段——由派生方法提供)。
#[test]
fn stale_threshold_is_derived_three_heartbeats() {
    assert_eq!(DISCOVERY_STALE_HEARTBEATS, 3);
    let defaults = LifecycleLimits::default();
    assert_eq!(defaults.discovery_stale_after_ms(), 15_000);

    let minimum = LifecycleLimits {
        discovery_heartbeat_ms: 1_000,
        ..Default::default()
    };
    assert_eq!(minimum.discovery_stale_after_ms(), 3_000);

    let maximum = LifecycleLimits {
        discovery_heartbeat_ms: 30_000,
        ..Default::default()
    };
    assert_eq!(maximum.discovery_stale_after_ms(), 90_000);
}

/// 越界的 limits 不能进入 acquire(装配层校验,Fail fast)。
#[test]
fn invalid_limits_block_owner_lock_setup_validate() {
    let limits = LifecycleLimits {
        discovery_heartbeat_ms: 30_001, // 超过 hard cap
        ..Default::default()
    };
    assert!(limits.validate().is_err());
}

// ─────────────────────── 附录 A4:命令/审计 retention( Issue #22) ───────────────────────

/// 附录 A4 原文行(唯一数值来源):name/default/min/max/hard cap。
fn appendix_a4_rows() -> Vec<RetentionParamSpec> {
    vec![
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
    ]
}

/// `RETENTION_PARAMS` 与附录 A4 逐行一致(参数名、默认、范围、hard cap)。
#[test]
fn retention_params_match_appendix_a4() {
    let expected = appendix_a4_rows();
    let actual = mf_kernel::limits::RETENTION_PARAMS.to_vec();
    assert_eq!(actual.len(), expected.len(), "A4 恰好 6 个参数");
    for (spec, want) in actual.iter().zip(&expected) {
        assert_eq!(spec.name, want.name);
        assert_eq!(spec.default, want.default, "{} 默认值", want.name);
        assert_eq!(spec.min, want.min, "{} 允许范围下限", want.name);
        assert_eq!(spec.max, want.max, "{} 允许范围上限", want.name);
        assert_eq!(spec.hard_cap, want.hard_cap, "{} hard cap", want.name);
        assert!(
            spec.min <= spec.default && spec.default <= spec.max,
            "{} 默认值必须在允许范围内",
            want.name
        );
    }
}

/// `RetentionLimits::default()` 等于 A4 默认值且自身合法。
#[test]
fn retention_defaults_equal_appendix_and_validate() {
    let defaults = RetentionLimits::default();
    let rows = appendix_a4_rows();
    assert_eq!(defaults.receipt_retention_days, rows[0].default);
    assert_eq!(defaults.receipt_max_rows_per_store, rows[1].default);
    assert_eq!(defaults.operation_retention_days, rows[2].default);
    assert_eq!(defaults.audit_retention_days, rows[3].default);
    assert_eq!(defaults.gc_interval_ms, rows[4].default);
    assert_eq!(defaults.operation_progress_interval_ms, rows[5].default);
    defaults.validate().unwrap();
}

/// 边界值全部接受:min 与 max(hard cap)是合法配置。
#[test]
fn retention_range_boundaries_accepted() {
    RetentionLimits {
        receipt_retention_days: 7,
        receipt_max_rows_per_store: 10_000,
        operation_retention_days: 365,
        audit_retention_days: 30,
        gc_interval_ms: 86_400_000,
        operation_progress_interval_ms: 250,
    }
    .validate()
    .unwrap();
    RetentionLimits {
        receipt_retention_days: 365,
        receipt_max_rows_per_store: 1_000_000,
        operation_retention_days: 7,
        audit_retention_days: 3_650,
        gc_interval_ms: 300_000,
        operation_progress_interval_ms: 10_000,
    }
    .validate()
    .unwrap();
}

/// 越界拒绝:低于 min、超过 max/hard cap 都报带参数名的稳定错误。
#[test]
fn retention_out_of_range_rejected_with_param_name() {
    for index in 0..mf_kernel::limits::RETENTION_PARAMS.len() {
        for bad in [
            mf_kernel::limits::RETENTION_PARAMS[index].min - 1,
            mf_kernel::limits::RETENTION_PARAMS[index].max + 1,
        ] {
            let mut limits = RetentionLimits::default();
            match index {
                0 => limits.receipt_retention_days = bad,
                1 => limits.receipt_max_rows_per_store = bad,
                2 => limits.operation_retention_days = bad,
                3 => limits.audit_retention_days = bad,
                4 => limits.gc_interval_ms = bad,
                _ => limits.operation_progress_interval_ms = bad,
            }
            let error = limits.validate().unwrap_err();
            assert_eq!(error.name, mf_kernel::limits::RETENTION_PARAMS[index].name);
            assert_eq!(error.value, bad);
            assert_eq!(
                error.hard_cap,
                mf_kernel::limits::RETENTION_PARAMS[index].hard_cap
            );
        }
    }
}

#[test]
fn journal_limits_load_partial_config_and_ignore_other_limit_groups() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(
        &path,
        "[limits]\njournal_max_events = 1234\nclient_event_queue_max_events = 321\nreceipt_retention_days = 30\n",
    )
    .unwrap();
    let limits = JournalLimits::load_from_path(&path).unwrap();
    assert_eq!(limits.journal_max_events, 1_234);
    assert_eq!(limits.client_event_queue_max_events, 321);
    assert_eq!(limits.journal_max_bytes, JOURNAL_MAX_BYTES_DEFAULT);
}

#[test]
fn journal_limits_config_parse_and_range_errors_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let malformed = tmp.path().join("malformed.toml");
    std::fs::write(&malformed, "[limits\njournal_max_events=1234").unwrap();
    assert!(JournalLimits::load_from_path(&malformed)
        .unwrap_err()
        .to_string()
        .starts_with("limits_config_parse:"));

    let invalid = tmp.path().join("invalid.toml");
    std::fs::write(&invalid, "[limits]\njournal_max_events=999\n").unwrap();
    assert!(JournalLimits::load_from_path(&invalid)
        .unwrap_err()
        .to_string()
        .contains("invalid_limits:journal_max_events=999"));
}

#[test]
fn missing_journal_limits_config_uses_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        JournalLimits::load_from_path(&tmp.path().join("missing.toml")).unwrap(),
        JournalLimits::default()
    );
}

#[test]
fn mf_agent_config_roundtrip_preserves_core_limits_table() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(
        &path,
        "[limits]\njournal_max_events = 4321\njournal_min_age_secs = 99\n",
    )
    .unwrap();
    let config = mf_agent::Config::load_from_path(&path).unwrap();
    config.save_to_path(&path).unwrap();
    let limits = JournalLimits::load_from_path(&path).unwrap();
    assert_eq!(limits.journal_max_events, 4_321);
    assert_eq!(limits.journal_min_age_secs, 99);
}
