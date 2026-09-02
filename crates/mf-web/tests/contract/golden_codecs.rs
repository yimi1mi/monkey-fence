//! T7b golden codecs 契约(Issue #39):Rust DTO ↔ fixtures 文件 ↔ TS
//! 类型三方对齐。
//!
//! 冻结方式:**语义等价**(roundtrip 后逐字段相等)+ 关键 wire 断言
//! (schema/type/字符串化 u64/opaque handle),fixtures 每次刷新写入供
//! TS golden(`web/src/api/__tests__`)按字段对照。不做逐字节比对:
//! serde_json 的 Map 键序受 workspace feature unification 影响
//! (preserve_order 开启与否),字节序不是 wire 契约的一部分——
//! §7.2 的冻结语义是字段与类型,不是 JSON 键顺序。

use mf_web::api::commands::{AggregateRef, CommandEnvelope, CommandOutcomeWire, ExpectedRevision};
use mf_web::api::events::EventEnvelope;
use mf_web::api::snapshot::SnapshotEnvelope;
use mf_web::problem::{negotiate_api, negotiate_ws_subprotocol, Problem, ProblemCode, Retry};
use std::path::PathBuf;

fn refresh_fixture(kind: &str, name: &str, value: &serde_json::Value) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(kind);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_string_pretty(value).unwrap() + "\n",
    )
    .unwrap();
}

fn workflow_aggregate() -> AggregateRef {
    AggregateRef {
        kind: "project_workflow".into(),
        handle: "wf_0123456789abcdef0123456789abcdef".into(),
    }
}

#[test]
fn golden_command_envelope() {
    let mut command = CommandEnvelope::new(
        "018f3e2a-1b2c-7d3e-9f4a-5b6c7d8e9f0a",
        "cl_0123456789abcdef0123456789abcdef",
        17,
        workflow_aggregate(),
        mf_web::api::commands::CommandType::WorkflowMoveNode,
        serde_json::json!({
            "node_handle": "step_0123456789abcdef0123456789abcdef",
            "x": 420,
            "y": 180
        }),
    );
    command.expected.push(ExpectedRevision {
        aggregate: workflow_aggregate(),
        semantic_revision: Some("13".into()),
        presentation_revision: Some("91".into()),
    });
    let json = serde_json::to_value(&command).unwrap();
    // 关键 wire 断言
    assert_eq!(json["schema"], "mf.command.v1");
    assert_eq!(json["type"], "workflow.move_node");
    assert_eq!(json["controller_lease_epoch"], "17");
    assert_eq!(json["expected"][0]["presentation_revision"], "91");
    refresh_fixture("commands", "move_node", &json);
    // roundtrip 语义等价
    let back: CommandEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(back, command);
}

#[test]
fn golden_command_outcomes() {
    let applied = CommandOutcomeWire::Applied {
        revisions: vec![ExpectedRevision {
            aggregate: workflow_aggregate(),
            semantic_revision: Some("14".into()),
            presentation_revision: None,
        }],
        replayed: false,
    };
    let json = serde_json::to_value(&applied).unwrap();
    assert_eq!(json["outcome"], "applied");
    assert_eq!(json["replayed"], false);
    refresh_fixture("commands", "outcome_applied", &json);
    assert_eq!(
        serde_json::from_value::<CommandOutcomeWire>(json).unwrap(),
        applied
    );

    let accepted = CommandOutcomeWire::Accepted {
        operation_handle: "op_0123456789abcdef0123456789abcdef".into(),
    };
    let json = serde_json::to_value(&accepted).unwrap();
    assert_eq!(json["outcome"], "accepted");
    refresh_fixture("commands", "outcome_accepted", &json);
    assert_eq!(
        serde_json::from_value::<CommandOutcomeWire>(json).unwrap(),
        accepted
    );
}

#[test]
fn golden_snapshot_envelope() {
    let mut snapshot = SnapshotEnvelope::new(
        "srv_golden",
        1_842,
        serde_json::json!({
            "projects": [
                {"handle": "proj_0123456789abcdef0123456789abcdef", "name": "demo"}
            ]
        }),
    );
    snapshot.cursor.stream_epoch = "ep_golden".into();
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json["schema"], "mf.snapshot.v1");
    assert_eq!(json["cursor"]["through_seq"], "1842");
    assert_eq!(json["cursor"]["stream_epoch"], "ep_golden");
    refresh_fixture("events", "snapshot_workspace", &json);
    assert_eq!(
        serde_json::from_value::<SnapshotEnvelope>(json).unwrap(),
        snapshot
    );
}

#[test]
fn golden_event_envelope() {
    let mut event = EventEnvelope::new(
        "workflow_run.needs_you",
        true,
        2_049,
        serde_json::json!({
            "run_handle": "run_0123456789abcdef0123456789abcdef",
            "reason": "awaiting_outcome"
        }),
    );
    event.stream_epoch = "ep_golden".into();
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["schema"], "mf.event.v1");
    assert_eq!(json["type"], "workflow_run.needs_you");
    assert_eq!(json["seq"], "2049");
    assert_eq!(json["critical"], true);
    refresh_fixture("events", "needs_you", &json);
    assert_eq!(
        serde_json::from_value::<EventEnvelope>(json).unwrap(),
        event
    );
}

#[test]
fn golden_problems_and_version_negotiation() {
    let revision_conflict = Problem {
        schema: "mf.problem.v1".to_string(),
        code: ProblemCode::RevisionConflict,
        message: "工作流已被更新".into(),
        trace_id: "trace_golden".into(),
        command_id: Some("018f3e2a-1b2c-7d3e-9f4a-5b6c7d8e9f0a".into()),
        retry: Some(Retry::AfterResync),
        current: Some(serde_json::json!({"semantic_revision": "13"})),
    };
    let json = serde_json::to_value(&revision_conflict).unwrap();
    assert_eq!(json["schema"], "mf.problem.v1");
    assert_eq!(json["code"], "revision_conflict");
    assert_eq!(json["retry"], "after_resync");
    assert_eq!(json["command_id"], "018f3e2a-1b2c-7d3e-9f4a-5b6c7d8e9f0a");
    refresh_fixture("problems", "revision_conflict", &json);
    assert_eq!(
        serde_json::from_value::<Problem>(json).unwrap(),
        revision_conflict
    );

    let terminal_gap = Problem::new(
        ProblemCode::TerminalHistoryGap,
        "请求的输出已被 journal 驱逐",
        Some(Retry::AfterResync),
    );
    let json = serde_json::to_value(&terminal_gap).unwrap();
    assert_eq!(json["code"], "terminal_history_gap");
    refresh_fixture("problems", "terminal_history_gap", &json);
    assert_eq!(
        serde_json::from_value::<Problem>(json).unwrap(),
        terminal_gap
    );

    // 版本协商:支持集内通过,无交集明确拒绝
    assert_eq!(negotiate_api(&["v2".into(), "v1".into()]).unwrap(), "v1");
    assert_eq!(
        negotiate_api(&["v2".into()]),
        Err(ProblemCode::UnsupportedApiVersion)
    );
    assert_eq!(
        negotiate_ws_subprotocol(&["mf-workflow.v2".into()]),
        Err(ProblemCode::UnsupportedWsSubprotocol)
    );
    assert_eq!(
        negotiate_ws_subprotocol(&["mf-terminal.v1".into()]).unwrap(),
        "mf-terminal.v1"
    );
    // 关键 problem code 的 HTTP 大类与 WS close code 稳定
    assert_eq!(ProblemCode::RevisionConflict.http_status(), 409);
    assert_eq!(ProblemCode::ResourceNotFound.http_status(), 404);
    assert_eq!(ProblemCode::FrameTooLarge.http_status(), 413);
    assert_eq!(ProblemCode::RateLimited.http_status(), 429);
    assert_eq!(mf_web::problem::close_code::RESYNC_OR_HISTORY_GAP, 4409);
    assert_eq!(mf_web::problem::close_code::FRAME_TOO_LARGE, 4413);
    assert_eq!(mf_web::problem::close_code::RATE_LIMITED, 4429);
}
