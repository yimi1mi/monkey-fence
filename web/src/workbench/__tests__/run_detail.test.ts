// 运行详情视图映射测试(#74/#99):Handoff wire(内核 HandoffSnapshot)
// → RunHandoffView;settle expected 的 agent_run/agent_session 前提由
// shell act 构造,此处冻结 wire 形态不被静默破坏。
import { test } from "node:test";
import assert from "node:assert/strict";
import { runDetailViewOf } from "../run_detail.ts";

const wireRun = {
  workflow_run: "01a067d3aa5b7e42",
  revision: { revision: "5" },
  title: "巡检",
  goal: "",
  status: "needs-you",
  needs_you: true,
  reason_count: 1,
  steps: [
    {
      step: "01a067d3aa5b7e4299",
      revision: { revision: "2" },
      key: "start",
      title: "检查变更",
      instructions: "",
      agent_instance_ref: "agent-main",
      status: "succeeded",
    },
  ],
  agent_runs: [
    {
      agent_run: "01a067d3aa5b7e42aa",
      revision: { revision: "3" },
      step: "01a067d3aa5b7e4299",
      agent_session: "01a067d3aa5b7e42bb",
      status: "awaiting-outcome",
      agent_state: "idle",
      outcome: null,
    },
  ],
  agent_sessions: [
    {
      agent_session: "01a067d3aa5b7e42bb",
      revision: { revision: "4" },
      title: "检查变更",
      runtime: "acp",
      status: "live",
    },
  ],
  handoffs: [
    {
      step: "01a067d3aa5b7e4299",
      agent_run: "01a067d3aa5b7e42aa",
      handoff: {
        status: "complete",
        summary: "检查了 3 个变更文件,发现 1 个测试失败",
        changed_files: ["src/main.rs", "src/lib.rs"],
        artifacts: ["target/report.md"],
        blockers: ["依赖缺失"],
        recommendations: ["补测试"],
        output: { failed_file: "src/main.rs", checked: 3 },
      },
    },
  ],
  open_questions: [],
};

test("handoff wire 映射为交接视图", () => {
  const view = runDetailViewOf(wireRun as never);
  assert.equal(view.handoffs.length, 1);
  const handoff = view.handoffs[0];
  assert.equal(handoff.step, "01a067d3aa5b7e4299");
  assert.equal(handoff.status, "complete");
  assert.match(handoff.summary, /3 个变更文件/);
  assert.deepEqual(handoff.changedFiles, ["src/main.rs", "src/lib.rs"]);
  assert.deepEqual(handoff.blockers, ["依赖缺失"]);
  const parsed = JSON.parse(handoff.outputJson ?? "null");
  assert.equal(parsed.failed_file, "src/main.rs");
  assert.equal(parsed.checked, 3);
});

test("空 output 不产生 outputJson;缺失 handoffs 为空数组", () => {
  const withEmpty = runDetailViewOf({
    ...wireRun,
    handoffs: [
      {
        step: "01a067d3aa5b7e4299",
        handoff: { status: "fail", summary: "中断", output: {} },
      },
    ],
  } as never);
  assert.equal(withEmpty.handoffs[0].outputJson, null);
  const missing = runDetailViewOf({ ...wireRun, handoffs: undefined } as never);
  assert.deepEqual(missing.handoffs, []);
});

test("settle 前提数据可用:agent_run/session 带 revision", () => {
  const view = runDetailViewOf(wireRun as never);
  const run = view.agentRuns[0];
  assert.equal(run.revision, "3");
  assert.equal(run.agentSession, "01a067d3aa5b7e42bb");
  const session = view.sessions[0];
  assert.equal(session.revision, "4");
});
