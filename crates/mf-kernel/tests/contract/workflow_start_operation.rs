//! T2d 契约(Issue #26):Workflow Start 持久 Operation worker。
//!
//! - dispatch 只 accept:prepare → durable frozen payload(saga_state)→
//!   acceptance receipt/outbox → 立即 `202 accepted`,同步不跑启动;
//! - 后台 worker 在 L-PUBLISH 下执行 steps,同 command_id 不重复创建
//!   Task/Workflow Run;
//! - accept 后 crash / step receipt 后 crash / 重启 reconcile 都只从
//!   durable target receipt 恢复,绝不重放已提交业务 effect;
//! - 启动前失败完整回滚(补偿 + pins 释放);调度开始后失败保留 Run;
//! - 无 port 时显式 fail-closed,不伪装可用。

use crate::command::ServiceIdempotencyKey;
use crate::handles::{ClientId, CommandId, Principal, WorkflowHandle, WorkflowRunHandle};
use crate::kernel::{
    CoreKernel, InProcessCoreKernel, InProcessKernelRuntime, KernelCommand, KernelCommandRequest,
    KernelOutcome, KernelProblem, WorkflowRunCommand,
};
use crate::operation::{
    OperationAcceptFaultPoint, OperationFaultPoint, OperationOutcome, OperationState,
};
use crate::projection::{SnapshotData, SnapshotQuery};
use crate::workflow_run_commands::{FakeRunPort, RunFixture};
use crate::workflow_start::{workflow_start_payload, PreparedWorkflowStartPlan, WorkflowStartPort};
use mf_agent::model::RunMode;
use mf_agent::workflow::{
    workflow_content_digest, WorkflowNodeDraft, WorkflowNodeSnapshot, WorkflowSnapshot,
};
use mf_agent::{AgentInstanceSnapshot, ProjectWorkflowDraft, Store};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

// ───────────────────────── fake 编译 seam ─────────────────────────

fn fake_instance() -> AgentInstanceSnapshot {
    AgentInstanceSnapshot {
        id: "instance".into(),
        name: "instance".into(),
        agent_type: "mock".into(),
        version: 1,
        enabled: true,
        run_mode: RunMode::OneShot,
        executable: "mock".into(),
        argv: vec![],
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({}),
        // sealed ref 是 durable plan 允许的唯一凭据形态。
        sealed_secret_ids: vec!["sealed-1".into()],
        external_config: false,
    }
}

fn fake_snapshot() -> WorkflowSnapshot {
    WorkflowSnapshot {
        template_key: "project-workflow/wf-run-contract".into(),
        template_version: 1,
        nodes: vec![WorkflowNodeSnapshot {
            key: "work".into(),
            title: "work".into(),
            instructions: "do it".into(),
            instance: fake_instance(),
            deps: vec![],
            plugin: None,
        }],
        directory_provider: None,
    }
}

fn fake_content_digest(snapshot: &WorkflowSnapshot) -> String {
    let drafts: Vec<WorkflowNodeDraft> = snapshot
        .nodes
        .iter()
        .map(|node| WorkflowNodeDraft {
            key: node.key.clone(),
            title: node.title.clone(),
            instructions: node.instructions.clone(),
            agent_instance_id: node.instance.id.clone(),
            deps: node.deps.clone(),
        })
        .collect();
    workflow_content_digest(&drafts, false)
}

fn prepared_plan(snapshot: WorkflowSnapshot) -> PreparedWorkflowStartPlan {
    let digest = fake_content_digest(&snapshot);
    PreparedWorkflowStartPlan::new(
        WorkflowHandle::parse(uuid::Uuid::now_v7().to_string()).unwrap(),
        "start contract",
        snapshot,
        digest,
        false,
    )
    .unwrap()
}

#[test]
fn durable_start_rejects_sensitive_env_tuple_keys() {
    for key in ["OPENAI_API_KEY", "AUTH_TOKEN", "CLIENT_SECRET", "PASSWORD"] {
        let mut snapshot = fake_snapshot();
        snapshot.nodes[0].instance.env = vec![(key.into(), "plaintext-must-not-land".into())];
        let error = workflow_start_payload(&prepared_plan(snapshot)).unwrap_err();
        assert!(
            matches!(error, KernelProblem::ValidationFailed(_)),
            "sensitive env tuple key 必须 fail-closed:{key}:{error}"
        );
    }
}

#[test]
fn durable_start_rejects_sensitive_config_and_execution_contract_fields() {
    let mut config = fake_snapshot();
    config.nodes[0].instance.config = serde_json::json!({"nested":{"api_key":"plaintext"}});
    assert!(matches!(
        workflow_start_payload(&prepared_plan(config)),
        Err(KernelProblem::ValidationFailed(_))
    ));

    let mut contract = fake_snapshot();
    contract.nodes[0].instance.execution_contract =
        serde_json::json!({"auth":{"access_token":"plaintext"}});
    assert!(matches!(
        workflow_start_payload(&prepared_plan(contract)),
        Err(KernelProblem::ValidationFailed(_))
    ));
}

#[test]
fn durable_start_allows_sealed_secret_refs() {
    let plan = prepared_plan(fake_snapshot());
    let payload = workflow_start_payload(&plan).expect("sealed_secret_ids 是允许的凭据形态");
    assert_eq!(
        payload.pointer("/plan/pipeline/nodes/0/instance/sealed_secret_ids/0"),
        Some(&serde_json::json!("sealed-1"))
    );
}

/// 可信编译 seam 的 fake:同 command_id 恒返回同一 prepared plan;
/// release 钩子记录调用供补偿断言。
#[derive(Default)]
struct FakeStartPort {
    plans: Mutex<HashMap<String, PreparedWorkflowStartPlan>>,
    releases: Mutex<Vec<String>>,
    prepare_calls: Mutex<usize>,
}

impl FakeStartPort {
    fn compile(
        &self,
        workflow: &crate::handles::WorkflowHandle,
        goal: &str,
    ) -> PreparedWorkflowStartPlan {
        let snapshot = fake_snapshot();
        PreparedWorkflowStartPlan::new(
            workflow.clone(),
            goal,
            snapshot.clone(),
            fake_content_digest(&snapshot),
            false,
        )
        .unwrap()
    }
}

impl WorkflowStartPort for FakeStartPort {
    fn prepare(
        &self,
        command_id: &CommandId,
        workflow: &crate::handles::WorkflowHandle,
        goal: &str,
    ) -> Result<PreparedWorkflowStartPlan, KernelProblem> {
        *self.prepare_calls.lock() += 1;
        if let Some(plan) = self.plans.lock().get(command_id.as_str()) {
            return Ok(plan.clone());
        }
        let plan = self.compile(workflow, goal);
        self.plans
            .lock()
            .insert(command_id.as_str().to_owned(), plan.clone());
        Ok(plan)
    }

    fn release_pre_start_resources(
        &self,
        command_id: &CommandId,
        _plan: &PreparedWorkflowStartPlan,
    ) -> Result<(), KernelProblem> {
        self.releases.lock().push(command_id.as_str().to_owned());
        Ok(())
    }
}

// ───────────────────────── fixture helpers ─────────────────────────

struct StartFixture {
    fixture: RunFixture,
    start_port: Arc<FakeStartPort>,
    lifecycle_port: Arc<FakeRunPort>,
}

impl StartFixture {
    fn new() -> Self {
        let fixture = RunFixture::new();
        let start_port = Arc::new(FakeStartPort::default());
        let lifecycle_port = Arc::new(FakeRunPort::default());
        fixture
            .kernel
            .register_run_lifecycle_port(&fixture.project, lifecycle_port.clone())
            .unwrap();
        fixture
            .kernel
            .register_workflow_start_port(&fixture.project, start_port.clone())
            .unwrap();
        Self {
            fixture,
            start_port,
            lifecycle_port,
        }
    }

    fn start_request(&self, goal: &str) -> KernelCommandRequest {
        self.start_request_with_id(CommandId::new(), goal)
    }

    fn start_request_with_id(&self, command_id: CommandId, goal: &str) -> KernelCommandRequest {
        self.fixture.request_with_id(
            command_id,
            WorkflowRunCommand::Start {
                project: self.fixture.project.clone(),
                workflow: self.fixture.workflow.clone(),
                goal: goal.into(),
                expected_semantic_revision: 1,
            },
        )
    }

    fn task_count(&self) -> i64 {
        self.fixture
            .store
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM agent_tasks", [], |row| row.get(0))
                    .map_err(Into::into)
            })
            .unwrap()
    }
}

fn accepted_handle(
    outcome: Result<KernelOutcome, KernelProblem>,
) -> crate::operation::OperationHandle {
    match outcome {
        Ok(KernelOutcome::Accepted { operation_handle }) => operation_handle,
        other => panic!("期望 Accepted,得到:{other:?}"),
    }
}

// ───────────────────────── accept 契约 ─────────────────────────

/// dispatch 立即返回稳定 202 handle:不创建 Task、不跑启动;同 command_id
/// 重试由 acceptance receipt 恢复同一 handle。
#[test]
fn start_accepts_immediately_with_stable_handle_and_no_business_writes() {
    let f = StartFixture::new();
    let before_tasks = f.task_count();
    let command_id = CommandId::new();
    let request = f.start_request_with_id(command_id.clone(), "修复登录");

    let handle = accepted_handle(f.fixture.kernel.dispatch(request.clone()));
    assert!(handle.as_str().starts_with("op_"));
    assert_eq!(
        crate::operation::operation_of(&f.fixture.service, &handle)
            .unwrap()
            .state,
        OperationState::Accepted,
        "accept 后 operation 处于 accepted,worker 尚未运行"
    );
    assert_eq!(
        f.task_count(),
        before_tasks,
        "202 返回前绝不创建 Task/Workflow Run"
    );
    assert_eq!(*f.start_port.prepare_calls.lock(), 1);

    // 同 command_id 重试:prepare 幂等,accept 从 receipt 恢复同一 handle。
    let retried = accepted_handle(f.fixture.kernel.dispatch(request));
    assert_eq!(retried, handle);
    assert_eq!(*f.start_port.prepare_calls.lock(), 2);
    assert_eq!(f.task_count(), before_tasks);
}

/// goal 参与 Operation 身份:同 command_id 换 goal → command_id_reused。
#[test]
fn start_goal_and_digest_pin_operation_identity() {
    let f = StartFixture::new();
    let command_id = CommandId::new();
    accepted_handle(
        f.fixture
            .kernel
            .dispatch(f.start_request_with_id(command_id.clone(), "修复登录")),
    );
    assert_eq!(
        f.fixture
            .kernel
            .dispatch(f.start_request_with_id(command_id, "修复支付")),
        Err(KernelProblem::CommandIdReused),
        "同 command_id 不同 goal(不同 durable plan digest)必须拒绝"
    );
}

/// acceptance target commit 后、handle 返回前崩溃:重试恢复同一 handle,
/// 不写第二条 acceptance receipt/outbox。
#[test]
fn accept_crash_after_target_commit_recovers_same_handle() {
    let f = StartFixture::new();
    let request = f.start_request("修复登录");
    let error = f
        .fixture
        .kernel
        .dispatch_workflow_run_with_accept_fault(
            request.clone(),
            Some(OperationAcceptFaultPoint::AfterTargetCommit),
        )
        .unwrap_err();
    assert!(matches!(error, KernelProblem::Internal(_)));

    let (receipts, outbox) = f
        .fixture
        .store
        .with_conn(|conn| {
            Ok((
                conn.query_row("SELECT COUNT(*) FROM command_receipt", [], |row| row.get(0))?,
                conn.query_row("SELECT COUNT(*) FROM projection_outbox", [], |row| {
                    row.get(0)
                })?,
            ))
        })
        .unwrap();
    assert_eq!((receipts, outbox), (1, 1), "fault 前 acceptance 已提交一次");

    let handle = accepted_handle(f.fixture.kernel.dispatch(request));
    let after = f
        .fixture
        .store
        .with_conn(|conn| {
            Ok((
                conn.query_row("SELECT COUNT(*) FROM command_receipt", [], |row| row.get(0))?,
                conn.query_row("SELECT COUNT(*) FROM projection_outbox", [], |row| {
                    row.get(0)
                })?,
            ))
        })
        .unwrap();
    assert_eq!(
        after,
        (1, 0),
        "重试不得写第二条 acceptance receipt;acceptance 事件行在 L-PUBLISH 内收口移除"
    );
    assert_eq!(
        crate::operation::operation_of(&f.fixture.service, &handle)
            .unwrap()
            .state,
        OperationState::Accepted
    );
}

/// Observer 等价身份不能借 Start 写:lease 复验先于一切 durable 写。
#[test]
fn observer_principal_cannot_start_workflow() {
    let f = StartFixture::new();
    let before = f.fixture.write_counts();
    let request = KernelCommandRequest::new(
        CommandId::new(),
        ClientId::parse("observer-client").unwrap(),
        f.fixture.principal.clone(),
        f.fixture.epoch,
        KernelCommand::WorkflowRun(WorkflowRunCommand::Start {
            project: f.fixture.project.clone(),
            workflow: f.fixture.workflow.clone(),
            goal: "contract".into(),
            expected_semantic_revision: 1,
        }),
    );
    assert_eq!(
        f.fixture.kernel.dispatch(request),
        Err(KernelProblem::ControllerLeaseExpired)
    );
    assert_eq!(f.fixture.write_counts(), before);
    assert_eq!(*f.start_port.prepare_calls.lock(), 0);
}

/// 无可信编译 seam:显式 fail-closed,无 intent/receipt/outbox 写。
#[test]
fn start_without_port_fails_closed_without_durable_writes() {
    let f = RunFixture::new();
    let before = f.write_counts();
    assert_eq!(
        f.dispatch(WorkflowRunCommand::Start {
            project: f.project.clone(),
            workflow: f.workflow.clone(),
            goal: "contract".into(),
            expected_semantic_revision: 1,
        }),
        Err(KernelProblem::ServiceUnavailable(
            "workflow_start_port_not_registered".into()
        ))
    );
    assert_eq!(f.write_counts(), before);
}

// ───────────────────────── worker 契约 ─────────────────────────

/// worker 执行 steps:恰创建一个 Task/Workflow Run,激活调度并投递
/// DispatchReady;重复调用幂等;Operation Snapshot 暴露最终 workflow_run。
#[test]
fn worker_creates_single_workflow_run_and_exposes_it_in_snapshot() {
    let f = StartFixture::new();
    let before_tasks = f.task_count();
    let handle = accepted_handle(
        f.fixture
            .kernel
            .dispatch(f.start_request("修复登录\n并验证")),
    );

    let outcome = f
        .fixture
        .kernel
        .run_workflow_start_operation(&f.fixture.project, &handle)
        .unwrap();
    assert_eq!(outcome, OperationOutcome::Completed { compensated: false });
    assert_eq!(f.task_count(), before_tasks + 1, "恰创建一个 Workflow Run");

    // 新 Run 处于 running、step 已就绪、DispatchReady 已投递。
    let events = f
        .fixture
        .store
        .with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT event_json FROM projection_outbox ORDER BY outbox_id")?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.contains("workflow_run.start.applied")),
        "step 投影事件必须进入 outbox"
    );
    assert!(
        f.lifecycle_port
            .deliveries
            .lock()
            .iter()
            .any(|delivery| matches!(delivery.action, mf_agent::RunAction::DispatchReady { .. })),
        "DispatchReady 必须经 RunLifecyclePort 投递"
    );

    // 同一 operation 再次驱动 worker:幂等返回终态,不重跑 steps。
    let repeated = f
        .fixture
        .kernel
        .run_workflow_start_operation(&f.fixture.project, &handle)
        .unwrap();
    assert_eq!(repeated, OperationOutcome::Completed { compensated: false });
    assert_eq!(f.task_count(), before_tasks + 1);

    // Operation Snapshot:终态 + workflow_run + 全部 forward receipts。
    let snapshot = f
        .fixture
        .kernel
        .snapshot(SnapshotQuery::Operation { operation: handle })
        .unwrap();
    let SnapshotData::Operation(data) = snapshot.data else {
        panic!("expected Operation snapshot");
    };
    assert_eq!(data.state, "completed");
    assert_eq!(data.progress.forward_succeeded, 2);
    assert!(data.progress.problem.is_none());
    let workflow_run = data
        .workflow_run
        .expect("终态 snapshot 必须暴露 workflow_run");
    let task = f
        .fixture
        .store
        .task_view_by_handle(workflow_run.as_str())
        .unwrap()
        .expect("snapshot workflow_run 必须指向真实 Run");
    assert_eq!(task.status.as_str(), "running");
    assert_eq!(task.goal, "修复登录\n并验证");
    assert_eq!(task.title, "修复登录");
    let steps = f.fixture.store.task_steps(task.id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status.as_str(), "ready");
}

/// 同 command_id 的重复 dispatch 不会让 worker 创建第二个 Task。
#[test]
fn duplicate_command_id_never_creates_second_task_or_run() {
    let f = StartFixture::new();
    let before_tasks = f.task_count();
    let command_id = CommandId::new();
    let request = f.start_request_with_id(command_id, "唯一一次启动");
    let handle = accepted_handle(f.fixture.kernel.dispatch(request.clone()));
    assert_eq!(
        accepted_handle(f.fixture.kernel.dispatch(request)),
        handle,
        "重复 dispatch 返回同一 operation handle"
    );

    f.fixture
        .kernel
        .run_workflow_start_operation(&f.fixture.project, &handle)
        .unwrap();
    assert_eq!(f.task_count(), before_tasks + 1);
    let intents: i64 = f
        .fixture
        .service
        .with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM command_intent", [], |row| row.get(0))
                .map_err(Into::into)
        })
        .unwrap();
    assert_eq!(intents, 1, "重复 dispatch 不得追加 intent");
}

/// step target 提交后、service finalize 前崩溃:重新驱动 worker 时
/// materialize 由 target receipt 跳过,绝不重复创建 Task。
#[test]
fn worker_crash_after_step_receipt_resumes_without_replaying_effect() {
    let f = StartFixture::new();
    let before_tasks = f.task_count();
    let handle = accepted_handle(f.fixture.kernel.dispatch(f.start_request("断点续跑")));

    let error = f
        .fixture
        .kernel
        .run_workflow_start_operation_with_fault(
            &f.fixture.project,
            &handle,
            Some(OperationFaultPoint::AfterStepTargetCommit(0)),
        )
        .unwrap_err();
    assert!(matches!(error, KernelProblem::Internal(_)));
    assert_eq!(
        f.task_count(),
        before_tasks + 1,
        "materialize 的业务写已随 target 事务提交"
    );

    // 恢复:step0 依 receipt 补 succeeded,不重放建 Task;step1 继续。
    let outcome = f
        .fixture
        .kernel
        .run_workflow_start_operation(&f.fixture.project, &handle)
        .unwrap();
    assert_eq!(outcome, OperationOutcome::Completed { compensated: false });
    assert_eq!(
        f.task_count(),
        before_tasks + 1,
        "resume 绝不重复创建 Task/Workflow Run"
    );
}

/// 重启 reconcile:durable Workflow Start 不被通用只读 reconcile 撤销；
/// 生产 worker 随后凭 receipt 跳过 materialize 并继续 activate。
#[test]
fn restart_reconcile_never_replays_committed_business_effect() {
    let f = StartFixture::new();
    let before_tasks = f.task_count();
    let handle = accepted_handle(f.fixture.kernel.dispatch(f.start_request("重启恢复")));

    // materialize 完整生效后崩溃(含 service finalize)。
    let _ = f
        .fixture
        .kernel
        .run_workflow_start_operation_with_fault(
            &f.fixture.project,
            &handle,
            Some(OperationFaultPoint::AfterStepFinalized(0)),
        )
        .unwrap_err();
    assert_eq!(f.task_count(), before_tasks + 1);
    let created = created_workflow_run(&f, before_tasks);

    // 模拟 Core 重启:通用 startup reconcile 保留可重建的 Start。
    let targets = registered_targets(&f.fixture);
    crate::reconcile::reconcile_startup(&f.fixture.service, &targets, chrono::Utc::now()).unwrap();

    let record = crate::operation::operation_of(&f.fixture.service, &handle).unwrap();
    assert_eq!(record.state, OperationState::Running);
    assert_eq!(
        f.task_count(),
        before_tasks + 1,
        "reconcile 只读 receipt,绝不重放/清理已提交业务效果"
    );
    // materialize 的 Draft Run 保留(供用户处置),activate 未生效。
    let task = f
        .fixture
        .store
        .task_view_by_handle(created.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(task.status.as_str(), "draft", "activate 从未执行");

    let outcome = f
        .fixture
        .kernel
        .run_workflow_start_operation(&f.fixture.project, &handle)
        .unwrap();
    assert_eq!(outcome, OperationOutcome::Completed { compensated: false });
    assert_eq!(
        f.task_count(),
        before_tasks + 1,
        "worker 不得重复 materialize"
    );
}

/// 真正的 opaque runtime 重建 + open_project + port 注册会自动扫描
/// 未终态 Start；无需测试直接调用 worker。
#[test]
fn runtime_open_project_resumes_accepted_start_in_background() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let store = Store::open(&mf_agent::project_db_path(&project_root)).unwrap();
    store
        .save_project_workflow(&ProjectWorkflowDraft {
            key: "wf-run-contract".into(),
            name: "Run contract".into(),
            nodes: vec![WorkflowNodeDraft {
                key: "work".into(),
                title: "work".into(),
                instructions: "do it".into(),
                agent_instance_id: "instance".into(),
                deps: vec![],
            }],
            allow_unsafe_parallel: false,
        })
        .unwrap();
    let workflow = crate::handles::WorkflowHandle::parse(
        store
            .load_project_workflow("wf-run-contract")
            .unwrap()
            .unwrap()
            .public_handle,
    )
    .unwrap();
    let service =
        crate::project_registry::ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
    let key = ServiceIdempotencyKey::for_test(vec![0x68; 32]).unwrap();

    // 第一进程只完成 accept；没有安装生产 worker。
    let first = Arc::new(InProcessCoreKernel::new(service.clone(), key.clone()));
    let project = first
        .register_project_store(&project_root, store.clone())
        .unwrap();
    let client = ClientId::parse("restart-client").unwrap();
    let principal = Principal::parse("restart-user").unwrap();
    let epoch = first.grant_controller(&client, &principal).unwrap();
    first
        .register_run_lifecycle_port(&project, Arc::new(FakeRunPort::default()))
        .unwrap();
    first
        .register_workflow_start_port(&project, Arc::new(FakeStartPort::default()))
        .unwrap();
    let handle = accepted_handle(first.dispatch(KernelCommandRequest::new(
        CommandId::new(),
        client,
        principal,
        epoch,
        KernelCommand::WorkflowRun(WorkflowRunCommand::Start {
            project: project.clone(),
            workflow,
            goal: "restart".into(),
            expected_semantic_revision: 1,
        }),
    )));
    assert!(
        store.list_tasks(true).unwrap().is_empty(),
        "accept 阶段不得写业务 Task"
    );
    drop(first);

    // 第二进程的 opaque runtime 自带 Weak-backed worker。open_project 的
    // 通用 reconcile 保留 resumable Start；两个 port 就绪后注册 Start
    // port 即触发 durable scan。
    let (runtime, _client) = InProcessKernelRuntime::for_test(
        service.clone(),
        key,
        ClientId::parse("restart-client").unwrap(),
        Principal::parse("restart-user").unwrap(),
    )
    .unwrap();
    let reopened = runtime.open_project(&project_root).unwrap();
    runtime
        .register_run_lifecycle_port(reopened.handle(), Arc::new(FakeRunPort::default()))
        .unwrap();
    runtime
        .register_workflow_start_port(reopened.handle(), Arc::new(FakeStartPort::default()))
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let record = crate::operation::operation_of(&service, &handle).unwrap();
        if record.state.is_terminal() || std::time::Instant::now() >= deadline {
            assert_eq!(record.state, OperationState::Completed);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        store.list_tasks(true).unwrap().len(),
        1,
        "resume 只能创建一个业务 Task"
    );
}

/// 启动前失败(CAS 前提失效):无已生效 step → 完整回滚,不创建 Run,
/// port 的 pre-start 资源(pins)被释放。
#[test]
fn pre_start_failure_compensates_and_releases_pre_start_resources() {
    let f = StartFixture::new();
    let before_tasks = f.task_count();
    let handle = accepted_handle(f.fixture.kernel.dispatch(f.start_request("冻结前提失效")));

    // accept 与 worker 之间工作流被并发编辑:materialize 的 CAS 前提失效。
    f.fixture
        .store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE project_workflows SET semantic_revision = semantic_revision + 1",
                [],
            )
            .map_err(Into::into)
        })
        .unwrap();

    let outcome = f
        .fixture
        .kernel
        .run_workflow_start_operation(&f.fixture.project, &handle)
        .unwrap();
    assert_eq!(
        outcome,
        OperationOutcome::Completed { compensated: true },
        "调度开始前的失败必须完整回滚,不伪造成功"
    );
    assert_eq!(f.task_count(), before_tasks, "不得创建半途 Run");
    assert_eq!(
        f.start_port.releases.lock().len(),
        1,
        "完整回滚后必须释放 pre-start 资源(pins)"
    );
}

/// materialize 已生效、activate 未执行时:kernel 编译的 discard 补偿
/// effect 能且只能删除尚无 Agent Run 的 Draft Task。
#[test]
fn discard_compensation_removes_draft_task_before_scheduling() {
    let f = StartFixture::new();
    let before_tasks = f.task_count();
    let command_id = CommandId::new();
    let request = f.start_request_with_id(command_id.clone(), "补偿清理");
    let handle = accepted_handle(f.fixture.kernel.dispatch(request));

    // materialize 完整生效(Draft Task + target receipt),activate 未执行。
    let _ = f
        .fixture
        .kernel
        .run_workflow_start_operation_with_fault(
            &f.fixture.project,
            &handle,
            Some(OperationFaultPoint::AfterStepFinalized(0)),
        )
        .unwrap_err();
    assert_eq!(f.task_count(), before_tasks + 1);

    // 直接执行 kernel 编译的真实 discard effect(从事务内 receipt 读 handle)。
    let payload = crate::operation::durable_payload(&f.fixture.service, &handle)
        .unwrap()
        .expect("durable payload 必须存在");
    let steps = crate::operation::steps_of(&f.fixture.service, &handle).unwrap();
    assert_eq!(steps[0].state, crate::operation::StepState::Succeeded);
    let (_, prepared) = crate::workflow_start::rebuild_workflow_start_plan(
        &command_id,
        &f.fixture.project,
        &payload,
        &steps,
    )
    .unwrap();
    let mut effects = crate::workflow_start::workflow_start_effects(&prepared, &steps).unwrap();
    let output = f
        .fixture
        .store
        .with_tx(|tx| (effects.pop().unwrap())(tx).map_err(anyhow::Error::new))
        .unwrap();
    assert_eq!(
        output.result_revisions.get("discarded"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        f.task_count(),
        before_tasks,
        "调度未开始的 Draft Task 必须被补偿删除"
    );
}

/// 调度开始后(DispatchReady 已提交)投递失败:Run 保留,snapshot 阻断,
/// 重投后恢复——绝不回滚已调度的 Run。
#[test]
fn post_scheduling_delivery_failure_keeps_run_and_recovers_on_redrive() {
    let f = StartFixture::new();
    let before_tasks = f.task_count();
    let handle = accepted_handle(f.fixture.kernel.dispatch(f.start_request("投递失败保留")));

    f.lifecycle_port.fail_next();
    let error = f
        .fixture
        .kernel
        .run_workflow_start_operation(&f.fixture.project, &handle)
        .unwrap_err();
    assert_eq!(
        error,
        KernelProblem::ServiceUnavailable("fake_action_failure".into())
    );
    assert_eq!(f.task_count(), before_tasks + 1, "Run 已创建,不得回滚");
    let created = created_workflow_run(&f, before_tasks);
    let task = f
        .fixture
        .store
        .task_view_by_handle(created.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(task.status.as_str(), "running", "调度已开始,Run 保留");

    // 未投递 run actions 阻断 snapshot。
    assert_eq!(
        f.fixture.kernel.snapshot(SnapshotQuery::Workspace),
        Err(KernelProblem::ResyncRequired)
    );

    // 重驱 worker(幂等终态 + run-action 收口)后恢复。
    let outcome = f
        .fixture
        .kernel
        .run_workflow_start_operation(&f.fixture.project, &handle)
        .unwrap();
    assert_eq!(outcome, OperationOutcome::Completed { compensated: false });
    assert!(matches!(
        f.fixture.kernel.snapshot(SnapshotQuery::Workspace),
        Ok(_)
    ));
    assert!(
        f.start_port.releases.lock().is_empty(),
        "调度已开始,不得释放 pre-start 资源"
    );
}

// ───────────────────────── 观测 helpers ─────────────────────────

/// 全部已注册 Project 的 target(与注册时同一 store 实例)。
fn registered_targets(fixture: &RunFixture) -> Vec<crate::command::TargetDatabase> {
    fixture
        .kernel
        .run_control_projects()
        .into_iter()
        .map(|entry| {
            crate::command::TargetDatabase::project(entry.project.as_str(), entry.store.clone())
                .unwrap()
        })
        .collect()
}

fn created_workflow_run(f: &StartFixture, before_tasks: i64) -> WorkflowRunHandle {
    let handle: String = f
        .fixture
        .store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT public_handle FROM agent_tasks ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
        })
        .unwrap();
    assert!(f.task_count() > before_tasks, "Run 必须已创建");
    WorkflowRunHandle::parse(handle).unwrap()
}
