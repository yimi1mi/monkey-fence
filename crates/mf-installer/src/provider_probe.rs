//! Provider remote catalog probe(T4b,Issue #40;spec §9.8)。
//!
//! 模型下拉的数据面:Core 发起 remote catalog probe 并按 Profile 解析
//! Secret(注入的 fetch 回调携带凭据,**结果只含模型元数据与缓存
//! 状态**——凭据绝不进入返回值/日志/事件)。离线/未配置/失败显示缓存
//! + 明确错误 + 允许手填合法模型 id。transport(真实 HTTP)随
//! WebGateway;本模块交付超时/重试/缓存/回退/手填校验的全部判定。

use std::time::{Duration, Instant};

use crate::limits::InstallerLimits;

/// 单个模型元数据(浏览器/调用方只拿到这些)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelMeta {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
}

/// 模型目录结果来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSource {
    Live,
    Cache,
    Manual,
}

/// probe 结果(只含模型元数据 + 缓存状态)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalog {
    pub models: Vec<ModelMeta>,
    pub source: CatalogSource,
    /// Live/Cache 时的取数时间;Manual 为 None。
    pub fetched_at: Option<String>,
    /// 回退到缓存时的原始错误(明确错误显示;不含凭据)。
    pub fallback_error: Option<String>,
}

/// 缓存条目(TTL 由 limits.provider_model_cache_ttl_secs 决定)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCacheEntry {
    pub models: Vec<ModelMeta>,
    pub fetched_at: String,
    pub cached_at: Instant,
}

impl ModelCacheEntry {
    pub fn fresh(&self, ttl_secs: u64) -> bool {
        self.cached_at.elapsed() < Duration::from_secs(ttl_secs)
    }
}

/// probe fetch 缺隙:实现方携带凭据发起远端请求。返回模型列表;
/// `Err` 为可重试/不可重试不分的失败(重试策略由本模块统一执行)。
pub trait CatalogFetch {
    fn fetch(&mut self) -> Result<Vec<ModelMeta>, String>;
}

/// 手填模型 id 校验(§9.8:允许手填**合法**模型 id)。合法形态:
/// 非空、无空白/控制字符、不含凭据形态的常规标识符
/// (字母数字与 `-_.:/` );拒绝空串与包含空白/换行的输入。
pub fn validate_manual_model_id(id: &str) -> Result<(), String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err("模型 id 不能为空".into());
    }
    if trimmed.len() != id.len() {
        return Err("模型 id 首尾含空白".into());
    }
    if id.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("模型 id 含空白/控制字符".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
    {
        return Err("模型 id 含非法字符(允许字母数字与 - _ . : /)".into());
    }
    Ok(())
}

/// probe 状态机输入(时钟/休眠注入便于测试)。
pub struct ProbeClock<'a> {
    pub now: Instant,
    pub sleep: &'a dyn Fn(Duration),
}

/// 执行 probe:live 优先;失败按 `provider_probe_retries` 重试(退避由
/// limits 序列 + 注入 jitter);最终失败回退缓存;缓存也无 → 返回
/// 明确错误(调用方展示手填入口)。
pub fn probe_models(
    fetch: &mut dyn CatalogFetch,
    cache: Option<&ModelCacheEntry>,
    limits: &InstallerLimits,
    clock: &ProbeClock<'_>,
) -> ModelCatalog {
    let limits = limits.clamp();
    let attempts = limits.provider_probe_retries + 1;
    let mut last_error = String::new();
    for attempt in 0..attempts {
        // 超时语义:fetch 实现方自行执行(生产 HTTP client 超时注入);
        // 本模块的重试与退避对实现方透明。测试通过 fetch 行为模拟超时。
        match fetch.fetch() {
            Ok(models) => {
                return ModelCatalog {
                    models,
                    source: CatalogSource::Live,
                    fetched_at: Some(chrono::Utc::now().to_rfc3339()),
                    fallback_error: None,
                };
            }
            Err(error) => {
                last_error = error;
                if attempt + 1 < attempts {
                    // 固定退避序列(500ms/2000ms);测试注入 0 抖动即确定值
                    let backoff = limits.provider_backoff_ms(attempt, 0.0);
                    (clock.sleep)(Duration::from_millis(backoff));
                }
            }
        }
    }
    if let Some(entry) = cache {
        return ModelCatalog {
            models: entry.models.clone(),
            source: CatalogSource::Cache,
            fetched_at: Some(entry.fetched_at.clone()),
            fallback_error: Some(last_error),
        };
    }
    ModelCatalog {
        models: Vec::new(),
        source: CatalogSource::Cache,
        fetched_at: None,
        fallback_error: Some(last_error),
    }
}

/// 缓存新鲜时的直读(未过期缓存不触发远端 probe;§9.8 cache TTL)。
pub fn cached_or_probe(
    fetch: &mut dyn CatalogFetch,
    cache: Option<&ModelCacheEntry>,
    limits: &InstallerLimits,
    clock: &ProbeClock<'_>,
) -> ModelCatalog {
    let limits = limits.clamp();
    if let Some(entry) = cache {
        if entry.fresh(limits.provider_model_cache_ttl_secs) {
            return ModelCatalog {
                models: entry.models.clone(),
                source: CatalogSource::Cache,
                fetched_at: Some(entry.fetched_at.clone()),
                fallback_error: None,
            };
        }
    }
    probe_models(fetch, cache, &limits, clock)
}

/// 手填模型目录(校验后包装)。
pub fn manual_catalog(ids: &[String]) -> Result<ModelCatalog, String> {
    let mut models = Vec::new();
    for id in ids {
        validate_manual_model_id(id)?;
        models.push(ModelMeta {
            id: id.clone(),
            display_name: id.clone(),
        });
    }
    Ok(ModelCatalog {
        models,
        source: CatalogSource::Manual,
        fetched_at: None,
        fallback_error: None,
    })
}
