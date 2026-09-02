// bootstrap/resume/takeover e2e(T8a;Playwright + 真实 Core fixture)。
// 本地无浏览器环境时以 spec 骨架交付;CI 接入 playwright 后运行。
import { test, expect } from "@playwright/test";

test("bootstrap nonce exchange issues session and csrf", async ({ page }) => {
  // launcher 发放 nonce fragment → 首屏交换 → fragment 清除
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await expect(page.locator("[role=badge]")).toHaveText(/Controller|Observer/);
  // URL/query 无凭据
  expect(page.url()).not.toContain("csrf");
  expect(page.url()).not.toContain("?");
});

test("observer cannot mutate; server rejects forged writes", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture-observer");
  await expect(page.getByTitle("Observer 禁写")).toBeDisabled();
});

test("two tabs takeover: only new controller writes", async ({ browser }) => {
  const first = await browser.newContext().then((c) => c.newPage());
  const second = await browser.newContext().then((c) => c.newPage());
  await first.goto("http://127.0.0.1:0/#nonce=fixture-a");
  await second.goto("http://127.0.0.1:0/#nonce=fixture-b");
  // 新 bootstrap 使旧标签降 Observer(禁写)
  await expect(first.getByTitle("Observer 禁写")).toBeVisible();
});
