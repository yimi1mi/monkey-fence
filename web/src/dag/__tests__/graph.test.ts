// DAG 预检与布局契约(T8b):cycle 预检、未知依赖、确定性布局、
// position delta 预算。
import { test } from "node:test";
import assert from "node:assert/strict";
import { autoLayout, positionDelta, validateDeps, wireGraph, wouldCreateCycle } from "../graph.ts";

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

// 真实 wire 形态:handle 是不透明 UUID,key 是语义依赖命名空间——
// 两个命名空间不同值,混用即分层失效(T0 回归)。
const wire = [
  { handle: "0192a1b0-anal", key: "analysis", deps: [] },
  { handle: "0192a1b0-impl", key: "impl", deps: ["analysis"] },
  { handle: "0192a1b0-revw", key: "review", deps: ["impl"] },
];

test("wireGraph translates key deps into handle namespace for A→B→C", () => {
  const graph = wireGraph(wire);
  assert.deepEqual(
    graph.nodes.map((node) => node.deps),
    [[], ["0192a1b0-anal"], ["0192a1b0-impl"]],
    "deps 全部翻译为 handle",
  );
  const laid = new Map(autoLayout(graph, "TB").map((p) => [p.id, p.y]));
  assert(laid.get("0192a1b0-anal")! < laid.get("0192a1b0-impl")!, "三层:A 上 B 下");
  assert(laid.get("0192a1b0-impl")! < laid.get("0192a1b0-revw")!, "三层:B 上 C 下");
});

test("wireGraph handles parallel join A/B→C on real wire shape", () => {
  const graph = wireGraph([
    { handle: "h-a", key: "a", deps: [] },
    { handle: "h-b", key: "b", deps: [] },
    { handle: "h-c", key: "c", deps: ["a", "b"] },
  ]);
  const laid = new Map(autoLayout(graph, "TB").map((p) => [p.id, p]));
  assert(laid.get("h-a")!.y === laid.get("h-b")!.y, "A/B 同层");
  assert(laid.get("h-c")!.y > laid.get("h-a")!.y, "C 在汇合层之下");
  assert(laid.get("h-a")!.x !== laid.get("h-b")!.x, "同层节点横向错开");
});

test("wireGraph drops unresolvable or self deps instead of misplacing", () => {
  const graph = wireGraph([
    { handle: "h-a", key: "a", deps: ["ghost", "a"] },
    { handle: "h-b", key: "b", deps: ["a"] },
  ]);
  assert.deepEqual(graph.nodes[0].deps, [], "未知 key 与自连被过滤");
});
