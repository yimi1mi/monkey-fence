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
    /// 活动任务 pin 数(>0 = 有冻结运行引用,不可清理)。
    pub active_pins: usize,
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

/// 从插件注册表构建汇总行(真实包哈希/兼容性/全量贡献计数/活动 pin)。
pub fn summaries_from_registry(
    registry: &mf_plugins::PluginRegistry,
) -> Vec<PluginContributionSummary> {
    registry
        .summaries()
        .into_iter()
        .map(|s| {
            let mut counts: Vec<(String, usize)> = vec![
                ("agent_types".into(), s.agent_types_count),
                ("node_types".into(), s.node_types_count),
                ("ui_schemas".into(), s.ui_schemas_count),
                (
                    "execution_directories".into(),
                    s.execution_directories_count,
                ),
                ("secret_stores".into(), s.secret_stores_count),
                ("workflow_templates".into(), s.workflow_templates_count),
                ("skills".into(), s.skills_count),
                ("tools".into(), s.tools_count),
            ];
            counts.retain(|(_, c)| *c > 0);
            PluginContributionSummary {
                full_id: s.full_id.clone(),
                name: s.name.clone(),
                version: s.version.clone(),
                // 真实包内容哈希(内置合成插件为空)
                content_hash: s.content_hash.clone(),
                enabled: s.enabled,
                authorized_at: s.authorized_at,
                contribution_counts: counts,
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
                // min_app_version 计算值
                compatible: s.compatible,
                active_pins: s.active_pins,
            }
        })
        .collect()
}
