# Agent Workflow UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver Agent Instance management, workflow editing, task assignment, task-level CLI launch, and run monitoring in GPUI.

**Architecture:** Keep stateful UI logic in small testable models and render with GPUI host controls. Plugin UI contributions remain declarative. The workflow editor defaults to the left-library canvas layout and supports the stacked alternative as a persisted preference.

**Tech Stack:** Rust, GPUI, mf-agent, mf-plugins, test-support GPUI.

**Spec:** `docs/superpowers/specs/2026-08-28-agent-workflow-plugin-design.md`

## Global Constraints

- Default layout is B: Agent Instance library left, DAG canvas center, inspector right.
- Layout A is selectable and remembered per user.
- Every Task has a `+` menu for default detected CLIs, Agent Instances, temporary instances, and terminals.
- Ad-hoc CLI sessions do not mutate Task status.
- No UI action writes real CLI global configuration.
- Commit locally after every task; do not push.

---

### Task 1: Declarative form renderer and Agent Instance page

**Files:**
- Create: `crates/mf/src/declarative_form.rs`
- Create: `crates/mf/src/agent_instances_view.rs`
- Create: `crates/mf/src/agent_instance_editor.rs`
- Modify: `crates/mf/src/settings.rs`
- Modify: `crates/mf/src/main.rs`
- Test: `crates/mf/src/agent_instance_tests.rs`

**Interfaces:**
- Consumes Agent Type schemas and CatalogStore CRUD from earlier plans
- Produces `AgentInstancesViewModel` and `AgentInstanceEditorState`

- [ ] **Step 1: Write failing pure-state tests**

```rust
#[test]
fn unavailable_type_is_visible_but_cannot_save_instance() {
    let mut state = AgentInstanceEditorState::new(unavailable_type());
    state.set_name("Review");
    assert!(state.validation().iter().any(|e| e.code == "cli-not-detected"));
    assert!(!state.can_save());
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf agent_instance_tests -- --nocapture`

Expected: compilation fails because view-model types are absent.

- [ ] **Step 3: Implement list, filtering, editor, and Secret masking**

Show Agent Types and instances together but visually distinguish default CLI entries from persisted instances. Buttons are Save and Validate only; do not add Apply/Activate wording. Render Schema fields through `DeclarativeForm`, including permission and Secret annotations.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf agent_instance_tests -- --nocapture`

Expected: available, unavailable, masking, validation, project override, and save tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf/src/declarative_form.rs crates/mf/src/agent_instances_view.rs crates/mf/src/agent_instance_editor.rs crates/mf/src/settings.rs crates/mf/src/main.rs crates/mf/src/agent_instance_tests.rs
git commit -m "feat(ui): add agent instance management"
```

### Task 2: Workflow Editor layouts and node inspector

**Files:**
- Create: `crates/mf/src/workflow_editor.rs`
- Create: `crates/mf/src/workflow_canvas.rs`
- Create: `crates/mf/src/workflow_node_inspector.rs`
- Modify: `crates/mf/src/agent_workspace.rs`
- Modify: `crates/mf/src/theme.rs`
- Test: `crates/mf/src/workflow_editor_tests.rs`

**Interfaces:**
- Produces: `WorkflowLayout::{Sidebar, Stacked}` with default `Sidebar`
- Produces: `WorkflowEditorState` independent of GPUI rendering
- Consumes compiler diagnostics from orchestration plan

- [ ] **Step 1: Write failing editor-state tests**

```rust
#[test]
fn default_layout_is_sidebar_and_user_choice_persists() {
    let mut prefs = MemoryPrefs::default();
    let mut state = WorkflowEditorState::load(&prefs);
    assert_eq!(state.layout(), WorkflowLayout::Sidebar);
    state.set_layout(WorkflowLayout::Stacked, &mut prefs);
    assert_eq!(WorkflowEditorState::load(&prefs).layout(), WorkflowLayout::Stacked);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf workflow_editor_tests -- --nocapture`

Expected: compilation fails because editor modules are absent.

- [ ] **Step 3: Implement canvas model and GPUI views**

Support drag-from-library, stable node selection, dependency creation/removal, topological auto-layout, zoom, pan, compiler diagnostics, and a collapsible inspector. Preserve the current `AgentWorkspace` tab entry but move workflow-specific state out of the 2k-line file.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf workflow_editor_tests -- --nocapture`

Expected: layout, drag/drop model, cycle rejection, node selection, inspector, and preference tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf/src/workflow_editor.rs crates/mf/src/workflow_canvas.rs crates/mf/src/workflow_node_inspector.rs crates/mf/src/agent_workspace.rs crates/mf/src/theme.rs crates/mf/src/workflow_editor_tests.rs
git commit -m "feat(ui): add workflow DAG editor"
```

### Task 3: Task workflow assignment and `+` CLI menu

**Files:**
- Modify: `crates/mf/src/task_composer.rs`
- Modify: `crates/mf/src/task_composer_tests.rs`
- Modify: `crates/mf/src/task_sidebar.rs`
- Create: `crates/mf/src/task_cli_menu.rs`
- Test: `crates/mf/src/task_cli_menu_tests.rs`

**Interfaces:**
- Produces Task creation choices: existing template or task-local workflow
- Produces CLI menu entries: terminal, detected default Agent Type, Agent Instance, temporary instance

- [ ] **Step 1: Write failing menu and assignment tests**

```rust
#[test]
fn plus_menu_lists_detected_types_and_instances() {
    let menu = build_task_cli_menu(catalog_fixture());
    assert!(menu.iter().any(|i| i.label == "Codex" && i.kind == MenuKind::DefaultCli));
    assert!(menu.iter().any(|i| i.label == "Codex Review" && i.kind == MenuKind::AgentInstance));
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf task_cli_menu_tests -- --nocapture`

Expected: compilation fails because menu builder is absent.

- [ ] **Step 3: Implement composer selection and task session rows**

Default CLI launches use plugin default command and inherit the external CLI's existing configuration without writing it. Agent Instance launches use frozen isolated config. Task-local workflows remain private until the user invokes Save as Template.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf task_composer_tests -- --nocapture`

Expected: task composer assignment tests pass.

Run: `cargo test -p mf task_cli_menu_tests -- --nocapture`

Expected: assignment, temporary workflow, menu filtering, ad-hoc launch, and Task-status independence tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf/src/task_composer.rs crates/mf/src/task_composer_tests.rs crates/mf/src/task_sidebar.rs crates/mf/src/task_cli_menu.rs crates/mf/src/task_cli_menu_tests.rs
git commit -m "feat(tasks): assign workflows and launch CLIs"
```

### Task 4: Run Monitor and needs-you actions

**Files:**
- Create: `crates/mf/src/run_monitor.rs`
- Create: `crates/mf/src/run_node_details.rs`
- Modify: `crates/mf/src/agent_workspace.rs`
- Modify: `crates/mf/src/project_overview.rs`
- Test: `crates/mf/src/run_monitor_tests.rs`

**Interfaces:**
- Consumes Step, Run, Session, Handoff, Execution Lease, and compiler diagnostics
- Produces actions: continue, fresh retry, skip, manual complete/fail, cancel, resolve merge

- [ ] **Step 1: Write failing action-availability tests**

```rust
#[test]
fn unknown_run_offers_observe_settle_or_retry_but_not_success_badge() {
    let model = RunNodeDetails::from(interrupted_run());
    assert!(model.actions.contains(&RunAction::Observe));
    assert!(model.actions.contains(&RunAction::FreshRetry));
    assert!(!model.is_success());
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf run_monitor_tests -- --nocapture`

Expected: compilation fails because monitor types are absent.

- [ ] **Step 3: Implement DAG status projection and details**

Show execution-directory provider, isolation state, unsafe parallel warning, logs, Handoff, file list, verification, session, attempts, and needs-you reasons. Every destructive or externally consequential action keeps the existing explicit confirmation pattern.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf run_monitor_tests -- --nocapture`

Expected: status, retry, skip, cancel, unknown, needs-input, and merge-conflict projections pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf/src/run_monitor.rs crates/mf/src/run_node_details.rs crates/mf/src/agent_workspace.rs crates/mf/src/project_overview.rs crates/mf/src/run_monitor_tests.rs
git commit -m "feat(ui): add workflow run monitor"
```

### Task 5: Plugin Manager contributions and end-to-end verification

**Files:**
- Modify: `crates/mf/src/settings.rs`
- Create: `crates/mf/src/plugin_contribution_view.rs`
- Modify: `crates/mf/src/workspace_interaction_tests.rs`
- Modify: `README.md`
- Test: `crates/mf/src/agent_workflow_e2e_tests.rs`

**Interfaces:**
- Shows contribution types, requested permissions, fixed versions, hashes, and compatibility
- Completes all acceptance criteria in the specification

- [ ] **Step 1: Write failing end-to-end acceptance test**

```rust
#[test]
fn two_claude_instances_run_without_global_config_writes() {
    let fx = WorkflowE2eFixture::new();
    fx.create_claude_instances(["implementation", "review"]);
    fx.run_parallel_workflow();
    assert_ne!(fx.run_env("implementation", "CLAUDE_CONFIG_DIR"), fx.run_env("review", "CLAUDE_CONFIG_DIR"));
    assert_eq!(fx.global_cli_config_hash_after(), fx.global_cli_config_hash_before());
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf agent_workflow_e2e_tests -- --nocapture`

Expected: acceptance fixture or behavior is absent.

- [ ] **Step 3: Implement plugin contribution presentation and documentation**

Use manifest-provided Schema and permission descriptions. Show that workers and CLI processes run as the current OS user. Document instance isolation, workflow assignment, the Task `+` menu, retries, and unsafe shared-directory parallelism.

- [ ] **Step 4: Run full verification**

Run: `cargo fmt --all -- --check`

Expected: exit 0.

Run: `cargo test --workspace`

Expected: all tests pass with zero failures.

Run: `cargo check --workspace`

Expected: exit 0.

Run: `git diff --check`

Expected: no output and exit 0.

- [ ] **Step 5: Commit**

```bash
git add crates/mf/src/settings.rs crates/mf/src/plugin_contribution_view.rs crates/mf/src/workspace_interaction_tests.rs crates/mf/src/agent_workflow_e2e_tests.rs README.md
git commit -m "feat: deliver plugin-first agent workflows"
```
