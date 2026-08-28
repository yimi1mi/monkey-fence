//! Agent 工作流端到端验收测试(UI Task 5;设计 §15):
//! 双 Claude 实例并行互不覆盖、全局 CLI 配置零写入、
//! 插件贡献视图汇总、unsafe-parallel 用户开关。

use crate::plugin_contribution_view::{
    contribution_summary, unsafe_parallel_allowed, PluginContributionSummary,
};

fn summary_fixture() -> Vec<PluginContributionSummary> {
    vec![
        PluginContributionSummary {
            full_id: "monkeyfence.claude".into(),
            name: "Claude (内置)".into(),
            version: "0.1.0".into(),
            content_hash: "hash-a".into(),
            enabled: true,
            authorized_at: Some("2026-08-28T00:00:00Z".into()),
            contribution_counts: vec![
                ("agent_types".into(), 1),
                ("execution_directory_providers".into(), 0),
            ],
            requested_permissions: vec!["net".into(), "hooks".into()],
            compatible: true,
        },
        PluginContributionSummary {
            full_id: "monkeyfence.git".into(),
            name: "Git worktree".into(),
            version: "0.1.0".into(),
            content_hash: "hash-b".into(),
            enabled: false,
            authorized_at: None,
            contribution_counts: vec![("execution_directory_providers".into(), 1)],
            requested_permissions: vec!["vcs".into()],
            compatible: true,
        },
    ]
}

#[test]
fn contribution_summary_lists_types_permissions_and_versions() {
    let rows = summary_fixture();
    let text = contribution_summary(&rows);
    assert!(text.contains("monkeyfence.claude"));
    assert!(text.contains("agent_types: 1"));
    assert!(text.contains("execution_directory_providers: 1"));
    assert!(text.contains("vcs"));
    assert!(text.contains("已禁用"), "禁用状态必须可见");
    // 固定版本与内容哈希可见(设计 §11.5)
    assert!(text.contains("0.1.0"));
    assert!(text.contains("hash-a"));
}

#[test]
fn unsafe_parallel_defaults_off_and_user_can_opt_in() {
    // 默认关闭:目录不能隔离时禁止并行(编译器拒绝)
    assert!(!unsafe_parallel_allowed(false, false));
    // 用户显式开启风险开关:允许(自行承担冲突)
    assert!(unsafe_parallel_allowed(false, true));
    // worktree 可隔离:无需开关
    assert!(unsafe_parallel_allowed(true, false));
}

#[test]
fn two_claude_instances_compile_without_global_config_writes() {
    // 编译路径:两个 Claude 实例的 run-temp 互不相同,
    // 且都不指向 ~/.claude(真实全局配置零写入)。
    use mf_agent::agent_instance::AgentInstanceDraft;
    use mf_agent::catalog_store::CatalogStore;
    use mf_agent::{InstanceScope, RunMode};
    use std::collections::HashSet;

    let catalog = CatalogStore::memory().unwrap();
    let mk = |name: &str| AgentInstanceDraft {
        name: name.into(),
        agent_type: "claude".into(),
        scope: InstanceScope::User,
        project_key: None,
        enabled: true,
        run_mode: RunMode::OneShot,
        executable: "claude".into(),
        argv: vec![],
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({ "completion": "process-exit" }),
        sealed_secret_ids: vec![],
    };
    let a = catalog.create_agent_instance(mk("implementation")).unwrap();
    let b = catalog.create_agent_instance(mk("review")).unwrap();
    assert_ne!(a.id, b.id, "同类型多实例必须彼此独立");

    let adapter = mf_plugins::builtin::adapter_for("claude-code").unwrap();
    let home = dirs::home_dir().unwrap();
    let mut seen_dirs: HashSet<std::path::PathBuf> = HashSet::new();
    for id in [a.id, b.id] {
        let snapshot = catalog.snapshot_agent_instance(&id, None).unwrap();
        let run_temp = std::env::temp_dir()
            .join("monkeyfence-e2e")
            .join(format!("{id:?}"));
        let ctx = mf_agent::LaunchContext::new(run_temp.clone(), std::path::PathBuf::from("."));
        let plan = adapter.compile_launch(&snapshot, &ctx).unwrap();
        let config_dir = std::path::PathBuf::from(
            plan.env
                .iter()
                .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
                .map(|(_, v)| v.clone())
                .unwrap(),
        );
        assert!(config_dir.starts_with(&run_temp), "必须在 run-temp 下");
        assert_ne!(config_dir, home.join(".claude"), "绝不指向真实全局配置");
        seen_dirs.insert(config_dir);
    }
    assert_eq!(seen_dirs.len(), 2, "两个实例的隔离目录互不重叠");
}
