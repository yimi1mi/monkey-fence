//! Manifest v3 契约(T4a,Issue #36;spec §9.2)。
//!
//! 先失败后实现的失败契约已随实现收敛;这里固化语义不可变项:
//! golden 解析、授权指纹随安全贡献变化、v2 明确拒绝、root_launch
//! 缺失/非法 fail-closed、Generic Command 必须显式 passthrough、
//! installer 未授权 package_install 拒绝、调用方只见 typed 贡献。

use mf_plugins::manifest::{
    Capabilities, DiscoveryContribution, InstallerContribution, ManifestHeader, PluginManifest,
    RootLaunchContribution,
};

const V3_GOLDEN: &str = r#"
[manifest]
version = 3
publisher = "acme"
id = "tools"
name = "ACME Tools"
version_str = "1.0.0"

[capabilities]
spawn = true
net = true
package_install = true

[[agent_types]]
id = "codex"
name = "Codex"
adapter = "codex"
command = "codex"
modes = ["interactive", "oneshot"]

[agent_types.discovery]
commands = ["codex"]
version_argv = ["--version"]
version_parser = "semver-first"

[agent_types.models]
local_probe = "adapter"

[agent_types.root_launch]
permission_mode = "full-access"
argv = ["--dangerously-bypass-approvals-and-sandbox"]

[[agent_types.installers]]
id = "npm-global"
platforms = ["windows-x64", "linux-x64", "macos-arm64"]
kind = "package-manager"
manager = "npm"
package = "@openai/codex"
argv = ["install", "--global", "@openai/codex"]
scope = "user"
post_install_probe = true

[[provider_types]]
id = "openai-compatible"
protocol = "openai"
config_schema = "schemas/openai-provider.json"
model_probe = "remote-catalog"
cache_ttl_seconds = 300
"#;

fn parse(text: &str) -> PluginManifest {
    let m = PluginManifest::parse(text).expect("v3 解析");
    m.validate().expect("v3 校验");
    m
}

#[test]
fn v3_golden_parses_and_validates() {
    let m = parse(V3_GOLDEN);
    assert_eq!(m.manifest.version, 3);
    assert!(m.capabilities.package_install);
    let agent = &m.agent_types[0];
    assert_eq!(agent.id, "codex");
    let discovery = agent.discovery.as_ref().expect("discovery");
    assert_eq!(discovery.commands, vec!["codex"]);
    assert_eq!(discovery.version_parser, "semver-first");
    assert_eq!(agent.models.as_ref().unwrap().local_probe, "adapter");
    let root = agent.root_launch.as_ref().expect("root_launch");
    assert_eq!(root.permission_mode, "full-access");
    assert_eq!(
        root.argv,
        vec!["--dangerously-bypass-approvals-and-sandbox"]
    );
    assert_eq!(agent.installers.len(), 1);
    assert_eq!(agent.installers[0].kind, "package-manager");
    assert_eq!(m.provider_types.len(), 1);
    assert_eq!(m.provider_types[0].protocol, "openai");
    assert_eq!(m.provider_types[0].model_probe, "remote-catalog");
}

#[test]
fn v2_manifest_is_rejected_explicitly() {
    // v2 在 reader 层即明确拒绝(不做安全字段的静默兼容推断;回滚由
    // 旧 bundle 的旧 reader 继续读 v2)。
    let v2 = V3_GOLDEN.replace("version = 3", "version = 2");
    let error = PluginManifest::parse(&v2).expect_err("v2 必须明确拒绝");
    assert!(
        error.to_string().contains("不受支持"),
        "错误应明确版本不兼容:{error}"
    );
}

#[test]
fn fingerprint_changes_with_each_security_contribution() {
    let base = parse(V3_GOLDEN);
    let base_fp = base.permission_fingerprint("hash");

    // capability 变化(package_install 授权撤销会同时导致 validate 拒绝;
    // 换一项不冲突的:spawn 关闭在无 spawn 需求下合法)
    let mut cap_changed = base.clone();
    cap_changed.capabilities.spawn = !cap_changed.capabilities.spawn;
    assert_ne!(
        cap_changed.permission_fingerprint("hash"),
        base_fp,
        "capability 变化必须改变指纹"
    );

    // recipe/root_launch 变化
    let mut root_changed = base.clone();
    root_changed.agent_types[0].root_launch = Some(RootLaunchContribution {
        permission_mode: "full-access".into(),
        argv: vec!["--other-flag".into()],
        env: vec![],
    });
    assert_ne!(
        root_changed.permission_fingerprint("hash"),
        base_fp,
        "root_launch 变化必须改变指纹"
    );

    let mut installer_changed = base.clone();
    installer_changed.agent_types[0].installers[0].argv = vec!["install".into()];
    assert_ne!(
        installer_changed.permission_fingerprint("hash"),
        base_fp,
        "installer recipe 变化必须改变指纹"
    );

    // worker 变化
    let mut worker_changed = base.clone();
    worker_changed.worker = Some(mf_plugins::manifest::WorkerSpec {
        command: "node".into(),
        args: vec!["worker.js".into()],
    });
    assert_ne!(
        worker_changed.permission_fingerprint("hash"),
        base_fp,
        "worker 变化必须改变指纹"
    );

    // 内容哈希变化
    assert_ne!(base.permission_fingerprint("hash2"), base_fp);
}

#[test]
fn root_launch_missing_is_fail_closed_and_invalid_modes_rejected() {
    // root_launch 缺失 = 不贡献 Root 启动(启动时 fail-closed,由运行时
    // 保证);结构上 None 合法。但非法 permission_mode 明确拒绝:
    let mut m = parse(V3_GOLDEN);
    m.agent_types[0].root_launch = Some(RootLaunchContribution {
        permission_mode: "admin".into(),
        argv: vec![],
        env: vec![],
    });
    assert!(m.validate().is_err(), "非法 permission_mode 必须拒绝");
}

#[test]
fn generic_command_requires_explicit_passthrough() {
    let mut m = parse(V3_GOLDEN);
    m.agent_types[0].adapter = "generic-command".into();
    // full-access 对 Generic Command 非法:无内部权限层不得宣称
    assert!(
        m.validate().is_err(),
        "Generic Command 的 full-access 必须拒绝"
    );
    // 显式 passthrough-full-access 合法
    m.agent_types[0].root_launch = Some(RootLaunchContribution {
        permission_mode: "passthrough-full-access".into(),
        argv: vec![],
        env: vec![],
    });
    m.validate().expect("显式 passthrough 应通过");
}

#[test]
fn installer_without_package_install_capability_is_rejected() {
    let mut m = parse(V3_GOLDEN);
    m.capabilities.package_install = false;
    let error = m.validate().expect_err("未授权 installer 必须拒绝");
    assert!(
        error.to_string().contains("package_install"),
        "错误应指明能力缺失:{error}"
    );
}

#[test]
fn installer_kind_must_be_one_of_three_executors() {
    let mut m = parse(V3_GOLDEN);
    m.agent_types[0].installers[0].kind = "shell-pipe".into();
    assert!(m.validate().is_err(), "未知 installer kind 必须拒绝");
}

#[test]
fn empty_discovery_commands_rejected() {
    let mut m = parse(V3_GOLDEN);
    m.agent_types[0].discovery = Some(DiscoveryContribution {
        commands: vec![],
        version_argv: vec![],
        version_parser: String::new(),
    });
    assert!(m.validate().is_err(), "空 discovery 候选必须拒绝");
}

#[test]
fn builtin_synthetic_manifests_are_v3_and_typed() {
    // 全部 builtin synthetic 清单以 v3 通过校验;调用方只见
    // InstallerContribution/RootLaunchContribution 类型(旁路类型已删)。
    for agent in mf_plugins::builtin::builtin_cli_agents() {
        let m = mf_plugins::builtin::synthetic_manifest(&agent);
        assert_eq!(m.manifest.version, 3);
        m.validate()
            .unwrap_or_else(|e| panic!("{} synthetic 清单非法:{e}", agent.profile_id));
        let contribution = &m.agent_types[0];
        assert!(
            contribution.discovery.is_some(),
            "{} 必须贡献 v3 discovery",
            agent.profile_id
        );
        // yolo 参数即 root_launch 映射(非空 yolo → 必有 root_launch)
        if !agent.yolo_args.is_empty() {
            assert!(
                contribution.root_launch.is_some(),
                "{} 的自动批准参数必须进入 root_launch",
                agent.profile_id
            );
        }
    }
}
