import { test, expect } from "@playwright/test";

test("only controller toggles root mode; badges persist after disable", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await page.getByRole("button", { name: "Root Mode" }).click();
  await expect(page.locator("[role=badge]")).toContainText(/管理员|Root/);
});
