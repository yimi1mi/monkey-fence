//! 跨 Project 的 Workspace 权威摘要投影。

use crate::handles::{ProjectStoreHandle, StepHandle, WorkflowHandle, WorkflowRunHandle};
use crate::kernel::KernelProblem;
use crate::projection::{
    ScalarRevision, WorkflowRunSummarySnapshot, WorkflowSummarySnapshot, WorkspaceProjectSnapshot,
    WorkspaceSnapshotData,
};
use mf_agent::model::WorkflowRunProjectionSource;
use mf_agent::Store;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub(crate) fn read_workspace(
    projects: Vec<(ProjectStoreHandle, String, Arc<Store>)>,
) -> Result<WorkspaceSnapshotData, KernelProblem> {
    let mut project_rows = Vec::with_capacity(projects.len());
    let mut active_workflow_runs = 0usize;
    let mut needs_you_count = 0usize;
    for (project, display_name, store) in projects {
        let sources = store
            .with_tx(|tx| Store::workflow_run_projection_sources_tx(tx))
            .map_err(|error| KernelProblem::Internal(format!("{error:#}")))?;
        // collection revision(workflow.create/delete 的 CAS 轴;与
        // sources 同 Store 但独立短事务——只读不参与 L-PUBLISH)。
        let workflow_collection_revision = store
            .workflow_collection_revision()
            .map_err(|error| KernelProblem::Internal(format!("{error:#}")))?
            .max(0) as u64;
        // 工作流摘要(#75 启动运行入口)
        let workflows = store
            .project_workflow_summaries()
            .map_err(|error| KernelProblem::Internal(format!("{error:#}")))?
            .into_iter()
            .map(|(handle, name, semantic, presentation)| {
                let workflow = WorkflowHandle::parse(handle.clone())
                    .map_err(|error| KernelProblem::Internal(error.to_string()))?;
                let summary = WorkflowSummarySnapshot {
                    workflow,
                    name,
                    semantic_revision: semantic.max(0) as u64,
                    presentation_revision: presentation.max(0) as u64,
                };
                Result::<_, KernelProblem>::Ok(summary)
            })
            .collect::<Result<Vec<_>, KernelProblem>>()?;
        let active_agent_sessions = sources
            .iter()
            .flat_map(|source| &source.sessions)
            .filter(|session| {
                !matches!(
                    session.status,
                    mf_agent::SessionStatus::Done
                        | mf_agent::SessionStatus::Dead
                        | mf_agent::SessionStatus::Hidden
                )
            })
            .map(|session| session.public_handle.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let workflow_runs = sources
            .into_iter()
            .map(summary_of)
            .collect::<Result<Vec<_>, _>>()?;
        active_workflow_runs += workflow_runs
            .iter()
            .filter(|run| matches!(run.status.as_str(), "ready" | "running" | "needs-you"))
            .count();
        needs_you_count += workflow_runs.iter().filter(|run| run.needs_you).count();
        project_rows.push(WorkspaceProjectSnapshot {
            project,
            display_name,
            workflow_collection_revision,
            workflows,
            workflow_runs,
            active_agent_sessions,
        });
    }
    Ok(WorkspaceSnapshotData {
        projects: project_rows,
        active_workflow_runs,
        needs_you_count,
    })
}

fn summary_of(
    source: WorkflowRunProjectionSource,
) -> Result<WorkflowRunSummarySnapshot, KernelProblem> {
    let task = source.task;
    let workflow_run = WorkflowRunHandle::parse(task.public_handle.clone())
        .map_err(|error| KernelProblem::Internal(error.to_string()))?;
    let active_steps = source
        .steps
        .iter()
        .map(|step| {
            StepHandle::parse(step.public_handle.clone())
                .map(|handle| (step.id, step.step_key.as_str(), handle, step.status))
                .map_err(|error| KernelProblem::Internal(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let step_by_id = active_steps
        .iter()
        .map(|(id, _, handle, _)| (*id, handle.clone()))
        .collect::<BTreeMap<_, _>>();
    let step_by_key = active_steps
        .iter()
        .map(|(_, key, handle, _)| ((*key).to_owned(), handle.clone()))
        .collect::<BTreeMap<_, _>>();
    let run_step_key = source
        .run_steps
        .iter()
        .map(|step| (step.id, step.step_key.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut reasons = BTreeMap::<(u8, String, String), Option<StepHandle>>::new();
    for (_, _, step, status) in &active_steps {
        let (priority, kind) = match status {
            mf_agent::StepStatus::NeedsInput => (0, "needs-input"),
            mf_agent::StepStatus::AwaitingOutcome => (1, "awaiting-outcome"),
            mf_agent::StepStatus::Failed => (3, "failed"),
            _ => continue,
        };
        reasons.insert(
            (priority, step.as_str().to_owned(), kind.into()),
            Some(step.clone()),
        );
    }
    for run in &source.runs {
        let (priority, kind) = match run.status {
            mf_agent::RunStatus::AwaitingOutcome => (1, "awaiting-outcome"),
            mf_agent::RunStatus::Interrupted => (3, "interrupted"),
            _ => continue,
        };
        let step = run_step_key
            .get(&run.step_id)
            .and_then(|key| step_by_key.get(*key))
            .cloned()
            .or_else(|| step_by_id.get(&run.step_id).cloned());
        let key = step
            .as_ref()
            .map(|step| step.as_str().to_owned())
            .unwrap_or_default();
        reasons.insert((priority, key, kind.into()), step);
    }
    for question in &source.open_questions {
        let step = question.step_id.and_then(|id| step_by_id.get(&id).cloned());
        let key = step
            .as_ref()
            .map(|step| step.as_str().to_owned())
            .unwrap_or_default();
        reasons.insert((0, key, "open-question".into()), step);
    }
    for pending in &source.pending_merges {
        let step = pending
            .lease
            .metadata
            .get("step_key")
            .and_then(serde_json::Value::as_str)
            .and_then(|key| step_by_key.get(key))
            .cloned();
        let key = step
            .as_ref()
            .map(|step| step.as_str().to_owned())
            .unwrap_or_default();
        reasons.insert((2, key, "merge-conflict".into()), step);
    }
    let focus_step = reasons.values().find_map(Clone::clone);
    let reason_count = reasons.len();
    let active_agent_runs = source
        .runs
        .iter()
        .filter(|run| {
            matches!(
                run.status,
                mf_agent::RunStatus::Running | mf_agent::RunStatus::AwaitingOutcome
            )
        })
        .count();
    Ok(WorkflowRunSummarySnapshot {
        workflow_run,
        revision: ScalarRevision {
            revision: u64::try_from(task.revision)
                .map_err(|_| KernelProblem::Internal("Workflow Run revision 溢出".into()))?,
        },
        title: task.title,
        status: task.status.as_str().to_owned(),
        paused: task.paused,
        unread: task.unread,
        needs_you: task.status == mf_agent::TaskStatus::NeedsYou,
        reason_count,
        focus_step,
        active_agent_runs,
    })
}
