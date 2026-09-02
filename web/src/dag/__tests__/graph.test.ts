// DAG 预检与布局契约(T8b):cycle 预检、未知依赖、确定性布局、
// position delta 预算。
import { test } from "node:test";
import assert from "node:assert/strict";
import { autoLayout, positionDelta, validateDeps, wouldCreateCycle } from "../graph.ts";

const graph = {
  nodes: [
    { id: "step_a", title: "A", instructions: "", agentInstanceId: "i", deps: [], x: 0, y: 0 },
    { id: "step_b", title: "B", instructions: "", agentInstanceId: "i", deps: ["step_a"], x: 0, y: 0 },
    { id: "step_c", title: "C", instructions: "", agentInstanceId: "i", deps: ["step_b"], x: 0, y: 0 },
  ],
};

test("cycle prediction rejects back-edge and self-dep", () => {
  assert.equal(wouldCreateCycle(graph, "step_a", ["step_c"]), true, "a→c→b→a 成环");
  assert.equal(wouldCreateCycle(graph, "step_c", ["step_a"]), false, "无环连线通过");
  assert.equal(wouldCreateCycle(graph, "step_a", ["step_a"]), true, "自连拒绝");
});

test("unknown deps are listed for rejection", () => {
  assert.deepEqual(validateDeps(graph, "step_c", ["step_a"]), []);
  assert.deepEqual(validateDeps(graph, "step_c", ["step_missing"]), ["step_missing"]);
});

test("auto layout is layered and deterministic", () => {
  const first = autoLayout(graph, "TB");
  const second = autoLayout(graph, "TB");
  assert.deepEqual(first, second, "确定性");
  const byId = new Map(first.map((m) => [m.id, m]));
  assert(byId.get("step_a")!.y < byId.get("step_b")!.y, "a 在 b 上");
  assert(byId.get("step_b")!.y < byId.get("step_c")!.y, "b 在 c 上");
});

test("position delta stays within budget shape", () => {
  const moves = Array.from({ length: 100 }, (_, i) => ({ id: `step_${i}`, x: i * 10, y: i * 20 }));
  const delta = positionDelta(moves);
  assert(delta.length <= 512 * 4, "批量增量紧凑(数组形态;实际预算在 UI 层分批)");
  assert(JSON.parse(delta).length === 100);
});
