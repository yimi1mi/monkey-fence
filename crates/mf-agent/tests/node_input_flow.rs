//! T2 节点输入链:统一编译 → 冻结记录 → 派发消费冻结 prompt →
//! 必填缺失不启动 → 输出约束在成功结算的事务前置校验中拒绝。

mod common;

use common::*;
use mf_agent::model::*;
use mf_agent::node_input;
use mf_agent::workflow::{ContextPolicy, InputBinding, WorkflowNodeDraft};
use std::time::Duration;

fn bound_node(
    key: &str,
    deps: &[&str],
    instructions: &str,
    instance: &str,
    bindings: Vec<InputBinding>,
    policy: Option<ContextPolicy>,
) -> WorkflowNodeDraft {
    let mut draft = node(key, deps, instructions, instance);
    draft.input_bindings = bindings;
    draft.context_policy = policy;
    draft
}

fn binding(name: &str, source: &str, path: &str, required: bool) -> InputBinding {
    InputBinding {
        name: name.into(),
        source_node_key: source.into(),
        field_path: path.into(),
        required,
        default_value: None,
    }
}

fn step_id_of(fx: &Fixture, task_id: i64, key: &str) -> i64 {
    let steps = fx.orch.store.task_steps(task_id).unwrap();
    steps.iter().find(|s| s.step_key == key).unwrap().id
}

#[test]
fn t2_dispatch_consumes_frozen_input_record_and_adapter_receives_it() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "inputs",
        vec![
            node("a", &[], "产出报告路径", &fx.instance_id),
            bound_node(
                "b",
                &["a"],
                "读取 ${inputs.report}",
                &fx.instance_id,
                vec![binding("report", "a", "output.report_path", true)],
                Some(ContextPolicy::ExplicitOnly),
            ),
        ],
    );
    let task = fx.orch.create_task("输入链", "验收输入冻结").unwrap();
    fx.assign_and_run(task.id, &version);

    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 1
    }));
    let token_a = token_of_node(&fx.orch, task.id, "a");
    fx.orch
        .settle_by_token(
            &token_a,
            Settlement::Complete {
                summary: "A 完成".into(),
                output: serde_json::json!({"report_path": "reports/r1.md"}),
            },
        )
        .unwrap();

    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 2
    }));

    // b 的冻结输入记录:解析值 + explicit_only 语义 + dispatched
    let step_b = step_id_of(&fx, task.id, "b");
    let record = fx
        .orch
        .store
        .latest_node_input_of_step(task.id, step_b)
        .unwrap()
        .expect("b 必须有冻结输入记录");
    assert_eq!(record.status, "dispatched");
    assert_eq!(record.compiled.missing_required, Vec::<String>::new());
    let resolved = record
        .compiled
        .bindings
        .iter()
        .find(|b| b.name == "report")
        .unwrap();
    assert_eq!(resolved.value.as_deref(), Some("reports/r1.md"));
    assert!(resolved.source.is_some(), "来源(节点/Run)已记录");

    // mock Adapter 收到的内容与冻结记录完全一致
    let (spec, _) = {
        let guard = fx.host.workflow.lock();
        guard
            .iter()
            .find(|(spec, _)| spec.node_key == "b")
            .cloned()
            .expect("b 已派发")
    };
    assert_eq!(spec.prompt, node_input::full_prompt(&record.compiled));
    assert!(
        spec.prompt.contains("reports/r1.md"),
        "解析值必须进入 prompt: {}",
        spec.prompt
    );
    assert!(
        !spec.prompt.contains("上游交接:"),
        "explicit_only 不自动注入祖先摘要"
    );
    assert!(!spec.prompt.contains("${inputs."), "引用必须被替换");

    fx.orch.stop();
}

#[test]
fn t2_missing_required_binding_does_not_launch_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "missing",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            bound_node(
                "c",
                &["a"],
                "用 ${inputs.must}",
                &fx.instance_id,
                vec![binding("must", "a", "output.absent_field", true)],
                Some(ContextPolicy::ExplicitOnly),
            ),
        ],
    );
    let task = fx.orch.create_task("缺字段", "不启动").unwrap();
    fx.assign_and_run(task.id, &version);

    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 1
    }));
    let token_a = token_of_node(&fx.orch, task.id, "a");
    fx.orch
        .settle_by_token(
            &token_a,
            Settlement::Complete {
                summary: "A 完成(无该字段)".into(),
                output: serde_json::json!({}),
            },
        )
        .unwrap();

    // 缺失发生在 attempt 前：进入可补值的持久门控，不能把尚未启动的节点锁死为失败。
    let step_c = step_id_of(&fx, task.id, "c");
    assert!(wait_until(Duration::from_secs(5), || fx
        .orch
        .store
        .latest_node_input_of_step(task.id, step_c)
        .unwrap()
        .is_some_and(|record| record.review_state == "awaiting_review")));
    assert!(fx
        .host
        .workflow
        .lock()
        .iter()
        .all(|(spec, _)| spec.node_key != "c"));
    let record = fx
        .orch
        .store
        .latest_node_input_of_step(task.id, step_c)
        .unwrap()
        .unwrap();
    assert_eq!(record.compiled.missing_required, vec!["must".to_string()]);
    assert_eq!(
        fx.orch.store.step_view(step_c).unwrap().unwrap().attempts,
        0
    );
    assert!(
        fx.orch
            .store
            .confirm_node_input(record.id, step_c, record.input_revision)
            .is_err(),
        "缺失补齐前不能确认"
    );
    let mut overrides = mf_agent::node_input::InputOverrides::default();
    overrides
        .binding_values
        .insert("must".into(), "USER_SUPPLIED_VALUE".into());
    fx.orch
        .store
        .save_input_overrides(record.id, step_c, &overrides, record.input_revision)
        .unwrap();
    fx.orch
        .store
        .confirm_node_input(record.id, step_c, record.input_revision + 1)
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .iter()
        .any(|(spec, _)| spec.node_key == "c")));
    let guard = fx.host.workflow.lock();
    let sent = &guard
        .iter()
        .find(|(spec, _)| spec.node_key == "c")
        .unwrap()
        .0;
    assert!(sent.prompt.contains("USER_SUPPLIED_VALUE"));
    assert_eq!(
        fx.orch.store.step_view(step_c).unwrap().unwrap().attempts,
        1
    );
    drop(guard);
    fx.orch.stop();
}

#[test]
fn t2_output_schema_rejects_bad_settlement_and_keeps_awaiting() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let mut a = node("a", &[], "产出判定", &fx.instance_id);
    a.output_schema = Some(serde_json::json!({
        "type": "object",
        "properties": {"verdict": {"type": "string"}},
        "required": ["verdict"]
    }));
    let version = fx.template("schema", vec![a]);
    let task = fx.orch.create_task("输出约束", "校验结算前置").unwrap();
    fx.assign_and_run(task.id, &version);

    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 1
    }));
    let token_a = token_of_node(&fx.orch, task.id, "a");

    // 违规结算:被拒绝,run 保持待结算
    let bad = fx.orch.settle_by_token(
        &token_a,
        Settlement::Complete {
            summary: "缺 verdict".into(),
            output: serde_json::json!({"issues": ["x"]}),
        },
    );
    let error_text = format!("{bad:?}");
    assert!(
        error_text.contains("OutputSchemaViolation"),
        "结算应被输出约束拒绝:{error_text}"
    );
    let run = fx
        .orch
        .store
        .list_runs_of_task(task.id)
        .unwrap()
        .into_iter()
        .find(|r| r.step_id == step_id_of(&fx, task.id, "a"))
        .unwrap();
    assert!(run.outcome.is_none(), "拒绝后保持未结算(可修正后重新结算)");

    // 修正后重新结算 → 成功
    fx.orch
        .settle_by_token(
            &token_a,
            Settlement::Complete {
                summary: "带判定".into(),
                output: serde_json::json!({"verdict": "pass"}),
            },
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch.store.task_view(task.id).unwrap().unwrap().status == TaskStatus::Succeeded
    }));
    fx.orch.stop();
}

// ── T3:人工检查门控 ────────────────────────────────────────────────

use mf_agent::node_input::InputOverrides;

#[test]
fn t3_review_gate_holds_until_confirmed_and_dispatches_edited_prompt_once() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let mut b = node("b", &["a"], "读取 ${inputs.report}", &fx.instance_id);
    b.input_bindings = vec![binding("report", "a", "output.report_path", true)];
    b.require_input_review = true;
    let version = fx.template("gate", vec![node("a", &[], "做 A", &fx.instance_id), b]);
    let task = fx.orch.create_task("门控", "确认后启动").unwrap();
    fx.assign_and_run(task.id, &version);

    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 1
    }));
    let token_a = token_of_node(&fx.orch, task.id, "a");
    fx.orch
        .settle_by_token(
            &token_a,
            Settlement::Complete {
                summary: "A 完成".into(),
                output: serde_json::json!({"report_path": "reports/r1.md"}),
            },
        )
        .unwrap();

    // 门控:b 不派发,awaiting_review 记录就位(重启后依然挂起)
    std::thread::sleep(Duration::from_millis(800));
    let step_b = step_id_of(&fx, task.id, "b");
    {
        let guard = fx.host.workflow.lock();
        assert!(
            guard.iter().all(|(spec, _)| spec.node_key != "b"),
            "确认前 b 不得派发"
        );
    }
    let record = fx
        .orch
        .store
        .latest_node_input_of_step(task.id, step_b)
        .unwrap()
        .expect("门控记录存在");
    assert_eq!(record.review_state, "awaiting_review");

    // 崩溃恢复语义由持久记录保证:review_state/overrides 落在项目库,
    // 重启后 gate 读取同一记录(此处不重启 orchestrator,见 T6 e2e 重启用例)

    // 用户编辑覆盖:必填补值 + prompt 覆盖 → 确认
    let mut overrides = InputOverrides::default();
    overrides
        .binding_values
        .insert("report".into(), "reports/OVERRIDDEN.md".into());
    overrides.business_prompt = Some("用户改写后的业务内容\n带上 reports/OVERRIDDEN.md".into());
    fx.orch
        .store
        .save_input_overrides(record.id, step_b, &overrides, record.input_revision)
        .unwrap();
    // R4:保存推进 input_revision——用户看过的旧版本确认必须被拒绝
    let stale = fx
        .orch
        .store
        .confirm_node_input(record.id, step_b, record.input_revision);
    assert!(stale.is_err(), "陈旧版本确认必须冲突:{stale:?}");
    let fresh_record = fx
        .orch
        .store
        .latest_node_input_of_step(task.id, step_b)
        .unwrap()
        .unwrap();
    assert!(
        fresh_record.input_revision > record.input_revision,
        "保存推进输入版本"
    );
    let transitioned = fx
        .orch
        .store
        .confirm_node_input(record.id, step_b, fresh_record.input_revision)
        .unwrap();
    assert!(transitioned, "首次确认发生转移");
    // 同版本重复确认幂等(不产生第二次转移)
    let replay = fx
        .orch
        .store
        .confirm_node_input(record.id, step_b, fresh_record.input_revision)
        .unwrap();
    assert!(!replay, "重复确认幂等");

    // 确认后派发恰好一次,且收到修改后的内容
    if !wait_until(Duration::from_secs(5), || {
        fx.host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "b")
    }) {
        let tv = fx.orch.store.task_view(task.id).unwrap().unwrap();
        eprintln!("TASK={:?} paused={}", tv.status, tv.paused);
        for st in fx.orch.store.task_steps(task.id).unwrap() {
            eprintln!(
                "STEP {} {:?} attempts={}",
                st.step_key, st.status, st.attempts
            );
        }
        let rec = fx
            .orch
            .store
            .latest_node_input_of_step(task.id, step_b)
            .unwrap();
        eprintln!(
            "INPUT review_state={:?}",
            rec.map(|r| (r.review_state, r.status))
        );
        panic!("b 未在确认后派发");
    }
    std::thread::sleep(Duration::from_millis(300));
    let specs = {
        let guard = fx.host.workflow.lock();
        guard
            .iter()
            .filter(|(spec, _)| spec.node_key == "b")
            .map(|(spec, _)| spec.prompt.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(specs.len(), 1, "确认后只派发一次");
    assert!(
        specs[0].contains("用户改写后的业务内容"),
        "收到覆盖后的 prompt: {}",
        specs[0]
    );
    assert!(specs[0].contains("reports/OVERRIDDEN.md"));
    // 协议段仍只读存在
    assert!(specs[0].contains("MF_RUN_TOKEN"));

    // 上游原始 Handoff 不被覆盖篡改
    let handoffs = fx.orch.store.list_handoff_rows(task.id).unwrap();
    let a_handoff = handoffs
        .iter()
        .find(|row| row.handoff.output.get("report_path").is_some())
        .expect("A 的原始交接仍在");
    assert_eq!(
        a_handoff.handoff.output["report_path"],
        serde_json::json!("reports/r1.md"),
        "上游原始输出保持不变"
    );
    fx.orch.stop();
}

#[test]
fn t3_overrides_after_confirm_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let mut b = node("b", &["a"], "做 B", &fx.instance_id);
    b.require_input_review = true;
    let version = fx.template("late", vec![node("a", &[], "做 A", &fx.instance_id), b]);
    let task = fx.orch.create_task("晚到覆盖", "拒绝").unwrap();
    fx.assign_and_run(task.id, &version);

    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 1
    }));
    let token_a = token_of_node(&fx.orch, task.id, "a");
    fx.orch
        .settle_by_token(
            &token_a,
            Settlement::Complete {
                summary: "A".into(),
                output: Default::default(),
            },
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let step_b = step_id_of(&fx, task.id, "b");
    let record = fx
        .orch
        .store
        .latest_node_input_of_step(task.id, step_b)
        .unwrap()
        .unwrap();
    fx.orch
        .store
        .confirm_node_input(record.id, step_b, record.input_revision)
        .unwrap();
    // 确认后覆盖被拒绝(输入已冻结)
    let late = fx.orch.store.save_input_overrides(
        record.id,
        step_b,
        &InputOverrides::default(),
        record.input_revision,
    );
    assert!(late.is_err(), "确认后的覆盖必须被拒绝");
    fx.orch.stop();
}

// ── S1:上一轮 v12 库自动升级(input_revision 列) ──────────────────

#[test]
fn s1_existing_v12_database_upgrades_input_revision_automatically() {
    use mf_agent::schema::PROJECT_SCHEMA_VERSION;
    use mf_agent::Store;

    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");

    // 构造上一轮 v12 形态库:当前 DDL 建库后去掉 input_revision 列并
    // 把 user_version 回拨到 12(列缺失 + 版本 12 = 真实旧 v12 库)
    {
        let store = Store::open(&db).unwrap();
        drop(store);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("ALTER TABLE node_inputs DROP COLUMN input_revision;")
            .unwrap();
        conn.pragma_update(None, "user_version", 12).unwrap();
    }

    // 重新打开:自动、事务性升级(迁移链 + 备份屏障),不要求删库/手工 ALTER
    let store = Store::open(&db).unwrap();
    let version_now: i64 = store
        .with_conn(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(version_now, PROJECT_SCHEMA_VERSION, "自动升级到当前版本");

    // 升级后旧数据的输入可读可编辑可确认:种一条待确认记录走完整门控语义
    let task = store.create_task("v12 升级", "probe").unwrap();
    let step: mf_agent::model::StepView = {
        let task_id = task.id;
        let snapshot = mf_agent::workflow::WorkflowSnapshot {
            template_key: "v12".into(),
            template_version: 1,
            nodes: vec![fake_node_snapshot("a")],
            directory_provider: None,
        };
        let step_view = store
            .with_tx(|tx| {
                let view =
                    mf_agent::Store::create_workflow_revision_tx(tx, task_id, &snapshot, None)
                        .unwrap();
                tx.execute(
                    "UPDATE pipeline_revisions SET status='active' WHERE id=?1",
                    rusqlite::params![view.id],
                )
                .unwrap();
                tx.execute(
                    "UPDATE agent_tasks SET active_revision=?2 WHERE id=?1",
                    rusqlite::params![task_id, view.id],
                )
                .unwrap();
                let step = tx
                    .query_row(
                        "SELECT id, revision_id FROM steps WHERE revision_id=?1 LIMIT 1",
                        rusqlite::params![view.id],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .unwrap();
                Ok(step)
            })
            .unwrap();
        let (id, revision_id) = step_view;
        store.step_view(id).unwrap().unwrap()
    };
    let compiled = mf_agent::node_input::compile_node_input(
        &store.task_view(task.id).unwrap().unwrap(),
        &fake_node_snapshot("a"),
        &Default::default(),
    );
    store
        .insert_node_input(
            task.id,
            step.revision_id,
            step.id,
            "a",
            &compiled,
            "awaiting_review",
        )
        .unwrap();
    let record = store
        .latest_node_input_of_step(task.id, step.id)
        .unwrap()
        .unwrap();
    assert_eq!(record.input_revision, 1, "升级库行默认 input_revision=1");
    let mut overrides = mf_agent::node_input::InputOverrides::default();
    overrides.binding_values.insert("x".into(), "v".into());
    store
        .save_input_overrides(record.id, step.id, &overrides, 1)
        .unwrap();
    assert!(
        store.confirm_node_input(record.id, step.id, 1).is_err(),
        "保存已推进版本,旧版本确认应冲突(证明列真正参与 CAS)"
    );
    store.confirm_node_input(record.id, step.id, 2).unwrap();

    // 重复打开幂等
    drop(store);
    let again = Store::open(&db).unwrap();
    let version_again: i64 = again
        .with_conn(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(version_again, PROJECT_SCHEMA_VERSION, "重复打开幂等");
}

/// 最小可编译快照节点(供 S1 播种;实例字段为占位,不参与断言)。
fn fake_node_snapshot(key: &str) -> mf_agent::workflow::WorkflowNodeSnapshot {
    mf_agent::workflow::WorkflowNodeSnapshot {
        key: key.into(),
        title: key.into(),
        instructions: "do".into(),
        instance: mf_agent::AgentInstanceSnapshot {
            id: "fixture".into(),
            name: "fixture".into(),
            agent_type: "generic-command".into(),
            version: 1,
            enabled: true,
            run_mode: mf_agent::model::RunMode::OneShot,
            executable: "x".into(),
            argv: vec![],
            env: vec![],
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({}),
            sealed_secret_ids: vec![],
            external_config: false,
        },
        deps: vec![],
        plugin: None,
        acceptance_criteria: String::new(),
        output_schema: None,
        input_bindings: vec![],
        context_policy: None,
        require_input_review: false,
    }
}
