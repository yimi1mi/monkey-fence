//! 插件贡献视图(UI 计划 Task 5;设计 §11.5):
//! 贡献类型、请求权限、固定版本、内容哈希与兼容状态;
//! unsafe-parallel 用户开关语义(默认关闭)。

/// 单个插件的贡献汇总行。
#[derive(Debug, Clone)]
pub struct PluginContributionSummary {
    pub full_id: String,
    pub name: String,
    pub version: String,
    pub content_hash: String,
    pub enabled: bool,
    pub authorized_at: Option<String>,
    /// (贡献类型, 数量)。
    pub contribution_counts: Vec<(String, usize)>,
    /// 权限位字符串(net/vcs/shell/…)。
    pub requested_permissions: Vec<String>,
    pub compatible: bool,
}

/// 汇总为人类可读文本(设置页/诊断用;面向普通用户)。
pub fn contribution_summary(rows: &[PluginContributionSummary]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for row in rows {
        let _ = writeln!(
            out,
            "{} {}(版本 {},哈希 {})· {}",
            row.full_id,
            row.name,
            row.version,
            &row.content_hash[..row.content_hash.len().min(8)],
            if row.enabled {
                "已启用"
            } else {
                "已禁用"
            }
        );
        if !row.compatible {
            let _ = writeln!(out, "  ⚠ 与当前版本不兼容");
        }
        if let Some(at) = &row.authorized_at {
            let _ = writeln!(out, "  授权于 {at}");
        }
        for (kind, count) in &row.contribution_counts {
            if *count > 0 {
                let _ = writeln!(out, "  {kind}: {count}");
            }
        }
        if !row.requested_permissions.is_empty() {
            let _ = writeln!(
                out,
                "  请求权限:{}(worker/进程以当前系统用户运行)",
                row.requested_permissions.join(", ")
            );
        }
    }
    out
}

/// unsafe-parallel 开关语义:目录提供器不能隔离时,
/// 并行必须由用户显式开启风险开关(默认关闭;设计 §9.4)。
pub fn unsafe_parallel_allowed(directory_isolates: bool, user_opt_in: bool) -> bool {
    directory_isolates || user_opt_in
}

/// 从插件注册表构建汇总行。
pub fn summaries_from_registry(
    registry: &mf_plugins::PluginRegistry,
) -> Vec<PluginContributionSummary> {
    registry
        .summaries()
        .into_iter()
        .map(|s| PluginContributionSummary {
            full_id: s.full_id.clone(),
            name: s.name.clone(),
            version: s.version.clone(),
            content_hash: s
                .capabilities
                .fingerprint_part()
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string(),
            enabled: s.enabled,
            authorized_at: s.authorized_at,
            contribution_counts: {
                let mut counts: Vec<(String, usize)> = vec![
                    ("agent_types".into(), s.agents.len()),
                    (
                        "ui_schemas".into(),
                        0, // 由清单详情补全;汇总先以 0 占位
                    ),
                ];
                counts.retain(|(_, c)| *c > 0);
                counts
            },
            requested_permissions: {
                let cap = &s.capabilities;
                let mut perms = Vec::new();
                if cap.net {
                    perms.push("net".into());
                }
                if cap.fs_read {
                    perms.push("fs_read".into());
                }
                if cap.fs_write {
                    perms.push("fs_write".into());
                }
                if cap.spawn {
                    perms.push("spawn".into());
                }
                if cap.shell {
                    perms.push("shell".into());
                }
                if cap.secrets {
                    perms.push("secrets".into());
                }
                if cap.vcs {
                    perms.push("vcs".into());
                }
                if cap.background_worker {
                    perms.push("background_worker".into());
                }
                if cap.hooks {
                    perms.push("hooks".into());
                }
                perms
            },
            compatible: true,
        })
        .collect()
}
