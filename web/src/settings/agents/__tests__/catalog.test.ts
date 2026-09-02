// T9a 契约:状态分组、双安装并存排序、identity 替换 unavailable、
// 未安装显示动作。
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  cardStatus,
  instanceAvailability,
  sortInstallations,
  type CatalogCard,
  type CatalogInstallation,
} from "../catalog.ts";

function installation(overrides: Partial<CatalogInstallation>): CatalogInstallation {
  return {
    handle: "inst_0123456789abcdef0123456789abcdef",
    agentTypeId: "codex",
    state: "detected",
    version: "1.2.3",
    source: "external",
    scope: "user",
    canonicalPath: "/usr/local/bin/codex",
    identityDigest: "abcd0123",
    ...overrides,
  };
}

test("absent card shows install action, not greyed out", () => {
  const card: CatalogCard = { agentTypeId: "codex", displayName: "Codex", installations: [] };
  assert.equal(cardStatus(card), "absent");
});

test("external and managed coexist; repair needed wins", () => {
  const external = installation({ handle: "ext", source: "external" });
  const managed = installation({ handle: "mng", source: "managed" });
  const card: CatalogCard = { agentTypeId: "codex", displayName: "Codex", installations: [external, managed] };
  assert.equal(cardStatus(card), "managed");
  const broken = installation({ handle: "brk", state: "repair_needed" });
  const brokenCard: CatalogCard = { ...card, installations: [...card.installations, broken] };
  assert.equal(cardStatus(brokenCard), "repair_needed");
});

test("managed-first ordering across scope", () => {
  const sorted = sortInstallations([
    installation({ handle: "ext-user", source: "external", scope: "user" }),
    installation({ handle: "mng-machine", source: "managed", scope: "machine" }),
    installation({ handle: "ext-machine", source: "external", scope: "machine" }),
  ]);
  assert.equal(sorted[0].handle, "mng-machine");
});

test("identity replacement makes instance unavailable", () => {
  const card: CatalogCard = {
    agentTypeId: "codex",
    displayName: "Codex",
    installations: [installation({ handle: "inst_0123456789abcdef0123456789abcdef", identityDigest: "aaaabbbb" })],
  };
  const draft = {
    name: "我的 Codex",
    agentTypeId: "codex",
    installationHandle: "inst_0123456789abcdef0123456789abcdef",
    providerProfileHandle: null,
  };
  assert.equal(instanceAvailability(draft, [card], null), "available");
  assert.equal(instanceAvailability(draft, [card], "aaaabbbb"), "available");
  assert.equal(instanceAvailability(draft, [card], "ccccdddd"), "unavailable", "目标被替换");
  const incomplete = { ...draft, installationHandle: null };
  assert.equal(instanceAvailability(incomplete, [card], null), "incomplete");
});
