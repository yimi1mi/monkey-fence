// T9c 契约:冻结三元组提交、无 argv 泄漏、终态判定、pin 阻断。
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  destructiveActionBlocked,
  installTicketCommand,
  isTerminalSuccess,
  needsUserRepair,
  type InstallPreviewUi,
} from "../lifecycle.ts";

const preview: InstallPreviewUi = {
  planHandle: "iplan_0123456789abcdef0123456789abcdef",
  recipeDigest: "a".repeat(64),
  catalogRevision: "7",
  source: "npm",
  argvSummary: ["install", "-g", "pkg@1.2.3"],
  targetDirectory: "/managed/codex",
  checksum: "abcd0123",
  rollbackAvailable: true,
  pinnedRevisions: 0,
  requiresElevation: false,
};

const ctx = { commandId: "018f3e2a-1b2c-7d3e-9f4a-5b6c7d8e9f0a", clientId: "cl_x", controllerLeaseEpoch: "3" };

test("command carries only the frozen triple; no argv in payload", () => {
  const command = installTicketCommand(ctx, preview, "install");
  assert.equal(command.payload.install_plan_handle, preview.planHandle);
  assert.equal(command.payload.recipe_digest, preview.recipeDigest);
  assert.equal(command.payload.catalog_revision, preview.catalogRevision);
  const json = JSON.stringify(command);
  assert(!json.includes("install -g"), "payload 不含解析 argv");
  assert(!json.includes("npm"), "payload 不含来源程序");
});

test("only installed is success; repair_needed needs user", () => {
  assert(isTerminalSuccess("installed"));
  for (const state of ["queued", "resolving", "downloading", "executing", "verifying", "failed", "cancelled"] as const) {
    assert(!isTerminalSuccess(state), `${state} 不是成功`);
  }
  assert(needsUserRepair("repair_needed"));
});

test("destructive actions blocked while revisions pinned", () => {
  assert(destructiveActionBlocked(2));
  assert(!destructiveActionBlocked(0));
});
