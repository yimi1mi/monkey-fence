//! LaunchPlan 编译层 contract(Issue #27):
//! - Core 提供的可信路径(run-temp / workdir / materialization root)
//!   不可被 adapter 改写,违规稳定拒绝;
//! - argv/env 冻结:值来自实例快照一次编译,typed 计划只读;
//! - Secret 不进 Debug(LaunchPlan/TypedLaunchPlan 均不实现 Serialize,
//!   Debug 全脱敏;明文仅存在于 zeroizing 租约与启动期 redaction API);
//! - 插件 pin 漂移稳定拒绝(内容寻址 + 内置合成两条路径)。
//!
//! 全部使用 tempfile/内存目录库,绝不触碰 ~/.monkeyfence 真实数据。

use mf_agent::secrets::SecretStore;
use mf_agent::workflow::PluginSourcePin;
use mf_agent::{
    AgentAdapter, AgentInstanceSnapshot, CatalogStore, CompletionDetector, Config, InputInjection,
    LaunchContext, LaunchPlan, RunMode, TempFileSpec,
};
use mf_plugins::adapter_launch::{
    compile_instance_launch, resolve_adapter_for_pin, resolve_agent_type_pin, verify_trusted_paths,
    workflow_plugin_index, LaunchPlanProvenance,
};
use mf_plugins::host::PluginHost;
use mf_plugins::install::InstallSource;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SECRET_PLAINTEXT: &[u8] = b"plain-secret-value-9f2a";

fn instance(agent_type: &str) -> AgentInstanceSnapshot {
    AgentInstanceSnapshot {
        id: "inst_contract".into(),
        name: agent_type.into(),
        agent_type: agent_type.into(),
        version: 7,
        enabled: true,
        run_mode: RunMode::Interactive,
        executable: agent_type.into(),
        argv: vec![],
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({}),
        sealed_secret_ids: vec![],
        external_config: false,
    }
}

/// 空宿主:临时插件根 + 内存目录库,不加载任何插件(legacy 回退生效)。
fn empty_host() -> (Arc<PluginHost>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (PluginHost::empty_at(tmp.path().to_path_buf()), tmp)
}

fn memory_catalog() -> Arc<CatalogStore> {
    CatalogStore::memory().expect("内存目录库初始化不应失败")
}

/// 模拟"合规 adapter"产物:与 generic CLI 适配器同形状。
fn compliant_plan(ctx: &LaunchContext) -> LaunchPlan {
    LaunchPlan {
        run_temp: ctx.run_temp.clone(),
        executable: PathBuf::from("demo"),
        argv: vec![],
        env: vec![],
        secret_env: vec![],
        cwd: Some(ctx.workdir.clone()),
        temp_files: vec![],
        input: InputInjection::Argv(String::new()),
        completion: CompletionDetector::ProcessExit,
        uses_shell: false,
    }
}

// ---------- 可信路径不可覆盖 ----------

#[test]
fn trusted_paths_cannot_be_overridden() {
    let base = tempfile::tempdir().unwrap();
    let run_temp = base.path().join("run");
    let workdir = base.path().join("work");
    let ctx = LaunchContext::new(run_temp.clone(), workdir.clone());

    // 合规计划通过
    assert!(verify_trusted_paths(&compliant_plan(&ctx), &ctx).is_ok());

    // run-temp(materialization root)改写 → 拒绝;重复调用错误文本一致(稳定拒绝)
    let mut plan = compliant_plan(&ctx);
    plan.run_temp = base.path().join("evil-root");
    let e1 = verify_trusted_paths(&plan, &ctx).unwrap_err().to_string();
    let e2 = verify_trusted_paths(&plan, &ctx).unwrap_err().to_string();
    assert_eq!(e1, e2);
    assert!(e1.contains("run-temp"), "{e1}");

    // workdir 改写 → 拒绝
    let mut plan = compliant_plan(&ctx);
    plan.cwd = Some(base.path().join("evil-work"));
    let err = verify_trusted_paths(&plan, &ctx).unwrap_err().to_string();
    assert!(err.contains("工作目录"), "{err}");

    // cwd 缺省(None)同样拒绝:可信工作目录必须显式冻结
    let mut plan = compliant_plan(&ctx);
    plan.cwd = None;
    assert!(verify_trusted_paths(&plan, &ctx)
        .unwrap_err()
        .to_string()
        .contains("工作目录"));

    // 待物化临时文件必须是相对路径,且不允许 `..` 逃逸
    for (path, needle) in [
        (base.path().join("abs.txt"), "相对路径"),
        (PathBuf::from("../escape.txt"), "逃逸"),
        (PathBuf::from("sub/../../escape.txt"), "逃逸"),
        // 前导 `./` 与空路径同样拒绝:组件规则与 Runtime Host 物化一致
        (PathBuf::from("./config.toml"), "逃逸"),
        (PathBuf::new(), "不能为空"),
    ] {
        let mut plan = compliant_plan(&ctx);
        plan.temp_files = vec![TempFileSpec {
            path,
            contents: Vec::new(),
        }];
        let err = verify_trusted_paths(&plan, &ctx).unwrap_err().to_string();
        assert!(err.contains(needle), "path 应被拒绝: {err}");
    }

    // Windows 形态逃逸:`C:file`(盘符相对,join 会整体替换基路径)与
    // `\file`(无盘符根,join 保留盘符但丢弃目录)在 is_absolute 上均为假,
    // 必须按组件拒绝(与 Host materialize_temp_files 一致)
    #[cfg(windows)]
    for path in [PathBuf::from("C:evil.txt"), PathBuf::from(r"\evil.txt")] {
        let mut plan = compliant_plan(&ctx);
        plan.temp_files = vec![TempFileSpec {
            path,
            contents: Vec::new(),
        }];
        let err = verify_trusted_paths(&plan, &ctx).unwrap_err().to_string();
        assert!(err.contains("逃逸"), "path 应被拒绝: {err}");
    }

    // 提示文件/结果文件必须位于可信 run-temp 之下(含兄弟目录前缀碰撞:
    // `starts_with` 是组件前缀匹配,`run-evil` 不是 `run` 的子目录)
    for path in [
        base.path().join("elsewhere").join("prompt.txt"),
        run_temp.join("..").join("prompt.txt"),
        run_temp
            .parent()
            .unwrap()
            .join("run-evil")
            .join("prompt.txt"),
    ] {
        let mut plan = compliant_plan(&ctx);
        plan.input = InputInjection::PromptFile(path);
        let err = verify_trusted_paths(&plan, &ctx).unwrap_err().to_string();
        assert!(
            err.contains("提示文件") && err.contains("run-temp"),
            "{err}"
        );
    }
    for path in [
        base.path().join("elsewhere").join("result.json"),
        run_temp.join("..").join("result.json"),
        run_temp
            .parent()
            .unwrap()
            .join("run-evil")
            .join("result.json"),
    ] {
        let mut plan = compliant_plan(&ctx);
        plan.completion = CompletionDetector::ResultFile(path);
        let err = verify_trusted_paths(&plan, &ctx).unwrap_err().to_string();
        assert!(
            err.contains("结果文件") && err.contains("run-temp"),
            "{err}"
        );
    }

    // run-temp 之下的合法引用通过
    let mut plan = compliant_plan(&ctx);
    plan.input = InputInjection::PromptFile(run_temp.join("prompt.txt"));
    plan.completion = CompletionDetector::ResultFile(run_temp.join("result.json"));
    plan.temp_files = vec![TempFileSpec {
        path: PathBuf::from("sub").join("config.toml"),
        contents: Vec::new(),
    }];
    assert!(verify_trusted_paths(&plan, &ctx).is_ok());
}

/// 链路级:adapter 产物经编译层校验,typed 计划冻结 Core 提供的可信路径与身份。
#[test]
fn compile_freezes_core_trusted_paths_and_identity() {
    let (host, _tmp) = empty_host();
    let catalog = memory_catalog();
    let root = tempfile::tempdir().unwrap();
    let run_temp = root.path().join("run");
    let workdir = root.path().join("work");

    let mut inst = instance("generic-command");
    inst.argv = vec!["--fast".into()];
    inst.env = vec![("MF_TEST_FLAG".into(), "1".into())];
    inst.config = serde_json::json!({"provider_profile": "pprof-main"});

    let typed = compile_instance_launch(
        &host,
        &catalog,
        &inst,
        None,
        run_temp.clone(),
        workdir.clone(),
        None,
        "tok-freeze",
        false,
        None,
    )
    .unwrap();

    // 可信路径逐字节等于 Core 输入,adapter 无法改写
    let plan = typed.plan();
    assert_eq!(plan.run_temp, run_temp);
    assert_eq!(plan.cwd.as_deref(), Some(workdir.as_path()));

    // 冻结身份:实例/类型/适配器/pin/provider 一次编译附着
    assert_eq!(
        typed.provenance(),
        &LaunchPlanProvenance {
            agent_instance_id: "inst_contract".into(),
            agent_instance_revision: 7,
            agent_type: "generic-command".into(),
            adapter_id: "generic-command".into(),
            plugin_pin: None,
            provider_identity: Some("pprof-main".into()),
        }
    );
}

// ---------- argv / env 冻结 ----------

#[test]
fn argv_and_env_are_frozen_from_snapshot() {
    let (host, _tmp) = empty_host();
    let catalog = memory_catalog();
    let root = tempfile::tempdir().unwrap();

    let mut inst = instance("generic-command");
    inst.argv = vec!["--fast".into(), "-m".into(), "gpt-x".into()];
    inst.env = vec![("MF_TEST_FLAG".into(), "1".into())];

    // 无提示:argv 恰为快照声明,不追加
    let typed = compile_instance_launch(
        &host,
        &catalog,
        &inst,
        None,
        root.path().join("run"),
        root.path().join("work"),
        None,
        "tok-argv-1",
        false,
        None,
    )
    .unwrap();
    let plan = typed.plan();
    assert_eq!(plan.argv, ["--fast", "-m", "gpt-x"]);
    assert_eq!(plan.env, [("MF_TEST_FLAG".to_string(), "1".to_string())]);
    assert!(plan.secret_env.is_empty());
    assert!(!plan.uses_shell);

    // 有提示(argv 模式):快照 argv 原样保留,提示只追加在尾部
    let typed = compile_instance_launch(
        &host,
        &catalog,
        &inst,
        None,
        root.path().join("run"),
        root.path().join("work"),
        Some("do the thing".into()),
        "tok-argv-2",
        false,
        None,
    )
    .unwrap();
    assert_eq!(typed.plan().argv, ["--fast", "-m", "gpt-x", "do the thing"]);
}

// ---------- Secret 不进 Debug / Serialize ----------
//
// LaunchPlan 与 TypedLaunchPlan 均不实现 Serialize(类型系统保证计划无法
// 被 serde 序列化;若派生 Serialize 会即刻破坏所有下游编译),因此这里
// 只需运行时验证 Debug 脱敏。

#[test]
fn secrets_never_leak_through_debug() {
    let (host, _tmp) = empty_host();
    let catalog = memory_catalog();
    let root = tempfile::tempdir().unwrap();

    // 全链路:seal → 实例声明 → compile 解封进 secret_env
    let store = mf_plugins::builtin_secret_store::BuiltinSecretStore::with_master_key(
        catalog.clone(),
        [7u8; 32],
    )
    .unwrap();
    let secret_id = store.seal("api-key", SECRET_PLAINTEXT).unwrap();

    let mut inst = instance("generic-command");
    inst.config = serde_json::json!({"secret_env": {"MY_TOKEN": secret_id}});
    inst.sealed_secret_ids = vec![secret_id.clone()];

    let typed = compile_instance_launch(
        &host,
        &catalog,
        &inst,
        None,
        root.path().join("run"),
        root.path().join("work"),
        None,
        "tok-secret",
        false,
        Some([7u8; 32]),
    )
    .unwrap();

    let plan = typed.plan();
    // Secret 只进入 secret_env(租约引用),绝不混入普通 env
    assert_eq!(
        plan.secret_env
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        ["MY_TOKEN"]
    );
    assert!(plan.env.is_empty());

    // Debug 全脱敏:两个层面都不含明文
    let plain = String::from_utf8_lossy(SECRET_PLAINTEXT);
    assert!(!format!("{plan:?}").contains(plain.as_ref()));
    assert!(!format!("{typed:?}").contains(plain.as_ref()));
    assert!(format!("{typed:?}").contains("redacted"));

    // redaction_values 是唯一明文出口,仅供启动期输出过滤,不进日志
    assert!(typed.redaction_values().contains(&plain.as_ref()));
}

// ---------- 插件 pin 漂移拒绝 ----------

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// `Arc<dyn AgentAdapter>` 不实现 Debug,unwrap_err 需要自备错误提取。
fn expect_resolve_err(res: Result<Arc<dyn AgentAdapter>, anyhow::Error>) -> anyhow::Error {
    match res {
        Ok(adapter) => panic!("应被拒绝,却解析到适配器 {}", adapter.id()),
        Err(e) => e,
    }
}

fn install_demo_v1(host: &PluginHost) -> mf_plugins::host::ResolvedPlugin {
    let dir = fixtures_dir().join("demo-v1");
    host.install_package(
        &dir,
        InstallSource::Local {
            path: dir.display().to_string(),
        },
    )
    .expect("安装 demo-v1 fixture 不应失败")
}

#[test]
fn content_addressed_pin_drift_is_rejected() {
    let (host, _tmp) = empty_host();
    let resolved = install_demo_v1(&host);

    let pin = |version: &str, hash: &str| PluginSourcePin {
        full_id: resolved.full_id.clone(),
        version: version.into(),
        content_hash: hash.into(),
        contribution_id: String::new(),
    };

    // 精确 pin:按内容寻址解析到 fixture 声明的 adapter
    let ok = resolve_adapter_for_pin(
        &host,
        Some(&pin("0.1.0", &resolved.content_hash)),
        "demo-agent",
    )
    .unwrap();
    assert_eq!(ok.id(), "generic-command");

    // 版本漂移 → 稳定拒绝(错误文本确定且重复一致)
    let drifted = pin("9.9.9", &resolved.content_hash);
    let e1 = expect_resolve_err(resolve_adapter_for_pin(&host, Some(&drifted), "demo-agent"))
        .to_string();
    let e2 = expect_resolve_err(resolve_adapter_for_pin(&host, Some(&drifted), "demo-agent"))
        .to_string();
    assert_eq!(e1, e2);
    assert!(!e1.is_empty());

    // 内容哈希漂移 → 拒绝(不存在的哈希按内容寻址失败处理)
    let tampered = pin("0.1.0", "deadbeef");
    assert!(expect_resolve_err(resolve_adapter_for_pin(
        &host,
        Some(&tampered),
        "demo-agent"
    ))
    .to_string()
    .contains("插件包不存在"));

    // pin 的包不贡献该 Agent Type → 拒绝
    let stranger = PluginSourcePin {
        full_id: resolved.full_id.clone(),
        version: "0.1.0".into(),
        content_hash: resolved.content_hash.clone(),
        contribution_id: String::new(),
    };
    assert!(expect_resolve_err(resolve_adapter_for_pin(
        &host,
        Some(&stranger),
        "not-declared"
    ))
    .to_string()
    .contains("不贡献"));
}

#[test]
fn builtin_pin_drift_and_legacy_fallback_semantics_hold() {
    // 内置合成插件注册表:临时插件根,不扫描真实 ~/.monkeyfence
    let tmp = tempfile::tempdir().unwrap();
    let host = PluginHost::load_at_with_catalog(
        tmp.path().to_path_buf(),
        memory_catalog(),
        &Config::default(),
        &[],
    );

    // builtin pin 版本漂移 → 拒绝(合成插件 pin 不随注册表漂移)
    let drifted = PluginSourcePin {
        full_id: "monkeyfence.codex".into(),
        version: "9.9.9".into(),
        content_hash: String::new(),
        contribution_id: String::new(),
    };
    let err =
        expect_resolve_err(resolve_adapter_for_pin(&host, Some(&drifted), "codex")).to_string();
    assert!(err.contains("不一致"), "{err}");

    // pin 指向不存在的插件包 → 拒绝
    let missing = PluginSourcePin {
        full_id: "no.such.plugin".into(),
        version: "1.0.0".into(),
        content_hash: String::new(),
        contribution_id: String::new(),
    };
    assert!(
        expect_resolve_err(resolve_adapter_for_pin(&host, Some(&missing), "codex"))
            .to_string()
            .contains("不存在或不属于")
    );

    // 精确 builtin pin:当前注册表版本一致 → 解析到 codex 适配器
    let pinned = PluginSourcePin {
        full_id: "monkeyfence.codex".into(),
        version: "0.1.0".into(),
        content_hash: String::new(),
        contribution_id: String::new(),
    };
    assert_eq!(
        resolve_adapter_for_pin(&host, Some(&pinned), "codex")
            .unwrap()
            .id(),
        "codex"
    );

    // 无 pin(离散会话/default-cli):回退当前注册表 + legacy 映射,语义保持
    assert_eq!(
        resolve_adapter_for_pin(&host, None, "claude").unwrap().id(),
        "claude-code"
    );
    // 空注册表上的 legacy 回退同样成立(旧内置类型映射不变)
    let (empty, _t2) = empty_host();
    assert_eq!(
        resolve_adapter_for_pin(&empty, None, "claude")
            .unwrap()
            .id(),
        "claude-code"
    );
}

// ---------- 短别名安全(Issue #27:禁止 first-match 影子化) ----------

/// 带内置合成插件的宿主:临时插件根 + 内存目录库,不扫描真实安装目录。
fn builtin_host() -> (
    std::sync::Arc<PluginHost>,
    std::sync::Arc<CatalogStore>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = memory_catalog();
    (
        PluginHost::load_at_with_catalog(
            tmp.path().to_path_buf(),
            catalog.clone(),
            &Config::default(),
            &[],
        ),
        catalog,
        tmp,
    )
}

/// 写入临时清单并安装第三方插件
/// (完整贡献 ID = `{publisher}.{id}.{agent_id}`),返回插件 full_id。
/// 只安装不启用:enable() 会持久化锁文件,而后续 install 会按锁文件
/// 重载内存状态 —— 多包场景须装完再统一 enable(见各测试)。
fn install_third_party(host: &PluginHost, publisher: &str, id: &str, agent_id: &str) -> String {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(
        src.path().join("monkeyfence-plugin.toml"),
        format!(
            r#"[manifest]
version = 3
publisher = "{publisher}"
id = "{id}"
name = "{publisher}.{id} alias fixture"
version_str = "0.1.0"
description = "short-alias conflict fixture"

[[agent_types]]
id = "{agent_id}"
name = "{agent_id} Agent"
adapter = "generic-command"
command = "{agent_id}"
modes = ["interactive", "oneshot"]
"#
        ),
    )
    .unwrap();
    let resolved = host
        .install_package(
            src.path(),
            InstallSource::Local {
                path: src.path().display().to_string(),
            },
        )
        .expect("安装第三方 fixture 不应失败");
    resolved.full_id
}

/// 安装并立即启用(仅单包场景安全:之后不再有 install 触发重载)。
fn install_and_enable(host: &PluginHost, publisher: &str, id: &str, agent_id: &str) -> String {
    let full_id = install_third_party(host, publisher, id, agent_id);
    host.enable(&full_id, true).expect("启用 fixture 不应失败");
    full_id
}

/// aaa.codex.codex 不可影子化内置 monkeyfence.codex.codex 的短别名 codex:
/// 内置短别名稳定归属内置包,完整贡献 ID(内置/第三方)始终精确可用。
#[test]
fn builtin_short_alias_cannot_be_shadowed_by_third_party() {
    let (host, _catalog, _tmp) = builtin_host();
    install_and_enable(&host, "aaa", "codex", "codex");

    let index = workflow_plugin_index(&host);

    // 短别名 codex 仍归属内置包;字典序更靠前的 aaa 不可抢占
    let short = index.get("codex").expect("内置短别名必须保持可用");
    assert_eq!(short.full_id, "monkeyfence.codex");
    assert_eq!(short.contribution_id, "monkeyfence.codex.codex");

    // 完整贡献 ID 始终精确可用:内置与第三方各自指向自己的包
    let builtin_full = index
        .get("monkeyfence.codex.codex")
        .expect("内置完整贡献 ID 必须可用");
    assert_eq!(builtin_full.full_id, "monkeyfence.codex");
    let third_full = index
        .get("aaa.codex.codex")
        .expect("第三方完整贡献 ID 必须可用");
    assert_eq!(third_full.full_id, "aaa.codex");
    assert_ne!(third_full.content_hash, builtin_full.content_hash);

    // 单点解析与索引一致:短别名 → 内置 pin,且 pin 冻结完整贡献身份
    let pin = resolve_agent_type_pin(&host, "codex").unwrap();
    assert_eq!(pin, *short);

    // 端到端:按冻结 pin 派发解析到内置 codex 适配器,不是 aaa 的
    let adapter = resolve_adapter_for_pin(&host, Some(&pin), "codex").unwrap();
    assert_eq!(adapter.id(), "codex");
}

/// 两个第三方贡献短别名相同(x/y 各贡献 foo)时,foo 显式歧义:
/// 不进索引(绝不按字典序选一个)、单点解析稳定拒绝并列出全部候选、
/// 要求完整贡献 ID;两个完整贡献 ID 不受影响;安装顺序不影响结果。
#[test]
fn ambiguous_third_party_short_alias_is_stably_rejected() {
    let (host_a, _catalog, _tmp) = builtin_host();
    let x = install_third_party(&host_a, "x", "tools", "foo"); // x.tools.foo
    let y = install_third_party(&host_a, "y", "tools", "foo"); // y.tools.foo
    for full_id in [&x, &y] {
        host_a.enable(full_id, true).unwrap();
    }

    let index_a = workflow_plugin_index(&host_a);

    // 短别名显式歧义:不进索引(查找落空 → 调用方稳定拒绝)
    assert!(index_a.get("foo").is_none(), "歧义短别名不得注册");
    // 完整贡献 ID 始终精确可用,且 pin 冻结完整贡献身份
    for full in ["x.tools.foo", "y.tools.foo"] {
        let pin = index_a.get(full).expect("完整贡献 ID 必须精确可用");
        assert_eq!(pin.contribution_id, full);
    }

    // 单点解析:错误文本确定(重复一致)、列出候选、要求完整贡献 ID
    let e1 = resolve_agent_type_pin(&host_a, "foo")
        .unwrap_err()
        .to_string();
    let e2 = resolve_agent_type_pin(&host_a, "foo")
        .unwrap_err()
        .to_string();
    assert_eq!(e1, e2, "歧义拒绝必须稳定");
    assert!(e1.contains("歧义"), "{e1}");
    assert!(
        e1.contains("x.tools.foo") && e1.contains("y.tools.foo"),
        "{e1}"
    );
    assert!(e1.contains("完整贡献 ID"), "{e1}");

    // 完整贡献 ID 的单点解析不受别名冲突影响
    assert_eq!(
        resolve_agent_type_pin(&host_a, "x.tools.foo")
            .unwrap()
            .full_id,
        "x.tools"
    );
    assert_eq!(
        resolve_agent_type_pin(&host_a, "y.tools.foo")
            .unwrap()
            .full_id,
        "y.tools"
    );

    // 安装顺序不影响:反向安装(装完统一启用)得到完全相同的索引
    let (host_b, _c2, _t2) = builtin_host();
    let y2 = install_third_party(&host_b, "y", "tools", "foo");
    let x2 = install_third_party(&host_b, "x", "tools", "foo");
    for full_id in [&y2, &x2] {
        host_b.enable(full_id, true).unwrap();
    }
    assert_eq!(workflow_plugin_index(&host_b), index_a);
}

/// 唯一第三方短别名保持兼容:可解析、与完整贡献 ID 指向同一 pin。
#[test]
fn unique_third_party_short_alias_stays_resolvable() {
    let (host, _catalog, _tmp) = builtin_host();
    install_and_enable(&host, "x", "tools", "foo");

    let index = workflow_plugin_index(&host);
    let short = index.get("foo").expect("唯一第三方短别名保持兼容可用");
    assert_eq!(short.full_id, "x.tools");
    assert_eq!(short.contribution_id, "x.tools.foo");
    assert_eq!(*short, *index.get("x.tools.foo").unwrap());

    // 按该 pin 派发(内容寻址 + 贡献身份校验通过)
    let adapter = resolve_adapter_for_pin(&host, Some(short), "foo").unwrap();
    assert_eq!(adapter.id(), "generic-command");
    assert_eq!(resolve_agent_type_pin(&host, "foo").unwrap(), *short);
}

/// pin 解析校验贡献身份与包寻址,不把短别名当冻结身份:
/// - 篡改 contribution_id → 稳定拒绝;旧 pin(contribution_id 空)兼容放行;
/// - 空内容哈希 pin 指向第三方包 → 拒绝(不得绕过内容寻址)。
#[test]
fn pin_resolution_validates_contribution_identity_not_short_alias() {
    let (host, _catalog, _tmp) = builtin_host();
    install_and_enable(&host, "x", "tools", "foo");

    // 新 pin 冻结完整贡献身份;身份被篡改 → 稳定拒绝
    let base = workflow_plugin_index(&host)
        .get("x.tools.foo")
        .expect("完整贡献 ID 必须可用")
        .clone();
    let forged = PluginSourcePin {
        contribution_id: "x.tools.bar".into(),
        ..base.clone()
    };
    let e1 = expect_resolve_err(resolve_adapter_for_pin(&host, Some(&forged), "foo")).to_string();
    let e2 = expect_resolve_err(resolve_adapter_for_pin(&host, Some(&forged), "foo")).to_string();
    assert_eq!(e1, e2);
    assert!(e1.contains("贡献身份"), "{e1}");

    // 旧 pin(contribution_id 为空)兼容放行:已冻结 Revision 行为不变
    let legacy = PluginSourcePin {
        contribution_id: String::new(),
        ..forged.clone()
    };
    assert_eq!(
        resolve_adapter_for_pin(&host, Some(&legacy), "foo")
            .unwrap()
            .id(),
        "generic-command"
    );

    // 空内容哈希 pin 指向第三方包 → 拒绝(第三方必须内容寻址)
    let bypass = PluginSourcePin {
        content_hash: String::new(),
        contribution_id: String::new(),
        ..forged
    };
    assert!(
        expect_resolve_err(resolve_adapter_for_pin(&host, Some(&bypass), "foo"))
            .to_string()
            .contains("不存在或不属于")
    );
}

/// 别名解析的错误与 Debug 输出不含 Secret:目录库中封存的明文/secret id
/// 不得出现在任何错误文本或索引 Debug 里(pin 只承载非敏感标识)。
#[test]
fn alias_errors_and_pins_never_leak_secrets() {
    let (host, catalog, _tmp) = builtin_host();
    let x = install_third_party(&host, "x", "tools", "foo");
    let y = install_third_party(&host, "y", "tools", "foo");
    for full_id in [&x, &y] {
        host.enable(full_id, true).unwrap();
    }

    let store = mf_plugins::builtin_secret_store::BuiltinSecretStore::with_master_key(
        catalog.clone(),
        [9u8; 32],
    )
    .unwrap();
    let secret_id = store
        .seal("TOKEN", b"alias-secret-plaintext-7c31")
        .expect("封存不应失败");

    let err = resolve_agent_type_pin(&host, "foo")
        .unwrap_err()
        .to_string();
    let dbg = format!("{:?}", workflow_plugin_index(&host));
    assert!(!err.contains("alias-secret-plaintext-7c31"));
    assert!(!dbg.contains("alias-secret-plaintext-7c31"));
    assert!(!dbg.contains(&secret_id));
    // PluginSourcePin 只承载插件标识/版本/哈希/贡献 ID,无敏感字段
    let pin = workflow_plugin_index(&host)
        .get("x.tools.foo")
        .unwrap()
        .clone();
    assert!(!format!("{pin:?}").contains("alias-secret-plaintext-7c31"));
}
