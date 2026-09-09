//! R1–R6 审查退回项的正式回归(自 review-probe 迁移):
//! - R1 运行图面板 DTO 的实例身份与字段完整性(真实 Catalog 解析)
//! - R2 图补丁删除已启动节点必须被拒(prepare + 事务内)
//! - R6 改图继承后输入历史仍可见(按 node_key 投影)
//! 生产编译缝隙(OrchestratorRunLifecyclePort::prepare_graph_patch)
//! + 真实 Store 事务,不启动外部 Agent CLI。

use mf_agent::workflow::*;
use mf_agent::Store;
use mf_agent::{RetryMode, Settlement};
use mf_kernel::handles::*;
use mf_kernel::kernel::{WorkflowRunCommand, WorkflowRunExpected};
use mf_kernel::run_lifecycle::{RunLifecyclePort, RunPreparation};
use mf_web::execution_ports::{OrchestratorRunLifecyclePort, OrchestratorWorkflowStartPort};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

// 复用 mf-agent 集成测试的公共装配(RecordingHost/fixture)。
#[path = "../../../mf-agent/tests/common/mod.rs"]
mod common;
#[path = "../../../mf-agent/tests/common/run_lifecycle.rs"]
mod lifecycle;
use common::*;

struct StrictResolver(Arc<mf_agent::CatalogStore>);
impl mf_agent::orchestrator::WorkflowInstanceResolver for StrictResolver {
    fn resolve(&self, reference: &str) -> anyhow::Result<mf_agent::AgentInstanceSnapshot> {
        self.0.snapshot_agent_instance(reference, None)
    }
}

/// 生产形态 port:真实 Catalog 解析(不接受任意引用,暴露 R1 的身份错误)。
fn production_port(fx: &Fixture) -> OrchestratorRunLifecyclePort {
    let directory: Arc<dyn mf_agent::execution_directory::ExecutionDirectoryProvider> =
        fx.directory.clone();
    let start = OrchestratorWorkflowStartPort::new(
        fx.orch.clone(),
        plugin_index(),
        Arc::new(StrictResolver(fx.catalog.clone())),
        &directory,
        Some(plugin_pin("scripted", "hash-scripted")),
    );
    OrchestratorRunLifecyclePort {
        orchestrator: fx.orch.clone(),
        start: Some(Arc::new(start)),
    }
}

fn patch_prepare(
    fx: &Fixture,
    task_id: i64,
    nodes: Vec<WorkflowNodeDraft>,
) -> Result<RunPreparation, mf_kernel::kernel::KernelProblem> {
    let task = fx.orch.store.task_view(task_id).unwrap().unwrap();
    let rev = fx.orch.store.active_revision(task_id).unwrap().unwrap();
    production_port(fx).prepare(
        &CommandId::new(),
        &WorkflowRunCommand::ApplyGraphPatch {
            project: ProjectStoreHandle::generate(),
            workflow_run: WorkflowRunHandle::parse(task.public_handle).unwrap(),
            base_revision: rev.public_handle,
            nodes,
            expected: WorkflowRunExpected::only_run(task.revision as u64),
        },
    )
}

fn start_ab(review: bool, bound: bool) -> (tempfile::TempDir, Fixture, i64) {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let mut b = node(
        "b",
        &["a"],
        if bound {
            "read ${inputs.report}"
        } else {
            "do B"
        },
        &fx.instance_id,
    );
    b.require_input_review = review;
    if bound {
        b.input_bindings = vec![InputBinding {
            name: "report".into(),
            source_node_key: "a".into(),
            field_path: "output.path".into(),
            required: true,
            default_value: None,
        }];
    }
    let version = fx.template(
        "review-probe",
        vec![node("a", &[], "do A", &fx.instance_id), b],
    );
    let task = fx.orch.create_task("probe", "probe").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    (tmp, fx, task.id)
}

fn complete_a(fx: &Fixture, task: i64) {
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task, "a"),
            Settlement::Complete {
                summary: "A done".into(),
                output: serde_json::json!({"path": "UPSTREAM_VALID.md"}),
            },
        )
        .unwrap();
}

fn b_id(fx: &Fixture, task: i64) -> i64 {
    fx.orch
        .store
        .task_steps(task)
        .unwrap()
        .into_iter()
        .find(|s| s.step_key == "b")
        .unwrap()
        .id
}

fn await_review(fx: &Fixture, task: i64, step: i64) -> mf_agent::store::NodeInputRecord {
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch
            .store
            .latest_node_input_of_step(task, step)
            .unwrap()
            .map(|r| r.review_state == "awaiting_review")
            .unwrap_or(false)
    }));
    fx.orch
        .store
        .latest_node_input_of_step(task, step)
        .unwrap()
        .unwrap()
}

// ── R1 ─────────────────────────────────────────────────────────────

/// R1:运行图面板的完整节点 DTO(含冻结扩展字段)必须能通过生产 Catalog
/// 的实例解析;agent_type 冒充 instance_id 的旧行为已修复。
#[test]
fn graph_panel_roundtrip_must_resolve_saved_agent_instances() {
    let (_tmp, fx, task) = start_ab(false, false);
    fx.orch.pause_task(task).unwrap();
    let steps = fx.orch.store.task_steps(task).unwrap();
    // 面板 DTO 现携带真实 instance id(frozen snapshot 的 instance.id)
    let nodes = steps
        .iter()
        .map(|s| WorkflowNodeDraft {
            key: s.step_key.clone(),
            title: s.title.clone(),
            instructions: s.instructions.clone(),
            agent_instance_id: fx.instance_id.clone(),
            deps: s
                .deps
                .iter()
                .map(|id| steps.iter().find(|p| p.id == *id).unwrap().step_key.clone())
                .collect(),
            ..Default::default()
        })
        .collect();
    let result = patch_prepare(&fx, task, nodes);
    fx.orch.stop();
    assert!(
        result.is_ok(),
        "run-panel DTO failed with production catalog resolver: {result:?}"
    );
}

/// R1(字段完整性):含五个扩展字段的完整节点经面板回传 → 补丁编译保留。
#[test]
fn graph_patch_preserves_extended_node_fields_for_unstarted_nodes() {
    let (_tmp, fx, task) = start_ab(false, false);
    fx.orch.pause_task(task).unwrap();
    let mut b_draft = node("b", &["a"], "do B with ${inputs.report}", &fx.instance_id);
    b_draft.acceptance_criteria = "逐条对照".into();
    b_draft.output_schema = Some(serde_json::json!({
        "type": "object",
        "properties": {"verdict": {"type": "string"}},
        "required": ["verdict"]
    }));
    b_draft.input_bindings = vec![InputBinding {
        name: "report".into(),
        source_node_key: "a".into(),
        field_path: "output.path".into(),
        required: true,
        default_value: None,
    }];
    b_draft.context_policy = Some(ContextPolicy::ExplicitOnly);
    b_draft.require_input_review = true;
    let nodes = vec![node("a", &[], "do A", &fx.instance_id), b_draft];
    let prepared = patch_prepare(&fx, task, nodes.clone()).unwrap();
    fx.orch.stop();
    let RunPreparation::GraphPatch { pipeline_json, .. } = prepared else {
        panic!("expected graph patch preparation");
    };
    let snapshot: WorkflowSnapshot = serde_json::from_str(&pipeline_json).unwrap();
    let b = snapshot.nodes.iter().find(|n| n.key == "b").unwrap();
    assert_eq!(b.acceptance_criteria, "逐条对照");
    assert_eq!(
        b.output_schema.as_ref().unwrap()["required"],
        serde_json::json!(["verdict"])
    );
    assert_eq!(b.input_bindings.len(), 1);
    assert_eq!(b.context_policy, Some(ContextPolicy::ExplicitOnly));
    assert!(b.require_input_review);
}

// ── R2 ─────────────────────────────────────────────────────────────

/// R2:prepare 必须拒绝删除已启动节点。
#[test]
fn graph_patch_must_reject_removing_a_started_node() {
    let (_tmp, fx, task) = start_ab(false, false);
    fx.orch.pause_task(task).unwrap();
    let result = patch_prepare(&fx, task, vec![node("b", &[], "do B", &fx.instance_id)]);
    fx.orch.stop();
    assert!(
        result.is_err(),
        "production prepare accepted deleting running A"
    );
}

/// R2(事务内):prepare 之后、提交之前状态变化——直接走 Store 事务路径
/// 删除已启动节点也必须被拒,且无部分写入。
#[test]
fn graph_patch_tx_must_reject_removing_started_node_after_prepare() {
    let (_tmp, fx, task) = start_ab(false, false);
    fx.orch.pause_task(task).unwrap();
    // 合法 prepare(含 a/b),但提交的快照丢掉 a——模拟 prepare/提交间竞争
    let prepared = patch_prepare(
        &fx,
        task,
        vec![
            node("a", &[], "do A", &fx.instance_id),
            node("b", &["a"], "do B", &fx.instance_id),
        ],
    )
    .unwrap();
    let RunPreparation::GraphPatch {
        pipeline_json,
        digest,
    } = prepared
    else {
        panic!("expected preparation");
    };
    let mut snapshot: WorkflowSnapshot = serde_json::from_str(&pipeline_json).unwrap();
    snapshot.nodes.retain(|n| n.key != "a");
    let before_revision = fx.orch.store.active_revision(task).unwrap().unwrap().id;
    let result = fx
        .orch
        .store
        .with_tx(|tx| Store::create_patched_revision_tx(tx, task, &snapshot, &digest));
    fx.orch.stop();
    assert!(result.is_err(), "事务内必须拒绝删除已启动节点");
    let after = fx.orch.store.active_revision(task).unwrap().unwrap().id;
    assert_eq!(before_revision, after, "拒绝后活动 Revision 不变");
}

// ── R3 ─────────────────────────────────────────────────────────────

/// R3:仅改绑定值(不整段覆盖 prompt)必须进入实际发送的 prompt。
#[test]
fn binding_only_override_must_reach_the_sent_prompt() {
    let (_tmp, fx, task) = start_ab(true, true);
    complete_a(&fx, task);
    let b = b_id(&fx, task);
    let rec = await_review(&fx, task, b);
    let mut overrides = InputOverrides_default();
    overrides
        .binding_values
        .insert("report".into(), "USER_FIXED.md".into());
    let compiled = mf_agent::node_input::apply_overrides(rec.compiled, &overrides);
    fx.orch.stop();
    assert!(
        mf_agent::node_input::full_prompt(&compiled).contains("USER_FIXED.md"),
        "binding value changed but prompt still contains original input: {}",
        compiled.business_prompt
    );
    assert!(
        !compiled.business_prompt.contains("UPSTREAM_VALID.md"),
        "旧上游值不得残留"
    );
}

/// R3(补值场景):必填缺失经补值后,占位文字被实际值替换且必填错误清除。
#[test]
fn missing_required_backfill_must_render_actual_value() {
    let (_tmp, fx, task) = start_ab(true, true);
    // A 不产出 path → B 必填缺失
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task, "a"),
            Settlement::Complete {
                summary: "A done without path".into(),
                output: serde_json::json!({}),
            },
        )
        .unwrap();
    let b = b_id(&fx, task);
    let rec = await_review(&fx, task, b);
    assert!(!rec.compiled.missing_required.is_empty());
    let mut overrides = InputOverrides_default();
    overrides
        .binding_values
        .insert("report".into(), "BACKFILLED.md".into());
    let compiled = mf_agent::node_input::apply_overrides(rec.compiled, &overrides);
    fx.orch.stop();
    assert!(compiled.missing_required.is_empty());
    assert!(
        compiled.business_prompt.contains("BACKFILLED.md"),
        "补值必须进入渲染结果: {}",
        compiled.business_prompt
    );
    assert!(
        !compiled
            .business_prompt
            .contains("(输入映射 `report` 缺失)"),
        "缺失占位不得残留"
    );
}

fn InputOverrides_default() -> mf_agent::node_input::InputOverrides {
    mf_agent::node_input::InputOverrides::default()
}

// ── R4(行为级;命令路径见 mf-kernel input_gate_commands)──────────

/// R4:保存覆盖推进输入版本轴(独立轴,不依赖 run/step revision)。
#[test]
fn saving_input_must_advance_a_confirmation_revision() {
    let (_tmp, fx, task) = start_ab(true, true);
    complete_a(&fx, task);
    let b = b_id(&fx, task);
    let rec = await_review(&fx, task, b);
    fx.orch.pause_task(task).unwrap();
    let mut overrides = InputOverrides_default();
    overrides.business_prompt = Some("different business input".into());
    fx.orch
        .store
        .save_input_overrides(rec.id, b, &overrides, rec.input_revision)
        .unwrap();
    let after = fx
        .orch
        .store
        .latest_node_input_of_step(task, b)
        .unwrap()
        .unwrap();
    fx.orch.stop();
    assert!(
        after.input_revision > rec.input_revision,
        "保存必须推进输入版本轴(用户看过的旧确认将失效)"
    );
}

// ── R5 ─────────────────────────────────────────────────────────────

/// R5:人工检查节点失败重试后,新待确认输入仍解析有效上游 Handoff。
#[test]
fn review_retry_must_keep_upstream_handoff() {
    let (_tmp, fx, task) = start_ab(true, true);
    complete_a(&fx, task);
    let b = b_id(&fx, task);
    let rec = await_review(&fx, task, b);
    fx.orch
        .store
        .confirm_node_input(rec.id, b, rec.input_revision)
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch
            .store
            .latest_node_input_of_step(task, b)
            .unwrap()
            .map(|r| r.status == "dispatched")
            .unwrap_or(false)
    }));
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task, "b"),
            Settlement::Fail {
                reason: "retry me".into(),
            },
        )
        .unwrap();
    lifecycle::retry_step(&fx.orch, b, RetryMode::FreshSession).unwrap();
    let next = await_review(&fx, task, b);
    fx.orch.stop();
    assert!(
        next.compiled.missing_required.is_empty(),
        "valid upstream output became missing on retry: {:?}",
        next.compiled.bindings
    );
    let resolved = next
        .compiled
        .bindings
        .iter()
        .find(|binding| binding.name == "report")
        .unwrap();
    assert_eq!(resolved.value.as_deref(), Some("UPSTREAM_VALID.md"));
    assert!(resolved.source.is_some(), "来源谱系指向真实上游");
}

// ── R6 ─────────────────────────────────────────────────────────────

/// R6:改图继承后,按 node_key 的权威投影仍能看到 A 的输入记录;
/// 原 step_id 归属不变(历史事实不动)。
#[test]
fn patch_must_retain_input_history_for_inherited_step() {
    let (_tmp, fx, task) = start_ab(false, false);
    fx.orch.pause_task(task).unwrap();
    complete_a(&fx, task);
    let old_a = fx
        .orch
        .store
        .task_steps(task)
        .unwrap()
        .into_iter()
        .find(|s| s.step_key == "a")
        .unwrap();
    assert!(fx
        .orch
        .store
        .latest_node_input_of_step(task, old_a.id)
        .unwrap()
        .is_some());
    let nodes = vec![
        node("a", &[], "do A", &fx.instance_id),
        node("b", &["a"], "do B", &fx.instance_id),
    ];
    let prepared = patch_prepare(&fx, task, nodes).unwrap();
    let RunPreparation::GraphPatch {
        pipeline_json,
        digest,
    } = prepared
    else {
        panic!("expected preparation");
    };
    let snapshot: WorkflowSnapshot = serde_json::from_str(&pipeline_json).unwrap();
    fx.orch
        .store
        .with_tx(|tx| Store::create_patched_revision_tx(tx, task, &snapshot, &digest))
        .unwrap();
    let new_a = fx
        .orch
        .store
        .task_steps(task)
        .unwrap()
        .into_iter()
        .find(|s| s.step_key == "a")
        .unwrap();
    let by_step = fx
        .orch
        .store
        .latest_node_input_of_step(task, new_a.id)
        .unwrap();
    let by_key = fx.orch.store.latest_node_input_of_key(task, "a").unwrap();
    fx.orch.stop();
    assert!(
        by_key.is_some(),
        "改图后按 node_key 必须能看到继承节点的输入历史"
    );
    assert!(
        by_step.is_none(),
        "新 Step 行没有属于自己的输入记录(历史归属旧行,事实不变)"
    );
    let record = by_key.unwrap();
    assert_eq!(record.step_id, old_a.id, "输入记录保持原 Step 归属");
}

/// R6(权威查询级):改图后按 node_key 的权威输入查询仍返回记录。
/// 注:该测试调用 Store 权威接口而非 Kernel snapshot——投影级断言见
/// S4/S5 用例(actual_projection_after_stop)。
#[test]
fn inherited_input_remains_reachable_by_key_after_patch() {
    use mf_kernel::handles::WorkflowRunHandle;
    let (_tmp, fx, task) = start_ab(false, false);
    fx.orch.pause_task(task).unwrap();
    complete_a(&fx, task);
    let nodes = vec![
        node("a", &[], "do A", &fx.instance_id),
        node("b", &["a"], "do B", &fx.instance_id),
    ];
    let prepared = patch_prepare(&fx, task, nodes).unwrap();
    let RunPreparation::GraphPatch {
        pipeline_json,
        digest,
    } = prepared
    else {
        panic!("expected preparation");
    };
    let snapshot: WorkflowSnapshot = serde_json::from_str(&pipeline_json).unwrap();
    fx.orch
        .store
        .with_tx(|tx| Store::create_patched_revision_tx(tx, task, &snapshot, &digest))
        .unwrap();
    // 投影按 node_key 命中:改图后 a 的输入卡仍在
    let view = fx.orch.store.task_view(task).unwrap().unwrap();
    let record = fx
        .orch
        .store
        .latest_node_input_of_key(task, "a")
        .unwrap()
        .unwrap();
    let _ = WorkflowRunHandle::parse(view.public_handle);
    fx.orch.stop();
    assert!(!record.compiled.business_prompt.is_empty());
}

// 防 unused 警告(共享装配的部分项仅个别用例使用)
#[allow(dead_code)]
fn _unused(m: &Mutex<()>) {
    let _ = m;
}

// ── S3:prepare 后节点启动,提交改变其定义必须拒绝(Kernel 命令路径) ──

#[test]
fn graph_commit_must_recheck_newly_started_node_definition() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "s3",
        vec![
            node("a", &[], "do A", &fx.instance_id),
            node("b", &["a"], "OLD B", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("s3", "probe").unwrap();
    let task = task.id;
    fx.assign_and_run(task, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    complete_a(&fx, task);

    // 暂停,B 未启动;prepare 一份修改 B 指令的合法补丁
    fx.orch.pause_task(task).unwrap();
    let rev = fx.orch.store.active_revision(task).unwrap().unwrap();
    let port = production_port(&fx);
    let prepared = port
        .prepare(
            &mf_kernel::handles::CommandId::new(),
            &WorkflowRunCommand::ApplyGraphPatch {
                project: ProjectStoreHandle::generate(),
                workflow_run: WorkflowRunHandle::parse(
                    fx.orch
                        .store
                        .task_view(task)
                        .unwrap()
                        .unwrap()
                        .public_handle,
                )
                .unwrap(),
                base_revision: rev.public_handle.clone(),
                nodes: vec![
                    node("a", &[], "do A", &fx.instance_id),
                    node("b", &["a"], "NEW B INSTRUCTIONS", &fx.instance_id),
                ],
                expected: WorkflowRunExpected::only_run(1),
            },
        )
        .unwrap();
    let RunPreparation::GraphPatch {
        pipeline_json,
        digest,
    } = prepared
    else {
        panic!("expected preparation");
    };

    // 恢复派发:B 按旧指令启动(attempts>0),再次暂停
    fx.orch.resume_task(task).unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch
            .store
            .task_steps(task)
            .unwrap()
            .iter()
            .find(|s| s.step_key == "b")
            .map(|s| s.attempts > 0)
            .unwrap_or(false)
    }));
    fx.orch.pause_task(task).unwrap();

    // 提交之前准备的补丁:事务必须拒绝(B 已启动且定义变化)
    let snapshot: WorkflowSnapshot = serde_json::from_str(&pipeline_json).unwrap();
    let active_before = fx.orch.store.active_revision(task).unwrap().unwrap().id;
    let result = fx
        .orch
        .store
        .with_tx(|tx| Store::create_patched_revision_tx(tx, task, &snapshot, &digest));
    fx.orch.stop();
    assert!(result.is_err(), "提交不得改变已启动节点的定义:{result:?}");
    let active_after = fx.orch.store.active_revision(task).unwrap().unwrap().id;
    assert_eq!(active_before, active_after, "原图保持不变");
}

// ── S4/S5:真正 Kernel 投影回归(InProcessKernelRuntime + Legacy client)──

fn actual_projection_after_stop(
    fx: &Fixture,
    task: i64,
    tmp: &std::path::Path,
) -> serde_json::Value {
    use mf_kernel::command::ServiceIdempotencyKey;
    use mf_kernel::handles::{ClientId, Principal, WorkflowRunHandle};
    use mf_kernel::kernel::InProcessKernelRuntime;
    use mf_kernel::project_registry::ServiceStore;

    let projectroot = tmp.join("projection-project");
    std::fs::create_dir_all(projectroot.join(".mf-agent")).unwrap();
    let target = projectroot.join(".mf-agent/workflow-v1.db");
    // VACUUM INTO 不接受绑定参数(SQLite 限制),路径来自临时目录,无注入面
    let escaped = target.to_string_lossy().replace("'", "''");
    let vacuum_sql = format!("VACUUM INTO '{}'", escaped);
    fx.orch
        .store
        .with_conn(move |c| {
            c.execute_batch(&vacuum_sql)?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();
    let service = ServiceStore::open(&tmp.join("projection-service.db")).unwrap();
    let (runtime, client) = InProcessKernelRuntime::for_test(
        service,
        ServiceIdempotencyKey::for_test(vec![0x39; 32]).unwrap(),
        ClientId::parse("review-client").unwrap(),
        Principal::parse("review-user").unwrap(),
    )
    .unwrap();
    let project = runtime.open_project(&projectroot).unwrap();
    let run = fx.orch.store.task_view(task).unwrap().unwrap();
    let snapshot = client
        .workflow_run_snapshot(
            project.handle(),
            &WorkflowRunHandle::parse(run.public_handle).unwrap(),
        )
        .unwrap();
    match snapshot.data {
        mf_kernel::projection::SnapshotData::WorkflowRun(data) => {
            serde_json::to_value(data).unwrap()
        }
        _ => panic!("wrong snapshot kind"),
    }
}

#[test]
fn run_graph_preserves_frozen_dependency_order_for_roundtrip_editing() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "ordered-join",
        vec![
            node("a", &[], "a", &fx.instance_id),
            node("b", &[], "b", &fx.instance_id),
            node("join", &["b", "a"], "join", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("join order", "join order").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 2));
    fx.orch.pause_task(task.id).unwrap();
    fx.orch.stop();
    let projection = actual_projection_after_stop(&fx, task.id, tmp.path());
    let steps = projection["steps"].as_array().unwrap();
    let joined = steps.iter().find(|step| step["key"] == "join").unwrap();
    let keys = joined["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|handle| {
            steps.iter().find(|step| step["step"] == *handle).unwrap()["key"]
                .as_str()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(keys, vec!["b", "a"]);
}

/// S4:未启动节点改指令后,当前输入不再显示旧模板(真投影)。
#[test]
fn patched_unstarted_node_must_not_show_obsolete_input_as_current() {
    let (tmp, fx, task) = start_ab(true, false);
    complete_a(&fx, task);
    let b = b_id(&fx, task);
    let _rec = await_review(&fx, task, b);

    fx.orch.pause_task(task).unwrap();
    let mut changed = node("b", &["a"], "NEW B INSTRUCTIONS", &fx.instance_id);
    changed.require_input_review = true;
    let prepared = patch_prepare(
        &fx,
        task,
        vec![node("a", &[], "do A", &fx.instance_id), changed],
    )
    .unwrap();
    let RunPreparation::GraphPatch {
        pipeline_json,
        digest,
    } = prepared
    else {
        panic!("wrong preparation");
    };
    let snapshot: WorkflowSnapshot = serde_json::from_str(&pipeline_json).unwrap();
    fx.orch
        .store
        .with_tx(|tx| Store::create_patched_revision_tx(tx, task, &snapshot, &digest))
        .unwrap();
    fx.orch.stop();

    let projection = actual_projection_after_stop(&fx, task, tmp.path());
    let bview = projection["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "b")
        .unwrap();
    let input = &bview["input"];
    assert!(
        input.is_null() || input["template"] == "NEW B INSTRUCTIONS",
        "current B instructions={} but current input shows obsolete template {:?}",
        bview["instructions"],
        input["template"]
    );
}

/// S5:取消运行后 input-review 提醒不再存在(真投影)。
#[test]
fn cancelled_gate_must_not_remain_in_needs_you() {
    let (tmp, fx, task) = start_ab(true, false);
    complete_a(&fx, task);
    let b = b_id(&fx, task);
    await_review(&fx, task, b);

    fx.orch.cancel_task(task).unwrap();
    fx.orch.stop();

    let projection = actual_projection_after_stop(&fx, task, tmp.path());
    assert_eq!(projection["status"], "cancelled");
    assert_eq!(
        projection["needs_you"], false,
        "cancelled workflow still advertises input-review: {}",
        projection["needs_you_reasons"]
    );
}

// ── U1:同实例 ID 不同版本/执行配置的冻结竞争,事务必须拒绝 ──────────

#[test]
fn third_started_node_must_keep_full_frozen_agent_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "u1",
        vec![
            node("a", &[], "do A", &fx.instance_id),
            node("b", &["a"], "do B", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("u1", "probe").unwrap();
    let task = task.id;
    fx.assign_and_run(task, &version);
    assert!(wait_until(Duration::from_secs(5), || fx
        .host
        .workflow
        .lock()
        .len()
        == 1));
    complete_a(&fx, task);

    // 暂停(B 未启动);更新 Catalog 同一实例 ID → v2,改 executable/argv
    fx.orch.pause_task(task).unwrap();
    let mut upgrade = instance_draft("fixture worker", "agent-v2.exe");
    upgrade.argv = vec!["--new".into()];
    fx.catalog
        .update_agent_instance(&fx.instance_id, upgrade)
        .unwrap();
    let resolved_v2 = fx
        .catalog
        .snapshot_agent_instance(&fx.instance_id, None)
        .unwrap();
    assert_eq!(resolved_v2.version, 2, "fixture: instance upgraded to v2");
    let _ = resolved_v2;

    // prepare 一份不改 B 职责字段标题/指令/依赖的补丁(via 生产 port:
    // 编译用当前 Catalog → B 冻结为 v2 配置)
    let rev = fx.orch.store.active_revision(task).unwrap().unwrap();
    let port = production_port(&fx);
    let prepared = port
        .prepare(
            &mf_kernel::handles::CommandId::new(),
            &WorkflowRunCommand::ApplyGraphPatch {
                project: ProjectStoreHandle::generate(),
                workflow_run: WorkflowRunHandle::parse(
                    fx.orch
                        .store
                        .task_view(task)
                        .unwrap()
                        .unwrap()
                        .public_handle,
                )
                .unwrap(),
                base_revision: rev.public_handle.clone(),
                nodes: vec![
                    node("a", &[], "do A", &fx.instance_id),
                    node("b", &["a"], "do B", &fx.instance_id),
                ],
                expected: WorkflowRunExpected::only_run(1),
            },
        )
        .unwrap();
    let RunPreparation::GraphPatch {
        pipeline_json,
        digest,
    } = prepared
    else {
        panic!("expected preparation");
    };

    // 恢复派发:B 按当前活动 Revision 的 v1 启动;再暂停
    fx.orch.resume_task(task).unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.orch
            .store
            .task_steps(task)
            .unwrap()
            .iter()
            .find(|s| s.step_key == "b")
            .map(|s| s.attempts > 0)
            .unwrap_or(false)
    }));
    fx.orch.pause_task(task).unwrap();
    // 断言 B 实际按 v1 启动(实例版本来自冻结快照)
    let sent_version = {
        let guard = fx.host.workflow.lock();
        let (spec, _) = guard.iter().find(|(spec, _)| spec.node_key == "b").unwrap();
        spec.instance.version
    };
    assert_eq!(sent_version, 1, "fixture: B started with frozen v1");

    // 提交补丁:事务必须拒绝(v1 已启动,补丁把冻结配置改成 v2)
    let snapshot: WorkflowSnapshot = serde_json::from_str(&pipeline_json).unwrap();
    let proposed_version = snapshot
        .nodes
        .iter()
        .find(|n| n.key == "b")
        .unwrap()
        .instance
        .version;
    assert_eq!(proposed_version, 2, "fixture: patch proposes v2");
    let active_before = fx.orch.store.active_revision(task).unwrap().unwrap().id;
    let result = fx
        .orch
        .store
        .with_tx(|tx| Store::create_patched_revision_tx(tx, task, &snapshot, &digest));
    fx.orch.stop();
    assert!(
        result.is_err(),
        "U1: sent version 1, proposed version 2 — transaction must reject: {result:?}"
    );
    let active_after = fx.orch.store.active_revision(task).unwrap().unwrap().id;
    assert_eq!(active_before, active_after, "当前 Revision 不变");
}
