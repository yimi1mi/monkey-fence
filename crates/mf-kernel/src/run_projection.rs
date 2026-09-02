//! Workflow Run 的 Store 权威只读投影。
//!
//! 本模块只组装 Project Store 已持久化事实，不缓存、不推演状态，
//! 也不暴露 rowid、capability token 或终端内容。

use crate::handles::{AgentRunHandle, AgentSessionHandle, StepHandle, WorkflowRunHandle};
use crate::projection::{
    AgentRunSnapshot, AgentSessionSnapshot, ExecutionLeaseSnapshot, HandoffSnapshot,
    NeedsYouReasonSnapshot, OpenQuestionSnapshot, PendingMergeSnapshot, PipelineRevisionSnapshot,
    ScalarRevision, WorkflowRunSnapshotData, WorkflowRunStepSnapshot,
};
use mf_agent::Store;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn read_workflow_run(
    store: &Store,
    workflow_run: &WorkflowRunHandle,
) -> anyhow::Result<Option<WorkflowRunSnapshotData>> {
    let Some(source) =
        store.with_tx(|tx| Store::workflow_run_projection_source_tx(tx, workflow_run.as_str()))?
    else {
        return Ok(None);
    };
    let task = source.task;
    let active_revision = source.active_revision;
    let raw_steps = source.steps;
    let active_step_by_key = raw_steps
        .iter()
        .map(|step| (step.step_key.clone(), step.public_handle.clone()))
        .collect::<BTreeMap<_, _>>();
    let step_handles = raw_steps
        .iter()
        .map(|step| StepHandle::parse(step.public_handle.clone()).map(|handle| (step.id, handle)))
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let steps = raw_steps
        .iter()
        .map(|step| {
            let dependencies = step
                .deps
                .iter()
                .filter_map(|id| step_handles.get(id).cloned())
                .collect();
            Ok(WorkflowRunStepSnapshot {
                step: step_handles
                    .get(&step.id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Step handle 映射缺失"))?,
                revision: ScalarRevision {
                    revision: u64::try_from(step.revision)?,
                },
                key: step.step_key.clone(),
                title: step.title.clone(),
                instructions: step.instructions.clone(),
                agent_instance_ref: step.agent_profile.clone(),
                session_policy: step.session_policy.clone(),
                status: step.status.as_str().to_string(),
                attempts: step.attempts,
                auto_retry: step.auto_retry,
                result: step.result.clone(),
                dependencies,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let raw_runs = source.runs;
    let raw_handoffs = source.handoffs;
    let raw_execution_leases = source.execution_leases;
    let raw_pending_merges = source.pending_merges;
    let mut session_ids = BTreeSet::new();
    let mut run_handles = BTreeMap::new();
    let mut all_step_handles = step_handles;
    let run_steps = source
        .run_steps
        .into_iter()
        .map(|step| (step.id, step))
        .collect::<BTreeMap<_, _>>();
    for run in &raw_runs {
        run_handles.insert(run.id, AgentRunHandle::parse(run.public_handle.clone())?);
        if let Some(session_id) = run.session_id {
            session_ids.insert(session_id);
        }
        if let std::collections::btree_map::Entry::Vacant(entry) =
            all_step_handles.entry(run.step_id)
        {
            let step = run_steps
                .get(&run.step_id)
                .ok_or_else(|| anyhow::anyhow!("Agent Run 引用了不存在的 Step"))?;
            entry.insert(StepHandle::parse(step.public_handle.clone())?);
        }
    }
    let mut session_handles = BTreeMap::new();
    let mut agent_sessions = Vec::new();
    let sessions_by_id = source
        .sessions
        .into_iter()
        .map(|session| (session.id, session))
        .collect::<BTreeMap<_, _>>();
    for session_id in session_ids {
        let session = sessions_by_id
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Agent Run 引用了不存在的 Agent Session"))?;
        let handle = AgentSessionHandle::parse(session.public_handle)?;
        session_handles.insert(session_id, handle.clone());
        agent_sessions.push(AgentSessionSnapshot {
            agent_session: handle,
            revision: ScalarRevision {
                revision: u64::try_from(session.revision)?,
            },
            title: session.title,
            runtime: session.runtime,
            status: session.status.as_str().to_string(),
            unread: session.unread,
        });
    }
    let agent_runs = raw_runs
        .iter()
        .map(|run| {
            Ok(AgentRunSnapshot {
                agent_run: run_handles
                    .get(&run.id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Agent Run handle 映射缺失"))?,
                revision: ScalarRevision {
                    revision: u64::try_from(run.revision)?,
                },
                // Settlement 以当前 active Revision 的同 key Step 为目标；
                // 若该节点已被删除，回退到本次 attempt 的历史 Step。
                step: run_steps
                    .get(&run.step_id)
                    .and_then(|attempt| active_step_by_key.get(&attempt.step_key))
                    .map(|handle| StepHandle::parse(handle.clone()))
                    .transpose()?
                    .or_else(|| all_step_handles.get(&run.step_id).cloned())
                    .ok_or_else(|| anyhow::anyhow!("Agent Run 的 Step handle 映射缺失"))?,
                agent_session: run
                    .session_id
                    .and_then(|id| session_handles.get(&id).cloned()),
                status: run.status.as_str().to_string(),
                agent_state: run.agent_state.as_str().to_string(),
                outcome: run.outcome.clone(),
                outcome_payload: run.outcome_payload.clone(),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let open_questions: Vec<OpenQuestionSnapshot> = source
        .open_questions
        .into_iter()
        .map(|question| {
            let step = question
                .step_id
                .and_then(|id| all_step_handles.get(&id).cloned());
            let agent_run = question.run_id.and_then(|id| run_handles.get(&id).cloned());
            OpenQuestionSnapshot {
                question_id: question.id,
                step,
                agent_run,
                question: question.question,
            }
        })
        .collect();
    let handoffs = raw_handoffs
        .into_iter()
        .map(|row| HandoffSnapshot {
            step: row
                .step_id
                .and_then(|id| all_step_handles.get(&id).cloned()),
            agent_run: row.run_id.and_then(|id| run_handles.get(&id).cloned()),
            handoff: row.handoff,
        })
        .collect();
    let execution_leases = raw_execution_leases
        .into_iter()
        .filter_map(|lease| {
            Some(ExecutionLeaseSnapshot {
                step: all_step_handles.get(&lease.step_id)?.clone(),
                agent_run: lease.run_id.and_then(|id| run_handles.get(&id).cloned()),
                provider: lease.provider,
                isolated: lease.isolated,
                status: lease.status,
            })
        })
        .collect::<Vec<_>>();
    let pending_merges = raw_pending_merges
        .into_iter()
        .map(|pending| {
            let step = pending
                .lease
                .metadata
                .get("step_key")
                .and_then(serde_json::Value::as_str)
                .and_then(|key| active_step_by_key.get(key))
                .and_then(|handle| StepHandle::parse(handle.clone()).ok());
            PendingMergeSnapshot {
                step,
                conflicts: pending.conflicts,
            }
        })
        .collect::<Vec<_>>();
    let mut reasons = BTreeMap::<(u8, String, String), NeedsYouReasonSnapshot>::new();
    for step in &steps {
        let (priority, kind) = match step.status.as_str() {
            "needs-input" => (0, "needs-input"),
            "awaiting-outcome" => (1, "awaiting-outcome"),
            "failed" => (3, "failed"),
            _ => continue,
        };
        reasons.insert(
            (priority, step.step.as_str().to_owned(), kind.to_owned()),
            NeedsYouReasonSnapshot {
                kind: kind.to_owned(),
                step: Some(step.step.clone()),
            },
        );
    }
    for run in &agent_runs {
        let (priority, kind) = match run.status.as_str() {
            "awaiting-outcome" => (1, "awaiting-outcome"),
            "interrupted" => (3, "interrupted"),
            _ => continue,
        };
        reasons.insert(
            (priority, run.step.as_str().to_owned(), kind.to_owned()),
            NeedsYouReasonSnapshot {
                kind: kind.to_owned(),
                step: Some(run.step.clone()),
            },
        );
    }
    for question in &open_questions {
        let key = question
            .step
            .as_ref()
            .map(|step| step.as_str().to_owned())
            .unwrap_or_default();
        reasons.insert(
            (0, key, "open-question".into()),
            NeedsYouReasonSnapshot {
                kind: "open-question".into(),
                step: question.step.clone(),
            },
        );
    }
    for pending in &pending_merges {
        let key = pending
            .step
            .as_ref()
            .map(|step| step.as_str().to_owned())
            .unwrap_or_default();
        reasons.insert(
            (2, key, "merge-conflict".into()),
            NeedsYouReasonSnapshot {
                kind: "merge-conflict".into(),
                step: pending.step.clone(),
            },
        );
    }
    let needs_you_reasons = reasons.into_values().collect::<Vec<_>>();
    let focus_step = needs_you_reasons
        .iter()
        .find_map(|reason| reason.step.clone());
    let reason_count = needs_you_reasons.len();
    let pipeline_revision = active_revision
        .map(|revision| -> anyhow::Result<_> {
            Ok(PipelineRevisionSnapshot {
                handle: revision.public_handle,
                number: u64::try_from(revision.revision)?,
                status: revision.status.as_str().to_string(),
            })
        })
        .transpose()?;
    Ok(Some(WorkflowRunSnapshotData {
        workflow_run: workflow_run.clone(),
        revision: ScalarRevision {
            revision: u64::try_from(task.revision)?,
        },
        title: task.title,
        goal: task.goal,
        status: task.status.as_str().to_string(),
        paused: task.paused,
        unread: task.unread,
        needs_you: task.status == mf_agent::TaskStatus::NeedsYou,
        pipeline_revision,
        steps,
        agent_runs,
        agent_sessions,
        open_questions,
        handoffs,
        execution_leases,
        pending_merges,
        needs_you_reasons,
        reason_count,
        focus_step,
    }))
}
