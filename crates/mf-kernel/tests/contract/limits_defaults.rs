//! T1e 契约(Issue #20):附录 A7 生命周期参数默认值/允许范围/hard cap。
//!
//! 表驱动断言 `LIFECYCLE_PARAMS` 与 spec 附录 A7 逐行一致;边界值
//! (min/max)接受、越界(min-1/max+1,即超过 hard cap)拒绝;stale 派生
//! = 3×heartbeat。

use mf_kernel::limits::{LifecycleLimits, LifecycleParamSpec, DISCOVERY_STALE_HEARTBEATS};

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
