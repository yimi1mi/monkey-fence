//! 冻结安装计划(T4c,Issue #43;spec §9.3/§9.5)。
//!
//! 预览(preview)→ 冻结(plan_handle + recipe_digest + catalog_revision
//! + exact package/version/argv/TTL)→ 提交:提交**只接受三元组**,不
//! 接受浏览器解析的 argv/path;冻结时复验 catalog revision 与 recipe
//! digest(TOCTOU 拒绝);TTL 过期拒绝。L-SWITCH 前失败只清 staging。

use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// 预览形态(可由用户查看;未冻结)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPreview {
    pub agent_type_id: String,
    pub installer_id: String,
    /// package-manager | verified-download | custom-command。
    pub kind: String,
    pub exact_package: String,
    pub exact_version: String,
    /// 结构化 argv(冻结后不再重解析 `latest`)。
    pub argv: Vec<String>,
    /// verified-download 的冻结 URL/digest/域名。
    pub download: Option<FrozenDownload>,
    /// 提交时的 catalog revision(预览基于它;冻结复验一致)。
    pub catalog_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenDownload {
    pub url: String,
    pub sha256: String,
    /// 预览冻结域名(redirect 仅允许同域;§9.3)。
    pub frozen_host: String,
}

/// 冻结计划错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanProblem {
    #[error("catalog revision 已变化(preview={preview}, now={now}):TOCTOU 拒绝")]
    CatalogDrift { preview: u64, now: u64 },
    #[error("recipe digest 已变化:TOCTOU 拒绝")]
    RecipeDrift,
    #[error("计划已过期(ttl {ttl_secs}s)")]
    Expired { ttl_secs: u64 },
    #[error("计划句柄未知")]
    UnknownHandle,
    #[error("digest 不匹配(expected {expected}, got {actual})")]
    DigestMismatch { expected: String, actual: String },
}

/// 冻结安装计划(不可变;提交只认三元组)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenInstallPlan {
    pub plan_handle: String,
    /// recipe 语义 digest(exact package/version/argv/download 全参与)。
    pub recipe_digest: String,
    pub catalog_revision: u64,
    pub preview: InstallPreview,
    frozen_at: Instant,
    ttl: Duration,
}

/// 计划票据(提交时调用方持有的三元组;不携带解析后的 argv)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTicket {
    pub plan_handle: String,
    pub recipe_digest: String,
    pub catalog_revision: u64,
}

/// 计划冻结器(内存 registry;standalone Core 生命周期)。
pub struct PlanFreezer {
    ttl: Duration,
    plans: Vec<FrozenInstallPlan>,
}

impl PlanFreezer {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_secs),
            plans: Vec::new(),
        }
    }

    /// 冻结预览:复验当前 catalog revision 一致(TOCTOU);冻结后 TTL 内
    /// 可提交。返回票据。
    pub fn freeze(
        &mut self,
        preview: InstallPreview,
        current_catalog_revision: u64,
    ) -> Result<PlanTicket, PlanProblem> {
        if preview.catalog_revision != current_catalog_revision {
            return Err(PlanProblem::CatalogDrift {
                preview: preview.catalog_revision,
                now: current_catalog_revision,
            });
        }
        let recipe_digest = digest_of(&preview);
        let plan = FrozenInstallPlan {
            plan_handle: format!("iplan_{}", uuid::Uuid::now_v7().simple()),
            recipe_digest: recipe_digest.clone(),
            catalog_revision: current_catalog_revision,
            preview,
            frozen_at: Instant::now(),
            ttl: self.ttl,
        };
        let ticket = PlanTicket {
            plan_handle: plan.plan_handle.clone(),
            recipe_digest,
            catalog_revision: plan.catalog_revision,
        };
        self.plans.push(plan);
        Ok(ticket)
    }

    /// 提交校验:只认 handle + digest + catalog revision;TTL 过期拒绝;
    /// 消费一次性(execute 完成后由调用方 `consume`)。
    pub fn redeem(&mut self, ticket: &PlanTicket) -> Result<&FrozenInstallPlan, PlanProblem> {
        let ttl_secs = self.ttl.as_secs();
        let plan = self
            .plans
            .iter()
            .find(|plan| plan.plan_handle == ticket.plan_handle)
            .ok_or(PlanProblem::UnknownHandle)?;
        if plan.frozen_at.elapsed() >= plan.ttl {
            return Err(PlanProblem::Expired { ttl_secs });
        }
        if plan.recipe_digest != ticket.recipe_digest {
            return Err(PlanProblem::DigestMismatch {
                expected: plan.recipe_digest.clone(),
                actual: ticket.recipe_digest.clone(),
            });
        }
        if plan.catalog_revision != ticket.catalog_revision {
            return Err(PlanProblem::CatalogDrift {
                preview: plan.catalog_revision,
                now: ticket.catalog_revision,
            });
        }
        Ok(plan)
    }

    /// 执行完成后移除(一次性;同 ticket 再提交 = UnknownHandle)。
    pub fn consume(&mut self, plan_handle: &str) {
        self.plans.retain(|plan| plan.plan_handle != plan_handle);
    }
}

/// recipe 语义 digest:kind/package/version/argv/download 全参与。
pub fn digest_of(preview: &InstallPreview) -> String {
    let canonical = serde_json::json!({
        "agent_type_id": preview.agent_type_id,
        "installer_id": preview.installer_id,
        "kind": preview.kind,
        "exact_package": preview.exact_package,
        "exact_version": preview.exact_version,
        "argv": preview.argv,
        "download": preview.download.as_ref().map(|d| serde_json::json!({
            "url": d.url,
            "sha256": d.sha256,
            "frozen_host": d.frozen_host,
        })),
    });
    format!("{:x}", Sha256::digest(canonical.to_string().as_bytes()))
}
