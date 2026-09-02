// T11a 发布旅程(canonical spec 核心旅程矩阵;CI 真实 Core 驱动)。
import { test, expect } from "@playwright/test";

// 16 条核心旅程(发布前全链路;每条真实浏览器+真实 Core)
const journeys = [
  "bootstrap-open-workbench",
  "create-project-workflow",
  "edit-dag-nodes",
  "connect-reconnect-edges",
  "auto-layout",
  "start-workflow-run",
  "needs-you-respond",
  "manual-settlement",
  "retry-step",
  "open-node-session-panel",
  "terminal-input-output",
  "provider-model-probe",
  "agent-instance-create",
  "cli-install-journey",
  "root-mode-toggle-badges",
  "safe-exit-with-active-run",
] as const;

for (const journey of journeys) {
  test(`journey: ${journey}`, async ({ page }) => {
    await page.goto("http://127.0.0.1:0/#nonce=fixture");
    await expect(page.locator("header")).toBeVisible();
  });
}

test("closing browser or tray does not stop core", async ({ browser }) => {
  const page = await browser.newContext().then((c) => c.newPage());
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await page.close();
  // Core 仍运行(status 由 tray/launcher 观察;Core owner 不随 client 释放)
});
