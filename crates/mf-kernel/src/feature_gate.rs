//! Durable feature registry 与 reader-before-writer gate(T5b,Issue #45;
//! spec §13/迁移规则)。
//!
//! 每个 durable feature 有两个版本号:N = reader 版本(已可读旧数据或
//! 阻断),N+1 = writer 版本(新数据写入)。writer enable 决策要求:
//! ① bundle 已声明该 feature(#35 compatibility);② 当前 Core 的
//! writer 开关(#45 前 production deny);③ 读者已就绪(reader ≥ N)。
//! 出现 writer 数据后,回滚只能落到声明可读该 feature 的 bundle
//! (T5a eligibility 已实现拒绝);未知 feature/version 一律安全阻断。

use std::collections::BTreeMap;

/// 一个 durable feature 的版本契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureContract {
    /// 读者版本(N):`reader >= N` 才能读旧数据,否则明确阻断。
    pub reader_version: u32,
    /// 写者版本(N+1):writer enable 需要 bundle+runtime 双授权。
    pub writer_version: u32,
}

/// 阻断原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeatureGateProblem {
    #[error("未知 durable feature `{0}`(安全阻断,不猜语义)")]
    UnknownFeature(String),
    #[error("feature `{feature}` reader v{have} 低于要求的 v{need}:明确阻断(升级 bundle 后重读)")]
    ReaderTooOld {
        feature: String,
        have: u32,
        need: u32,
    },
    #[error("feature `{0}` writer 未授权(runtime gate 关闭):无生产写路径")]
    WriterNotEnabled(String),
    #[error("feature `{feature}` writer 版本漂移(bundle={bundle}, registry={registry})")]
    WriterVersionDrift {
        feature: String,
        bundle: u32,
        registry: u32,
    },
}

/// registry(Core 生命周期内静态;writer 授权由 config 注入)。
pub struct DurableFeatureRegistry {
    contracts: BTreeMap<String, FeatureContract>,
    /// runtime writer 开关(production 默认 deny;#47 handoff 后开)。
    writer_enabled: BTreeMap<String, bool>,
}

impl DurableFeatureRegistry {
    /// T1/T3/T4 已落地的 durable feature 全集与版本契约。
    pub fn baseline() -> Self {
        let mut contracts = BTreeMap::new();
        let mut add = |name: &str, reader: u32| {
            contracts.insert(
                name.to_string(),
                FeatureContract {
                    reader_version: reader,
                    writer_version: reader + 1,
                },
            );
        };
        // T1 存储 durable 面
        add("project-schema-identity", 1);
        add("catalog-v2", 1);
        add("service-registry", 1);
        add("command-receipt-outbox", 1);
        add("operation-saga", 1);
        // T3 durable 面
        add("terminal-transcript", 1); // #32 v11 列
                                       // T4 durable 面
        add("manifest-v3", 1);
        add("installation-receipt", 1);
        // Root host(#33 fake seam;receipt 面)
        add("root-host-receipt", 1);
        Self {
            contracts,
            writer_enabled: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, feature: &str, contract: FeatureContract) {
        self.contracts.insert(feature.to_string(), contract);
    }

    /// writer 授权注入(config/handoff;production 默认全 deny)。
    pub fn set_writer_enabled(&mut self, feature: &str, enabled: bool) {
        self.writer_enabled.insert(feature.to_string(), enabled);
    }

    /// reader 判定:N reader——能读则 Ok;版本过老明确阻断;未知 feature
    /// 安全阻断(Bridge A 对全部 T1/T3/T4 durable 状态的统一入口)。
    pub fn check_reader(
        &self,
        feature: &str,
        reader_version: u32,
    ) -> Result<(), FeatureGateProblem> {
        let contract = self
            .contracts
            .get(feature)
            .ok_or_else(|| FeatureGateProblem::UnknownFeature(feature.to_string()))?;
        if reader_version < contract.reader_version {
            return Err(FeatureGateProblem::ReaderTooOld {
                feature: feature.to_string(),
                have: reader_version,
                need: contract.reader_version,
            });
        }
        Ok(())
    }

    /// writer enable 决策(§迁移:N 只登记 reader,N+1 才开 writer):
    /// registry 契约 + runtime 开关 + bundle 声明三方一致才放行。
    pub fn check_writer(
        &self,
        feature: &str,
        bundle_writer_version: Option<u32>,
    ) -> Result<(), FeatureGateProblem> {
        let contract = self
            .contracts
            .get(feature)
            .ok_or_else(|| FeatureGateProblem::UnknownFeature(feature.to_string()))?;
        if !self.writer_enabled.get(feature).copied().unwrap_or(false) {
            return Err(FeatureGateProblem::WriterNotEnabled(feature.to_string()));
        }
        match bundle_writer_version {
            Some(version) if version == contract.writer_version => Ok(()),
            Some(version) => Err(FeatureGateProblem::WriterVersionDrift {
                feature: feature.to_string(),
                bundle: version,
                registry: contract.writer_version,
            }),
            // bundle 未声明该 feature = 该 bundle 不写此类数据
            None => Err(FeatureGateProblem::WriterNotEnabled(feature.to_string())),
        }
    }

    /// 全部已知 feature(reader 版本表;bundle eligibility 的 T5a 汇入)。
    pub fn reader_table(&self) -> BTreeMap<String, u32> {
        self.contracts
            .iter()
            .map(|(name, contract)| (name.clone(), contract.reader_version))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_policy_is_n_writer_n_plus_1() {
        let registry = DurableFeatureRegistry::baseline();
        let table = registry.reader_table();
        assert_eq!(table.get("terminal-transcript"), Some(&1));
        // N+1 关系
        for name in table.keys() {
            let contract = registry.contracts.get(name).unwrap();
            assert_eq!(contract.writer_version, contract.reader_version + 1);
        }
    }

    #[test]
    fn unknown_feature_and_old_reader_block_safely() {
        let registry = DurableFeatureRegistry::baseline();
        assert!(matches!(
            registry.check_reader("future-thing", 9),
            Err(FeatureGateProblem::UnknownFeature(_))
        ));
        assert!(matches!(
            registry.check_reader("terminal-transcript", 0),
            Err(FeatureGateProblem::ReaderTooOld {
                have: 0,
                need: 1,
                ..
            })
        ));
        assert!(registry.check_reader("terminal-transcript", 1).is_ok());
    }

    #[test]
    fn writer_requires_runtime_and_bundle_agreement() {
        let mut registry = DurableFeatureRegistry::baseline();
        // 默认 deny:无生产写路径(T4 writer gate 关闭)
        assert!(matches!(
            registry.check_writer("installation-receipt", Some(2)),
            Err(FeatureGateProblem::WriterNotEnabled(_))
        ));
        // runtime 开了但 bundle 未声明
        registry.set_writer_enabled("installation-receipt", true);
        assert!(matches!(
            registry.check_writer("installation-receipt", None),
            Err(FeatureGateProblem::WriterNotEnabled(_))
        ));
        // bundle 版本漂移
        assert!(matches!(
            registry.check_writer("installation-receipt", Some(3)),
            Err(FeatureGateProblem::WriterVersionDrift {
                bundle: 3,
                registry: 2,
                ..
            })
        ));
        // 三方一致
        registry
            .check_writer("installation-receipt", Some(2))
            .unwrap();
    }
}
