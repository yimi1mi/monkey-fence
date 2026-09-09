//! Workflow Run 的 Store 权威只读投影。
//!
//! 本模块只组装 Project Store 已持久化事实，不缓存、不推演状态，
//! 也不暴露 rowid、capability token 或终端内容。

use crate::handles::{AgentRunHandle, AgentSessionHandle, StepHandle, WorkflowRunHandle};
use crate::projection::{
    AgentRunSnapshot, AgentSessionSnapshot, ExecutionLeaseSnapshot, HandoffSnapshot,
    NeedsYouReasonSnapshot, OpenQuestionSnapshot, PendingMergeSnapshot, PendingProposalSnapshot,
    PendingProposalStepSnapshot, PipelineRevisionSnapshot, ScalarRevision, WorkflowRunSnapshotData,
    WorkflowRunStepSnapshot,
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
    // R1:活动 Revision 的冻结节点定义(真实实例 id + 扩展字段),
    // 运行图编辑按 key 原样回传,不从展示字段反推
    let frozen_nodes: BTreeMap<String, mf_agent::workflow::WorkflowNodeSnapshot> = active_revision
        .as_ref()
        .and_then(|rev| store.revision_snapshot(rev.id).ok().flatten())
        .map(|snapshot| {
            snapshot
                .nodes
                .into_iter()
                .map(|node| (node.key.clone(), node))
                .collect()
        })
        .unwrap_or_default();
    // T2:每步骤最近一次冻结输入(完整记录,只读投影)
    let run_handles_by_id: BTreeMap<i64, AgentRunHandle> = source
        .runs
        .iter()
        .map(|run| Ok((run.id, AgentRunHandle::parse(run.public_handle.clone())?)))
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    // R6/S4:按 node_key 归属输入——已发送(dispatched)记录始终可见
    // (改图继承后新 Step 行仍能看到旧 Step 行下的发送历史,原
    // step_id/revision 归属保持不变)。未发送(仅准备/待确认)记录只在其
    // 所属 Revision 仍是活动版本时作为「当前待发送输入」展示;图语义
    // 变更后旧准备记录不再冒充当前输入(显示为未就绪,由调度重新准备)。
    let active_revision_id = active_revision.as_ref().map(|rev| rev.id);
    let node_inputs: BTreeMap<String, crate::projection::NodeInputSnapshot> = store
        .node_input_summaries(task.id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|summary| {
            let record = store
                .latest_node_input_of_key(task.id, &summary.node_key)
                .ok()
                .flatten()?;
            let is_current = record.status == "dispatched"
                || active_revision_id
                    .map(|active| record.revision_id == active)
                    .unwrap_or(false);
            if !is_current {
                return None;
            }
            let agent_run = record
                .agent_run_id
                .and_then(|run_id| run_handles_by_id.get(&run_id).cloned());
            Some((
                summary.node_key,
                crate::projection::NodeInputSnapshot {
                    status: record.status.clone(),
                    summary: record.summary.clone(),
                    created_at: record.created_at.clone(),
                    agent_run,
                    template: record.compiled.template.clone(),
                    resolved_instructions: record.compiled.resolved_instructions.clone(),
                    business_prompt: record.compiled.business_prompt.clone(),
                    protocol_segment: record.compiled.protocol_segment.clone(),
                    bindings: serde_json::to_value(&record.compiled.bindings)
                        .unwrap_or(serde_json::Value::Null),
                    upstream_summaries: serde_json::to_value(&record.compiled.upstream_summaries)
                        .unwrap_or(serde_json::Value::Null),
                    missing_required: record.compiled.missing_required.clone(),
                    context_policy: match record.compiled.context_policy {
                        mf_agent::workflow::ContextPolicy::LegacyAncestors => {
                            "legacy_ancestors".to_string()
                        }
                        mf_agent::workflow::ContextPolicy::ExplicitOnly => {
                            "explicit_only".to_string()
                        }
                    },
                    review_state: record.review_state.clone(),
                    input_revision: u64::try_from(record.input_revision).unwrap_or(1),
                    overrides: record
                        .overrides
                        .as_ref()
                        .map(|overrides| serde_json::to_value(overrides).unwrap_or_default()),
                    effective_business_prompt: record.overrides.as_ref().map(|overrides| {
                        mf_agent::node_input::apply_overrides(record.compiled.clone(), overrides)
                            .business_prompt
                    }),
                },
            ))
        })
        .collect();
    let steps = raw_steps
        .iter()
        .map(|step| {
            let dependencies = if let Some(node) = frozen_nodes.get(&step.step_key) {
                // 编辑视图原样回传冻结定义；不要用 SQL 主键顺序改变 deps 的顺序。
                node.deps
                    .iter()
                    .filter_map(|key| active_step_by_key.get(key))
                    .map(|handle| StepHandle::parse(handle.clone()))
                    .collect::<anyhow::Result<Vec<_>>>()?
            } else {
                step.deps
                    .iter()
                    .filter_map(|id| step_handles.get(id).cloned())
                    .collect()
            };
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
                input: node_inputs.get(&step.step_key).cloned(),
                agent_instance_id: frozen_nodes
                    .get(&step.step_key)
                    .map(|node| node.instance.id.clone())
                    .unwrap_or_default(),
                acceptance_criteria: frozen_nodes
                    .get(&step.step_key)
                    .map(|node| node.acceptance_criteria.clone())
                    .unwrap_or_default(),
                output_schema: frozen_nodes
                    .get(&step.step_key)
                    .and_then(|node| node.output_schema.clone()),
                input_bindings: frozen_nodes
                    .get(&step.step_key)
                    .map(|node| node.input_bindings.clone())
                    .unwrap_or_default(),
                context_policy: frozen_nodes
                    .get(&step.step_key)
                    .and_then(|node| node.context_policy),
                require_input_review: frozen_nodes
                    .get(&step.step_key)
                    .map(|node| node.require_input_review)
                    .unwrap_or(false),
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
    // T3 缺口补齐:人工检查门控纳入运行级「需要你」——awaiting_review
    // 的输入按 node_key 定位到当前图的 Step(改图继承后仍可聚焦)。
    let step_handle_of_key: BTreeMap<&str, &StepHandle> = steps
        .iter()
        .map(|step| (step.key.as_str(), &step.step))
        .collect();
    // S5:input-review 提醒只来自当前仍可操作的有效门控——终态运行
    // (succeeded/failed/cancelled/archived)、步骤已终态、或记录所属
    // Revision 已被改图替换(过期门控)都不再贡献提醒。
    let task_terminal = matches!(
        task.status.as_str(),
        "succeeded" | "failed" | "cancelled" | "archived"
    );
    let terminal_keys: std::collections::HashSet<&str> = steps
        .iter()
        .filter(|step| {
            matches!(
                step.status.as_str(),
                "succeeded" | "failed" | "skipped" | "cancelled"
            )
        })
        .map(|step| step.key.as_str())
        .collect();
    let active_revision_id = active_revision.as_ref().map(|rev| rev.id);

    if !task_terminal {
        for summary in store.node_input_summaries(task.id).unwrap_or_default() {
            if summary.review_state != "awaiting_review" {
                continue;
            }
            if terminal_keys.contains(summary.node_key.as_str()) {
                continue;
            }
            // 过期门控:记录编译自旧 Revision(改图后定义已变),不再可操作
            if let Some(active) = active_revision_id {
                let record_current = store
                    .latest_node_input_of_key(task.id, &summary.node_key)
                    .ok()
                    .flatten()
                    .map(|record| record.revision_id == active)
                    .unwrap_or(false);
                if !record_current {
                    continue;
                }
            }
            let Some(step) = step_handle_of_key.get(summary.node_key.as_str()) else {
                continue;
            };

            reasons.insert(
                (0, step.as_str().to_owned(), "input-review".into()),
                NeedsYouReasonSnapshot {
                    kind: "input-review".into(),
                    step: Some((*step).clone()),
                },
            );
        }
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
        needs_you: reason_count > 0
            && !matches!(
                task.status,
                mf_agent::TaskStatus::Succeeded
                    | mf_agent::TaskStatus::Cancelled
                    | mf_agent::TaskStatus::Archived
            ),
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
        pending_proposals: store
            .draft_proposal_steps(task.id)
            .unwrap_or_default()
            .into_iter()
            .map(|(handle, revision, steps)| PendingProposalSnapshot {
                revision_handle: handle,
                revision: revision.max(0) as u64,
                steps: steps
                    .into_iter()
                    .map(|(key, title, agent)| PendingProposalStepSnapshot { key, title, agent })
                    .collect(),
            })
            .collect(),
    }))
}
