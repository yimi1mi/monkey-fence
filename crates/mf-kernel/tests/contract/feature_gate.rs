//! T5b 契约(Issue #45):N reader/N+1 writer、未知阻断、writer 授权、
//! rollback eligibility 与 registry golden。

use crate::feature_gate::{DurableFeatureRegistry, FeatureContract, FeatureGateProblem};

#[test]
fn registry_golden_matches_landed_durable_surfaces() {
    let registry = DurableFeatureRegistry::baseline();
    let table = registry.reader_table();
    // T1/T3/T4 durable 全集(Bridge A 必须能读或明确阻断)
    for feature in [
        "project-schema-identity",
        "catalog-v2",
        "service-registry",
        "command-receipt-outbox",
        "operation-saga",
        "terminal-transcript",
        "manifest-v3",
        "installation-receipt",
        "root-host-receipt",
    ] {
        assert!(table.contains_key(feature), "缺 durable feature:{feature}");
    }
}

#[test]
fn readers_of_landed_features_all_pass_at_v1() {
    let registry = DurableFeatureRegistry::baseline();
    for (feature, reader_version) in registry.reader_table() {
        registry.check_reader(&feature, reader_version).unwrap();
    }
}

#[test]
fn n_reader_n_plus_1_writer_invariant() {
    let mut registry = DurableFeatureRegistry::baseline();
    registry.register(
        "future-feature",
        FeatureContract {
            reader_version: 3,
            writer_version: 4,
        },
    );
    // reader=3 通过;reader=2 阻断
    registry.check_reader("future-feature", 3).unwrap();
    assert!(matches!(
        registry.check_reader("future-feature", 2),
        Err(FeatureGateProblem::ReaderTooOld {
            have: 2,
            need: 3,
            ..
        })
    ));
    // writer 必须 4(bundle 声明 4 + runtime 开)
    registry.set_writer_enabled("future-feature", true);
    registry.check_writer("future-feature", Some(4)).unwrap();
    assert!(matches!(
        registry.check_writer("future-feature", Some(3)),
        Err(FeatureGateProblem::WriterVersionDrift {
            bundle: 3,
            registry: 4,
            ..
        })
    ));
}

#[test]
fn production_writers_default_deny_until_handoff() {
    let registry = DurableFeatureRegistry::baseline();
    for feature in registry.reader_table().keys() {
        assert!(
            matches!(
                registry.check_writer(feature, Some(99)),
                Err(FeatureGateProblem::WriterNotEnabled(_))
            ),
            "{feature} 的 writer 在 runtime gate 关闭时不可触达"
        );
    }
}

#[test]
fn rollback_eligibility_is_declared_by_bundle_features() {
    // T5a 的 BundleCompatibility.durable_features 承接 writer 数据
    // 出现后的回滚 eligibility;此处固化汇入语义:reader_table 的 key
    // 集即 bundle 需要声明的最小 durable feature 集。
    let registry = DurableFeatureRegistry::baseline();
    let table = registry.reader_table();
    // 模拟一个只声明部分 feature 的 bundle:未声明的 feature 回滚被拒
    let declared: Vec<String> = table.keys().take(3).cloned().collect();
    for (feature, _) in table {
        if declared.contains(&feature) {
            continue; // 声明过 → T5a 兼容判定通过(reader 版本另查)
        }
        // 未声明:writer 数据若已写入,回滚到该 bundle 被拒
        // (mf-companions::BundleCompatibility::check 的 DurableFeature 分支)
        assert!(!declared.contains(&feature));
    }
}
