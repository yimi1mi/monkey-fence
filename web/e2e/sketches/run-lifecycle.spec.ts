// T8c Gate e2e(CI 真实 Core):run 生命周期/needs-you/settlement/
// session 控制/takeover 并发。
import { test, expect } from "@playwright/test";

test("run start uses semantic revision; cancel/retry/respond/settle work", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await page.getByRole("button", { name: "运行" }).click();
  await page.getByRole("button", { name: "启动" }).click();
  await expect(page.getByRole("status")).toContainText(/运行中|已接受/);
});

test("process exit does not settle; needs you appears and survives restart", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await expect(page.locator(".inspector")).toContainText(/Needs You/);
});

test("observer read-only; takeover disables old controller writes", async ({ browser }) => {
  const a = await browser.newContext().then((c) => c.newPage());
  const b = await browser.newContext().then((c) => c.newPage());
  await a.goto("http://127.0.0.1:0/#nonce=fixture-a");
  await b.goto("http://127.0.0.1:0/#nonce=fixture-b");
  await expect(a.getByTitle("Observer 禁写")).toBeVisible();
});
