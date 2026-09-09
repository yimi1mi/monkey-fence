// T4 e2e:暂停派发 → 上游结算后下游不启动 → 恢复后继续。
// 覆盖 workflow.run.pause / resume 命令全链路(内核事务性暂停前置)。
import { test, expect } from "@playwright/test";
import { addNode, connectNodes, openWorkbench, waitForStepStatus, stepItem } from "./fixtures/helpers.ts";

test.describe.configure({ mode: "serial" });

test("暂停后下游不派发,恢复后继续", async ({ page }) => {
  await openWorkbench(page);
  await expect(page.locator(".project-row").first()).toBeVisible({ timeout: 30_000 });

  await page.getByRole("button", { name: "新建工作流" }).click();
  const dialog = page.getByRole("dialog", { name: "新建工作流" });
  await dialog.getByLabel("工作流名称").fill("暂停验收链");
  await dialog.getByLabel("首节点标题").fill("首步");
  await dialog.getByLabel("Agent 实例 ID").fill("codex");
  await dialog.getByRole("button", { name: "创建", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("已创建", { timeout: 15_000 });

  await page
    .locator('.workflow-card:has-text("暂停验收链")')
    .getByTitle("打开 DAG 编辑器")
    .click();
  await expect(page.getByLabel("工作流编辑器")).toBeVisible();
  await page.getByRole("button", { name: "＋节点" }).click();
  const addDialog = page.getByRole("dialog", { name: "添加节点" });
  await addDialog.getByLabel(/节点 key/).fill("second");
  await addDialog.getByLabel(/职责\(标题/).fill("次步");
  await addDialog.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator('.react-flow__node:has-text("次步")')).toBeVisible();
  await connectNodes(page, "首步", "次步");
  await expect(page.locator(".editor-canvas .react-flow__edge")).toHaveCount(1);
  await expect(page.locator(".editor-canvas .react-flow__node")).toHaveCount(2);
  await page.getByRole("button", { name: "关闭编辑器" }).click();

  await page.reload();
  await expect(page.getByRole("status")).toContainText("已连接", { timeout: 20_000 });
  await page
    .locator('.workflow-card:has-text("暂停验收链")')
    .getByRole("button", { name: "启动运行" })
    .click();
  await page.getByPlaceholder("本次运行的目标(必填)").fill("暂停验收");
  await page.getByRole("button", { name: "启动", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("运行已启动", { timeout: 15_000 });
  await page.locator(".run-card").first().click();

  // 等首步待结算 → 暂停派发
  await waitForStepStatus(page, "首步", "待结算");
  await page.getByRole("button", { name: /暂停派发/ }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  await expect(page.locator(".inspector").getByText("已暂停")).toBeVisible({
    timeout: 20_000,
  });

  // 结算首步:次步不得派发(暂停 = 停止创建新 Agent Run)
  await stepItem(page, "首步").getByRole("button", { name: "结算成功" }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  await page.waitForTimeout(1500);
  // 次步保持就绪(未派发):刷新后确认无「执行中/待结算」
  await page.reload();
  await expect(page.getByRole("status")).toContainText("已连接", { timeout: 20_000 });
  await page.locator(".run-card").first().click();
  await waitForStepStatus(page, "次步", "就绪");

  // 恢复:次步派发 → 待结算 → 结算 → 运行成功
  await page.getByRole("button", { name: "恢复派发" }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  await waitForStepStatus(page, "次步", "待结算");
  await stepItem(page, "次步").getByRole("button", { name: "结算成功" }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  await expect(page.locator(".inspector .leaf-count").first()).toContainText("已成功", {
    timeout: 90_000,
  });
});

test("暂停后经面板插入节点并应用,恢复只跑新图", async ({ page }) => {
  await openWorkbench(page);
  await expect(page.locator(".project-row").first()).toBeVisible({ timeout: 30_000 });

  await page.getByRole("button", { name: "新建工作流" }).click();
  const dialog = page.getByRole("dialog", { name: "新建工作流" });
  await dialog.getByLabel("工作流名称").fill("改图验收链");
  await dialog.getByLabel("首节点标题").fill("甲");
  await dialog.getByLabel("Agent 实例 ID").fill("codex");
  await dialog.getByRole("button", { name: "创建", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("已创建", { timeout: 15_000 });

  await page.reload();
  await expect(page.getByRole("status")).toContainText("已连接", { timeout: 20_000 });
  await page
    .locator('.workflow-card:has-text("改图验收链")')
    .getByRole("button", { name: "启动运行" })
    .click();
  await page.getByPlaceholder("本次运行的目标(必填)").fill("改图验收");
  await page.getByRole("button", { name: "启动", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("运行已启动", { timeout: 15_000 });
  await page.locator(".run-card", { hasText: "改图验收" }).first().click();

  await waitForStepStatus(page, "甲", "待结算");
  // 暂停 → 编辑运行图:新增「乙」依赖「甲」
  await page.getByRole("button", { name: /暂停派发/ }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  await page.reload();
  await expect(page.getByRole("status")).toContainText("已连接", { timeout: 20_000 });
  await page.locator(".run-card", { hasText: "改图验收" }).first().click();
  await waitForStepStatus(page, "甲", "待结算");
  await page.getByRole("button", { name: /编辑运行图/ }).click();
  const graphDialog = page.getByRole("dialog", { name: "编辑运行图" });
  await expect(graphDialog).toBeVisible();
  await addNode(page, { key: "yi", title: "乙", instance: "codex", instructions: "接续甲的结果" });
  await connectNodes(page, "甲", "乙");  await graphDialog.getByRole("button", { name: "应用新版本" }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  await expect(graphDialog).toBeHidden({ timeout: 10_000 });
  await expect(graphDialog).toBeHidden({ timeout: 10_000 });

  // 结算甲(暂停期间)→ 乙不启动;恢复 → 乙派发 → 结算 → 成功
  await stepItem(page, "甲").getByRole("button", { name: "结算成功" }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  await page.getByRole("button", { name: "恢复派发" }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  await waitForStepStatus(page, "乙", "待结算");
  await stepItem(page, "乙").getByRole("button", { name: "结算成功" }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  await expect(page.locator(".inspector .leaf-count").first()).toContainText("已成功", {
    timeout: 90_000,
  });
});

test("无效连线(依赖环)在画布上被拒绝并给出可操作反馈", async ({ page }) => {
  await openWorkbench(page);
  await expect(page.locator(".project-row").first()).toBeVisible({ timeout: 30_000 });

  await page.getByRole("button", { name: "新建工作流" }).click();
  const dialog = page.getByRole("dialog", { name: "新建工作流" });
  await dialog.getByLabel("工作流名称").fill("环反馈验收");
  await dialog.getByLabel("首节点标题").fill("首");
  await dialog.getByLabel("Agent 实例 ID").fill("codex");
  await dialog.getByRole("button", { name: "创建", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("已创建", { timeout: 15_000 });

  await page
    .locator('.workflow-card:has-text("环反馈验收")')
    .getByTitle("打开 DAG 编辑器")
    .click();
  await expect(page.getByLabel("工作流编辑器")).toBeVisible();
  await page.getByRole("button", { name: "＋节点" }).click();
  const addDialog = page.getByRole("dialog", { name: "添加节点" });
  await addDialog.getByLabel(/节点 key/).fill("second");
  await addDialog.getByLabel(/职责\(标题/).fill("次");
  await addDialog.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator('.react-flow__node:has-text("次")')).toBeVisible();

  // 首 → 次(合法)
  await connectNodes(page, "首", "次");
  await expect(page.locator(".editor-canvas .react-flow__edge")).toHaveCount(1);
  await expect(page.locator(".editor-canvas .react-flow__node")).toHaveCount(2);
  // 次 → 首:成环,Core 必须拒绝;页面不残留假连线
  const beforeCycle = page.locator(".workflow-editor .react-flow__edge").count();
  await page.locator(".workflow-editor .react-flow__controls-fitview").click();
  await page.waitForTimeout(450);
  {
    const source = page
      .locator('.react-flow__node:has-text("次") .react-flow__handle.source')
      .first();
    const target = page
      .locator('.react-flow__node:has-text("首") .react-flow__handle.target')
      .first();
    const from = await source.boundingBox();
    const to = await target.boundingBox();
    if (from && to) {
      const start = { x: from.x + from.width / 2, y: from.y + from.height / 2 };
      const end = { x: to.x + to.width / 2, y: to.y + to.height / 2 };
      await page.mouse.move(start.x, start.y);
      await page.mouse.down();
      for (let step = 1; step <= 6; step++) {
        await page.mouse.move(
          start.x + (end.x - start.x) * (step / 6),
          start.y + (end.y - start.y) * (step / 6),
        );
      }
      await page.mouse.up();
    }
  }
  const _ = beforeCycle;
  await expect(page.getByRole("alert")).toContainText(/环|cycle/, { timeout: 5_000 });
  await expect(page.locator(".editor-canvas .react-flow__edge")).toHaveCount(1, { timeout: 5_000 });
  await page.getByRole("button", { name: "关闭编辑器" }).click();
});
