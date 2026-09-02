use anyhow::Result;
use mf_agent::{
    NextAttemptSession, PipelineDraft, RetryMode, RunAction, RunMutation, RunMutationOutput,
    RunStopOutcome, RunStopResult, SessionPolicy, Settlement, StepDraft, Store,
};
use rusqlite::params;

fn one_step() -> PipelineDraft {
    PipelineDraft {
        steps: vec![StepDraft {
            key: "work".into(),
            title: "work".into(),
            instructions: "do it".into(),
            agent_profile: "test".into(),
            session_policy: SessionPolicy::Fresh,
            deps: vec![],
        }],
    }
}

#[test]
fn skip_commits_step_downstream_task_and_unread_in_one_transaction() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("skip", "g")?;
    let revision = store.create_draft_revision(
        task.id,
        &PipelineDraft {
            steps: vec![
                StepDraft {
                    key: "a".into(),
                    title: "a".into(),
                    instructions: "".into(),
                    agent_profile: "test".into(),
                    session_policy: SessionPolicy::Fresh,
                    deps: vec![],
                },
                StepDraft {
                    key: "b".into(),
                    title: "b".into(),
                    instructions: "".into(),
                    agent_profile: "test".into(),
                    session_policy: SessionPolicy::Fresh,
                    deps: vec!["a".into()],
                },
            ],
        },
    )?;
    store.activate_revision(task.id)?;
    let steps = store.revision_steps(revision.id)?;
    let a = steps.iter().find(|step| step.step_key == "a").unwrap();
    let b = steps.iter().find(|step| step.step_key == "b").unwrap();
    store.set_step_status(a.id, mf_agent::StepStatus::Failed)?;
    store.set_step_status(b.id, mf_agent::StepStatus::Blocked)?;
    store.set_task_status_and_unread(task.id, mf_agent::TaskStatus::NeedsYou, true)?;
    let before = (
        store.task_view(task.id)?.unwrap().revision,
        store.step_view(a.id)?.unwrap().revision,
        store.step_view(b.id)?.unwrap().revision,
    );

    let rolled_back: Result<()> = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Skip { step_id: a.id })?;
        anyhow::bail!("crash before L-CMD commit")
    });
    assert!(rolled_back.is_err());
    assert_eq!(
        (
            store.task_view(task.id)?.unwrap().revision,
            store.step_view(a.id)?.unwrap().revision,
            store.step_view(b.id)?.unwrap().revision,
        ),
        before,
        "事务提交前崩溃不得留下任一 revision"
    );

    let result = store
        .with_tx(|tx| Store::apply_run_mutation_tx(tx, RunMutation::Skip { step_id: a.id }))?;
    let a = store.step_view(a.id)?.unwrap();
    let b = store.step_view(b.id)?.unwrap();
    let task = store.task_view(task.id)?.unwrap();
    assert_eq!(a.status, mf_agent::StepStatus::Skipped);
    assert_eq!(b.status, mf_agent::StepStatus::Ready);
    assert_eq!(task.status, mf_agent::TaskStatus::Running);
    assert!(!task.unread);
    assert_eq!(
        result.actions,
        vec![RunAction::AfterSkip { task_id: task.id }]
    );
    Ok(())
}

#[test]
fn skip_with_held_join_resources_is_fail_closed_needs_you() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("join", "g")?;
    let revision = store.create_draft_revision(
        task.id,
        &PipelineDraft {
            steps: vec![
                StepDraft {
                    key: "a".into(),
                    title: "a".into(),
                    instructions: "".into(),
                    agent_profile: "test".into(),
                    session_policy: SessionPolicy::Fresh,
                    deps: vec![],
                },
                StepDraft {
                    key: "b".into(),
                    title: "b".into(),
                    instructions: "".into(),
                    agent_profile: "test".into(),
                    session_policy: SessionPolicy::Fresh,
                    deps: vec![],
                },
                StepDraft {
                    key: "join".into(),
                    title: "join".into(),
                    instructions: "".into(),
                    agent_profile: "test".into(),
                    session_policy: SessionPolicy::Fresh,
                    deps: vec!["a".into(), "b".into()],
                },
            ],
        },
    )?;
    store.activate_revision(task.id)?;
    let steps = store.revision_steps(revision.id)?;
    let a = steps.iter().find(|step| step.step_key == "a").unwrap();
    let b = steps.iter().find(|step| step.step_key == "b").unwrap();
    let join = steps.iter().find(|step| step.step_key == "join").unwrap();
    store.set_step_status(a.id, mf_agent::StepStatus::Succeeded)?;
    store.set_step_status(b.id, mf_agent::StepStatus::Failed)?;
    store.set_step_status(join.id, mf_agent::StepStatus::Blocked)?;
    store.with_tx(|tx| {
        tx.execute(
            "INSERT INTO execution_leases
                 (lease_key,step_id,task_id,provider,path,isolated,status,created_at)
             VALUES ('held-a',?1,?2,'test','D:/held-a',1,'held','now')",
            params![a.id, task.id],
        )?;
        Ok(())
    })?;

    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Skip { step_id: b.id }).map(|_| ())
    })?;
    assert_eq!(
        store.step_view(join.id)?.unwrap().status,
        mf_agent::StepStatus::Ready,
        "DAG 提升属于 L-CMD 事务"
    );
    let task = store.task_view(task.id)?.unwrap();
    assert_eq!(task.status, mf_agent::TaskStatus::NeedsYou);
    assert!(
        task.unread,
        "外部 merge 未有第二 receipt 前必须 fail-closed"
    );
    Ok(())
}

#[test]
fn start_uses_caller_transaction_and_rolls_back_with_it() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("t", "g")?;
    store.create_draft_revision(task.id, &one_step())?;

    let failed: Result<()> = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id })?;
        anyhow::bail!("force rollback")
    });
    assert!(failed.is_err());
    assert_eq!(store.task_view(task.id)?.unwrap().status.as_str(), "draft");

    let result = store
        .with_tx(|tx| Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id }))?;
    assert!(matches!(result.output, RunMutationOutput::Started(_)));
    assert_eq!(
        result.actions,
        vec![RunAction::DispatchReady { task_id: task.id }]
    );
    assert_eq!(store.task_steps(task.id)?[0].status.as_str(), "ready");
    Ok(())
}

#[test]
fn durable_cancel_fence_blocks_same_task_writes_but_not_other_tasks() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("fenced", "g")?;
    let revision = store.create_draft_revision(task.id, &one_step())?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id }).map(|_| ())
    })?;
    let step = store.task_steps(task.id)?[0].clone();
    let run = store.create_run(task.id, step.id, revision.id, None)?;
    store.with_tx(|tx| {
        tx.execute(
            "INSERT INTO execution_leases(lease_key,run_id,step_id,task_id,provider,path,status,created_at)
             VALUES('lease-fenced',?1,?2,?3,'test','D:/fenced','held','now')",
            params![run.id, step.id, task.id],
        )?;
        Ok(())
    })?;
    store.with_tx(|tx| {
        Store::reserve_cancel_fence_tx(
            tx,
            "cancel-one",
            task.id,
            &[(run.id, run.public_handle.clone(), run.revision)],
        )
        .map(|_| ())
    })?;

    assert!(store
        .set_step_status(step.id, mf_agent::StepStatus::Failed)
        .is_err());
    assert!(store
        .create_run(task.id, step.id, revision.id, None)
        .is_err());
    store.with_tx(|tx| {
        for sql in [
            "DELETE FROM agent_tasks WHERE id=?1",
            "DELETE FROM steps WHERE task_id=?1",
            "DELETE FROM agent_runs WHERE task_id=?1",
            "DELETE FROM execution_leases WHERE task_id=?1",
            "UPDATE pipeline_revisions SET status='cancelled' WHERE task_id=?1",
        ] {
            assert!(tx.execute(sql, [task.id]).is_err(), "fence 未阻止:{sql}");
        }
        Ok(())
    })?;
    let conflicting = store
        .with_tx(|tx| Store::reserve_cancel_fence_tx(tx, "cancel-two", task.id, &[]).map(|_| ()));
    assert!(
        conflicting.is_err(),
        "同 Task 的不同 command 必须被 UNIQUE fence 拒绝"
    );

    let other = store.create_task("other", "g")?;
    let other_revision = store.create_draft_revision(other.id, &one_step())?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: other.id }).map(|_| ())
    })?;
    let other_step = store.task_steps(other.id)?[0].clone();
    store.create_run(other.id, other_step.id, other_revision.id, None)?;

    store.with_tx(|tx| {
        assert!(Store::claim_cancel_target_tx(tx, "cancel-one", run.id)?);
        Store::record_cancel_outcome_tx(tx, "cancel-one", run.id, RunStopOutcome::Confirmed)
    })?;
    store.with_tx(|tx| {
        let stops = Store::begin_cancel_finalize_tx(tx, "cancel-one")?;
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Cancel {
                task_id: task.id,
                run_stops: stops,
            },
        )?;
        Store::finish_cancel_fence_tx(tx, "cancel-one")
    })?;
    store.with_tx(|tx| {
        Store::reserve_cancel_fence_tx(tx, "cancel-after-final", task.id, &[]).map(|_| ())
    })?;
    Ok(())
}

#[test]
fn exit_is_not_settlement_and_interrupted_run_can_be_settled_after_restart() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("t", "g")?;
    let rev = store.create_draft_revision(task.id, &one_step())?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id }).map(|_| ())
    })?;
    let step = store.task_steps(task.id)?[0].clone();
    let run_id = store.with_tx(|tx| {
        tx.execute(
            "INSERT INTO agent_runs (task_id,step_id,revision_id,status,capability_token,agent_state,public_handle,started_at) VALUES (?1,?2,?3,'awaiting-outcome','token','done','run-handle','now')",
            params![task.id, step.id, rev.id],
        )?;
        Ok(tx.last_insert_rowid())
    })?;
    assert!(store.run_view(run_id)?.unwrap().outcome.is_none());

    store.with_tx(|tx| {
        tx.execute(
            "UPDATE agent_runs SET status='interrupted' WHERE id=?1",
            params![run_id],
        )?;
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Settle {
                run_id,
                settlement: Settlement::complete("ok"),
            },
        )
    })?;
    let run = store.run_view(run_id)?.unwrap();
    assert_eq!(run.status.as_str(), "succeeded");
    assert_eq!(run.outcome.as_deref(), Some("complete"));
    Ok(())
}

#[test]
fn respond_and_retry_return_only_post_commit_actions() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("t", "g")?;
    store.create_draft_revision(task.id, &one_step())?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id }).map(|_| ())
    })?;
    let step = store.task_steps(task.id)?[0].clone();
    store.set_step_status(step.id, mf_agent::StepStatus::NeedsInput)?;
    let q = store.ask_question(task.id, Some(step.id), None, "continue?")?;
    store.set_task_status(task.id, mf_agent::TaskStatus::NeedsYou)?;
    store.set_task_unread(task.id, true)?;
    let answered = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Respond {
                question_id: q.id,
                answer: "yes".into(),
            },
        )
    })?;
    assert!(answered.actions.is_empty());
    assert_eq!(
        store.step_view(step.id)?.unwrap().status.as_str(),
        "running"
    );
    let task_after_answer = store.task_view(task.id)?.unwrap();
    assert_eq!(task_after_answer.status.as_str(), "running");
    // T0B 既有语义：Needs You 已解除，但 unread 由投影消费方显式清除。
    assert!(task_after_answer.unread);

    store.set_step_status(step.id, mf_agent::StepStatus::AwaitingOutcome)?;
    let retried = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Retry {
                step_id: step.id,
                mode: RetryMode::FreshSession,
                continue_session_id: None,
            },
        )
    })?;
    assert!(!retried
        .actions
        .iter()
        .any(|action| matches!(action, RunAction::AnswerRuntime { .. })));
    assert_eq!(
        store.next_attempt_session(step.id)?,
        Some(NextAttemptSession::fresh())
    );
    assert_eq!(store.step_view(step.id)?.unwrap().status.as_str(), "ready");
    Ok(())
}

#[test]
fn retry_session_intent_survives_restart_and_dispatch_cas_consumes_once() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("retry-session.db");
    let store = Store::open(&path)?;
    let task = store.create_task("t", "g")?;
    let revision = store.create_draft_revision(task.id, &one_step())?;
    store.activate_revision(task.id)?;
    let step = store.task_steps(task.id)?[0].clone();
    store.set_step_status(step.id, mf_agent::StepStatus::Failed)?;
    let continued = store.create_session(None, "mock", "test", "continued")?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Retry {
                step_id: step.id,
                mode: RetryMode::ContinueSession,
                continue_session_id: Some(continued.id),
            },
        )
        .map(|_| ())
    })?;
    drop(store);

    // 模拟 retry commit 后、post-commit/dispatch 前进程崩溃。
    let recovered = Store::open(&path)?;
    let expected = NextAttemptSession::continue_session(continued.id);
    assert_eq!(recovered.next_attempt_session(step.id)?, Some(expected));
    recovered.dispatch_run_consuming(
        task.id,
        step.id,
        revision.id,
        continued.id,
        Some(expected),
    )?;
    assert_eq!(recovered.next_attempt_session(step.id)?, None);

    // 崩溃重放拿着旧选择再次派发时 CAS 拒绝，既不重复 attempt，也不
    // 能把陈旧 session 意图重新插回 Store。
    let error = recovered
        .dispatch_run_consuming(task.id, step.id, revision.id, continued.id, Some(expected))
        .unwrap_err();
    assert!(error.to_string().contains("并发消费或替换"));
    assert_eq!(recovered.next_attempt_session(step.id)?, None);
    assert_eq!(recovered.list_runs_of_step(step.id)?.len(), 1);
    Ok(())
}

#[test]
fn answer_runtime_action_is_bound_to_the_answered_question() -> Result<()> {
    // 两阶段契约:accept 只落私有投递表并产出 nonce 绑定 action;
    // 投递确认前 question 保持 open、Step 保持 needs-input;
    // action 序列化形态与 RunMutation Debug 都不得携带答案明文。
    let store = Store::memory()?;
    let task = store.create_task("t", "g")?;
    let revision = store.create_draft_revision(task.id, &one_step())?;
    store.activate_revision(task.id)?;
    let step = store.task_steps(task.id)?[0].clone();
    let session = store.create_session(None, "mock", "test", "s")?;
    let run = store.create_run(task.id, step.id, revision.id, Some(session.id))?;
    store.set_step_status(step.id, mf_agent::StepStatus::NeedsInput)?;
    store.set_task_status(task.id, mf_agent::TaskStatus::NeedsYou)?;
    let question = store.ask_question(task.id, Some(step.id), Some(run.id), "first?")?;
    let result = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Respond {
                question_id: question.id,
                answer: "first answer".into(),
            },
        )
    })?;

    // 投递前:问题仍未最终回答,Step/Task 不提前推进。
    assert_eq!(store.question(question.id)?.unwrap().status, "open");
    assert_eq!(
        store.step_view(step.id)?.unwrap().status.as_str(),
        "needs-input"
    );
    assert_eq!(
        store.task_view(task.id)?.unwrap().status.as_str(),
        "needs-you"
    );
    let delivery = store.answer_delivery_of_question(question.id)?.unwrap();
    assert_eq!(delivery.status, "pending");
    assert_eq!(delivery.run_id, run.id);
    assert_eq!(delivery.run_handle, run.public_handle);
    assert!(
        delivery.nonce.starts_with(&format!(
            "q{}:r{}:rev{}:",
            question.id, run.id, run.revision
        )),
        "nonce 必须绑定 question + run + revision: {}",
        delivery.nonce
    );

    let [RunAction::AnswerRuntime {
        question_id,
        run_id,
        run_handle,
        nonce,
    }] = result.actions.as_slice()
    else {
        anyhow::bail!(
            "必须恰好产出一个 AnswerRuntime action: {:?}",
            result.actions
        )
    };
    assert_eq!(*question_id, question.id);
    assert_eq!(*run_id, run.id);
    assert_eq!(run_handle, &run.public_handle);
    assert_eq!(nonce, &delivery.nonce);
    // action 会被序列化进 kernel 投影 outbox 的事件 JSON:不得含答案明文。
    let json = serde_json::to_string(result.actions.first().unwrap()).unwrap();
    assert!(!json.contains("first answer"), "{json}");
    // RunMutation 的 Debug(日志/错误链载体)同样脱敏。
    let mutation = RunMutation::Respond {
        question_id: question.id,
        answer: "first answer".into(),
    };
    let debug = format!("{mutation:?}");
    assert!(!debug.contains("first answer"), "{debug}");

    // 同答案重复 accept:幂等,重发同一 nonce 的 action。
    let dup = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Respond {
                question_id: question.id,
                answer: "first answer".into(),
            },
        )
    })?;
    let [RunAction::AnswerRuntime {
        nonce: dup_nonce, ..
    }] = dup.actions.as_slice()
    else {
        anyhow::bail!("幂等重放必须重发同一 action")
    };
    assert_eq!(dup_nonce, nonce);

    // 异答案:稳定冲突,且错误链不泄露已记录答案明文。
    let conflict = store
        .with_tx(|tx| {
            Store::apply_run_mutation_tx(
                tx,
                RunMutation::Respond {
                    question_id: question.id,
                    answer: "second answer".into(),
                },
            )
        })
        .unwrap_err();
    let chain = format!("{conflict:#}");
    assert!(chain.contains("冲突"), "{chain}");
    assert!(!chain.contains("first answer"), "{chain}");
    // 状态不受冲突影响。
    assert_eq!(store.question(question.id)?.unwrap().status, "open");
    assert_eq!(
        store
            .answer_delivery_of_question(question.id)?
            .unwrap()
            .nonce,
        *nonce
    );
    Ok(())
}

#[test]
fn cancel_never_redispatches_and_returns_resource_cleanup_actions() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("t", "g")?;
    let rev = store.create_draft_revision(task.id, &one_step())?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id }).map(|_| ())
    })?;
    let step = store.task_steps(task.id)?[0].clone();
    let run_id = store.with_tx(|tx| {
        tx.execute(
            "INSERT INTO agent_runs (task_id,step_id,revision_id,status,capability_token,agent_state,public_handle,started_at) VALUES (?1,?2,?3,'running','cancel-token','working','cancel-run','now')",
            params![task.id, step.id, rev.id],
        )?;
        Ok(tx.last_insert_rowid())
    })?;

    let result = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Cancel {
                task_id: task.id,
                run_stops: vec![RunStopResult {
                    run_id,
                    outcome: RunStopOutcome::Confirmed,
                }],
            },
        )
    })?;

    assert!(!result
        .actions
        .iter()
        .any(|action| matches!(action, RunAction::DispatchReady { .. })));
    assert!(result
        .actions
        .contains(&RunAction::ReleaseRunResources { run_id }));
    assert!(result
        .actions
        .contains(&RunAction::ReleaseTaskResources { task_id: task.id }));
    assert_eq!(
        store.task_view(task.id)?.unwrap().status.as_str(),
        "cancelled"
    );
    assert_eq!(
        store.step_view(step.id)?.unwrap().status.as_str(),
        "cancelled"
    );
    assert_eq!(
        store.run_view(run_id)?.unwrap().status.as_str(),
        "cancelled"
    );
    assert!(store.active_revision(task.id)?.is_none());
    Ok(())
}

#[test]
fn cancel_partial_stop_records_every_run_without_releasing_unconfirmed_lease() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("partial-cancel.db");
    let store = Store::open(&path)?;
    let task = store.create_task("t", "g")?;
    let rev = store.create_draft_revision(task.id, &one_step())?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id }).map(|_| ())
    })?;
    let step = store.task_steps(task.id)?[0].clone();
    let (confirmed, unconfirmed) = store.with_tx(|tx| {
        let insert = |tx: &rusqlite::Transaction<'_>, handle: &str, token: &str| -> Result<i64> {
            tx.execute(
                "INSERT INTO agent_runs (task_id,step_id,revision_id,status,capability_token,agent_state,public_handle,started_at) VALUES (?1,?2,?3,'running',?4,'working',?5,'now')",
                params![task.id, step.id, rev.id, token, handle],
            )?;
            Ok(tx.last_insert_rowid())
        };
        let confirmed = insert(tx, "confirmed-run", "confirmed-token")?;
        let unconfirmed = insert(tx, "unconfirmed-run", "unconfirmed-token")?;
        tx.execute(
            "INSERT INTO execution_leases (lease_key,run_id,step_id,task_id,provider,path,isolated,status,created_at) VALUES ('confirmed-lease',?1,?2,?3,'test','confirmed',1,'held','now')",
            params![confirmed, step.id, task.id],
        )?;
        tx.execute(
            "INSERT INTO execution_leases (lease_key,run_id,step_id,task_id,provider,path,isolated,status,created_at) VALUES ('unconfirmed-lease',?1,?2,?3,'test','unconfirmed',1,'held','now')",
            params![unconfirmed, step.id, task.id],
        )?;
        Ok((confirmed, unconfirmed))
    })?;

    let result = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Cancel {
                task_id: task.id,
                run_stops: vec![
                    RunStopResult {
                        run_id: confirmed,
                        outcome: RunStopOutcome::Confirmed,
                    },
                    RunStopResult {
                        run_id: unconfirmed,
                        outcome: RunStopOutcome::Unconfirmed,
                    },
                ],
            },
        )
    })?;
    assert!(matches!(
        result.output,
        RunMutationOutput::CancelNeedsYou(_)
    ));
    assert_eq!(
        store.run_view(confirmed)?.unwrap().status.as_str(),
        "cancelled"
    );
    assert_eq!(
        store.run_view(unconfirmed)?.unwrap().status.as_str(),
        "interrupted"
    );
    assert_eq!(
        store.task_view(task.id)?.unwrap().status.as_str(),
        "needs-you"
    );
    assert_ne!(
        store.step_view(step.id)?.unwrap().status.as_str(),
        "cancelled"
    );
    assert_eq!(store.active_revision(task.id)?.unwrap().id, rev.id);
    assert!(result
        .actions
        .contains(&RunAction::ReleaseRunResources { run_id: confirmed }));
    assert!(result.actions.contains(&RunAction::ReleaseRunSlot {
        run_id: unconfirmed
    }));
    assert!(!result.actions.contains(&RunAction::ReleaseRunResources {
        run_id: unconfirmed
    }));
    assert!(!result
        .actions
        .contains(&RunAction::ReleaseTaskResources { task_id: task.id }));

    drop(store);
    let reopened = Store::open(&path)?;
    reopened.recover_interrupted()?;
    assert_eq!(
        reopened.run_view(unconfirmed)?.unwrap().status.as_str(),
        "interrupted"
    );
    let leases = reopened.list_execution_leases(task.id)?;
    assert_eq!(
        leases
            .iter()
            .find(|lease| lease.run_id == Some(unconfirmed))
            .unwrap()
            .status,
        "held"
    );
    Ok(())
}

#[test]
fn cancel_rejects_incomplete_or_duplicate_stop_sets_atomically() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("t", "g")?;
    let rev = store.create_draft_revision(task.id, &one_step())?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id }).map(|_| ())
    })?;
    let step = store.task_steps(task.id)?[0].clone();
    let run_id = store.with_tx(|tx| {
        tx.execute(
            "INSERT INTO agent_runs (task_id,step_id,revision_id,status,capability_token,agent_state,public_handle,started_at) VALUES (?1,?2,?3,'running','token','working','run','now')",
            params![task.id, step.id, rev.id],
        )?;
        Ok(tx.last_insert_rowid())
    })?;
    for stops in [
        vec![],
        vec![
            RunStopResult {
                run_id,
                outcome: RunStopOutcome::Confirmed,
            },
            RunStopResult {
                run_id,
                outcome: RunStopOutcome::Confirmed,
            },
        ],
    ] {
        assert!(store
            .with_tx(|tx| Store::apply_run_mutation_tx(
                tx,
                RunMutation::Cancel {
                    task_id: task.id,
                    run_stops: stops
                },
            ))
            .is_err());
        assert_eq!(store.run_view(run_id)?.unwrap().status.as_str(), "running");
        assert_ne!(
            store.task_view(task.id)?.unwrap().status.as_str(),
            "cancelled"
        );
    }
    Ok(())
}

#[test]
fn failed_settlement_retry_decision_is_atomic_and_replay_cannot_rewind_new_run() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("t", "g")?;
    let rev = store.create_draft_revision(task.id, &one_step())?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id }).map(|_| ())
    })?;
    let step = store.task_steps(task.id)?[0].clone();
    store.set_step_auto_retry(step.id, 1)?;
    let run_id = store.with_tx(|tx| {
        tx.execute(
            "UPDATE steps SET attempts=1,status='running' WHERE id=?1",
            params![step.id],
        )?;
        tx.execute(
            "INSERT INTO agent_runs (task_id,step_id,revision_id,status,capability_token,agent_state,public_handle,started_at) VALUES (?1,?2,?3,'running','retry-token','working','retry-run','now')",
            params![task.id, step.id, rev.id],
        )?;
        Ok(tx.last_insert_rowid())
    })?;

    let first = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Settle {
                run_id,
                settlement: Settlement::Fail {
                    reason: "boom".into(),
                },
            },
        )
    })?;
    assert_eq!(
        store.step_view(step.id)?.unwrap().status,
        mf_agent::StepStatus::Ready
    );
    assert_eq!(
        store.task_view(task.id)?.unwrap().status,
        mf_agent::TaskStatus::Running
    );
    assert!(!first
        .actions
        .iter()
        .any(|action| matches!(action, RunAction::AfterSettlement { .. })));

    // 模拟 ready 已被调度为下一条 Run；同 settlement/outbox 重放不得再把它改回 ready。
    store.set_step_status(step.id, mf_agent::StepStatus::Running)?;
    let replay = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Settle {
                run_id,
                settlement: Settlement::Fail {
                    reason: "boom".into(),
                },
            },
        )
    })?;
    assert!(replay.actions.is_empty());
    assert_eq!(
        store.step_view(step.id)?.unwrap().status,
        mf_agent::StepStatus::Running
    );
    Ok(())
}

#[test]
fn exhausted_failed_settlement_blocks_descendants_and_marks_task_in_same_tx() -> Result<()> {
    let store = Store::memory()?;
    let task = store.create_task("t", "g")?;
    let draft = PipelineDraft {
        steps: vec![
            StepDraft {
                key: "a".into(),
                title: "a".into(),
                instructions: "".into(),
                agent_profile: "test".into(),
                session_policy: SessionPolicy::Fresh,
                deps: vec![],
            },
            StepDraft {
                key: "b".into(),
                title: "b".into(),
                instructions: "".into(),
                agent_profile: "test".into(),
                session_policy: SessionPolicy::Fresh,
                deps: vec!["a".into()],
            },
        ],
    };
    let rev = store.create_draft_revision(task.id, &draft)?;
    store.with_tx(|tx| {
        Store::apply_run_mutation_tx(tx, RunMutation::Start { task_id: task.id }).map(|_| ())
    })?;
    let steps = store.task_steps(task.id)?;
    let a = steps.iter().find(|step| step.step_key == "a").unwrap();
    let run_id = store.with_tx(|tx| {
        tx.execute("UPDATE steps SET attempts=1,status='running' WHERE id=?1", params![a.id])?;
        tx.execute("INSERT INTO agent_runs (task_id,step_id,revision_id,status,capability_token,agent_state,public_handle,started_at) VALUES (?1,?2,?3,'running','fail-token','working','fail-run','now')", params![task.id, a.id, rev.id])?;
        Ok(tx.last_insert_rowid())
    })?;
    let result = store.with_tx(|tx| {
        Store::apply_run_mutation_tx(
            tx,
            RunMutation::Settle {
                run_id,
                settlement: Settlement::Fail {
                    reason: "nope".into(),
                },
            },
        )
    })?;
    let steps = store.task_steps(task.id)?;
    assert_eq!(
        steps
            .iter()
            .find(|step| step.step_key == "a")
            .unwrap()
            .status,
        mf_agent::StepStatus::Failed
    );
    assert_eq!(
        steps
            .iter()
            .find(|step| step.step_key == "b")
            .unwrap()
            .status,
        mf_agent::StepStatus::Blocked
    );
    let task = store.task_view(task.id)?.unwrap();
    assert_eq!(task.status, mf_agent::TaskStatus::NeedsYou);
    assert!(task.unread);
    assert!(!result
        .actions
        .iter()
        .any(|action| matches!(action, RunAction::AfterSettlement { .. })));
    Ok(())
}
