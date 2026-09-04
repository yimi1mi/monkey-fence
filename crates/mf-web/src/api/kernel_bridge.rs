//! wire → CoreKernel 翻译层(T7c,Issue #41;spec §7.1/§7.4)。
//!
//! 所有写入只调用 `CoreKernel::dispatch`:web 命令 envelope 翻译为
//! kernel `KernelCommandRequest`(opaque handle 全程复验),响应翻译
//! 回 `applied/accepted`。kernel 尚未接管的命令族(session/catalog/
//! cli/root)返回明确 `invalid_envelope`(fail-closed,不旁路)。

use mf_kernel::handles::CommandId;
use mf_kernel::handles::{ClientId, Principal};
use mf_kernel::kernel::{
    CoreKernel, KernelCommand, KernelCommandRequest, KernelOutcome, KernelProblem,
    ProjectWorkflowCommand, WorkflowRenameCommand, WorkflowRunCommand,
};
use mf_kernel::projection::SnapshotEnvelope as KernelSnapshot;

use super::commands::{AggregateRef, CommandEnvelope, CommandOutcomeWire, ExpectedRevision};
use crate::problem::{Problem, ProblemCode, Retry};

/// 翻译问题(web 层错误)。
#[derive(Debug, thiserror::Error)]
#[error("{code:?}:{message}")]
pub struct TranslateError {
    pub code: ProblemCode,
    pub message: String,
}

impl TranslateError {
    fn new(code: ProblemCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn kernel_problem_to_problem(problem: KernelProblem) -> Problem {
    let code = match &problem {
        KernelProblem::ResourceNotFound => ProblemCode::ResourceNotFound,
        KernelProblem::InvalidEnvelope(_) => ProblemCode::InvalidEnvelope,
        KernelProblem::RevisionConflict => ProblemCode::RevisionConflict,
        KernelProblem::ValidationFailed(_) => ProblemCode::ValidationFailed,
        KernelProblem::WorkflowCycle(_) => ProblemCode::WorkflowCycle,
        KernelProblem::UnknownDependency(_) => ProblemCode::UnknownDependency,
        KernelProblem::CommandIdReused => ProblemCode::CommandIdReused,
        KernelProblem::CommandInProgress => ProblemCode::CommandInProgress,
        KernelProblem::ControllerLeaseExpired => ProblemCode::ControllerLeaseExpired,
        KernelProblem::RootEpochExpired => ProblemCode::RootEpochExpired,
        KernelProblem::ResyncRequired => ProblemCode::ResyncRequired,
        KernelProblem::ServiceUnavailable(_) => ProblemCode::ServiceUnavailable,
        KernelProblem::Internal(_) => ProblemCode::InternalError,
    };
    let retry = match code {
        ProblemCode::RevisionConflict | ProblemCode::ValidationFailed => Retry::AfterResync,
        ProblemCode::CommandIdReused => Retry::Never,
        ProblemCode::CommandInProgress => Retry::SameCommandId,
        ProblemCode::ControllerLeaseExpired | ProblemCode::RootEpochExpired => Retry::AfterReauth,
        ProblemCode::ResyncRequired => Retry::AfterResync,
        _ => Retry::Never,
    };
    Problem::new(code, problem.to_string(), Some(retry))
}

fn u64_of(value: &str, field: &str) -> Result<u64, TranslateError> {
    value.parse().map_err(|_| {
        TranslateError::new(
            ProblemCode::InvalidEnvelope,
            format!("{field} 必须是 u64 字符串"),
        )
    })
}

fn revision_of(
    expected: &[ExpectedRevision],
    aggregate_kind: &str,
    axis: &str,
) -> Result<u64, TranslateError> {
    // 找到目标 aggregate 的 expected 条目并取指定轴(缺失 → 0 不可能;
    // CAS 语义要求显式)
    for entry in expected {
        if entry.aggregate.kind == aggregate_kind {
            let value = match axis {
                "semantic" => entry.semantic_revision.as_deref(),
                "presentation" => entry.presentation_revision.as_deref(),
                _ => None,
            };
            if let Some(value) = value {
                return u64_of(value, "expected revision");
            }
        }
    }
    Err(TranslateError::new(
        ProblemCode::RevisionConflict,
        format!("expected 缺少 {aggregate_kind} 的 {axis}_revision"),
    ))
}

/// wire handle(`wf_/run_/proj_...` 前缀风格,§7.1)→ 存储裸 UUIDv7。
/// 内部 handle 一律裸 UUID;前缀只是 wire 表示层。
fn strip_wire_prefix(handle: &str) -> &str {
    for prefix in ["wf_", "run_", "step_", "sess_", "inst_", "op_", "proj_"] {
        if let Some(stripped) = handle.strip_prefix(prefix) {
            return stripped;
        }
    }
    handle
}

fn project_handle_of(
    handle: &str,
) -> Result<mf_kernel::handles::ProjectStoreHandle, TranslateError> {
    // ProjectStoreHandle 的存储形态即 `proj_<uuid>`(registry 生成);
    // wire 直接透传,不 strip。
    mf_kernel::handles::ProjectStoreHandle::parse(handle).map_err(|_| {
        TranslateError::new(
            ProblemCode::ResourceNotFound,
            "project handle 非法(opaque 复验失败)",
        )
    })
}

fn workflow_handle_of(handle: &str) -> Result<mf_kernel::handles::WorkflowHandle, TranslateError> {
    mf_kernel::handles::WorkflowHandle::parse(strip_wire_prefix(handle))
        .map_err(|e| TranslateError::new(ProblemCode::ResourceNotFound, e.to_string()))
}

fn run_handle_of(handle: &str) -> Result<mf_kernel::handles::WorkflowRunHandle, TranslateError> {
    mf_kernel::handles::WorkflowRunHandle::parse(strip_wire_prefix(handle))
        .map_err(|e| TranslateError::new(ProblemCode::ResourceNotFound, e.to_string()))
}

fn step_handle_of(handle: &str) -> Result<mf_kernel::handles::StepHandle, TranslateError> {
    mf_kernel::handles::StepHandle::parse(strip_wire_prefix(handle))
        .map_err(|e| TranslateError::new(ProblemCode::ResourceNotFound, e.to_string()))
}

fn agent_run_handle_of(handle: &str) -> Result<mf_kernel::handles::AgentRunHandle, TranslateError> {
    mf_kernel::handles::AgentRunHandle::parse(strip_wire_prefix(handle))
        .map_err(|e| TranslateError::new(ProblemCode::ResourceNotFound, e.to_string()))
}

/// settlement 负载:{"kind":"complete","summary":…,"output":…} 或
/// {"kind":"fail","reason":…}(与 mf_agent::Settlement 的 serde 形态一致)。
fn settlement_of(payload: &serde_json::Value) -> Result<mf_agent::Settlement, TranslateError> {
    let settlement = payload.get("settlement").ok_or_else(|| {
        TranslateError::new(ProblemCode::InvalidEnvelope, "payload 缺少 settlement 字段")
    })?;
    let kind = settlement
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let normalized = match kind {
        "complete" | "Complete" => serde_json::json!({
            "Complete": {
                "summary": settlement.get("summary").cloned().unwrap_or_default(),
                "output": settlement.get("output").cloned().unwrap_or_default(),
            }
        }),
        "fail" | "Fail" => serde_json::json!({
            "Fail": {
                "reason": settlement.get("reason").cloned().unwrap_or_default(),
            }
        }),
        other => {
            return Err(TranslateError::new(
                ProblemCode::InvalidEnvelope,
                format!("settlement.kind 非法:{other}(complete|fail)"),
            ))
        }
    };
    serde_json::from_value(normalized)
        .map_err(|e| TranslateError::new(ProblemCode::InvalidEnvelope, e.to_string()))
}

fn payload_u64(payload: &serde_json::Value, field: &str) -> Result<u64, TranslateError> {
    let text = match payload.get(field) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => {
            return Err(TranslateError::new(
                ProblemCode::InvalidEnvelope,
                format!("payload 缺少 u64 字段 {field}"),
            ))
        }
    };
    u64_of(&text, field)
}

fn payload_str<'a>(payload: &'a serde_json::Value, field: &str) -> Result<&'a str, TranslateError> {
    payload.get(field).and_then(|v| v.as_str()).ok_or_else(|| {
        TranslateError::new(
            ProblemCode::InvalidEnvelope,
            format!("payload 缺少字符串字段 {field}"),
        )
    })
}

/// wire 命令 → kernel 命令(kernel 未接管的族明确拒绝)。
pub fn translate_command(command: &CommandEnvelope) -> Result<KernelCommand, TranslateError> {
    use super::commands::CommandType as Wire;
    let payload = &command.payload;
    Ok(match command.command_type {
        Wire::WorkflowRename => KernelCommand::workflow_rename(
            project_handle_of(&command.target.handle)?,
            workflow_handle_of(payload_str(payload, "workflow_handle")?)?,
            payload_str(payload, "name")?,
            revision_of(&command.expected, "project_workflow", "presentation")?,
        ),
        Wire::WorkflowCreate => KernelCommand::ProjectWorkflow(ProjectWorkflowCommand::Create {
            project: project_handle_of(&command.target.handle)?,
            draft: serde_json::from_value(payload.get("draft").cloned().unwrap_or_default())
                .map_err(|e| TranslateError::new(ProblemCode::InvalidEnvelope, e.to_string()))?,
            expected_collection_revision: payload_u64(payload, "expected_collection_revision")?,
        }),
        Wire::WorkflowDelete => KernelCommand::ProjectWorkflow(ProjectWorkflowCommand::Delete {
            project: project_handle_of(&command.target.handle)?,
            workflow: workflow_handle_of(payload_str(payload, "workflow_handle")?)?,
            expected_collection_revision: payload_u64(payload, "expected_collection_revision")?,
            expected_semantic_revision: payload_u64(payload, "expected_semantic_revision")?,
            expected_presentation_revision: payload_u64(payload, "expected_presentation_revision")?,
        }),
        Wire::WorkflowMoveNode => {
            KernelCommand::ProjectWorkflow(ProjectWorkflowCommand::MoveNode {
                project: project_handle_of(&command.target.handle)?,
                workflow: workflow_handle_of(payload_str(payload, "workflow_handle")?)?,
                node_handle: payload_str(payload, "node_handle")?.to_string(),
                x: payload.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
                y: payload.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
                expected_presentation_revision: revision_of(
                    &command.expected,
                    "project_workflow",
                    "presentation",
                )?,
            })
        }
        Wire::WorkflowRunStart => KernelCommand::WorkflowRun(WorkflowRunCommand::Start {
            project: project_handle_of(&command.target.handle)?,
            workflow: workflow_handle_of(payload_str(payload, "workflow_handle")?)?,
            goal: payload_str(payload, "goal")?.to_string(),
            expected_semantic_revision: revision_of(
                &command.expected,
                "project_workflow",
                "semantic",
            )?,
        }),
        // run 命令族契约:target = workflow run handle;project 经
        // payload.project_handle(opaque proj_ 形态)——target 不可能同时
        // 是 project 与 run,原实现两者都取 target 是矛盾的。
        Wire::WorkflowRunCancel => KernelCommand::WorkflowRun(WorkflowRunCommand::Cancel {
            project: project_handle_of(payload_str(payload, "project_handle")?)?,
            workflow_run: run_handle_of(&command.target.handle)?,
            expected: workflow_expected(&command.expected)?,
        }),
        Wire::WorkflowRunRetryStep => KernelCommand::WorkflowRun(WorkflowRunCommand::RetryStep {
            project: project_handle_of(payload_str(payload, "project_handle")?)?,
            workflow_run: run_handle_of(&command.target.handle)?,
            step: step_handle_of(payload_str(payload, "step_handle")?)?,
            mode: match payload_str(payload, "mode")? {
                "continue_session" => mf_agent::RetryMode::ContinueSession,
                _ => mf_agent::RetryMode::FreshSession,
            },
            expected: workflow_expected(&command.expected)?,
        }),
        // 注:SkipStep 不在 v1 冻结命令族(wire 只含 start/cancel/
        // retry_step/respond/settle);如需跳过须 wire v2 或 additive 流程。
        // question_id 哨兵 0:Project Store rowid 有意不出 wire(§
        // OpenQuestionSnapshot);内核按 step 解析唯一 open question——
        // 与 Respond 事务内"恰有一个 open question"的既有校验一致。
        Wire::WorkflowRunRespond => KernelCommand::WorkflowRun(WorkflowRunCommand::Respond {
            project: project_handle_of(payload_str(payload, "project_handle")?)?,
            workflow_run: run_handle_of(&command.target.handle)?,
            step: step_handle_of(payload_str(payload, "step_handle")?)?,
            question_id: 0,
            answer: payload_str(payload, "answer")?.to_string(),
            expected: workflow_expected(&command.expected)?,
        }),
        Wire::WorkflowRunSettle => KernelCommand::WorkflowRun(WorkflowRunCommand::Settle {
            project: project_handle_of(payload_str(payload, "project_handle")?)?,
            workflow_run: run_handle_of(&command.target.handle)?,
            step: step_handle_of(payload_str(payload, "step_handle")?)?,
            agent_run: agent_run_handle_of(payload_str(payload, "agent_run_handle")?)?,
            settlement: settlement_of(payload)?,
            expected: workflow_expected(&command.expected)?,
        }),
        // kernel 尚未接管的命令族:fail-closed 明确拒绝(不旁路直写)
        other => {
            return Err(TranslateError::new(
                ProblemCode::InvalidEnvelope,
                format!(
                    "命令族 {} 尚未由 CoreKernel 接管(渐进迁移;拒绝旁路)",
                    other.as_str()
                ),
            ))
        }
    })
}

fn workflow_expected(
    expected: &[ExpectedRevision],
) -> Result<mf_kernel::kernel::WorkflowRunExpected, TranslateError> {
    for entry in expected {
        if entry.aggregate.kind == "workflow_run" {
            let revision = u64_of(
                entry.semantic_revision.as_deref().ok_or_else(|| {
                    TranslateError::new(
                        ProblemCode::RevisionConflict,
                        "expected 缺少 workflow_run semantic_revision",
                    )
                })?,
                "workflow_run_revision",
            )?;
            // run 级命令(RetryStep/Respond/Settle)要求 expected 携带
            // 目标 Step 的语义 revision(kernel L-CMD 复验)。
            let steps = expected
                .iter()
                .filter(|entry| entry.aggregate.kind == "workflow_step")
                .map(|entry| {
                    let handle = mf_kernel::handles::StepHandle::parse(strip_wire_prefix(
                        &entry.aggregate.handle,
                    ))
                    .map_err(|e| {
                        TranslateError::new(ProblemCode::ResourceNotFound, e.to_string())
                    })?;
                    let revision = u64_of(
                        entry.semantic_revision.as_deref().ok_or_else(|| {
                            TranslateError::new(
                                ProblemCode::RevisionConflict,
                                "expected 缺少 workflow_step semantic_revision",
                            )
                        })?,
                        "workflow_step_revision",
                    )?;
                    Ok(mf_kernel::kernel::VersionedHandle { handle, revision })
                })
                .collect::<Result<Vec<_>, TranslateError>>()?;
            let mut result = mf_kernel::kernel::WorkflowRunExpected::only_run(revision);
            result.steps = steps;
            return Ok(result);
        }
    }
    Err(TranslateError::new(
        ProblemCode::RevisionConflict,
        "expected 缺少 workflow_run 条目",
    ))
}

/// dispatch 全链:翻译 → kernel dispatch → wire 响应。
pub fn dispatch_via_kernel(
    kernel: &dyn CoreKernel,
    command: &CommandEnvelope,
    principal: &str,
) -> Result<CommandOutcomeWire, Problem> {
    command
        .validate()
        .map_err(|code| Problem::new(code, "envelope 校验失败", Some(Retry::Never)))?;
    let kernel_command = translate_command(command)
        .map_err(|e| Problem::new(e.code, e.message, Some(Retry::Never)))?;
    let request = KernelCommandRequest::new(
        CommandId::parse(&command.command_id).map_err(|e| {
            Problem::new(
                ProblemCode::InvalidEnvelope,
                e.to_string(),
                Some(Retry::Never),
            )
        })?,
        ClientId::parse(&command.client_id).map_err(|e| {
            Problem::new(
                ProblemCode::InvalidEnvelope,
                e.to_string(),
                Some(Retry::Never),
            )
        })?,
        Principal::parse(principal).map_err(|e| {
            Problem::new(
                ProblemCode::InvalidEnvelope,
                e.to_string(),
                Some(Retry::Never),
            )
        })?,
        command.controller_lease_epoch,
        kernel_command,
    );
    let outcome = kernel
        .dispatch(request)
        .map_err(kernel_problem_to_problem)?;
    Ok(match outcome {
        KernelOutcome::Applied {
            revisions,
            replayed,
        } => CommandOutcomeWire::Applied {
            revisions: vec![ExpectedRevision {
                aggregate: AggregateRef {
                    kind: "project_workflow".into(),
                    handle: command.target.handle.clone(),
                },
                semantic_revision: Some(revisions.semantic_revision.to_string()),
                presentation_revision: Some(revisions.presentation_revision.to_string()),
            }],
            replayed,
        },
        KernelOutcome::RunApplied { revision, replayed } => CommandOutcomeWire::Applied {
            revisions: vec![ExpectedRevision {
                aggregate: AggregateRef {
                    kind: "workflow_run".into(),
                    handle: command.target.handle.clone(),
                },
                semantic_revision: Some(revision.to_string()),
                presentation_revision: None,
            }],
            replayed,
        },
        KernelOutcome::Accepted { operation_handle } => CommandOutcomeWire::Accepted {
            operation_handle: operation_handle.as_str().to_string(),
        },
    })
}

/// Snapshot 投射:kernel envelope(内部 u64 数字)→ wire envelope
/// (字符串化 u64;data 为去标签的负载对象——Rust 枚举 tag 不进 wire,
/// 与前端 `data: Record<string, unknown>` 契约一致)。
pub fn snapshot_to_wire(snapshot: KernelSnapshot) -> super::snapshot::SnapshotEnvelope {
    use mf_kernel::projection::SnapshotData;
    let data = match snapshot.data {
        SnapshotData::Workspace(inner) => serde_json::to_value(inner),
        SnapshotData::Workflow(inner) => serde_json::to_value(inner),
        SnapshotData::WorkflowRun(inner) => serde_json::to_value(inner),
        SnapshotData::Operation(inner) => serde_json::to_value(inner),
    }
    .unwrap_or_default();
    super::snapshot::SnapshotEnvelope {
        schema: "mf.snapshot.v1".to_string(),
        server_instance_id: snapshot.server_instance_id.as_str().to_string(),
        cursor: super::snapshot::SnapshotCursor {
            stream_epoch: snapshot.cursor.stream_epoch.as_str().to_string(),
            through_seq: snapshot.cursor.through_seq,
        },
        data,
    }
}
