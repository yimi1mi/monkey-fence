// 双 revision CAS 与乐观更新契约(T8b):语义/presentation 分轴、
// 创建/删除 collection CAS、冲突回滚。
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  beginOptimistic,
  isSemanticCommand,
  settleOptimistic,
  workflowCommand,
  workflowCreateCommand,
} from "../workflow_commands.ts";

const revisions = { semantic: "13", presentation: "91", collection: "5" };

function envelope(type: never) {
  return workflowCommand({
    commandId: "018f3e2a-1b2c-7d3e-9f4a-5b6c7d8e9f0a",
    clientId: "cl_x",
    controllerLeaseEpoch: "17",
    projectHandle: "proj_0123456789abcdef0123456789abcdef",
    workflowHandle: "wf_0123456789abcdef0123456789abcdef",
    type,
    payload: {},
    revisions,
  });
}

test("semantic commands carry semantic revision; presentation carry presentation", () => {
  const update = envelope("workflow.update_node" as never);
  assert.equal(update.expected[0].semantic_revision, "13");
  assert.equal(update.expected[0].presentation_revision, undefined);
  const move = envelope("workflow.move_node" as never);
  assert.equal(move.expected[0].presentation_revision, "91");
  assert.equal(move.expected[0].semantic_revision, undefined);
});

test("create and delete additionally CAS project collection", () => {
  const create = envelope("workflow.create" as never);
  const collectionCas = create.expected.find((e) => e.aggregate.kind === "project");
  assert(collectionCas, "collection CAS 存在");
  assert.equal(collectionCas!.semantic_revision, "5");
  const remove = envelope("workflow.delete" as never);
  assert(remove.expected.some((e) => e.aggregate.kind === "project"), "删除双 CAS");
});

test("axis classification", () => {
  assert(isSemanticCommand("workflow.connect" as never), "连线是语义");
  assert(!isSemanticCommand("workflow.viewport" as never), "viewport 是 presentation");
});

test("optimistic update rolls back on conflict with refresh hint", () => {
  const update = beginOptimistic({ title: "旧" }, { title: "新" });
  const ok = settleOptimistic(update, { ok: true });
  assert.equal(ok.state.title, "新");
  const update2 = beginOptimistic({ title: "旧" }, { title: "新" });
  const conflict = settleOptimistic(update2, { ok: false, code: "revision_conflict" });
  assert.equal(conflict.state.title, "旧", "冲突回滚");
  assert.equal(conflict.conflict, true, "提示刷新");
});

test("create command targets project with payload collection CAS and valid node", () => {
  const envelope = workflowCreateCommand(
    {
      commandId: "018f3e2a-1b2c-7d3e-9f4a-5b6c7d8e9f0a",
      clientId: "cl_x",
      controllerLeaseEpoch: "17",
      projectHandle: "proj_0123456789abcdef0123456789abcdef",
      name: "巡检 流程",
      firstNodeTitle: "第一步",
      agentInstanceId: "agent-main",
    },
    "4",
  );
  assert.equal(envelope.type, "workflow.create");
  assert.equal(envelope.target.kind, "project", "创建 target 是 project(工作流尚不存在)");
  const draft = envelope.payload.draft as Record<string, unknown>;
  // key 仅 ASCII:中文名退化为分隔符,保留 command 派生后缀防碰撞
  assert.match(String(draft.key), /^-?[a-z0-9-]*-018f3e$/, "key 为 ASCII slug + 命令后缀");
  assert.equal(draft.key, "workflow-018f3e");
  const node = (draft.nodes as Array<Record<string, unknown>>)[0];
  assert.equal(node.agent_instance_id, "agent-main");
  assert.equal(node.title, "第一步");
  assert.equal(envelope.payload.expected_collection_revision, "4", "collection CAS 在 payload(kernel_bridge 口径)");
});
