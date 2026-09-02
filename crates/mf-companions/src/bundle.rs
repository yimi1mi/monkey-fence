//! Side-by-side whole-bundle manager(T5a,Issue #35)。
//!
//! 目录形态:`~/.monkeyfence/bundles/<bundle-id>/`(versioned,永不
//! 覆盖)+ `current.json` 原子指针(temp 写入 + rename;半写指针在
//! rename 前不可见)。安装流程:完整落盘 → **健康检查**(组件齐全、
//! hash 匹配、兼容性判定)→ 原子切 pointer;检查不过 pointer 不动,
//! 旧 bundle 继续服务。previous bundle 保留;存在新 durable 写入时
//! 禁止自动恢复旧备份(数据格式可能已前滚)。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// bundle 组件种类(全包一致性:不接受组件级更新/回滚)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    Core,
    Assets,
    Companions,
    Mfctl,
    Broker,
    Hosts,
}

/// 单组件的 manifest 条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentEntry {
    pub kind: ComponentKind,
    /// bundle 内相对路径。
    pub path: String,
    pub sha256: String,
}

/// bundle 兼容性声明(与 mf-kernel 的存储 schema 比对)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleCompatibility {
    /// 本 bundle 支持的 Project schema 上限(低于现状 → 拒绝)。
    pub max_project_schema: i64,
    pub max_catalog_schema: i64,
    pub max_service_schema: i64,
    /// bundle 引入的 durable feature 名单(eligibility:旧 bundle 缺
    /// feature 而数据已写入 → 该 feature 标记禁止降级)。
    #[serde(default)]
    pub durable_features: Vec<String>,
    /// elevated host protocol version(#33 的 PROTOCOL_VERSION 对齐)。
    pub host_protocol_version: u32,
}

/// bundle manifest(每 bundle 一份,随目录落盘)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleManifest {
    pub bundle_id: String,
    pub version: String,
    pub components: Vec<ComponentEntry>,
    pub compatibility: BundleCompatibility,
}

/// `current.json` 指针。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentPointer {
    pub bundle_id: String,
    pub switched_at: String,
}

/// 当前存储面观察到的 schema/durable 状态(兼容性判定的对照)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageEligibility {
    pub project_schema: i64,
    pub catalog_schema: i64,
    pub service_schema: i64,
    /// 已写入数据的 durable feature 名单(前滚标记)。
    #[serde(default)]
    pub durable_features_written: Vec<String>,
}

/// 兼容性问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompatibilityProblem {
    #[error("project schema {current} 高于 bundle 支持 {bundle}:schema-only 假兼容拒绝")]
    ProjectSchema { current: i64, bundle: i64 },
    #[error("catalog schema {current} 高于 bundle 支持 {bundle}")]
    CatalogSchema { current: i64, bundle: i64 },
    #[error("service schema {current} 高于 bundle 支持 {bundle}")]
    ServiceSchema { current: i64, bundle: i64 },
    #[error("durable feature `{0}` 已有数据写入,旧 bundle 缺少该 feature:禁止降级")]
    DurableFeature(String),
    #[error("host protocol {current} 高于 bundle 支持 {bundle}")]
    HostProtocol { current: u32, bundle: u32 },
}

impl BundleCompatibility {
    /// 判定 bundle 是否可服务当前存储(schema 前滚不可假兼容;已写入
    /// 的 durable feature 不可缺失——§迁移/回滚)。
    pub fn check(&self, storage: &StorageEligibility) -> Result<(), CompatibilityProblem> {
        if storage.project_schema > self.max_project_schema {
            return Err(CompatibilityProblem::ProjectSchema {
                current: storage.project_schema,
                bundle: self.max_project_schema,
            });
        }
        if storage.catalog_schema > self.max_catalog_schema {
            return Err(CompatibilityProblem::CatalogSchema {
                current: storage.catalog_schema,
                bundle: self.max_catalog_schema,
            });
        }
        if storage.service_schema > self.max_service_schema {
            return Err(CompatibilityProblem::ServiceSchema {
                current: storage.service_schema,
                bundle: self.max_service_schema,
            });
        }
        for written in &storage.durable_features_written {
            if !self.durable_features.contains(written) {
                return Err(CompatibilityProblem::DurableFeature(written.clone()));
            }
        }
        Ok(())
    }
}

/// bundle manager(root = `~/.monkeyfence`)。
pub struct BundleManager {
    root: PathBuf,
}

/// 安装产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub bundle_dir: PathBuf,
    pub previous: Option<String>,
}

/// 健康检查问题。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HealthError(pub String);

impl BundleManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundles_root(&self) -> PathBuf {
        self.root.join("bundles")
    }

    fn pointer_path(&self) -> PathBuf {
        self.root.join("current.json")
    }

    pub fn bundle_dir(&self, bundle_id: &str) -> PathBuf {
        self.bundles_root().join(bundle_id)
    }

    /// 当前 bundle(pointer 读取 + manifest 校验;半写指针不可见——
    /// 原子 rename 保证,残留 temp 文件被忽略)。
    pub fn current(&self) -> Result<Option<(String, BundleManifest)>> {
        let pointer: CurrentPointer = match std::fs::read(&self.pointer_path()) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("current.json 解析失败(pointer 损坏)")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let manifest = self.load_manifest(&pointer.bundle_id)?;
        Ok(Some((pointer.bundle_id, manifest)))
    }

    pub fn load_manifest(&self, bundle_id: &str) -> Result<BundleManifest> {
        let path = self.bundle_dir(bundle_id).join("bundle-manifest.json");
        let bytes =
            std::fs::read(&path).with_context(|| format!("bundle manifest 缺失:{path:?}"))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// 安装 bundle:全组件落盘 → 健康检查 → 原子切 pointer。
    /// `components` = (相对路径, 内容);manifest 由落盘内容计算 hash。
    pub fn install(
        &self,
        manifest: &BundleManifest,
        components: &[(String, Vec<u8>)],
        storage: &StorageEligibility,
    ) -> Result<Installed> {
        // 仅 UI 更新拒绝:bundle 必须全包一致(manifest 至少声明 core
        // 与 assets;缺任一组件 = 组件级更新,不接受)
        let kinds: Vec<ComponentKind> = manifest.components.iter().map(|c| c.kind).collect();
        for required in [ComponentKind::Core, ComponentKind::Assets] {
            if !kinds.contains(&required) {
                bail!("bundle 缺少 {required:?} 组件:不接受仅 UI/组件级更新(全包一致性)");
            }
        }
        // 兼容性(schema 前滚/durable feature/假兼容全在此拒绝)
        manifest.compatibility.check(storage)?;
        // 落盘(side-by-side:目标目录已存在且非空 → 拒绝覆盖)
        let bundle_dir = self.bundle_dir(&manifest.bundle_id);
        if bundle_dir.exists() && bundle_dir.read_dir()?.next().is_some() {
            bail!(
                "bundle 目录已存在(side-by-side 不覆盖):{}",
                bundle_dir.display()
            );
        }
        std::fs::create_dir_all(&bundle_dir)?;
        for (rel, content) in components {
            let target = bundle_dir.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, content)?;
        }
        std::fs::write(
            bundle_dir.join("bundle-manifest.json"),
            serde_json::to_vec_pretty(manifest)?,
        )?;
        // 健康检查(检查不过 → pointer 不动,旧 bundle 继续服务)
        self.health_check(&manifest.bundle_id).with_context(|| {
            format!("bundle {} 健康检查失败,pointer 未切换", manifest.bundle_id)
        })?;
        // 原子切 pointer(temp + rename)
        let previous = self.current()?.map(|(id, _)| id);
        self.switch_pointer(&manifest.bundle_id)?;
        Ok(Installed {
            bundle_dir,
            previous,
        })
    }

    /// 健康检查:组件齐全 + hash 匹配 + manifest 可解析。
    pub fn health_check(&self, bundle_id: &str) -> std::result::Result<(), HealthError> {
        let manifest = self
            .load_manifest(bundle_id)
            .map_err(|e| HealthError(format!("manifest 不可读:{e:#}")))?;
        for component in &manifest.components {
            let path = self.bundle_dir(bundle_id).join(&component.path);
            let bytes = std::fs::read(&path)
                .map_err(|_| HealthError(format!("组件缺失:{}/{}", bundle_id, component.path)))?;
            let actual = format!("{:x}", Sha256::digest(&bytes));
            if actual != component.sha256 {
                return Err(HealthError(format!(
                    "组件 hash 不符:{}/{}(期望 {}..,实际 {}..)",
                    bundle_id,
                    component.path,
                    &component.sha256[..8],
                    &actual[..8]
                )));
            }
        }
        Ok(())
    }

    /// 原子指针切换(temp 写入 + rename;崩溃窗口内旧 pointer 完整)。
    fn switch_pointer(&self, bundle_id: &str) -> Result<()> {
        let pointer = CurrentPointer {
            bundle_id: bundle_id.to_string(),
            switched_at: chrono::Utc::now().to_rfc3339(),
        };
        let pointer_path = self.pointer_path();
        if let Some(parent) = pointer_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = pointer_path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(&pointer)?)?;
        std::fs::rename(&temp, &pointer_path)
            .context("pointer 原子切换失败(rename)——旧 bundle 继续服务")?;
        Ok(())
    }

    /// 回滚到 previous bundle(pointer 切回;**previous bundle 保留**,
    /// 不删除任何 versioned 目录)。`storage` 需通过 previous 的兼容
    /// 性判定(durable 前滚时禁止自动恢复)。
    pub fn rollback(&self, storage: &StorageEligibility) -> Result<String> {
        let current = self
            .current()?
            .map(|(id, _)| id)
            .context("无 current bundle 可回滚")?;
        // previous = bundles 目录中最近切换的另一个 bundle;按目录
        // 修改时间取(指针只记 current)。
        let mut candidates: Vec<(std::time::SystemTime, String)> = Vec::new();
        for entry in std::fs::read_dir(self.bundles_root())? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if id == current {
                continue;
            }
            // manifest 缺失的半写目录不是候选
            if self.load_manifest(&id).is_err() {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((modified, id));
        }
        candidates.sort();
        let Some((_, previous)) = candidates.last() else {
            bail!("无可回滚的 previous bundle");
        };
        let previous_manifest = self.load_manifest(previous)?;
        previous_manifest
            .compatibility
            .check(storage)
            .context("previous bundle 与当前数据不兼容(存在新 durable 写入):禁止自动恢复")?;
        self.health_check(previous)
            .with_context(|| format!("previous bundle {previous} 健康检查失败"))?;
        self.switch_pointer(previous)?;
        Ok(previous.clone())
    }
}

pub use switch::switch_bundle;

mod switch {
    use super::{BundleManager, BundleManifest, Installed, StorageEligibility};

    /// T5c(Issue #47)whole-bundle 切换编排:Bridge A 交出的 Core 在新
    /// bundle 上恢复。切换 = 安装(健康检查通过才切 pointer)→ 旧 bundle
    /// 保留(retention);失败 = pointer 不动,旧 bundle 继续服务。
    pub fn switch_bundle(
        manager: &BundleManager,
        manifest: &BundleManifest,
        components: &[(String, Vec<u8>)],
        storage: &StorageEligibility,
    ) -> anyhow::Result<Installed> {
        // 复用安装路径(健康检查 + 兼容判定 + 原子 pointer)
        manager.install(manifest, components, storage)
    }
}
/// 活动清理保护:清理仅删除 current/previous 之外的 bundle 目录。
/// `active_pins` = 活动引用的 bundle id(如活动 Agent Run 的 Revision
/// pin)——即使非 current/previous 也不清理。
pub fn retention_keep_set(manager: &BundleManager, active_pins: &[String]) -> Result<Vec<String>> {
    let mut keep = Vec::new();
    if let Some((current, _)) = manager.current()? {
        keep.push(current);
    }
    // previous(最近的其他有效 bundle)
    let mut candidates: Vec<(std::time::SystemTime, String)> = Vec::new();
    if let Some((current, _)) = manager.current()? {
        for entry in std::fs::read_dir(manager.bundles_root())? {
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if id == current || manager.load_manifest(&id).is_err() {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((modified, id));
        }
        candidates.sort();
        if let Some((_, previous)) = candidates.last() {
            keep.push(previous.clone());
        }
    }
    keep.extend(active_pins.iter().cloned());
    Ok(keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str) -> BundleManifest {
        BundleManifest {
            bundle_id: id.into(),
            version: "1.0.0".into(),
            components: vec![
                ComponentEntry {
                    kind: ComponentKind::Core,
                    path: "core/monkeyfence-core.exe".into(),
                    sha256: "a".repeat(64),
                },
                ComponentEntry {
                    kind: ComponentKind::Assets,
                    path: "assets/index.html".into(),
                    sha256: "b".repeat(64),
                },
            ],
            compatibility: BundleCompatibility {
                max_project_schema: 11,
                max_catalog_schema: 1,
                max_service_schema: 4,
                durable_features: vec!["terminal-transcript".into()],
                host_protocol_version: 1,
            },
        }
    }

    #[test]
    fn compatibility_rejects_schema_rolls_and_missing_features() {
        let compat = manifest("x").compatibility;
        let ok = StorageEligibility {
            project_schema: 11,
            catalog_schema: 1,
            service_schema: 4,
            durable_features_written: vec!["terminal-transcript".into()],
        };
        assert!(compat.check(&ok).is_ok());
        // schema 前滚(schema-only 假兼容拒绝)
        let rolled = StorageEligibility {
            project_schema: 12,
            ..ok.clone()
        };
        assert!(matches!(
            compat.check(&rolled),
            Err(CompatibilityProblem::ProjectSchema { .. })
        ));
        // durable feature 已写入但 bundle 不含 → 禁止降级
        let featured = StorageEligibility {
            durable_features_written: vec!["terminal-transcript".into(), "future-feature".into()],
            ..ok
        };
        assert!(matches!(
            compat.check(&featured),
            Err(CompatibilityProblem::DurableFeature(_))
        ));
    }
}
