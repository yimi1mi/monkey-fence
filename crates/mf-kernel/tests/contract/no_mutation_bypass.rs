//! 仓库级 mutation bypass audit(Issue #28,T2 gate)。
//!
//! canonical spec §2.8:GPUI/Companion/测试 harness 之外不存在对
//! Store/Orchestrator/SessionRegistry/raw PTY 的直接 mutation 引用;
//! 全部写路径经 `CoreKernel::dispatch`,终端写输入只经
//! `attach_terminal` 返回的 `TerminalChannel`。
//!
//! 本 audit 以源文本扫描实现(与 `orchestrator_api_audit`、
//! `kernel_projection_audit` 同一手法),覆盖:
//!
//! 1. `send_prompt_raw` 类 raw 旁路不再是外部入口(可见性收窄到
//!    runtime_host.rs 的 crate 内部,且生产 UI 文件零调用);
//! 2. SessionRegistry 的终端 mutation(update_config/kill_session/
//!    stop_run)不出现在生产 UI 文件;
//! 3. `AppCtx`/`ProjectHandle` 字段私有(无 pub 字段泄漏可写句柄);
//! 4. `TerminalChannel::attach` 只在 mf-kernel 的 attach_terminal
//!    路径构造,UI/Companion 不自行造通道;
//! 5. Workflow Run mutation 族在 UI 生产文件无 Orchestrator 直调
//!    (跨 crate 复检,mf crate 内 `kernel_projection_audit` 的超集)。
//!
//! 豁免(§2.8 允许的宿主/owner 侧):`crates/mf/src/runtime_host.rs`
//! (SessionRegistry 定义与 TerminalHost shim)、`mf-terminal` 自身、
//! 测试文件(`*_tests.rs`/`tests/` 目录)。

use std::path::{Path, PathBuf};

/// 相对 mf-kernel crate 的 mf crate 源码目录(兄弟 crate)。
fn mf_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../mf/src")
        .canonicalize()
        .expect("定位 crates/mf/src")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(mf_src().join(rel))
        .unwrap_or_else(|error| panic!("读取 crates/mf/src/{rel} 失败:{error}"))
}

/// 生产 UI 源文件全集:动态扫描 `crates/mf/src/*.rs`(子目录与测试
/// 文件排除)。新增 UI 文件自动进入受检面,不经任何人维护清单。
/// 白名单(§2.8 豁免的宿主/owner/装配侧):
/// - `runtime_host.rs`:SessionRegistry 定义与 TerminalHost shim;
/// - `app_ctx.rs`:CoreKernel 装配件(apply_engine_settings 等统一入口
///   在此内部传播到 registry;其字段私有化由独立断言覆盖)。
fn ui_production_files() -> Vec<String> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(mf_src()).expect("扫描 crates/mf/src") {
        let entry = entry.expect("读取目录项");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".rs")
            || name.ends_with("_tests.rs")
            || name == "runtime_host.rs"
            || name == "app_ctx.rs"
        {
            continue;
        }
        files.push(name);
    }
    files.sort();
    assert!(
        files.len() >= 20,
        "受检面异常偏小({} 个文件),检查路径解析",
        files.len()
    );
    files
}

/// 终端/会话 mutation 的直接调用(任意接收者形式)在 UI 生产文件禁止;
/// 唯一路径是 `attach_terminal` → `TerminalChannel`。
const TERMINAL_MUTATIONS: [&str; 5] = [
    ".send_prompt_raw(",
    ".send_prompt(",
    ".kill_session(",
    ".stop_run(",
    ".update_config(",
];

#[test]
fn ui_production_files_have_no_direct_terminal_mutations() {
    for file in ui_production_files() {
        let source = read(&file);
        for banned in TERMINAL_MUTATIONS {
            assert!(
                !source.contains(banned),
                "crates/mf/src/{file} 不得直接调用终端 mutation `{banned}`;\
                 写输入/终止必须经 CoreKernel attach_terminal 返回的 TerminalChannel"
            );
        }
    }
}

#[test]
fn session_registry_mutations_are_not_public_entrypoints() {
    let runtime_host = read("runtime_host.rs");
    // 可见性收窄:外部 crate 无法再拿到这些 mutation;同 crate 的测试
    // harness(pub(crate))按 §2.8 豁免。
    for banned in [
        "pub fn send_prompt(",
        "pub fn send_prompt_raw(",
        "pub fn kill_session(",
        "pub fn stop_run(",
        "pub fn update_config(",
    ] {
        assert!(
            !runtime_host.contains(banned),
            "SessionRegistry 的 `{}` 必须是 pub(crate):\
             它不再是外部 mutation 入口(TerminalHost shim/runtime 内部使用)",
            banned.trim_start_matches("pub fn ")
        );
    }
}

#[test]
fn app_ctx_and_project_handle_have_no_public_mutable_fields() {
    let app_ctx = read("app_ctx.rs");
    for banned in [
        "pub registry:",
        "pub plugins:",
        "pub catalog_store:",
        "pub limiter:",
        "pub keep_awake:",
        "pub catalog:",
        "pub config:",
        "pub overview:",
        "pub orchestrator:",
    ] {
        assert!(
            !app_ctx.contains(banned),
            "AppCtx/ProjectHandle 字段必须私有(`{banned}` 违反 T2 字段私有化);\
             外部经访问器/attach_terminal/dispatch"
        );
    }
    // 终端通道唯一 UI 入口存在
    assert!(
        app_ctx.contains("pub fn attach_terminal("),
        "AppCtx 必须提供 attach_terminal 作为终端通道唯一 UI 入口"
    );
}

#[test]
fn terminal_channel_construction_is_kernel_only() {
    // TerminalChannel::attach 只允许出现在 mf-kernel 的 attach_terminal
    // 实现;UI/Companion 不得自行构造通道绕过存在性校验与宿主注入。
    for file in ui_production_files() {
        let source = read(&file);
        assert!(
            !source.contains("TerminalChannel::attach"),
            "crates/mf/src/{file} 不得直接构造 TerminalChannel;\
             通道只能来自 CoreKernel::attach_terminal"
        );
    }
    let runtime_host = read("runtime_host.rs");
    assert!(
        !runtime_host.contains("TerminalChannel::attach"),
        "runtime_host.rs 是宿主实现,也不得自行构造 TerminalChannel"
    );
}

#[test]
fn ui_production_files_have_no_workflow_run_orchestrator_bypass() {
    // §16.1 T2 验收复检:已接管 Kernel 的命令族不得残留 Orchestrator
    // 直调旁路(rename/workflow/run/settle 在 #23–#27 迁移完成)。
    // agent_workspace.rs 不在此列:其 `settle_run` 是同名 UI action
    // handler,内部经 `settle_agent_run_via_kernel`(kernel dispatch);
    // 语义由 `kernel_projection_audit_tests` 的 app_ctx 部分覆盖。
    const RUN_MUTATIONS: [&str; 5] = [
        ".cancel_task(",
        ".retry_step(",
        ".settle_run(",
        ".cancel_run(",
        ".answer_question(",
    ];
    for file in ui_production_files() {
        if file == "agent_workspace.rs" {
            continue;
        }
        let source = read(&file);
        for banned in RUN_MUTATIONS {
            assert!(
                !source.contains(banned),
                "crates/mf/src/{file} 不得直接调用 Workflow Run mutation `{banned}`;\
                 写路径必须经 CoreKernel::dispatch"
            );
        }
    }
}

#[test]
fn mf_terminal_is_the_only_terminal_channel_source() {
    // TerminalChannel/TerminalHost 的定义只存在于 mf-terminal;kernel
    // 仅 re-export。防止在 kernel/UI 侧复制类型形成第二套通道契约。
    let kernel =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/kernel.rs"))
            .expect("读取 kernel.rs");
    assert!(
        kernel.contains("pub use mf_terminal::channel::{"),
        "mf-kernel 的 TerminalChannel 必须 re-export 自 mf-terminal(单一契约源)"
    );
}
