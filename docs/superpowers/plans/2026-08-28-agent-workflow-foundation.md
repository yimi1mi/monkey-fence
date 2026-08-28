# Agent Workflow Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Establish a clean v1 storage namespace and versioned plugin contracts without preserving legacy MonkeyFence data.

**Architecture:** Keep domain state in `mf-agent`, plugin discovery and worker ownership in `mf-plugins`, and process/UI wiring in `mf`. New database filenames ignore old stores. Plugin contributions are declarative and versioned; active runs pin content-addressed packages.

**Tech Stack:** Rust 2021, rusqlite, serde, TOML, SHA-256, NDJSON, cargo test.

**Spec:** `docs/superpowers/specs/2026-08-28-agent-workflow-plugin-design.md`

## Global Constraints

- Do not read, migrate, or mutate legacy MonkeyFence data.
- Do not touch project source, `.git`, P4, real CLI config directories, or CC Switch data during storage reset work.
- Third-party plugins never inject GPUI or dynamic libraries.
- Existing `.zcode/` and `.superpowers/` content is user/local state and must not be staged.
- Commit locally after every task; do not push or create a PR.

---

### Task 1: Clean storage namespace and schema ownership

**Files:**
- Create: `crates/mf-agent/src/schema.rs`
- Create: `crates/mf-agent/src/catalog_store.rs`
- Modify: `crates/mf-agent/src/lib.rs`
- Modify: `crates/mf-agent/src/store.rs`
- Modify: `crates/mf/src/app_ctx.rs`
- Modify: `crates/mf/src/main.rs`
- Test: `crates/mf-agent/tests/fresh_schema.rs`

**Interfaces:**
- Produces: `CatalogStore::open(path: &Path) -> Result<Arc<CatalogStore>>`
- Produces: `CatalogStore::memory() -> Result<Arc<CatalogStore>>`
- Produces: `PROJECT_SCHEMA_VERSION: i64 = 1`
- Produces: `CATALOG_SCHEMA_VERSION: i64 = 1`
- Project DB path: `<project>/.mf-agent/workflow-v1.db`
- Catalog DB path: `~/.monkeyfence/catalog-v1.db`

- [ ] **Step 1: Write failing clean-schema tests**

```rust
#[test]
fn project_schema_starts_at_v1_without_legacy_tables() {
    let store = Store::memory().unwrap();
    let tables = store.table_names().unwrap();
    assert!(tables.contains(&"agent_tasks".to_string()));
    assert!(!tables.contains(&"runs".to_string()));
    assert_eq!(store.schema_version().unwrap(), 1);
}

#[test]
fn catalog_schema_is_independent() {
    let catalog = CatalogStore::memory().unwrap();
    assert_eq!(catalog.schema_version().unwrap(), 1);
}
```

- [ ] **Step 2: Run tests and verify the new interfaces are absent**

Run: `cargo test -p mf-agent --test fresh_schema -- --nocapture`

Expected: compilation fails because `schema`, `CatalogStore`, `table_names`, and `schema_version` do not exist.

- [ ] **Step 3: Implement the two schemas and new filenames**

```rust
pub const PROJECT_SCHEMA_VERSION: i64 = 1;
pub const CATALOG_SCHEMA_VERSION: i64 = 1;

pub fn initialize_schema(conn: &Connection, ddl: &str, version: i64) -> Result<()> {
    conn.execute_batch(ddl)?;
    conn.pragma_update(None, "user_version", version)?;
    Ok(())
}
```

Delete `up_v1`, `up_v2`, `migrations`, `schema_migrations`, and import-marker behavior from the active `Store` path. Retain current Task/Revision/Step/Session/Run tables in `PROJECT_SCHEMA_V1`. Add catalog tables only as empty foundations: `agent_instances`, `agent_instance_versions`, `workflow_templates`, `workflow_template_versions`, `sealed_secrets`, and `plugin_packages`.

Change every production `Store::open` call to `workflow-v1.db`. Open `CatalogStore` once in `AppCtx::new` at `catalog-v1.db`. Do not delete the old filenames.

- [ ] **Step 4: Run focused and package tests**

Run: `cargo test -p mf-agent --test fresh_schema -- --nocapture`

Expected: 2 tests pass.

Run: `cargo test -p mf-agent`

Expected: all `mf-agent` tests pass after old migration-only tests are removed or rewritten against v1.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-agent/src/schema.rs crates/mf-agent/src/catalog_store.rs crates/mf-agent/src/lib.rs crates/mf-agent/src/store.rs crates/mf-agent/tests/fresh_schema.rs crates/mf/src/app_ctx.rs crates/mf/src/main.rs
git commit -m "refactor(store): start clean workflow schema"
```

### Task 2: Manifest v2 contribution vocabulary

**Files:**
- Modify: `crates/mf-plugins/src/manifest.rs`
- Modify: `crates/mf-plugins/src/builtin.rs`
- Modify: `crates/mf-plugins/src/lib.rs`
- Test: `crates/mf-plugins/tests/manifest_v2.rs`

**Interfaces:**
- Consumes: clean catalog namespace from Task 1
- Produces: `MANIFEST_VERSION: i64 = 2`
- Produces: `AgentTypeContribution`, `NodeTypeContribution`, `ExecutionDirectoryContribution`, `SecretStoreContribution`, `UiSchemaContribution`
- Produces capabilities: `shell`, `secrets`, `vcs`, `background_worker`, while preserving `fs_read`, `fs_write`, `net`, and `spawn`

- [ ] **Step 1: Write failing manifest parse and fingerprint tests**

```rust
#[test]
fn parses_every_v2_contribution() {
    let manifest = PluginManifest::parse(include_str!("fixtures/manifest-v2.toml")).unwrap();
    assert_eq!(manifest.agent_types.len(), 1);
    assert_eq!(manifest.node_types.len(), 1);
    assert_eq!(manifest.execution_directory_providers.len(), 1);
    assert_eq!(manifest.secret_stores.len(), 1);
    assert_eq!(manifest.ui_schemas.len(), 1);
}

#[test]
fn shell_and_secret_permissions_change_fingerprint() {
    let mut caps = Capabilities::default();
    let before = caps.fingerprint_part();
    caps.shell = true;
    caps.secrets = true;
    assert_ne!(before, caps.fingerprint_part());
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-plugins --test manifest_v2 -- --nocapture`

Expected: compilation fails on the new contribution fields and capabilities.

- [ ] **Step 3: Implement exact v2 structures and validation**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTypeContribution {
    pub id: String,
    pub name: String,
    pub adapter: String,
    pub config_schema: String,
    #[serde(default)] pub command: String,
    #[serde(default)] pub detect_commands: Vec<String>,
    #[serde(default)] pub modes: Vec<String>,
    #[serde(default)] pub supports_isolated_config: bool,
}
```

Give every contribution class independent duplicate-ID validation and safe relative-path validation for referenced Schema files. Update all synthetic built-ins to emit v2 manifests; remove the v1 `agents` field rather than maintaining a compatibility alias.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-plugins --test manifest_v2 -- --nocapture`

Expected: all manifest v2 tests pass.

Run: `cargo test -p mf-plugins manifest`

Expected: existing manifest tests pass after fixtures are converted to v2.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-plugins/src/manifest.rs crates/mf-plugins/src/builtin.rs crates/mf-plugins/src/lib.rs crates/mf-plugins/tests/manifest_v2.rs crates/mf-plugins/tests/fixtures/manifest-v2.toml
git commit -m "feat(plugins): add manifest v2 contributions"
```

### Task 3: Content-addressed Plugin Host and active pins

**Files:**
- Create: `crates/mf-plugins/src/host.rs`
- Create: `crates/mf-plugins/src/contribution_registry.rs`
- Modify: `crates/mf-plugins/src/install.rs`
- Modify: `crates/mf-plugins/src/lib.rs`
- Test: `crates/mf-plugins/tests/plugin_host.rs`

**Interfaces:**
- Consumes: Manifest v2 from Task 2
- Produces: `PluginHost::resolve(full_id: &str, version: &str, hash: &str) -> Result<ResolvedPlugin>`
- Produces: `PluginHost::pin_for_run(run_key: &str, plugin: &ResolvedPlugin) -> Result<PluginPin>`
- Produces: `PluginHost::release_run_pins(run_key: &str) -> Result<()>`
- Produces: typed contribution lookup by full contribution ID

- [ ] **Step 1: Write failing pinning tests**

```rust
#[test]
fn update_does_not_replace_active_pin() {
    let host = fixture_host();
    let v1 = host.install_fixture("demo-v1").unwrap();
    let pin = host.pin_for_run("run-1", &v1).unwrap();
    host.install_fixture("demo-v2").unwrap();
    assert_eq!(host.resolve_pin(&pin).unwrap().content_hash, v1.content_hash);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-plugins --test plugin_host -- --nocapture`

Expected: compilation fails because `PluginHost` and pin interfaces do not exist.

- [ ] **Step 3: Implement content-addressed install and registry**

Store packages at `~/.monkeyfence/plugins/packages/<sha256>/`. Store enabled selections separately from package content. Never mutate a package directory after hash verification. Keep active pin reference counts in memory and catalog records; cleanup can remove only unreferenced hashes.

Replace `PluginRegistry` ownership with `PluginHost`; keep `pub type PluginRegistry = PluginHost` only if required to limit call-site churn within this task, and remove the alias by the end of Plan 2.

- [ ] **Step 4: Run tests**

Run: `cargo test -p mf-plugins --test plugin_host -- --nocapture`

Expected: pinning, resolution, disabled-plugin, and permission-fingerprint tests pass.

Run: `cargo test -p mf-plugins`

Expected: package test suite passes.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-plugins/src/host.rs crates/mf-plugins/src/contribution_registry.rs crates/mf-plugins/src/install.rs crates/mf-plugins/src/lib.rs crates/mf-plugins/tests/plugin_host.rs
git commit -m "feat(plugins): pin content-addressed packages"
```

### Task 4: Versioned NDJSON worker protocol

**Files:**
- Create: `crates/mf-plugins/src/worker_protocol.rs`
- Modify: `crates/mf-plugins/src/worker.rs`
- Test: `crates/mf-plugins/tests/worker_protocol.rs`

**Interfaces:**
- Produces: protocol version `1`
- Produces: `WorkerRequest { protocol, id, method, capability_token, params }`
- Produces: `WorkerResponse { protocol, id, result, error }`
- Produces: `WorkerClient::heartbeat() -> Result<WorkerHealth>`

- [ ] **Step 1: Write failing serialization and mismatch tests**

```rust
#[test]
fn rejects_response_from_another_protocol() {
    let response = r#"{"protocol":2,"id":1,"result":{}}"#;
    assert!(WorkerResponse::parse_for(1, response).is_err());
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p mf-plugins --test worker_protocol -- --nocapture`

Expected: compilation fails on `WorkerResponse`.

- [ ] **Step 3: Implement envelopes, deadline, heartbeat, and redaction**

Keep stderr bounded at 500 lines. Reject response IDs and protocol versions that do not match the request. Redact values whose keys contain `token`, `secret`, `password`, or `api_key` before storing worker diagnostic text.

- [ ] **Step 4: Run foundation verification**

Run: `cargo test -p mf-plugins`

Expected: all plugin tests pass.

Run: `cargo test -p mf-agent`

Expected: all agent-domain tests pass.

Run: `cargo check --workspace`

Expected: workspace check exits 0.

- [ ] **Step 5: Commit**

```bash
git add crates/mf-plugins/src/worker_protocol.rs crates/mf-plugins/src/worker.rs crates/mf-plugins/tests/worker_protocol.rs
git commit -m "feat(plugins): version worker protocol"
```
