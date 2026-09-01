//! T0a 基线 fixture 生成/校验 harness(Issue #12)。
//!
//! 生成器用冻结的 v6/v1 DDL 和确定性行构造迁移起点,
//! 因此生产 Store 升级后仍不会冒充新版本。规范化 dump
//! 只经公共读取 API 产出,键由 serde_json 排序
//!   (BTreeMap),行序由 SQL `ORDER BY` 决定,不受插入顺序漂移影响。
//!
//! fixture 不含任何真实用户数据、Secret 引用或运行令牌。

use anyhow::{Context as _, Result};
use mf_agent::agent_instance::AgentInstanceSnapshot;
use mf_agent::catalog_store::CatalogStore;
use mf_agent::model::RunMode;
use mf_agent::orchestrator::workflow_pin_key;
use mf_agent::schema::{initialize_schema, upgrade_project, CATALOG_SCHEMA_V1};
use mf_agent::store::Store;
use mf_agent::workflow::{
    workflow_content_digest, PluginSourcePin, WorkflowNodeDraft, WorkflowNodeSnapshot,
    WorkflowSnapshot,
};
use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// harness 版本:生成语义变化(加表、改规范化规则)时必须递增。
pub const GENERATOR_VERSION: &str = "2";
pub const FROZEN_PROJECT_SCHEMA_VERSION: i64 = 6;
pub const FROZEN_CATALOG_SCHEMA_VERSION: i64 = 1;

pub const PROJECT_FIXTURE: &str = "project-v6.db";
pub const CATALOG_FIXTURE: &str = "catalog-v1.db";
pub const SESSION_FIXTURE: &str = "session.json";

/// 确定性时间戳常量(ISO-8601,带 +00:00 偏移)。
const T_CREATED: &str = "2026-01-01T00:00:00+00:00";
const T_UPDATED: &str = "2026-01-02T00:00:00+00:00";
const T_STARTED: &str = "2026-01-01T01:00:00+00:00";
const T_ENDED: &str = "2026-01-01T02:00:00+00:00";

pub const SESSION_PROJECT_ALPHA: &str = "/mf-fixture/baseline-alpha";
pub const SESSION_MISSING_PROJECT: &str = "/mf-fixture/__missing_project__";

const AGENT_PLUGIN_ID: &str = "monkeyfence.builtin/generic-command";
const DIRECTORY_PLUGIN_ID: &str = "monkeyfence.builtin/project-dir";

fn plugin_pin(full_id: &str, contribution_id: &str) -> PluginSourcePin {
    PluginSourcePin {
        full_id: full_id.into(),
        version: "1.0.0".into(),
        content_hash: sha256_hex(full_id.as_bytes()),
        contribution_id: contribution_id.into(),
    }
}

fn agent_plugin_pin() -> PluginSourcePin {
    plugin_pin(AGENT_PLUGIN_ID, "")
}

fn directory_plugin_pin() -> PluginSourcePin {
    plugin_pin(DIRECTORY_PLUGIN_ID, "monkeyfence.builtin/project-dir")
}

pub fn fixture_pin_run_key(task_id: i64, revision_id: i64) -> String {
    workflow_pin_key(Path::new(SESSION_PROJECT_ALPHA), task_id, revision_id)
}

pub fn raw_schema_version(path: &Path) -> Result<i64> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("baseline")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// 生成:项目库 v6
// ---------------------------------------------------------------------------

pub fn generate_project_db(path: &Path) -> Result<()> {
    // 生成到同目录临时名,成功后再原子改名,避免半成品 fixture。
    let staging = path.with_extension("db.gen");
    let _ = fs::remove_file(&staging);
    let mut conn = Connection::open(&staging)?;
    upgrade_project(&mut conn, FROZEN_PROJECT_SCHEMA_VERSION)?;
    build_project_data(&mut conn)?;
    conn.execute("VACUUM", [])?;
    drop(conn);
    fs::rename(&staging, path)?;
    Ok(())
}

fn build_project_data(conn: &mut Connection) -> Result<()> {
    let workflow_nodes = vec![
        WorkflowNodeDraft {
            key: "fetch".into(),
            title: "拉取依赖".into(),
            instructions: "固定指令:拉取依赖清单".into(),
            agent_instance_id: "fixture-inst-user".into(),
            deps: vec![],
        },
        WorkflowNodeDraft {
            key: "build".into(),
            title: "构建".into(),
            instructions: "固定指令:构建产物".into(),
            agent_instance_id: "fixture-inst-user".into(),
            deps: vec!["fetch".into()],
        },
    ];
    let instance = AgentInstanceSnapshot {
        id: "fixture-inst-user".into(),
        name: "基线用户实例".into(),
        agent_type: "generic-command".into(),
        version: 2,
        enabled: true,
        run_mode: RunMode::OneShot,
        executable: "agent.exe".into(),
        argv: vec!["--baseline-v2".into()],
        env: vec![("MF_FIXTURE_ENV".into(), "1".into())],
        config: serde_json::json!({ "fixture": true }),
        execution_contract: serde_json::json!({ "completion": "process-exit" }),
        sealed_secret_ids: vec![],
        external_config: false,
    };
    let snapshot = WorkflowSnapshot {
        template_key: "tpl-baseline".into(),
        template_version: 1,
        nodes: workflow_nodes
            .iter()
            .map(|node| WorkflowNodeSnapshot {
                key: node.key.clone(),
                title: node.title.clone(),
                instructions: node.instructions.clone(),
                instance: instance.clone(),
                deps: node.deps.clone(),
                plugin: Some(agent_plugin_pin()),
            })
            .collect(),
        directory_provider: Some(directory_plugin_pin()),
    };
    let digest = workflow_content_digest(&workflow_nodes, false);
    let project_workflow_nodes = vec![
        WorkflowNodeDraft {
            key: "n1".into(),
            title: "节点一".into(),
            instructions: "固定指令".into(),
            agent_instance_id: "fixture-inst-user".into(),
            deps: vec![],
        },
        WorkflowNodeDraft {
            key: "n2".into(),
            title: "节点二".into(),
            instructions: "固定指令".into(),
            agent_instance_id: "fixture-inst-user".into(),
            deps: vec!["n1".into()],
        },
    ];
    let project_workflow_digest = workflow_content_digest(&project_workflow_nodes, false);
    let handoff = serde_json::json!({
        "status": "complete",
        "summary": "依赖清单已生成",
        "changed_files": [],
        "artifacts": [],
        "verification": null,
        "blockers": [],
        "recommendations": [],
        "output": { "report_path": "artifacts/deps.json", "count": 3 },
        "raw_log_ref": "agent-run:1",
    });

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO agent_tasks
         (id, title, goal, status, active_revision, paused, unread, created_at, updated_at)
         VALUES (1, ?1, ?2, 'ready', 1, 0, 0, ?3, ?4),
                (2, ?5, ?6, 'draft', NULL, 0, 0, ?3, ?4)",
        params![
            "基线任务A",
            "冻结 v6 基线:结算与交接终态",
            T_CREATED,
            T_UPDATED,
            "基线任务B",
            "草稿对照"
        ],
    )?;
    tx.execute(
        "INSERT INTO pipeline_revisions
         (id, task_id, revision, status, snapshot_json, created_at, content_digest)
         VALUES (1, 1, 1, 'active', ?1, ?2, ?3),
                (2, 2, 1, 'draft', NULL, ?2, NULL)",
        params![serde_json::to_string(&snapshot)?, T_CREATED, digest],
    )?;
    tx.execute(
        "INSERT INTO steps
         (id, revision_id, task_id, step_key, title, instructions, agent_profile,
          session_policy, status, attempts, auto_retry, result, started_at, ended_at,
          created_at, updated_at)
         VALUES
         (1, 1, 1, 'fetch', ?1, ?2, 'generic-command', 'fresh', 'succeeded',
          0, 0, ?3, ?4, ?5, ?6, ?7),
         (2, 1, 1, 'build', ?8, ?9, 'generic-command', 'fresh', 'pending',
          0, 0, NULL, NULL, NULL, ?6, ?7),
         (3, 2, 2, 'only', ?10, '', 'generic-command', 'fresh', 'pending',
          0, 0, NULL, NULL, NULL, ?6, ?7)",
        params![
            "拉取依赖",
            "固定指令:拉取依赖清单",
            "依赖清单已生成",
            T_STARTED,
            T_ENDED,
            T_CREATED,
            T_UPDATED,
            "构建",
            "固定指令:构建产物",
            "唯一节点"
        ],
    )?;
    tx.execute(
        "INSERT INTO step_deps (step_id, dep_step_id) VALUES (2, 1)",
        [],
    )?;
    tx.execute(
        "INSERT INTO agent_sessions
         (id, session_key, runtime, agent_profile, title, status, unread, created_at, updated_at)
         VALUES (1, 'shared', 'generic-command', 'codex', ?1, 'starting', 0, ?2, ?3)",
        params!["基线会话", T_CREATED, T_UPDATED],
    )?;
    tx.execute(
        "INSERT INTO agent_runs
         (id, task_id, step_id, revision_id, session_id, status, capability_token,
          agent_state, outcome, outcome_payload, started_at, ended_at)
         VALUES (1, 1, 1, 1, 1, 'succeeded', 'mft_fixture_0001', 'working',
                 'complete', ?1, ?2, ?3)",
        params!["依赖清单已生成", T_STARTED, T_ENDED],
    )?;
    tx.execute(
        "INSERT INTO handoffs (id, task_id, step_id, run_id, handoff_json, created_at)
         VALUES (1, 1, 1, 1, ?1, ?2)",
        params![serde_json::to_string(&handoff)?, T_CREATED],
    )?;
    tx.execute(
        "INSERT INTO events (id, kind, payload, created_at)
         VALUES (1, 'baseline', '{\"note\":\"fixture\"}', ?1)",
        [T_CREATED],
    )?;
    tx.execute(
        "INSERT INTO project_workflows
         (workflow_key, name, graph_json, allow_unsafe_parallel, content_digest,
          created_at, updated_at)
         VALUES ('baseline-flow', ?1, ?2, 0, ?3, ?4, ?5)",
        params![
            "基线工作流",
            serde_json::to_string(&project_workflow_nodes)?,
            project_workflow_digest,
            T_CREATED,
            T_UPDATED
        ],
    )?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 生成:目录库 v1
// ---------------------------------------------------------------------------

fn fixture_template_nodes() -> Vec<WorkflowNodeDraft> {
    vec![WorkflowNodeDraft {
        key: "t1".into(),
        title: "模板节点".into(),
        instructions: "固定指令".into(),
        agent_instance_id: "fixture-inst-user".into(),
        deps: vec![],
    }]
}

fn version_payload(name: &str, argv: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "agent_type": "generic-command",
        "run_mode": RunMode::OneShot,
        "executable": "agent.exe",
        "argv": [argv],
        "env": [["MF_FIXTURE_ENV", "1"]],
        "config": { "fixture": true },
        "execution_contract": { "completion": "process-exit" },
        "sealed_secret_ids": [],
    })
}

pub fn generate_catalog_db(path: &Path) -> Result<()> {
    let staging = path.with_extension("db.gen");
    let _ = fs::remove_file(&staging);
    let mut conn = Connection::open(&staging)?;
    initialize_schema(&conn, CATALOG_SCHEMA_V1, FROZEN_CATALOG_SCHEMA_VERSION)?;
    build_catalog_data(&mut conn)?;
    conn.execute("VACUUM", [])?;
    drop(conn);
    fs::rename(&staging, path)?;
    Ok(())
}

fn build_catalog_data(conn: &mut Connection) -> Result<()> {
    let run_key = fixture_pin_run_key(1, 1);
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO agent_instances
         (id, instance_key, name, agent_type, scope, project_key, current_version,
          enabled, created_at, updated_at)
         VALUES
         (1, 'fixture-inst-user', ?1, 'generic-command', 'user', NULL, 2, 1, ?2, ?3),
         (2, 'fixture-inst-project', ?4, 'generic-command', 'project',
          'fixture-project', 1, 1, ?2, ?3)",
        params!["基线用户实例", T_CREATED, T_UPDATED, "基线项目实例"],
    )?;
    for (id, instance_id, version, name, argv) in [
        (1, 1, 1, "基线用户实例", "--baseline"),
        (2, 2, 1, "基线项目实例", "--baseline"),
        (3, 1, 2, "基线用户实例", "--baseline-v2"),
    ] {
        tx.execute(
            "INSERT INTO agent_instance_versions
             (id, instance_id, version, config_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                instance_id,
                version,
                serde_json::to_string(&version_payload(name, argv))?,
                T_CREATED
            ],
        )?;
    }
    tx.execute(
        "INSERT INTO workflow_templates
         (id, template_key, name, current_version, task_local, created_at, updated_at)
         VALUES (1, 'tpl-baseline', ?1, 1, 0, ?2, ?3)",
        params!["基线模板", T_CREATED, T_UPDATED],
    )?;
    tx.execute(
        "INSERT INTO workflow_template_versions
         (id, template_id, version, graph_json, created_at)
         VALUES (1, 1, 1, ?1, ?2)",
        params![serde_json::to_string(&fixture_template_nodes())?, T_CREATED],
    )?;
    for pin in [agent_plugin_pin(), directory_plugin_pin()] {
        tx.execute(
            "INSERT INTO plugin_pins
             (run_key, full_id, version, content_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_key,
                pin.full_id,
                pin.version,
                pin.content_hash,
                T_CREATED
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

// 故意不用生产 Store 写 API 生成:它会在 Store 升级后把新 schema
// 冒充为 v6/v1。上面的冻结 DDL 生成器才是稳定的迁移起点。

// ---------------------------------------------------------------------------
// 生成:session.json
// ---------------------------------------------------------------------------

/// 确定性 session.json:固定假路径(不以真实用户目录为前缀),
/// 含一个不存在路径(迁移后保留为 missing 状态)。
pub fn generate_session_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "projects": [
            SESSION_PROJECT_ALPHA,
            "/mf-fixture/baseline-beta",
            SESSION_MISSING_PROJECT,
        ],
        "foreground": SESSION_PROJECT_ALPHA,
        "project_states": [
            {
                "root": SESSION_PROJECT_ALPHA,
                "selected_task_id": 1,
                "open_files": ["/mf-fixture/baseline-alpha/src/main.rs"],
                "active_file": "/mf-fixture/baseline-alpha/src/main.rs",
            },
            {
                "root": "/mf-fixture/baseline-beta",
                "selected_task_id": null,
                "open_files": [],
                "active_file": null,
            },
        ],
    }))
    .unwrap()
}

// ---------------------------------------------------------------------------
// 规范化 dump(只经公共读取 API)
// ---------------------------------------------------------------------------

fn snapshot_projection(snapshot: &WorkflowSnapshot) -> serde_json::Value {
    let pin_projection = |pin: &PluginSourcePin| {
        serde_json::json!({
            "full_id": pin.full_id,
            "version": pin.version,
            "content_hash": pin.content_hash,
            "contribution_id": pin.contribution_id,
        })
    };
    let nodes = snapshot
        .nodes
        .iter()
        .map(|node| {
            let instance = &node.instance;
            serde_json::json!({
                "key": node.key,
                "title": node.title,
                "instructions": node.instructions,
                "deps": node.deps,
                "instance": {
                    "id": instance.id,
                    "name": instance.name,
                    "agent_type": instance.agent_type,
                    "version": instance.version,
                    "enabled": instance.enabled,
                    "run_mode": instance.run_mode,
                    "executable": instance.executable,
                    "argv": instance.argv,
                    "env": instance.env,
                    "config": instance.config,
                    "execution_contract": instance.execution_contract,
                    "sealed_secret_ids": instance.sealed_secret_ids,
                    "external_config": instance.external_config,
                },
                "plugin": node.plugin.as_ref().map(&pin_projection),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "template_key": snapshot.template_key,
        "template_version": snapshot.template_version,
        "nodes": nodes,
        "directory_provider": snapshot.directory_provider.as_ref().map(pin_projection),
    })
}

pub fn dump_project(store: &Store) -> Result<serde_json::Value> {
    let mut tasks = Vec::new();
    for task in store.list_tasks(true)? {
        let revision_rows: Vec<(i64, i64, String, Option<String>)> = store.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, revision, status, content_digest FROM pipeline_revisions
                 WHERE task_id = ?1 ORDER BY revision",
            )?;
            let rows = stmt
                .query_map([task.id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let revisions: Vec<serde_json::Value> = revision_rows
            .into_iter()
            .map(|(id, revision, status, content_digest)| {
                let snapshot = store.revision_snapshot(id)?;
                Ok(serde_json::json!({
                    "id": id,
                    "revision": revision,
                    "status": status,
                    "content_digest": content_digest,
                    "snapshot": snapshot.as_ref().map(snapshot_projection),
                }))
            })
            .collect::<Result<_>>()?;
        let steps: Vec<_> = store
            .task_steps(task.id)?
            .iter()
            .map(|step| {
                serde_json::json!({
                    "id": step.id,
                    "revision_id": step.revision_id,
                    "task_id": step.task_id,
                    "step_key": step.step_key,
                    "title": step.title,
                    "instructions": step.instructions,
                    "agent_profile": step.agent_profile,
                    "session_policy": step.session_policy,
                    "status": step.status,
                    "attempts": step.attempts,
                    "auto_retry": step.auto_retry,
                    "result": step.result,
                    "started_at": step.started_at,
                    "ended_at": step.ended_at,
                    "deps": step.deps,
                })
            })
            .collect();
        let runs: Vec<_> = store
            .list_runs_of_task(task.id)?
            .iter()
            .map(|run| {
                serde_json::json!({
                    "id": run.id,
                    "task_id": run.task_id,
                    "step_id": run.step_id,
                    "revision_id": run.revision_id,
                    "session_id": run.session_id,
                    "status": run.status,
                    "agent_state": run.agent_state,
                    "capability_token": run.capability_token,
                    "outcome": run.outcome,
                    "outcome_payload": run.outcome_payload,
                    "started_at": run.started_at,
                    "ended_at": run.ended_at,
                })
            })
            .collect();
        let handoffs: Vec<_> = store
            .list_handoff_rows(task.id)?
            .into_iter()
            .map(|row| {
                serde_json::json!({
                    "id": row.id,
                    "step_id": row.step_id,
                    "run_id": row.run_id,
                    "handoff": row.handoff,
                })
            })
            .collect();
        let mut v = serde_json::json!({
            "id": task.id,
            "title": task.title,
            "goal": task.goal,
            "status": task.status,
            "paused": task.paused,
            "unread": task.unread,
            "active_revision": task.active_revision,
            "revision_count": task.revision_count,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
        });
        v["revisions"] = revisions.into();
        v["steps"] = steps.into();
        v["runs"] = runs.into();
        v["handoffs"] = handoffs.into();
        tasks.push(v);
    }
    let sessions: Vec<_> = store
        .list_sessions()?
        .iter()
        .map(|session| {
            serde_json::json!({
                "id": session.id,
                "session_key": session.session_key,
                "runtime": session.runtime,
                "agent_profile": session.agent_profile,
                "title": session.title,
                "status": session.status,
                "last_instruction": session.last_instruction,
                "last_reply": session.last_reply,
                "unread": session.unread,
                "created_at": session.created_at,
                "updated_at": session.updated_at,
            })
        })
        .collect();
    let workflows: Vec<_> = store
        .list_project_workflows()?
        .iter()
        .map(|w| {
            serde_json::json!({
                "key": w.key,
                "name": w.name,
                "nodes": w.nodes.iter().map(|node| serde_json::json!({
                    "key": node.key,
                    "title": node.title,
                    "instructions": node.instructions,
                    "agent_instance_id": node.agent_instance_id,
                    "deps": node.deps,
                })).collect::<Vec<_>>(),
                "allow_unsafe_parallel": w.allow_unsafe_parallel,
                "content_digest": w.content_digest,
            })
        })
        .collect();
    let events: Vec<_> = store
        .list_events(1000)?
        .into_iter()
        .map(|(id, kind, payload, created_at)| {
            serde_json::json!({ "id": id, "kind": kind, "payload": payload, "created_at": created_at })
        })
        .collect();
    Ok(serde_json::json!({
        "tasks": tasks,
        "sessions": sessions,
        "project_workflows": workflows,
        "events": events,
    }))
}

pub fn dump_catalog(catalog: &CatalogStore) -> Result<serde_json::Value> {
    let instances: Vec<_> = catalog
        .list_agent_instances(Some("fixture-project"))?
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.id,
                "name": i.name,
                "agent_type": i.agent_type,
                "scope": i.scope.as_str(),
                "project_key": i.project_key,
                "current_version": i.current_version,
                "enabled": i.enabled,
            })
        })
        .collect();
    let mut versions = Vec::new();
    for inst in catalog.list_agent_instances(Some("fixture-project"))? {
        for v in catalog.agent_instance_versions(&inst.id)? {
            versions.push(serde_json::json!({
                "instance_id": v.instance_id,
                "version": v.version,
                "name": v.name,
                "agent_type": v.agent_type,
                "run_mode": v.run_mode,
                "executable": v.executable,
                "argv": v.argv,
                "env": v.env,
                "config": v.config,
                "execution_contract": v.execution_contract,
                "sealed_secret_ids": v.sealed_secret_ids,
                "created_at": v.created_at,
                "external_config": v.external_config,
            }));
        }
    }
    versions.sort_by(|a, b| {
        let key = |v: &serde_json::Value| {
            (
                v["instance_id"].as_str().unwrap_or("").to_string(),
                v["version"].as_i64().unwrap_or(0),
            )
        };
        key(a).cmp(&key(b))
    });
    let pins: Vec<_> = catalog
        .list_plugin_pins()?
        .iter()
        .map(|p| {
            serde_json::json!({
                "run_key": p.run_key,
                "full_id": p.full_id,
                "version": p.version,
                "content_hash": p.content_hash,
            })
        })
        .collect();
    let templates: Vec<_> = catalog
        .list_templates(true)?
        .iter()
        .map(|t| {
            serde_json::json!({
                "key": t.key,
                "name": t.name,
                "current_version": t.current_version,
                "task_local": t.task_local,
            })
        })
        .collect();
    let sealed_secret_count: i64 = catalog
        .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM sealed_secrets", [], |r| r.get(0))?))?;
    Ok(serde_json::json!({
        "agent_instances": instances,
        "agent_instance_versions": versions,
        "plugin_pins": pins,
        "templates": templates,
        "sealed_secret_count": sealed_secret_count,
    }))
}

/// session.json 的规范化投影:只保留迁移关心的字段(项目列表 +
/// 前台项目),路径分隔符统一为 `/`,键由 serde_json 排序。
pub fn dump_session(raw: &str) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(raw).context("session.json 解析失败")?;
    let normalize =
        |p: &serde_json::Value| -> String { p.as_str().unwrap_or_default().replace('\\', "/") };
    let projects: Vec<String> = v["projects"]
        .as_array()
        .map(|a| a.iter().map(normalize).collect())
        .unwrap_or_default();
    Ok(serde_json::json!({
        "projects": projects,
        "foreground": v["foreground"].as_str().map(|s| normalize(&serde_json::Value::String(s.into()))),
    }))
}

// ---------------------------------------------------------------------------
// 写出完整基线(db + session + expected dumps + manifest)
// ---------------------------------------------------------------------------

pub fn write_baseline(dir: &Path) -> Result<()> {
    let mut workflow_goldens = Vec::new();
    let expected = dir.join("expected");
    if expected.is_dir() {
        for entry in fs::read_dir(&expected)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("workflow-") && name.ends_with(".json") {
                workflow_goldens.push((name, fs::read(entry.path())?));
            }
        }
    }
    write_baseline_with_workflow_goldens(dir, &workflow_goldens)
}

/// 生成完整基线包并原子换入精确的生命周期 golden 集合。
/// 调用者必须先把所有场景渲染完;任一场景失败时不触碰已提交基线。
pub fn write_baseline_with_workflow_goldens(
    dir: &Path,
    workflow_goldens: &[(String, Vec<u8>)],
) -> Result<()> {
    let parent = dir.parent().context("基线目录缺少父目录")?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".mf-baseline-staging-")
        .tempdir_in(parent)?;
    write_baseline_contents(staging.path())?;
    for (name, bytes) in workflow_goldens {
        anyhow::ensure!(
            name.starts_with("workflow-")
                && name.ends_with(".json")
                && Path::new(name).file_name().and_then(|value| value.to_str()) == Some(name),
            "非法生命周期 golden 文件名: {name}"
        );
        fs::write(staging.path().join("expected").join(name), bytes)?;
    }
    write_manifest(staging.path())?;
    install_baseline_directory(staging, dir)
}

/// manifest 覆盖除自身外全部文件,按路径排序。
fn write_manifest(dir: &Path) -> Result<()> {
    let mut files = Vec::new();
    for rel in fixture_file_paths(dir)? {
        if rel.to_string_lossy().replace('\\', "/") == "manifest.json" {
            continue; // manifest 不哈希自身
        }
        let bytes = fs::read(dir.join(&rel))?;
        files.push(serde_json::json!({
            "path": rel.to_string_lossy().replace('\\', "/"),
            "bytes": bytes.len(),
            "sha256": sha256_hex(&bytes),
        }));
    }
    let manifest = serde_json::json!({
        "generator_version": GENERATOR_VERSION,
        "files": files,
    });
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    Ok(())
}

fn write_baseline_contents(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir.join("expected"))?;
    generate_project_db(&dir.join(PROJECT_FIXTURE))?;
    generate_catalog_db(&dir.join(CATALOG_FIXTURE))?;
    let session = generate_session_json();
    fs::write(dir.join(SESSION_FIXTURE), &session)?;

    // expected dump 从刚写出的 fixture 读取(而非生成过程内存态),
    // 保证「打开 fixture → dump」这一被测路径自身就是期望来源。
    let store = Store::open(&dir.join(PROJECT_FIXTURE))?;
    fs::write(
        dir.join("expected/project-v6.dump.json"),
        serde_json::to_string_pretty(&dump_project(&store)?)? + "\n",
    )?;
    drop(store);
    let catalog = CatalogStore::open(&dir.join(CATALOG_FIXTURE))?;
    fs::write(
        dir.join("expected/catalog-v1.dump.json"),
        serde_json::to_string_pretty(&dump_catalog(&catalog)?)? + "\n",
    )?;
    drop(catalog);
    fs::write(
        dir.join("expected/session.dump.json"),
        serde_json::to_string_pretty(&dump_session(&session)?)? + "\n",
    )?;

    Ok(())
}

/// 递归列出基线目录中的所有文件(相对路径、排序稳定)。
pub fn fixture_file_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    fn visit(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                visit(root, &path, out)?;
            } else {
                out.push(path.strip_prefix(root)?.to_path_buf());
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(dir, dir, &mut paths)?;
    paths.sort();
    Ok(paths)
}

/// 所有产物都在兄弟 staging 目录生成且验证后,再一次性换入。
/// Windows 不支持带目标的 rename,因此先保留整目录回滚副本。
fn install_baseline_directory(staging: tempfile::TempDir, target: &Path) -> Result<()> {
    let staged = staging.path().to_path_buf();
    if !target.exists() {
        fs::rename(&staged, target).context("安装新基线目录失败")?;
        return Ok(());
    }

    let parent = target.parent().context("基线目录缺少父目录")?;
    let backup_slot = tempfile::Builder::new()
        .prefix(".mf-baseline-previous-")
        .tempdir_in(parent)?;
    let backup = backup_slot.path().to_path_buf();
    backup_slot.close().context("准备基线回滚位置失败")?;
    fs::rename(target, &backup).context("保留旧基线目录失败")?;
    if let Err(error) = fs::rename(&staged, target) {
        let restore = fs::rename(&backup, target);
        return match restore {
            Ok(()) => Err(error).context("安装基线目录失败,已恢复原目录"),
            Err(restore_error) => {
                let staged = staging.keep();
                Err(anyhow::anyhow!(
                    "安装基线目录失败: {error}; 恢复原目录也失败: \
                     {restore_error}; 新生成目录位于 {}, 回滚副本位于 {}",
                    staged.display(),
                    backup.display()
                ))
            }
        };
    }
    fs::remove_dir_all(&backup).context("删除旧基线目录副本失败")?;
    Ok(())
}

pub fn read_manifest(dir: &Path) -> Result<serde_json::Value> {
    let raw = fs::read_to_string(dir.join("manifest.json")).context("manifest.json 缺失")?;
    Ok(serde_json::from_str(&raw)?)
}
