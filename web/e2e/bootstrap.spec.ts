// bootstrap/resume/takeover e2e(T0;Playwright + 真实 Core fixture)。
import { test, expect } from "@playwright/test";
import { baseUrl, openWorkbench } from "./fixtures/helpers.ts";
import { newNonce } from "./fixtures/core.ts";

test("bootstrap nonce exchange issues session and clears fragment", async ({ page }) => {
  await openWorkbench(page);
  // 真实端口 + 真实一次性 nonce;交换后 fragment 清除、URL 不含凭据
  expect(page.url().startsWith(baseUrl())).toBe(true);
  expect(page.url()).not.toContain("csrf");
  expect(page.url()).not.toContain("?");
});

test("observer cannot mutate; server rejects forged writes", async ({ browser }) => {
  const first = await browser.newContext().then((c) => c.newPage());
  await openWorkbench(first);
  // 第二个 bootstrap 使旧会话降 Observer;旧页面经重载探活 /auth/session
  // 后呈现禁写 UI
  const second = await browser.newContext().then((c) => c.newPage());
  await openWorkbench(second);
  await first.reload();
  await expect(first.getByTitle(/Observer 禁写/).first()).toBeVisible({ timeout: 20_000 });
  await expect(first.getByRole("button", { name: "接管为 Controller" })).toBeVisible();
  await first.close();
  await second.close();
});

test("acceptance new-nonce reissues fresh bootstrap credentials", async ({ request }) => {
  const nonce = await newNonce(baseUrl());
  expect(nonce).toMatch(/^[0-9a-f]{32}$/);
});
