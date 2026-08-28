# Agent Workflow Orchestration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Compile versioned Workflow Templates into immutable revisions and execute them with Handoff, retries, directory leases, and optional Git worktree isolation.

**Architecture:** Workflow Compiler is pure domain logic. Run Coordinator owns state transitions only. Execution Directory Providers and merge behavior are plugins, so Task and Step remain VCS-agnostic.

**Tech Stack:** Rust, rusqlite, serde_json, crossbeam-channel, git2 through `mf-vcs`.

**Spec:** `docs/superpowers/specs/2026-08-28-agent-workflow-plugin-design.md`

## Global Constraints

- First release supports serial, parallel fan-out, and join only.
- Complete/fail Settlement remains the only terminal success/failure authority.
- Unknown process state is not failure.
- Worktree state never participates in Task success.
- Commit locally after every task; do not push.

---

### Task 1: Workflow Templates, snapshots, and Handoff persistence

**Files:**
- Create: `crates/mf-agent/src/workflow.rs`
- Create: `crates/mf-agent/src/handoff.rs`
- Modify: `crates/mf-agent/src/catalog_store.rs`
- Modify: `crates/mf-agent/src/store.rs`
- Modify: `crates/mf-agent/src/model.rs`
- Modify: `crates/mf-agent/src/pipeline.rs`
- Test: `crates/mf-agent/tests/workflow_snapshots.rs`

**Interfaces:**
- Produces: `WorkflowTemplate`, `WorkflowTemplateVersion`, `WorkflowSnapshot`, `Handoff`
- Step drafts reference `agent_instance_id`, not `agent_profile`
- Revision stores serialized `WorkflowSnapshot`

- [ ] **Step 1: Write failing immutable snapshot and Handoff tests**

```rust
#[test]
fn template_edit_does_not_change_revision_snapshot() {
    let fixture = fixture();
    let version = fixture.catalog.save_template(template("v1")).unwrap();
    let revision = fixture.compile(version.id).unwrap();
    fixture.catalog.save_template(template("v2")).unwrap();
    assert_eq!(revision.snapshot.nodes[0].instructions, "v1");
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-agent --test workflow_snapshots -- --nocapture`

Expected: compilation fails on workflow and Handoff types.

- [ ] **Step 3: Implement versioned rows and fixed Handoff fields**

Use stable node keys. Store custom output as `serde_json::Value`; store raw logs only as references. Remove `agent_profile` from new schema and model APIs.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-agent --test workflow_snapshots -- --nocapture`

Expected: template, task-local template, snapshot, and Handoff round-trip tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-agent/src/workflow.rs crates/mf-agent/src/handoff.rs crates/mf-agent/src/catalog_store.rs crates/mf-agent/src/store.rs crates/mf-agent/src/model.rs crates/mf-agent/src/pipeline.rs crates/mf-agent/tests/workflow_snapshots.rs
git commit -m "feat(workflow): persist templates and handoffs"
```

### Task 2: Pure Workflow Compiler

**Files:**
- Create: `crates/mf-agent/src/workflow_compiler.rs`
- Modify: `crates/mf-agent/src/lib.rs`
- Test: `crates/mf-agent/tests/workflow_compiler.rs`

**Interfaces:**
- Produces: `WorkflowCompiler::compile(input: CompileInput) -> Result<WorkflowSnapshot, Vec<CompileError>>`
- Validates DAG, variables, instances, plugin pins, and parallel safety

- [ ] **Step 1: Write failing compiler table tests**

```rust
#[test]
fn rejects_cycle_and_unknown_output_reference_together() {
    let errors = compiler().compile(cyclic_with_bad_reference()).unwrap_err();
    assert!(errors.iter().any(|e| e.code == "cycle"));
    assert!(errors.iter().any(|e| e.code == "unknown-output"));
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-agent --test workflow_compiler -- --nocapture`

Expected: compilation fails because compiler interfaces do not exist.

- [ ] **Step 3: Implement deterministic compilation**

Return every validation error in stable node-key order. Reject conditions and cycles. Reject parallel reuse of the same interactive session. Require an explicit unsafe-shared-directory flag when the selected directory provider cannot isolate parallel nodes.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-agent --test workflow_compiler -- --nocapture`

Expected: valid, cycle, missing dependency, variable, plugin, instance, and unsafe-parallel cases pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-agent/src/workflow_compiler.rs crates/mf-agent/src/lib.rs crates/mf-agent/tests/workflow_compiler.rs
git commit -m "feat(workflow): compile immutable revisions"
```

### Task 3: Retry-aware Run Coordinator and Handoff unlocking

**Files:**
- Modify: `crates/mf-agent/src/orchestrator.rs`
- Modify: `crates/mf-agent/src/store.rs`
- Modify: `crates/mf-agent/src/model.rs`
- Test: `crates/mf-agent/tests/retry_and_handoff.rs`

**Interfaces:**
- Produces: `RetryPolicy { automatic_attempts: u32 }`
- Produces: `RetryMode::{ContinueSession, FreshSession}`
- Produces: `Orchestrator::retry_step(step_id, mode)`
- Downstream steps unlock only after successful Settlement and persisted Handoff

- [ ] **Step 1: Write failing state-machine tests**

```rust
#[test]
fn retry_success_unblocks_descendant_but_other_branch_keeps_running() {
    let fx = parallel_fixture();
    fx.fail("build");
    assert_eq!(fx.status("package"), StepStatus::Blocked);
    assert_eq!(fx.status("docs"), StepStatus::Running);
    fx.retry_and_complete("build");
    assert_eq!(fx.status("package"), StepStatus::Ready);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-agent --test retry_and_handoff -- --nocapture`

Expected: test fails because descendants remain blocked or retry mode is absent.

- [ ] **Step 3: Implement retry attempts and atomic Handoff settlement**

Persist Handoff and Settlement in one transaction. Automatic retries create fresh sessions and preserve files. Manual continue is allowed only for a live interactive session. Exhausted retries leave Step failed and descendants blocked.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-agent --test retry_and_handoff -- --nocapture`

Expected: branch independence, retry exhaustion, skip, cancel, and idempotent settlement tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-agent/src/orchestrator.rs crates/mf-agent/src/store.rs crates/mf-agent/src/model.rs crates/mf-agent/tests/retry_and_handoff.rs
git commit -m "feat(orchestrator): add retries and handoffs"
```

### Task 4: Execution Directory Provider seam

**Files:**
- Create: `crates/mf-agent/src/execution_directory.rs`
- Modify: `crates/mf-agent/src/runtime.rs`
- Modify: `crates/mf-agent/src/orchestrator.rs`
- Create: `crates/mf-plugins/src/project_directory_provider.rs`
- Test: `crates/mf-agent/tests/execution_leases.rs`

**Interfaces:**
- Produces: `ExecutionDirectoryProvider::{acquire, merge, release}`
- Produces: `ExecutionLease { id, path, isolated, provider, metadata }`
- Produces: `MergeOutcome::{Merged, NeedsUser { conflicts }, NotRequired}`

- [ ] **Step 1: Write failing lifecycle tests**

```rust
#[test]
fn lease_is_released_after_terminal_run() {
    let provider = RecordingProvider::default();
    fixture_with(provider.clone()).complete_one_run();
    assert_eq!(provider.released_ids(), provider.acquired_ids());
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-agent --test execution_leases -- --nocapture`

Expected: compilation fails on provider and lease interfaces.

- [ ] **Step 3: Implement seam and default project provider**

Coordinator sees only the interface. Persist the lease before process launch and release it after terminal settlement/cancel. Unknown process state keeps the lease held.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-agent --test execution_leases -- --nocapture`

Expected: acquire, release, retained-on-unknown, and unsafe-parallel tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-agent/src/execution_directory.rs crates/mf-agent/src/runtime.rs crates/mf-agent/src/orchestrator.rs crates/mf-plugins/src/project_directory_provider.rs crates/mf-agent/tests/execution_leases.rs
git commit -m "feat(runtime): add execution directory leases"
```

### Task 5: Git worktree provider and deterministic merge

**Files:**
- Modify: `crates/mf-plugins/Cargo.toml`
- Create: `crates/mf-plugins/src/git_worktree_provider.rs`
- Modify: `crates/mf-vcs/src/git.rs`
- Test: `crates/mf-plugins/tests/git_worktree_provider.rs`

**Interfaces:**
- Consumes: Execution Directory Provider from Task 4
- Worktree names: `mf-run-<task>-<step>-<attempt>`
- Merge order: topological dependency order, then stable step key

- [ ] **Step 1: Write failing real-repository tests**

```rust
#[test]
fn conflicting_join_returns_needs_user_without_overwrite() {
    let fx = git_fixture_with_conflicting_branches();
    let outcome = fx.provider.merge(&fx.leases).unwrap();
    assert!(matches!(outcome, MergeOutcome::NeedsUser { .. }));
    assert_eq!(fs::read_to_string(fx.root.join("shared.txt")).unwrap(), "base\n");
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-plugins --test git_worktree_provider -- --nocapture`

Expected: compilation fails because provider is absent.

- [ ] **Step 3: Implement safe worktree create, merge, and release**

Validate every resolved worktree path is under the repository sibling `.worktrees` directory before recursive removal. On conflict, abort the merge and retain leases. Never delete the project root, repository metadata, or an unresolved path.

- [ ] **Step 4: Run orchestration verification**

Run: `cargo test -p mf-plugins --test git_worktree_provider -- --nocapture`

Expected: isolated create, ordered merge, conflict, non-Git fallback, and cleanup tests pass.

Run: `cargo test -p mf-agent`

Expected: full domain suite passes.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-plugins/Cargo.toml crates/mf-plugins/src/git_worktree_provider.rs crates/mf-vcs/src/git.rs crates/mf-plugins/tests/git_worktree_provider.rs
git commit -m "feat(worktree): isolate parallel agent runs"
```

### Task 6: Restart recovery with unknown-state semantics

**Files:**
- Modify: `crates/mf-agent/src/store.rs`
- Modify: `crates/mf-agent/src/orchestrator.rs`
- Modify: `crates/mf-agent/src/model.rs`
- Test: `crates/mf-agent/tests/restart_recovery.rs`

**Interfaces:**
- Produces recovery states: reattached, awaiting outcome, interrupted/unknown
- Unknown runs never settle as failed automatically

- [ ] **Step 1: Write failing restart matrix tests**

```rust
#[test]
fn lost_process_becomes_interrupted_not_failed() {
    let recovered = recover_fixture(false, None);
    assert_eq!(recovered.run.status, RunStatus::Interrupted);
    assert_eq!(recovered.step.status, StepStatus::AwaitingOutcome);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-agent --test restart_recovery -- --nocapture`

Expected: old recovery marks the run failed or lacks interrupted state.

- [ ] **Step 3: Implement reattach and unknown-state recovery**

Reattach when Runtime Host confirms a session handle. Mark exited-without-result as awaiting outcome. Keep Execution Lease when process state is unknown. Expose manual settle and retry actions.

- [ ] **Step 4: Run complete orchestration suite**

Run: `cargo test -p mf-agent`

Expected: all tests pass.

Run: `cargo test -p mf-plugins`

Expected: all tests pass.

Run: `cargo check --workspace`

Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-agent/src/store.rs crates/mf-agent/src/orchestrator.rs crates/mf-agent/src/model.rs crates/mf-agent/tests/restart_recovery.rs
git commit -m "fix(recovery): preserve unknown run state"
```
