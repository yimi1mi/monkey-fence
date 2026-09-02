//! T4b 契约(Issue #40;spec §9.8):probe live/retry/cache/fallback、
//! 手填校验与 limits(附录 A5)。

use std::time::{Duration, Instant};

use mf_installer::limits::InstallerLimits;
use mf_installer::provider_probe::{
    cached_or_probe, manual_catalog, probe_models, CatalogFetch, CatalogSource, ModelCacheEntry,
    ModelMeta, ProbeClock,
};

struct ScriptedFetch {
    results: Vec<Result<Vec<ModelMeta>, String>>,
    calls: usize,
    slept: Vec<Duration>,
}

impl ScriptedFetch {
    fn new(results: Vec<Result<Vec<ModelMeta>, String>>) -> Self {
        Self {
            results,
            calls: 0,
            slept: Vec::new(),
        }
    }
}

impl CatalogFetch for ScriptedFetch {
    fn fetch(&mut self) -> Result<Vec<ModelMeta>, String> {
        let index = self.calls.min(self.results.len().saturating_sub(1));
        self.calls += 1;
        self.results[index].clone()
    }
}

fn no_sleep_clock() -> ProbeClock<'static> {
    // 静态 no-op sleep(测试不真正等待)
    static NOOP: fn(Duration) = |_| {};
    ProbeClock {
        now: Instant::now(),
        sleep: &NOOP,
    }
}

fn model(id: &str) -> ModelMeta {
    ModelMeta {
        id: id.into(),
        display_name: id.into(),
    }
}

#[test]
fn live_probe_returns_models_without_secrets() {
    let mut fetch = ScriptedFetch::new(vec![Ok(vec![model("gpt-5"), model("gpt-5-mini")])]);
    let catalog = probe_models(
        &mut fetch,
        None,
        &InstallerLimits::default(),
        &no_sleep_clock(),
    );
    assert_eq!(catalog.source, CatalogSource::Live);
    assert_eq!(catalog.models.len(), 2);
    assert!(catalog.fallback_error.is_none());
    // 响应只含模型元数据与缓存状态:类型上无凭据字段(编译期保证)
    assert_eq!(fetch.calls, 1);
}

#[test]
fn retries_then_falls_back_to_cache() {
    // 两次失败(retries=2 → 共 3 次尝试),然后回退缓存
    let mut fetch = ScriptedFetch::new(vec![
        Err("timeout".into()),
        Err("timeout".into()),
        Err("timeout".into()),
    ]);
    let cache = ModelCacheEntry {
        models: vec![model("cached-model")],
        fetched_at: "2026-09-02T00:00:00Z".into(),
        cached_at: Instant::now() - Duration::from_secs(3600),
    };
    let catalog = probe_models(
        &mut fetch,
        Some(&cache),
        &InstallerLimits::default(),
        &no_sleep_clock(),
    );
    assert_eq!(fetch.calls, 3, "retries=2 → 3 次尝试");
    assert_eq!(catalog.source, CatalogSource::Cache);
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(
        catalog.fallback_error.as_deref(),
        Some("timeout"),
        "回退必须带明确错误"
    );
}

#[test]
fn retry_succeeds_on_second_attempt() {
    let mut fetch = ScriptedFetch::new(vec![Err("flaky".into()), Ok(vec![model("gpt-5")])]);
    let catalog = probe_models(
        &mut fetch,
        None,
        &InstallerLimits::default(),
        &no_sleep_clock(),
    );
    assert_eq!(catalog.source, CatalogSource::Live);
    assert_eq!(fetch.calls, 2);
}

#[test]
fn no_cache_and_all_fail_yields_explicit_error() {
    let mut fetch = ScriptedFetch::new(vec![Err("offline".into())]);
    let catalog = probe_models(
        &mut fetch,
        None,
        &InstallerLimits::default(),
        &no_sleep_clock(),
    );
    assert!(catalog.models.is_empty());
    assert_eq!(catalog.fallback_error.as_deref(), Some("offline"));
}

#[test]
fn fresh_cache_short_circuits_probe() {
    let mut fetch = ScriptedFetch::new(vec![Ok(vec![model("live")])]);
    let cache = ModelCacheEntry {
        models: vec![model("cached")],
        fetched_at: "2026-09-02T00:00:00Z".into(),
        cached_at: Instant::now(),
    };
    let catalog = cached_or_probe(
        &mut fetch,
        Some(&cache),
        &InstallerLimits::default(),
        &no_sleep_clock(),
    );
    assert_eq!(catalog.source, CatalogSource::Cache);
    assert_eq!(catalog.models[0].id, "cached");
    assert_eq!(fetch.calls, 0, "未过期缓存不触发远端 probe");
}

#[test]
fn stale_cache_triggers_live_probe() {
    let mut fetch = ScriptedFetch::new(vec![Ok(vec![model("live")])]);
    let cache = ModelCacheEntry {
        models: vec![model("cached")],
        fetched_at: "2026-09-01T00:00:00Z".into(),
        cached_at: Instant::now() - Duration::from_secs(3600),
    };
    let catalog = cached_or_probe(
        &mut fetch,
        Some(&cache),
        &InstallerLimits::default(),
        &no_sleep_clock(),
    );
    assert_eq!(catalog.source, CatalogSource::Live);
    assert_eq!(catalog.models[0].id, "live");
}

#[test]
fn manual_models_are_validated() {
    assert!(manual_catalog(&["gpt-5".into(), "deepseek/chat-v3".into()]).is_ok());
    let catalog = manual_catalog(&["gpt-5".into()]).unwrap();
    assert_eq!(catalog.source, CatalogSource::Manual);
    for bad in [
        "",
        " ",
        " leading",
        "trailing ",
        "with space",
        "line\nbreak",
        "weird$char",
    ] {
        assert!(
            manual_catalog(&[bad.into()]).is_err(),
            "非法手填必须拒绝:{bad:?}"
        );
    }
}

#[test]
fn limits_defaults_and_clamp_match_appendix_a5() {
    let defaults = InstallerLimits::default();
    assert_eq!(defaults.discovery_probe_timeout_ms, 5_000);
    assert_eq!(defaults.provider_model_cache_ttl_secs, 300);
    assert_eq!(defaults.provider_probe_timeout_ms, 10_000);
    assert_eq!(defaults.provider_probe_retries, 2);
    assert_eq!(mf_installer::limits::INSTALL_REDIRECT_MAX, 5);
    // 钳制
    let clamped = InstallerLimits {
        discovery_probe_timeout_ms: 1,
        provider_model_cache_ttl_secs: 1,
        provider_probe_timeout_ms: 1,
        provider_probe_retries: 9,
        ..InstallerLimits::default()
    }
    .clamp();
    assert_eq!(clamped.discovery_probe_timeout_ms, 1_000);
    assert_eq!(clamped.provider_model_cache_ttl_secs, 60);
    assert_eq!(clamped.provider_probe_timeout_ms, 2_000);
    assert_eq!(clamped.provider_probe_retries, 3, "retries hard cap 3");
    // 固定退避序列(0 抖动):500ms、2000ms
    assert_eq!(defaults.provider_backoff_ms(0, 0.0), 500);
    assert_eq!(defaults.provider_backoff_ms(1, 0.0), 2_000);
}
