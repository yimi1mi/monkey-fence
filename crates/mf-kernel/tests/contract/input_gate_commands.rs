//! R4 内核级回归:输入保存/确认走真实 Core 命令路径的版本 CAS。
//! 陈旧确认拒绝、同 command_id 不同内容冲突、成功确认恰好一次。

use crate::handles::CommandId;
use crate::kernel::{WorkflowRunCommand, WorkflowRunExpected};
use crate::workflow_run_commands::{FakeRunPort, RunFixture};
use mf_agent::node_input::{CompiledNodeInput, InputOverrides};
use mf_agent::workflow::ContextPolicy;
use std::sync::Arc;

fn empty_compiled(node_key: &str) -> CompiledNodeInput {
    CompiledNodeInput {
        node_key: node_key.into(),
        template: "do it".into(),
        bindings: Vec::new(),
        resolved_instructions: "do it".into(),
        upstream_summaries: Vec::new(),
        business_prompt: "do it".into(),
        protocol_segment: String::new(),
        protocol_segment_version: mf_agent::node_input::PROTOCOL_SEGMENT_VERSION.into(),
        context_policy: ContextPolicy::LegacyAncestors,
        missing_required: Vec::new(),
        node_title: node_key.into(),
        task_title: "run".into(),
        task_goal: String::new(),
        acceptance: String::new(),
        output_schema: None,
        nodes_resolved_template: "do it".into(),
    }
}

fn fixture_with_port() -> RunFixture {
    let fx = RunFixture::new();
    fx.register_port(Arc::new(FakeRunPort::default()));
    fx
}

fn seed_awaiting_input(fx: &RunFixture) -> i64 {
    let task = fx
        .store
        .task_view_by_handle(fx.workflow_run.as_str())
        .unwrap()
        .unwrap();
    let step = fx
        .store
        .step_view_by_handle(fx.step.as_str())
        .unwrap()
        .unwrap();
    fx.store
        .insert_node_input(
            task.id,
            step.revision_id,
            step.id,
            &step.step_key,
            &empty_compiled(&step.step_key),
            "awaiting_review",
        )
        .unwrap()
}

fn save_cmd(fx: &RunFixture, rev: u64, prompt: &str) -> WorkflowRunCommand {
    let mut overrides = InputOverrides::default();
    overrides.business_prompt = Some(prompt.to_string());
    WorkflowRunCommand::SaveInputOverrides {
        project: fx.project.clone(),
        workflow_run: fx.workflow_run.clone(),
        step: fx.step.clone(),
        expected_input_revision: rev,
        overrides,
        expected: fx.current_expected(),
    }
}

fn confirm_cmd(fx: &RunFixture, rev: u64) -> WorkflowRunCommand {
    WorkflowRunCommand::ConfirmInput {
        project: fx.project.clone(),
        workflow_run: fx.workflow_run.clone(),
        step: fx.step.clone(),
        expected_input_revision: rev,
        expected: fx.current_expected(),
    }
}

#[test]
fn stale_input_confirmation_is_rejected_through_real_command_path() {
    let fx = fixture_with_port();
    let input_id = seed_awaiting_input(&fx);

    // 保存新覆盖 → input_revision 1 → 2
    fx.dispatch(save_cmd(&fx, 1, "v2 content")).unwrap();

    // 用户看过的 v1 确认:必须冲突,且不进入 confirmed
    let stale = fx.dispatch(confirm_cmd(&fx, 1));
    assert!(stale.is_err(), "陈旧版本确认必须被拒绝:{stale:?}");
    let record = fx
        .store
        .latest_node_input_of_step(
            fx.store
                .task_view_by_handle(fx.workflow_run.as_str())
                .unwrap()
                .unwrap()
                .id,
            fx.store
                .step_view_by_handle(fx.step.as_str())
                .unwrap()
                .unwrap()
                .id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(record.id, input_id);
    assert_eq!(record.review_state, "awaiting_review", "陈旧确认不得放行");
    assert_eq!(record.input_revision, 2);

    // 当前版本 v2 确认:成功;同版本重复确认幂等
    fx.dispatch(confirm_cmd(&fx, 2)).unwrap();
    fx.dispatch(confirm_cmd(&fx, 2)).unwrap();
    let record = fx
        .store
        .latest_node_input_of_step(
            fx.store
                .task_view_by_handle(fx.workflow_run.as_str())
                .unwrap()
                .unwrap()
                .id,
            fx.store
                .step_view_by_handle(fx.step.as_str())
                .unwrap()
                .unwrap()
                .id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(record.review_state, "confirmed");
}

#[test]
fn same_command_id_with_different_overrides_conflicts() {
    let fx = fixture_with_port();
    seed_awaiting_input(&fx);
    let command_id = CommandId::new();
    let first = fx.dispatch_id(command_id.clone(), save_cmd(&fx, 1, "content A"));
    assert!(first.is_ok(), "{first:?}");
    // 相同 command_id 携带不同内容:固定其余 expected 条件(同一 input
    // revision),只变内容——幂等摘要必须区分,不得回放 A 的结果
    let replay = fx.dispatch_id(command_id, save_cmd(&fx, 1, "content B"));
    assert!(replay.is_err(), "同 id 不同内容必须冲突:{replay:?}");
    let _ = WorkflowRunExpected::only_run(1);
}

#[test]
fn save_failure_does_not_confirm_stale_value() {
    // 保存(陈旧版本)失败后,确认也使用同一陈旧版本 → 一并被拒;
    // 对应 Web「保存失败不得继续确认」的服务端兜底
    let fx = fixture_with_port();
    seed_awaiting_input(&fx);
    fx.dispatch(save_cmd(&fx, 1, "v2")).unwrap();
    let failed_save = fx.dispatch(save_cmd(&fx, 1, "v3 on stale"));
    assert!(failed_save.is_err(), "陈旧保存必须失败");
    let failed_confirm = fx.dispatch(confirm_cmd(&fx, 1));
    assert!(failed_confirm.is_err(), "陈旧确认必须失败");
    let step = fx
        .store
        .step_view_by_handle(fx.step.as_str())
        .unwrap()
        .unwrap();
    let record = fx
        .store
        .latest_node_input_of_step(
            fx.store
                .task_view_by_handle(fx.workflow_run.as_str())
                .unwrap()
                .unwrap()
                .id,
            step.id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(record.review_state, "awaiting_review");
    assert_eq!(record.input_revision, 2, "失败保存不推进版本");
}
