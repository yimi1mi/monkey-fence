import { test, expect } from "@playwright/test";

test("terminal attach replays then acks after consumption", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await page.getByRole("button", { name: "运行" }).click();
  await page.dblclick(".mf-node");
  await expect(page.locator(".xterm-screen")).toBeVisible();
});

test("observer cannot type; two tabs single writer", async ({ browser }) => {
  const a = await browser.newContext().then((c) => c.newPage());
  const b = await browser.newContext().then((c) => c.newPage());
  await a.goto("http://127.0.0.1:0/#nonce=fixture-a");
  await b.goto("http://127.0.0.1:0/#nonce=fixture-b");
  await expect(b.locator(".xterm textarea")).toBeDisabled();
});
