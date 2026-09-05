//! T7c 契约(Issue #41):CoreKernel 三接口经 web transport 投射的
//! 端到端行为。使用真 `InProcessCoreKernel`(装配模式与 mf-kernel
//! 契约一致):dispatch 只经 kernel、snapshot 投射字符串化 u64、
//! events resume/poll 与慢客户端隔离、命令速率限制。

use mf_agent::{ProjectWorkflowDraft, Store, WorkflowNodeDraft};
use mf_kernel::handles::{ClientId, Principal, ProjectStoreHandle};
use mf_kernel::kernel::{
    CoreKernel, InProcessCoreKernel, InProcessKernelRuntime, ProjectWorkflowCommand,
};
use mf_kernel::project_registry::ServiceStore;
use mf_web::api::commands::{AggregateRef, CommandEnvelope, CommandType, ExpectedRevision};
use mf_web::api::kernel_bridge::{dispatch_via_kernel, snapshot_to_wire};
use mf_web::problem::ProblemCode;
use mf_web::ws::events::{CommandRateLimiter, EventsControl, EventsSession};
use std::sync::Arc;

struct Fixture {
    _tmp: tempfile::TempDir,
    kernel: Arc<InProcessCoreKernel>,
    client_id: ClientId,
    epoch: u64,
    project: ProjectStoreHandle,
    workflow: String,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let service = ServiceStore::open(&tmp.path().join("service-v1.db")).unwrap();
        let (runtime, client) = InProcessKernelRuntime::for_test(
            service,
            mf_kernel::command::ServiceIdempotencyKey::for_test(vec![0x2d; 32]).unwrap(),
            ClientId::parse("web-client").unwrap(),
            Principal::parse("web-user").unwrap(),
        )
        .unwrap();
        let kernel = runtime.kernel().clone();
        // 项目 + 一条工作流(presentation revision 从 1 起):先建库写
        // 工作流,再经 runtime 注册(kernel test-support 路径)
        let store = Store::open(&mf_agent::project_db_path(tmp.path())).unwrap();
        let record = store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: "wf-web".into(),
                name: "Web".into(),
                nodes: vec![WorkflowNodeDraft {
                    key: "node-a".into(),
                    title: "节点 A".into(),
                    instructions: "做事".into(),
                    agent_instance_id: "instance".into(),
                    deps: vec![],
                }],
                allow_unsafe_parallel: false,
            })
            .unwrap();
        let record_handle = record.public_handle.clone();
        let registered = runtime.open_project(tmp.path()).unwrap();
        let project = registered.handle().clone();
        let workflow = record_handle;
        let _ = client;
        Self {
            _tmp: tmp,
            kernel,
            client_id: ClientId::parse("web-client").unwrap(),
            epoch: 1,
            project,
            workflow,
        }
    }

    /// wire 形态(wf_ 前缀;bridge 负责去前缀映射到存储裸 UUID)。
    fn workflow_handle(&self) -> String {
        format!("wf_{}", self.workflow)
    }

    fn command(
        &self,
        command_type: CommandType,
        payload: serde_json::Value,
        expected: Vec<ExpectedRevision>,
    ) -> CommandEnvelope {
        let mut envelope = CommandEnvelope::new(
            &uuid::Uuid::now_v7().to_string(),
            self.client_id.as_str(),
            self.epoch,
            AggregateRef {
                kind: "project".into(),
                handle: format!("proj_{}", self.project.as_str().trim_start_matches("proj_")),
            },
            command_type,
            payload,
        );
        envelope.expected = expected;
        envelope
    }
}

#[test]
fn snapshot_projection_stringifies_u64_and_keeps_cursor() {
    let fixture = Fixture::new();
    let kernel_snapshot = fixture
        .kernel
        .snapshot(mf_kernel::projection::SnapshotQuery::Workspace)
        .unwrap();
    let wire = snapshot_to_wire(kernel_snapshot);
    assert_eq!(wire.schema, "mf.snapshot.v1");
    assert!(!wire.server_instance_id.is_empty());
    let cursor = wire.cursor.through_seq.to_string();
    assert_eq!(cursor.parse::<u64>().unwrap(), wire.cursor.through_seq);
    assert!(!wire.cursor.stream_epoch.is_empty());
    assert!(!wire.data.is_null(), "workspace 数据非空");
}

#[test]
fn rename_dispatches_through_kernel_and_cas_conflict_maps() {
    let fixture = Fixture::new();
    let workflow = fixture.workflow_handle();
    // 正确 presentation CAS(首版 revision = 1)
    let mut command = fixture.command(
        CommandType::WorkflowRename,
        serde_json::json!({
            "workflow_handle": workflow,
            "name": "改名后的工作流"
        }),
        vec![ExpectedRevision {
            aggregate: AggregateRef {
                kind: "project_workflow".into(),
                handle: workflow.clone(),
            },
            semantic_revision: None,
            presentation_revision: Some("1".into()),
        }],
    );
    command.target = AggregateRef {
        kind: "project".into(),
        handle: fixture.project.as_str().to_string(),
    };
    let outcome = dispatch_via_kernel(&*fixture.kernel, &command, "web-user").unwrap();
    match outcome {
        mf_web::api::commands::CommandOutcomeWire::Applied {
            revisions,
            replayed,
        } => {
            assert!(!replayed);
            assert_eq!(revisions[0].presentation_revision.as_deref(), Some("2"));
        }
        other => panic!("期望 applied:{other:?}"),
    }
    // 同 command_id + 同 canonical digest → 幂等重放返回原结果(§7.4)
    let replay = dispatch_via_kernel(&*fixture.kernel, &command, "web-user").unwrap();
    match replay {
        mf_web::api::commands::CommandOutcomeWire::Applied { replayed, .. } => {
            assert!(replayed, "同 command_id 幂等重放必须标记 replayed");
        }
        other => panic!("期望幂等 applied:{other:?}"),
    }
}

#[test]
fn non_opaque_target_fails_closed_404() {
    let fixture = Fixture::new();
    let mut command = fixture.command(
        CommandType::WorkflowRename,
        serde_json::json!({"workflow_handle": "wf_x", "name": "x"}),
        vec![],
    );
    command.target = AggregateRef {
        kind: "project".into(),
        handle: "../escape".into(),
    };
    let problem = dispatch_via_kernel(&*fixture.kernel, &command, "web-user").unwrap_err();
    assert_eq!(problem.code, ProblemCode::ResourceNotFound);
    assert_eq!(problem.code.http_status(), 404);
}

#[test]
fn unadopted_command_families_fail_closed() {
    let fixture = Fixture::new();
    // session/catalog/cli/root 族尚未由 kernel 接管 → invalid_envelope
    for command_type in [
        CommandType::SessionStartPreview,
        CommandType::CliInstall,
        CommandType::RootEnable,
    ] {
        let command = fixture.command(command_type, serde_json::json!({}), vec![]);
        let problem = dispatch_via_kernel(&*fixture.kernel, &command, "web-user").unwrap_err();
        assert_eq!(
            problem.code,
            ProblemCode::InvalidEnvelope,
            "{} 必须明确拒绝",
            command_type.as_str()
        );
    }
}

#[test]
fn events_resume_poll_and_slow_client_isolation() {
    let fixture = Fixture::new();
    // 触发一次写事件(rename)
    let workflow = fixture.workflow_handle();
    let command = fixture.command(
        CommandType::WorkflowRename,
        serde_json::json!({"workflow_handle": workflow, "name": "事件源"}),
        vec![ExpectedRevision {
            aggregate: AggregateRef {
                kind: "project_workflow".into(),
                handle: workflow,
            },
            semantic_revision: None,
            presentation_revision: Some("1".into()),
        }],
    );
    dispatch_via_kernel(&*fixture.kernel, &command, "web-user").unwrap();

    // 两个 client 各自 resume(同 cursor)——队列按 client 隔离。
    // 先取 cursor(在 rename 之前),rename 产生事件后 resume 该 cursor。
    let (epoch, through_seq, hello_epoch) = {
        let snapshot = fixture
            .kernel
            .snapshot(mf_kernel::projection::SnapshotQuery::Workspace)
            .unwrap();
        (
            snapshot.cursor.stream_epoch.as_str().to_string(),
            // 从头恢复(rename 事件已在 journal 中;after_seq=0 全量回放)
            0,
            snapshot.cursor.stream_epoch.as_str().to_string(),
        )
    };
    let _ = hello_epoch;
    let control = EventsControl::Resume {
        stream_epoch: epoch,
        through_seq: through_seq.to_string(),
    };
    let mut slow = EventsSession::resume(&*fixture.kernel, &control).unwrap();
    let mut fast = EventsSession::resume(&*fixture.kernel, &control).unwrap();
    // hello:字符串化 seq + 当前 epoch
    assert_eq!(slow.hello().stream_epoch, hello_epoch);
    assert!(slow.hello().last_seq.parse::<u64>().is_ok());
    // 两边都收到事件;互不影响
    let slow_events = match slow.poll() {
        mf_web::ws::events::PollOutcome::Events(events) => events,
        other => panic!("期望事件:{other:?}"),
    };
    assert!(!slow_events.is_empty());
    match fast.poll() {
        mf_web::ws::events::PollOutcome::Events(events) => assert!(!events.is_empty()),
        other => panic!("fast client 不受 slow 影响:{other:?}"),
    }
}

#[test]
fn command_rate_limit_throttles_at_burst() {
    let mut limiter = CommandRateLimiter::new(40);
    for _ in 0..120 {
        limiter.allow().expect("burst 120 内放行");
    }
    let problem = limiter.allow().unwrap_err();
    assert_eq!(problem.code, ProblemCode::RateLimited);
    assert_eq!(problem.code.http_status(), 429);
}

/// 直接 kernel 层对照:web 翻译的 move_node 与 kernel 原生命令等价。
#[test]
fn translated_move_node_matches_kernel_native_semantics() {
    let fixture = Fixture::new();
    let workflow = fixture.workflow_handle();
    // 经 web 层翻译
    let command = fixture.command(
        CommandType::WorkflowMoveNode,
        serde_json::json!({
            "project_handle": format!("proj_{}", fixture.project.as_str().trim_start_matches("proj_")),
            "workflow_handle": workflow,
            "node_handle": "node-a",
            "x": 420.0,
            "y": 180.0
        }),
        vec![ExpectedRevision {
            aggregate: AggregateRef {
                kind: "project_workflow".into(),
                handle: workflow.clone(),
            },
            semantic_revision: None,
            presentation_revision: Some("1".into()),
        }],
    );
    // node-a 不存在 → 验证失败(422 语义)但路径完整经过 kernel
    let problem = dispatch_via_kernel(&*fixture.kernel, &command, "web-user").unwrap_err();
    assert!(
        matches!(
            problem.code,
            ProblemCode::ValidationFailed | ProblemCode::ResourceNotFound
        ),
        "未知节点应稳定失败:{problem:?}"
    );
    // 对照:kernel 原生命令同输入同样失败(翻译保真)
    let native = fixture
        .kernel
        .dispatch(mf_kernel::kernel::KernelCommandRequest::new(
            mf_kernel::handles::CommandId::parse(uuid::Uuid::now_v7().to_string()).unwrap(),
            fixture.client_id.clone(),
            Principal::parse("web-user").unwrap(),
            fixture.epoch,
            mf_kernel::kernel::KernelCommand::ProjectWorkflow(ProjectWorkflowCommand::MoveNode {
                project: fixture.project.clone(),
                workflow: mf_kernel::handles::WorkflowHandle::parse(
                    workflow.trim_start_matches("wf_"),
                )
                .unwrap(),
                node_handle: "node-a".into(),
                x: 420.0,
                y: 180.0,
                expected_presentation_revision: 1,
            }),
        ));
    assert!(native.is_err(), "kernel 原生命令同输入应同样失败");
}
