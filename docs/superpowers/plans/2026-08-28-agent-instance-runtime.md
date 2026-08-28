# Agent Instance Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add encrypted Agent Instances, process-scoped CLI configuration, adapter contracts, and task-attached ad-hoc sessions.

**Architecture:** `mf-agent` owns instance and launch domain types. `mf-plugins` supplies adapters and Secret Store implementations. `mf` Runtime Host launches only resolved `LaunchPlan` values and never edits external CLI homes.

**Tech Stack:** Rust, rusqlite, serde, keyring, aes-gcm, rand, zeroize, portable-pty.

**Spec:** `docs/superpowers/specs/2026-08-28-agent-workflow-plugin-design.md`

## Global Constraints

- Never write `~/.claude`, `~/.codex`, or another real CLI global configuration directory.
- Default execution uses executable plus argv; Shell mode requires explicit plugin permission.
- Secret plaintext exists only in launch-time memory and must be zeroized.
- Claude uses `CLAUDE_CONFIG_DIR`; Codex CLI uses `CODEX_HOME`. Create the target directory before launch.
- Commit locally after every task; do not push.

---

### Task 1: Agent Instance domain and catalog persistence

**Files:**
- Create: `crates/mf-agent/src/agent_instance.rs`
- Modify: `crates/mf-agent/src/catalog_store.rs`
- Modify: `crates/mf-agent/src/model.rs`
- Modify: `crates/mf-agent/src/lib.rs`
- Test: `crates/mf-agent/tests/agent_instances.rs`

**Interfaces:**
- Produces: `AgentInstance`, `AgentInstanceDraft`, `AgentInstanceVersion`, `InstanceScope`, `RunMode`
- Produces catalog CRUD and project-overlay resolution

- [ ] **Step 1: Write failing CRUD and snapshot tests**

```rust
#[test]
fn editing_instance_creates_version_without_mutating_snapshot() {
    let store = CatalogStore::memory().unwrap();
    let first = store.create_agent_instance(draft("review")).unwrap();
    let snapshot = store.snapshot_agent_instance(&first.id, None).unwrap();
    store.update_agent_instance(&first.id, draft("implementation")).unwrap();
    assert_eq!(snapshot.name, "review");
}
```

- [ ] **Step 2: Run and verify failure**

Run: `cargo test -p mf-agent --test agent_instances -- --nocapture`

Expected: compilation fails because instance types and CRUD methods do not exist.

- [ ] **Step 3: Implement normalized rows and immutable versions**

```rust
pub struct AgentInstance {
    pub id: String,
    pub name: String,
    pub agent_type: String,
    pub scope: InstanceScope,
    pub current_version: i64,
    pub enabled: bool,
}
```

Store executable, argv, non-secret env, config JSON, execution contract, and sealed-secret IDs in version rows. Project overrides merge only declared override keys and produce a resolved immutable snapshot.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-agent --test agent_instances -- --nocapture`

Expected: CRUD, overlay, and snapshot tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-agent/src/agent_instance.rs crates/mf-agent/src/catalog_store.rs crates/mf-agent/src/model.rs crates/mf-agent/src/lib.rs crates/mf-agent/tests/agent_instances.rs
git commit -m "feat(agent): persist versioned instances"
```

### Task 2: Encrypted Secret Store

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/mf-agent/Cargo.toml`
- Modify: `crates/mf-plugins/Cargo.toml`
- Create: `crates/mf-agent/src/secrets.rs`
- Create: `crates/mf-plugins/src/builtin_secret_store.rs`
- Test: `crates/mf-plugins/tests/secret_store.rs`

**Interfaces:**
- Produces: `SecretStore` trait with `seal`, `unseal_for_run`, `delete`, `describe`
- Produces: `SecretLease` whose buffer zeroizes on drop
- Produces: `Redacted<T>` debug/display behavior

- [ ] **Step 1: Write failing encryption and redaction tests**

```rust
#[test]
fn ciphertext_and_debug_never_contain_secret() {
    let store = InMemorySecretStore::new([7u8; 32]);
    let id = store.seal("api-key", b"secret-value").unwrap();
    assert!(!store.ciphertext(&id).contains("secret-value"));
    assert!(!format!("{:?}", store.describe(&id).unwrap()).contains("secret-value"));
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-plugins --test secret_store -- --nocapture`

Expected: compilation fails on Secret Store types.

- [ ] **Step 3: Implement AES-256-GCM and OS-wrapped master key**

Add workspace dependencies `aes-gcm = "0.10"`, `rand = "0.8"`, `zeroize = "1"`, and `keyring = "3"`. The `mf-agent` crate owns only the `SecretStore` interface and zeroizing lease type; `mf-plugins` owns AES-GCM, keyring access, and the built-in adapter. Use service `MonkeyFence` and account `agent-instance-master-key`. Unit tests inject a deterministic key and never access the OS keyring.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-plugins --test secret_store -- --nocapture`

Expected: encryption, wrong-key, deletion, and redaction tests pass.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/mf-agent/Cargo.toml crates/mf-plugins/Cargo.toml crates/mf-agent/src/secrets.rs crates/mf-plugins/src/builtin_secret_store.rs crates/mf-plugins/tests/secret_store.rs
git commit -m "feat(secrets): encrypt agent credentials"
```

### Task 3: LaunchPlan and Agent Adapter contract

**Files:**
- Modify: `crates/mf-agent/src/runtime.rs`
- Create: `crates/mf-agent/src/agent_adapter.rs`
- Create: `crates/mf-plugins/src/generic_command_adapter.rs`
- Modify: `crates/mf-plugins/src/builtin.rs`
- Test: `crates/mf-plugins/tests/generic_command_adapter.rs`

**Interfaces:**
- Replaces `AgentProfileSpec` launch ownership with `AgentTypeDescriptor` plus `AgentInstanceSnapshot`
- Produces: `LaunchPlan { executable, argv, env, cwd, temp_files, input, completion, redactions }`
- Produces: `AgentAdapter::validate`, `compile_launch`, `observe`, `extract_handoff`

- [ ] **Step 1: Write failing no-shell tests**

```rust
#[test]
fn generic_adapter_preserves_argument_boundaries() {
    let plan = adapter().compile_launch(snapshot_with_args(["--prompt", "a; rm -rf x"]), ctx()).unwrap();
    assert_eq!(plan.executable, PathBuf::from("agent.exe"));
    assert_eq!(plan.argv[1], "a; rm -rf x");
    assert!(!plan.uses_shell);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-plugins --test generic_command_adapter -- --nocapture`

Expected: compilation fails on adapter and LaunchPlan interfaces.

- [ ] **Step 3: Implement contract and Generic Command adapter**

Completion detectors are `ProcessExit`, `StdoutMarker(String)`, `ResultFile(PathBuf)`, and `Manual`. Input injectors are `Argv`, `Stdin`, and `PromptFile`. Shell execution is a separate boolean that validates the plugin has `capabilities.shell`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-plugins --test generic_command_adapter -- --nocapture`

Expected: argument, env, input, completion, and Shell-permission tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-agent/src/runtime.rs crates/mf-agent/src/agent_adapter.rs crates/mf-plugins/src/generic_command_adapter.rs crates/mf-plugins/src/builtin.rs crates/mf-plugins/tests/generic_command_adapter.rs
git commit -m "feat(runtime): add agent adapter contract"
```

### Task 4: Claude Code and Codex isolated adapters

**Files:**
- Create: `crates/mf-plugins/src/claude_adapter.rs`
- Create: `crates/mf-plugins/src/codex_adapter.rs`
- Modify: `crates/mf-plugins/src/builtin.rs`
- Test: `crates/mf-plugins/tests/isolated_cli_adapters.rs`

**Interfaces:**
- Consumes: LaunchPlan from Task 3
- Claude isolation: `CLAUDE_CONFIG_DIR=<run-temp>/claude`
- Codex isolation: `CODEX_HOME=<run-temp>/codex`

- [ ] **Step 1: Write failing isolation tests**

```rust
#[test]
fn adapters_never_target_real_homes() {
    let root = tempdir().unwrap();
    let claude = claude_adapter().compile_launch(instance(), ctx_at(root.path())).unwrap();
    let codex = codex_adapter().compile_launch(instance(), ctx_at(root.path())).unwrap();
    assert_eq!(claude.env["CLAUDE_CONFIG_DIR"], root.path().join("claude"));
    assert_eq!(codex.env["CODEX_HOME"], root.path().join("codex"));
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-plugins --test isolated_cli_adapters -- --nocapture`

Expected: compilation fails because adapters are absent.

- [ ] **Step 3: Implement adapters and config materialization**

Create temp directories before process launch. Materialize only the instance snapshot into those directories. Do not copy the user's existing CLI home. Add source comments linking Claude's official `CLAUDE_CONFIG_DIR` reference and OpenAI Codex's `CODEX_HOME` configuration loader.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-plugins --test isolated_cli_adapters -- --nocapture`

Expected: path, config-file, secret-redaction, and unsupported-isolation tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-plugins/src/claude_adapter.rs crates/mf-plugins/src/codex_adapter.rs crates/mf-plugins/src/builtin.rs crates/mf-plugins/tests/isolated_cli_adapters.rs
git commit -m "feat(adapters): isolate Claude and Codex homes"
```

### Task 5: Runtime Host and ad-hoc CLI sessions

**Files:**
- Modify: `crates/mf-agent/src/model.rs`
- Modify: `crates/mf-agent/src/store.rs`
- Modify: `crates/mf-agent/src/runtime.rs`
- Modify: `crates/mf/src/runtime_host.rs`
- Modify: `crates/mf/src/app_ctx.rs`
- Test: `crates/mf-agent/tests/ad_hoc_sessions.rs`

**Interfaces:**
- Produces: `AdHocSessionView`
- Produces: `Orchestrator::create_ad_hoc_session(task_id, instance_snapshot, launch_mode)`
- Ad-hoc sessions have no `step_id` and never mutate Task status

- [ ] **Step 1: Write failing task-independence test**

```rust
#[test]
fn ad_hoc_session_does_not_change_task_status() {
    let fixture = fixture();
    let task = fixture.store.create_task("t", "goal").unwrap();
    fixture.create_ad_hoc(task.id).unwrap();
    assert_eq!(fixture.store.task_view(task.id).unwrap().unwrap().status, TaskStatus::Draft);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-agent --test ad_hoc_sessions -- --nocapture`

Expected: compilation fails on ad-hoc interfaces.

- [ ] **Step 3: Implement persistence and Runtime Host launch**

Add `ad_hoc_sessions` rows with task, instance snapshot, session status, launch timestamps, and optional submitted Handoff. Add Runtime Host routing by `(project, session_id)` without inventing a Step or Agent Run.

- [ ] **Step 4: Run runtime verification**

Run: `cargo test -p mf-agent --test ad_hoc_sessions -- --nocapture`

Expected: task independence, submit-handoff, and restart-read tests pass.

Run: `cargo test -p mf-plugins`

Expected: all adapter tests pass.

Run: `cargo check --workspace`

Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-agent/src/model.rs crates/mf-agent/src/store.rs crates/mf-agent/src/runtime.rs crates/mf/src/runtime_host.rs crates/mf/src/app_ctx.rs crates/mf-agent/tests/ad_hoc_sessions.rs
git commit -m "feat(runtime): add task ad-hoc sessions"
```
