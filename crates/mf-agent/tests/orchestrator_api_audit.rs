#[test]
fn orchestrator_has_no_public_retry_or_respond_store_bypass() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/orchestrator.rs"),
    )
    .expect("读取 orchestrator.rs");

    for forbidden in ["pub fn retry_step", "pub fn answer_question"] {
        assert!(
            !source.contains(forbidden),
            "Orchestrator 不得重新暴露绕过 Core/CAS 的 `{forbidden}`"
        );
    }
}

#[test]
fn after_skip_post_commit_seam_never_rewrites_domain_projection() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/orchestrator.rs"),
    )
    .expect("读取 orchestrator.rs");
    let body = source
        .split("fn after_skip_for_delivery")
        .nth(1)
        .and_then(|tail| tail.split("fn task_has_active_runs").next())
        .expect("定位 after_skip_for_delivery");
    for forbidden in [
        "promote_ready_tx",
        "set_task_status(",
        "set_task_status_and_unread(",
        "set_task_unread(",
        "SchedulerEvent::",
    ] {
        assert!(
            !body.contains(forbidden),
            "AfterSkip post-commit 不得重写已冻结领域投影:{forbidden}"
        );
    }
}
