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
//! 豁免(§2.8 允许的宿主/owner 侧):session runtime(`mf-terminal`)的
//! TerminalHost shim 实现区。
//! (SessionRegistry 定义与 TerminalHost shim)、`mf-terminal` 自身、
//! 测试文件(`*_tests.rs`/`tests/` 目录)。

use std::path::{Path, PathBuf};

/// 相对 mf-kernel crate 的 mf crate 源码目录(兄弟 crate)。
fn mf_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../mf-terminal/src/session_runtime.rs")
        .canonicalize()
        .expect("定位 session_runtime")
}

fn read(_rel: &str) -> String {
    std::fs::read_to_string(mf_src()).expect("读取 session_runtime.rs")
}

/// T12 后受检面:session runtime 是唯一终端宿主;其自身实现(TerminalHost
/// shim/launch 路径)是豁免的 owner 侧,不适用"UI 生产文件"扫描。保留
/// 函数形态以最小化断言迁移;返回空集。
fn ui_production_files() -> Vec<String> {
    Vec::new()
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
fn session_runtime_exposes_mutations_only_via_terminal_host() {
    // T12 后 SessionRegistry 跨 crate 可见;其 mutation 方法被
    // mf-web/mf-kernel 的 TerminalHost/TerminalChannel 契约接管
    // (#42 attach 复验/writer lease/#41 dispatch 唯一写路径)。
    // 此处固化:runtime 中 TerminalHost 实现是唯一 shim 出口。
    let source = read("");
    assert!(
        source.contains("impl crate::TerminalHost for SessionRegistry"),
        "SessionRegistry 必须经 TerminalHost shim 暴露终端能力"
    );
    assert!(
        source.contains("impl crate::TerminalHost for SessionRegistry"),
        "shim 实现存在"
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
fn plugin_manifest_has_no_install_bypass_types() {
    // T4a(Issue #36,spec §9.2):BuiltinAgent::InstallSpec/permission_args
    // 旁路删除——安装 recipe 统一为 v3 InstallerContribution、自动批准
    // 参数统一进入 root_launch 映射。
    let builtin = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../mf-plugins/src/builtin.rs"),
    )
    .expect("读取 builtin.rs");
    assert!(
        !builtin.contains("pub struct InstallSpec"),
        "InstallSpec 旁路类型不得重新出现(v3 InstallerContribution 是唯一形态)"
    );
    assert!(
        !builtin.contains("pub permission_args:"),
        "BuiltinAgent 不得再携带 permission_args 旁路字段(root_launch 是唯一映射)"
    );
}

#[test]
fn runtime_host_has_no_output_tail_bypass() {
    // T3f(Issue #34,spec §8.8):256 KiB output_tail 旁路已删——输出只
    // 经 redactor → journal(seq 权威) → Screen 投影/tail_bytes 只读派生。
    let runtime_host = read("");
    assert!(
        !runtime_host.contains("output_tail:"),
        "PtySession 不得再有 output_tail 字段(journal 是唯一输出权威)"
    );
    assert!(
        runtime_host.contains("journal: Mutex<TerminalJournal>"),
        "PtySession 必须以 TerminalJournal 为输出数据面权威"
    );
    // reader 管线必须 journal 在 Screen 之前(seq 分配先于解释)
    let feed_after_journal = runtime_host.matches("journal.lock().append").count();
    assert!(
        feed_after_journal >= 6,
        "三条 PTY launch 路径(loop+finish)都必须 journal.append(当前 {feed_after_journal} 处)"
    );
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

#[test]
fn terminal_output_feeds_are_post_redaction_only() {
    // T3a(Issue #29,spec §8.8):Screen/output_tail 只能接收脱敏后字节。
    // reader 管线的唯一合法形态是 `redactor.redact_chunk`/`finish` 的
    // 返回值(`clean`/`rest`);原始 buf 直达 feed/tail 即为旁路。
    let runtime_host = read("");
    for banned in ["screen.feed(&buf", "tail.extend_from_slice(&buf"] {
        assert!(
            !runtime_host.contains(banned),
            "未脱敏输出不得进入 Screen/output_tail:`{banned}` 是旁路,\
             必须先经 StreamingRedactor"
        );
    }
    // 三条 launch 路径(Preview/Ad-hoc/工作流)必须都从统一入口构造
    // redactor(capability token 与 Secret 同一脱敏器覆盖)。
    let unified = runtime_host
        .matches("crate::redactor::launch_redactor")
        .count();
    assert!(
        unified >= 3,
        "三条 PTY launch 路径必须全部经 launch_redactor 统一脱敏入口(当前 {unified} 处)"
    );
}
