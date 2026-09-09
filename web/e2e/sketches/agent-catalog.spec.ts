// T9a e2e:catalog 卡片/双安装/identity 替换/instance CRUD。
import { test, expect } from "@playwright/test";

test("catalog shows install action for absent types", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await page.getByRole("button", { name: "Agent 设置" }).click();
  await expect(page.getByRole("button", { name: /安装/ })).toBeVisible();
});

test("agent instance references pinned installation", async ({ page }) => {
  await page.goto("http://127.0.0.1:0/#nonce=fixture");
  await page.getByRole("button", { name: "新建实例" }).click();
  await expect(page.getByLabel("CLI Installation")).toBeVisible();
});
