use anyhow::Result;
use mf_agent::{
    AgentState, PipelineDraft, RunStatus, SessionPolicy, SessionStatus, StepDraft, StepStatus,
    Store, TaskStatus,
};

fn one_step() -> PipelineDraft {
    PipelineDraft {
        steps: vec![StepDraft {
            key: "build".into(),
            title: "Build".into(),
            instructions: "build it".into(),
            agent_profile: "test-agent".into(),
            session_policy: SessionPolicy::Fresh,
            deps: vec![],
        }],
    }
}

#[test]
fn aggregate_views_resolve_by_public_handle_and_revisions_advance() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(&dir.path().join("project.db"))?;

    let task = store.create_task("Task", "Goal")?;
    assert_eq!(task.revision, 1);
    assert_eq!(
        store.task_view_by_handle(&task.public_handle)?.unwrap().id,
        task.id
    );

    let pipeline = store.create_draft_revision(task.id, &one_step())?;
    assert_eq!(
        store
            .revision_view_by_handle(&pipeline.public_handle)?
            .unwrap()
            .id,
        pipeline.id
    );
    let task_after_draft = store.task_view(task.id)?.unwrap();
    assert_eq!(task_after_draft.revision, task.revision + 1);

    store.activate_revision(task.id)?;
    let step = store.task_steps(task.id)?.remove(0);
    assert_eq!(
        step.revision, 2,
        "pending -> ready 是一次 Step 生命周期 mutation"
    );
    assert_eq!(
        store.step_view_by_handle(&step.public_handle)?.unwrap().id,
        step.id
    );

    let step = store
        .set_step_status(step.id, StepStatus::Running)?
        .unwrap();
    assert_eq!(step.revision, 3);
    let session = store.create_session(None, "cli", "test-agent", "Session")?;
    let session = store
        .update_session(session.id, Some(SessionStatus::Working), None, None)?
        .unwrap();
    assert_eq!(session.revision, 2);
    assert_eq!(
        store
            .session_view_by_handle(&session.public_handle)?
            .unwrap()
            .id,
        session.id
    );

    let run = store.create_run(task.id, step.id, pipeline.id, Some(session.id))?;
    let run = store
        .set_run_agent_state(run.id, AgentState::Waiting)?
        .unwrap();
    assert_eq!(run.revision, 2);
    let run = store
        .set_run_status(run.id, RunStatus::AwaitingOutcome)?
        .unwrap();
    assert_eq!(run.revision, 3);
    assert_eq!(
        store.run_view_by_handle(&run.public_handle)?.unwrap().id,
        run.id
    );

    store.with_tx(|tx| {
        assert_eq!(
            Store::task_view_by_handle_tx(tx, &task.public_handle)?
                .unwrap()
                .id,
            task.id
        );
        assert_eq!(
            Store::revision_view_by_handle_tx(tx, &pipeline.public_handle)?
                .unwrap()
                .id,
            pipeline.id
        );
        assert_eq!(
            Store::step_view_by_handle_tx(tx, &step.public_handle)?
                .unwrap()
                .id,
            step.id
        );
        assert_eq!(
            Store::session_view_by_handle_tx(tx, &session.public_handle)?
                .unwrap()
                .id,
            session.id
        );
        assert_eq!(
            Store::run_view_by_handle_tx(tx, &run.public_handle)?
                .unwrap()
                .id,
            run.id
        );
        Ok(())
    })?;

    let task = store
        .set_task_status(task.id, TaskStatus::Running)?
        .unwrap();
    assert_eq!(
        task.revision,
        task_after_draft.revision + 2,
        "activate 与 status mutation 各推进一次"
    );
    Ok(())
}
