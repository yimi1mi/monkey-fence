//! 内嵌 assets(T7a,Issue #38;spec §6.3:与 Core 同版本内嵌、内容
//! 哈希命名、离线可用、无 CDN)。

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

/// 单个内嵌 asset。
#[derive(Debug, Clone)]
pub struct EmbeddedAsset {
    pub name: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

/// assets 注册表:内容哈希命名 + 索引页引用。
pub struct AssetRegistry {
    /// 原名 → (hash 文件名, asset)。
    entries: BTreeMap<&'static str, (String, EmbeddedAsset)>,
    index: String,
}

impl AssetRegistry {
    /// 注册一组内嵌 asset(发行 bundle 由打包器生成静态表;测试注入)。
    pub fn new(assets: Vec<EmbeddedAsset>) -> Self {
        let mut entries = BTreeMap::new();
        for asset in assets {
            let digest = format!("{:x}", Sha256::digest(asset.bytes));
            let hash_name = format!(
                "{}.{}.{}",
                asset
                    .name
                    .rsplit_once('.')
                    .map_or(asset.name, |(stem, _)| stem),
                &digest[..12],
                asset.name.rsplit_once('.').map_or("", |(_, ext)| ext)
            );
            entries.insert(asset.name, (hash_name, asset));
        }
        let index = entries
            .get("index.html")
            .map(|(hash_name, _)| hash_name.clone())
            .unwrap_or_default();
        Self { entries, index }
    }

    /// hash 名 → asset。
    pub fn by_hash_name(&self, hash_name: &str) -> Option<&EmbeddedAsset> {
        self.entries
            .values()
            .find(|(name, _)| name == hash_name)
            .map(|(_, asset)| asset)
    }

    /// 索引页的 hash 文件名。
    pub fn index_hash_name(&self) -> &str {
        &self.index
    }

    /// 发行校验:引用的 URL 不得含外部域(schema/http 主机/CDN)。
    pub fn audit_no_external_references(&self) -> Result<(), String> {
        for (hash_name, (_, asset)) in &self.entries {
            let text = String::from_utf8_lossy(asset.bytes);
            for token in ["http://", "https://", "//"] {
                if text.contains(token) {
                    return Err(format!(
                        "asset `{hash_name}` 引用外部资源 `{token}...`(无 CDN 契约)"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// 发行 bundle 无 Node runtime:注册表只接受静态字节,构建期无任何
/// 脚本执行路径(编译期保证,运行时无 eval/child process)。
pub const NO_NODE_RUNTIME: bool = true;
