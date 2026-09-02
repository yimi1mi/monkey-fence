//! 静态审计(Issue #26):三个生产 UI 文件的 Workflow Run 读取只经
//! Core Snapshot 投影,不得直接调用 Orchestrator 的 Workflow Run
//! mutation(cancel/retry/settle/respond 等)。
//!
//! - `execute_action` 之类直接走 Orchestrator mutation 的测试助手必须
//!   留在测试文件,不得回流生产文件;
//! - 读路径必须经 `collect_via_kernel` / `KernelProjectionSource` 接缝。

const AUDITED_FILES: [(&str, &str); 3] = [
    ("project_overview.rs", include_str!("project_overview.rs")),
    (
        "workflow_runs_page.rs",
        include_str!("workflow_runs_page.rs"),
    ),
    ("run_monitor.rs", include_str!("run_monitor.rs")),
];

const APP_CTX: &str = include_str!("app_ctx.rs");
const AGENT_WORKSPACE: &str = include_str!("agent_workspace.rs");

/// Workflow Run mutation 的直接调用(任意接收者形式)在生产 UI 文件禁止。
const FORBIDDEN_MUTATIONS: [&str; 5] = [
    "cancel_task(",
    "retry_step(",
    "settle_run(",
    "cancel_run(",
    "answer_question(",
];

#[test]
fn production_ui_files_do_not_call_workflow_run_mutations() {
    for (name, source) in AUDITED_FILES {
        for banned in FORBIDDEN_MUTATIONS {
            assert!(
                !source.contains(banned),
                "{name} 不得直接调用 Workflow Run mutation `{banned}`;\
                 写路径必须经 AppCtx 的 CoreKernel seam"
            );
        }
    }
}

#[test]
fn workflow_run_reads_route_through_kernel_snapshots() {
    let (_, overview) = AUDITED_FILES[0];
    assert!(
        overview.contains("KernelProjectionSource"),
        "总览必须经 KernelProjectionSource 读取 Core Workspace Snapshot"
    );
    let (_, run_monitor) = AUDITED_FILES[2];
    assert!(
        run_monitor.contains("collect_via_kernel"),
        "RunMonitor 必须经 Core WorkflowRunSnapshot 读取业务事实"
    );
}

#[test]
fn workflow_start_and_ui_actions_have_no_orchestrator_write_bypass() {
    let run_project_workflow = APP_CTX
        .split("pub fn run_project_workflow")
        .nth(1)
        .and_then(|tail| tail.split("fn secret_store").next())
        .expect("run_project_workflow source section");
    for banned in ["create_task(", "confirm_and_run(", "discard_task("] {
        assert!(
            !run_project_workflow.contains(banned),
            "run_project_workflow 必须返回 Core Accepted Operation，不得调用 `{banned}`"
        );
    }
    for (name, source) in [
        ("run_monitor.rs", AUDITED_FILES[2].1),
        ("agent_workspace.rs", AGENT_WORKSPACE),
    ] {
        for banned in [
            ".skip_step(",
            ".send_prompt(",
            ".cancel_task(",
            ".confirm_and_run(",
        ] {
            assert!(
                !source.contains(banned),
                "{name} 不得绕过 CoreKernel 调用 `{banned}`"
            );
        }
    }
    assert!(
        !APP_CTX.contains(".cancel_task("),
        "Project close 必须先经 Core durable cancel fence"
    );
}
