// T9d 契约:restart 强制 off、徽标语义、新请求拒、指示灯。
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  adminBadge,
  newElevatedRequestAllowed,
  rootIndicatorVisible,
  rootStateAfterCoreRestart,
} from "../mode.ts";

test("core restart forces root mode off", () => {
  const on = { enabled: true, rootEpoch: "3", authorizing: false };
  const after = rootStateAfterCoreRestart(on);
  assert.equal(after.enabled, false);
  assert.equal(after.rootEpoch, null);
  assert.equal(after.authorizing, false);
});

test("admin badge persists for objects launched under root even after disable", () => {
  assert(adminBadge({ rootModeEnabled: true, launchedUnderRoot: true }));
  assert(adminBadge({ rootModeEnabled: false, launchedUnderRoot: true }), "既有高权限对象仍带徽标");
  assert(!adminBadge({ rootModeEnabled: true, launchedUnderRoot: false }));
});

test("new elevated requests rejected once disabled", () => {
  assert(newElevatedRequestAllowed({ enabled: true, rootEpoch: "3", authorizing: false }));
  assert(!newElevatedRequestAllowed({ enabled: false, rootEpoch: null, authorizing: false }));
});

test("root indicator stays visible while authorizing", () => {
  assert(rootIndicatorVisible({ enabled: false, rootEpoch: null, authorizing: true }));
  assert(!rootIndicatorVisible({ enabled: false, rootEpoch: null, authorizing: false }));
});
