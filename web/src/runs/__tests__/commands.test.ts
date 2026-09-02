// T8c 契约:start 只认 semantic revision、run 命令 CAS、exit≠settlement、
// Needs You 过滤、双击 pending/created intent。
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  needsYouFilter,
  openNodeIntent,
  requiresExplicitSettlement,
  runCommand,
  sessionCommand,
} from "../commands.ts";

const ctx = { commandId: "018f3e2a-1b2c-7d3e-9f4a-5b6c7d8e9f0a", clientId: "cl_x", controllerLeaseEpoch: "3" };

test("run start only accepts confirmed semantic revision", () => {
  const start = runCommand(ctx, {
    type: "workflow.run.start" as never,
    workflowHandle: "wf_0123456789abcdef0123456789abcdef",
    runHandle: "",
    runRevision: "13",
    payload: { goal: "交付" },
  });
  assert.equal(start.expected[0].semantic_revision, "13");
  assert.equal(start.target.kind, "project_workflow");
  assert.equal(start.expected[0].presentation_revision, undefined);
});

test("run-level commands CAS run revision", () => {
  for (const type of ["workflow.run.cancel", "workflow.run.retry_step", "workflow.run.settle"]) {
    const command = runCommand(ctx, {
      type: type as never,
      runHandle: "run_0123456789abcdef0123456789abcdef",
      runRevision: "7",
      payload: {},
    });
    assert.equal(command.target.kind, "workflow_run");
    assert.equal(command.expected[0].semantic_revision, "7", type);
  }
});

test("process exit / idle / done never settle automatically", () => {
  assert(requiresExplicitSettlement("process_exit"));
  assert(requiresExplicitSettlement("terminal_idle"));
  assert(requiresExplicitSettlement("done_reported"));
  assert(!requiresExplicitSettlement("user_settle"));
});

test("needs-you filtering by reason bucket", () => {
  const items = [
    { run: "run_a", reason: "awaiting_outcome" },
    { run: "run_b", reason: "question" },
    { run: "run_c", reason: "crash_incomplete" },
  ];
  assert.equal(needsYouFilter(items, "all").length, 3);
  assert.deepEqual(needsYouFilter(items, "question"), [{ run: "run_b", reason: "question" }]);
  assert.equal(needsYouFilter(items, "lost_session").length, 0);
});

test("double-click builds navigation intent only", () => {
  const pending = openNodeIntent("run_a", "step_b", false);
  assert.equal(pending.state, "pending", "会话不存在→pending(不伪造 attach)");
  const created = openNodeIntent("run_a", "step_b", true);
  assert.equal(created.state, "created");
});

test("session start/stop commands are controller-routed envelopes", () => {
  const start = sessionCommand(ctx, {
    type: "session.start_preview" as never,
    projectHandle: "proj_0123456789abcdef0123456789abcdef",
    payload: { agent_type_id: "codex" },
  });
  assert.equal(start.type, "session.start_preview");
  assert.equal(start.target.kind, "project");
});
