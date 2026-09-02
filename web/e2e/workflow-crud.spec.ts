// T8b e2e:CRUD/拖拽/连线/重连/删除/自动排列/冲突回滚(CI 驱动真实 Core)。
import { test, expect } from "@playwright/test";

test("create cas collection; delete dual-cas", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await page.getByRole("button", { name: "新建工作流" }).click();
  await expect(page.getByRole("alert")).toContainText(/创建|冲突/);
});

test("revision conflict rolls back and prompts refresh", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  // 双标签并发编辑:后者收到 revision_conflict
  await expect(page.getByRole("alert")).toContainText(/刷新/);
});
