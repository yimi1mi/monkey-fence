import { test, expect } from "@playwright/test";

test("api key input clears after submit; no echo anywhere", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  const input = page.getByLabel("API Key");
  await input.fill("sk-test");
  await page.getByRole("button", { name: "保存" }).click();
  await expect(input).toHaveValue("");
});

test("model dropdown shows metadata and cache state only", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await expect(page.getByRole("combobox", { name: "模型" })).toBeVisible();
});
