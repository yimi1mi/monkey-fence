// #multi-folder 验收:一主多附。
// - 设置页「文件夹」列:primary 徽标 + 附加 chips + 添加/移除入口。
// - 真实 Core 路由:POST/DELETE /api/v1/projects/{handle}/folders(页面
//   会话凭据发起,与 UI 同一 Cookie/CSRF 面)。
// - 快照投影:folders 随 workspace 快照下发;代码浏览顶部出现文件夹
//   切换器;主文件夹不可移除。
import { test, expect } from "@playwright/test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openWorkbench } from "./fixtures/helpers.ts";

test.describe.configure({ mode: "serial" });

async function openProjectSettings(page: import("@playwright/test").Page) {
  await page.getByRole("button", { name: "设置" }).click();
  await page.getByRole("button", { name: "项目", exact: true }).click();
  await expect(page.locator(".project-table-row").first()).toBeVisible({ timeout: 15_000 });
}

/** 以页面会话凭据调用文件夹 API(与 UI 同一 Cookie/CSRF 面)。 */
async function folderApi(
  page: import("@playwright/test").Page,
  projectHandle: string,
  method: "POST" | "DELETE",
  path?: string,
) {
  return await page.evaluate(
    async ({ projectHandle, method, path }) => {
      const session = JSON.parse(
        sessionStorage.getItem("mf.workbench.session") ?? "{}",
      ) as { client_id?: string; csrf_token?: string };
      const url =
        method === "POST"
          ? `/api/v1/projects/${encodeURIComponent(projectHandle)}/folders`
          : `/api/v1/projects/${encodeURIComponent(projectHandle)}/folders?path=${encodeURIComponent(path ?? "")}`;
      const response = await fetch(url, {
        method,
        headers: {
          ...(method === "POST" ? { "Content-Type": "application/json" } : {}),
          "X-CSRF-Token": session.csrf_token ?? "",
          "X-Client-Id": session.client_id ?? "",
        },
        body: method === "POST" ? JSON.stringify({ path }) : undefined,
      });
      return { status: response.status, body: await response.json().catch(() => null) };
    },
    { projectHandle, method, path },
  );
}

test("项目可添加/移除附加文件夹,主文件夹不可移除", async ({ page }) => {
  await openWorkbench(page);
  await openProjectSettings(page);

  // 验收沙箱项目行:文件夹摘要(主文件夹名,单行省略)+ 添加入口可见
  const row = page.locator(".project-table-row:not(.head)").first();
  await expect(row.locator(".folders-toggle")).toBeVisible();
  // 真实 handle 从 workspace 快照取(行内 title 只是截断展示)
  const projectHandle = await page.evaluate(async () => {
    const response = await fetch("/api/v1/snapshots/workspace");
    const envelope = (await response.json()) as {
      data?: { projects?: Array<{ project?: string }> };
    };
    return envelope.data?.projects?.[0]?.project ?? "";
  });
  expect(projectHandle).toMatch(/^proj_/);
  // 展开明细行:唯一一行且为主文件夹,无移除按钮
  await row.locator(".folders-toggle").click();
  const detail = page.locator(".folders-detail");
  await expect(detail.locator(".pfd-row")).toHaveCount(1);
  await expect(detail.locator(".pfd-kind.primary")).toHaveText("主");
  await expect(detail.locator(".pfd-row .icon-btn.danger")).toHaveCount(0);

  // 真实路由:添加附加文件夹(fixture 重定向 TEMP,落在本次运行数据目录)
  const extra = mkdtempSync(join(tmpdir(), "mf-extra-folder-"));
  const added = await folderApi(page, projectHandle, "POST", extra);
  expect(added.status).toBe(200);
  expect(added.body?.folders?.length).toBe(2);
  expect(added.body?.folders?.[0]?.kind).toBe("primary");
  expect(added.body?.folders?.[1]?.kind).toBe("additional");

  // 重复添加幂等
  const again = await folderApi(page, projectHandle, "POST", extra);
  expect(again.status).toBe(200);
  expect(again.body?.folders?.length).toBe(2);

  // 快照下发 → 摘要出现 +1 徽标;展开明细可见主/附两行
  await page.reload();
  await expect(page.getByRole("status")).toContainText("已连接", { timeout: 20_000 });
  await openProjectSettings(page);
  const refreshed = page.locator(".project-table-row:not(.head)").first();
  await expect(refreshed.locator(".folders-badge")).toHaveText("+1");
  await refreshed.locator(".folders-toggle").click();
  const detailAfter = page.locator(".folders-detail");
  await expect(detailAfter.locator(".pfd-row")).toHaveCount(2);
  await expect(detailAfter.locator(".pfd-kind.primary")).toHaveText("主");

  // 代码浏览:多文件夹时顶部出现切换器(两个选项,主在首位)——
  // 从设置页行的「浏览代码」按钮打开(工作流分组头无运行时不渲染)
  await refreshed.getByTitle("浏览代码").click();
  const switcher = page.getByLabel("切换项目文件夹");
  await expect(switcher).toBeVisible();
  await expect(switcher.locator("option")).toHaveCount(2);
  await expect(switcher.locator("option").first()).toContainText("主");
  await page.getByRole("dialog", { name: "代码浏览" }).getByRole("button", { name: "关闭" }).click();

  // 主文件夹移除被拒绝(注册表校验;主路径取明细行全路径)
  const primaryPath = await detailAfter
    .locator(".pfd-row")
    .first()
    .locator(".pfd-path")
    .getAttribute("title");
  const denied = await folderApi(page, projectHandle, "DELETE", primaryPath ?? "");
  expect(denied.status).toBe(422);
  expect(JSON.stringify(denied.body)).toContain("project_folder_primary_immutable");

  // 附加文件夹可经明细行 ✕ 移除,行数回落为 1
  await detailAfter.locator(".pfd-row .icon-btn.danger").click();
  await expect(page.locator(".folders-detail .pfd-row")).toHaveCount(1);
  const removed = await folderApi(page, projectHandle, "DELETE", extra);
  // 已在 UI 移除,API 重复移除按未知路径拒绝(非 200)
  expect(removed.status).not.toBe(200);
});
