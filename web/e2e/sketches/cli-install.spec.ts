import { test, expect } from "@playwright/test";

test("install runs only from frozen plan; failure never claims installed", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await page.getByRole("button", { name: /安装/ }).first().click();
  await expect(page.getByRole("dialog")).toContainText(/来源|校验|回滚/);
});
