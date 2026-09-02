//! 集成测试专用的 Workflow Run mutation 驱动器。
//!
//! 生产入口必须经过 CoreKernel/CAS。集成测试为了直接验证 mf-agent 的
//! Store + Orchestrator post-commit 语义，在测试 crate 内显式组合权威
//! transaction seam 与 durable action executor；不得把这条组合重新暴露
//! 成生产 library API。

use anyhow::{Context as _, Result};
use mf_agent::model::{RetryMode, SessionStatus, StepView};
use mf_agent::orchestrator::Orchestrator;
use mf_agent::run_mutation::{RunMutation, RunMutationOutput};
use mf_agent::store::Store;

pub fn retry_step(orchestrator: &Orchestrator, step_id: i64, mode: RetryMode) -> Result<StepView> {
    let continue_session_id = match mode {
        RetryMode::FreshSession => None,
        RetryMode::ContinueSession => {
            let session_id = orchestrator
                .store
                .list_runs_of_step(step_id)?
                .into_iter()
                .rev()
                .find_map(|run| run.session_id)
                .with_context(|| {
                    format!("Step {step_id} 没有存活的会话可继续;请使用 FreshSession")
                })?;
            let session = orchestrator
                .store
                .session_view(session_id)?
                .filter(|session| {
                    !matches!(session.status, SessionStatus::Dead | SessionStatus::Hidden)
                })
                .with_context(|| {
                    format!("Step {step_id} 没有存活的会话可继续;请使用 FreshSession")
                })?;
            Some(session.id)
        }
    };

    let result = orchestrator.store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Retry {
                step_id,
                mode,
                continue_session_id,
            },
        )
    })?;
    let RunMutationOutput::Retried(step) = result.output else {
        anyhow::bail!("retry mutation 返回了非重试结果")
    };
    for action in result.actions {
        orchestrator.execute_durable_run_action(&action)?;
    }
    Ok(step)
}

pub fn answer_question(orchestrator: &Orchestrator, question_id: i64, answer: &str) -> Result<()> {
    let question = orchestrator
        .store
        .question(question_id)?
        .with_context(|| format!("问题 {question_id} 不存在"))?;
    if question.run_id.is_some() && !orchestrator.supports_question_bound_answers() {
        anyhow::bail!(
            "运行时不支持 question-bound 幂等回答；为避免旧答案误答下一题，本次 Respond fail-closed"
        );
    }

    let result = orchestrator.store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Respond {
                question_id,
                answer: answer.to_string(),
            },
        )
    })?;
    let RunMutationOutput::Responded(_) = result.output else {
        anyhow::bail!("respond mutation 返回了非回答结果")
    };
    for action in result.actions {
        orchestrator.execute_durable_run_action(&action)?;
    }
    Ok(())
}
