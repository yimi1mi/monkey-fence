// T0 验收主场景(真实浏览器 + 真实 Core + mock echo Agent):
// 创建三节点工作流 → 连线 → 自动布局 → 保存(重载) → 启动运行 →
// 逐节点 mock 结算 → 运行成功。
import { test, expect, type Page } from "@playwright/test";
import {
  addNode,
  connectNodes,
  openWorkbench,
  settleStep,
  stepItem,
  waitForStepStatus,
} from "./fixtures/helpers.ts";

test.describe.configure({ mode: "serial" });

const WF_NAME = "三节点验收链";

async function openEditor(page: Page) {
  await page
    .locator(`.workflow-card:has-text("${WF_NAME}")`)
    .getByTitle("打开 DAG 编辑器")
    .click();
  await expect(page.getByLabel("工作流编辑器")).toBeVisible();
}

async function nodeTop(page: Page, title: string) {
  const box = await page.locator(`.react-flow__node:has-text("${title}")`).first().boundingBox();
  if (!box) throw new Error(`节点不可见:${title}`);
  return box.y;
}

test("创建三节点工作流并连线", async ({ page }) => {
  await openWorkbench(page);
  // 验收沙箱项目自动注册;等左侧项目出现
  await expect(page.locator(".project-row").first()).toBeVisible({ timeout: 30_000 });

  await page.getByRole("button", { name: "新建工作流" }).click();
  const dialog = page.getByRole("dialog", { name: "新建工作流" });
  await dialog.getByLabel("工作流名称").fill(WF_NAME);
  await dialog.getByLabel("首节点标题").fill("分析");
  await dialog.getByLabel("Agent 实例 ID").fill("codex");
  await dialog.getByRole("button", { name: "创建", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("已创建", { timeout: 15_000 });

  await openEditor(page);
  await expect(page.locator(".editor-canvas .react-flow__node")).toHaveCount(1);
  await addNode(page, { key: "impl", title: "实现", instance: "codex", review: true });
  await addNode(page, { key: "review", title: "审查", instance: "codex" });
  await expect(page.locator(".editor-canvas .react-flow__node")).toHaveCount(3);

  // T1:职责扩展字段经表单 → 内核命令 → 快照往返
  await page.locator('.react-flow__node:has-text("分析")').first().click();
  await page.getByRole("button", { name: "编辑节点" }).click();
  const editDialog = page.getByRole("dialog", { name: "编辑节点" });
  await editDialog.getByLabel(/验收说明/).fill("覆盖目标、范围外与风险");
  await editDialog
    .locator("#mf-node-schema")
    .fill('{"type":"object","properties":{"report_path":{"type":"string"}},"required":["report_path"]}');
  await editDialog.getByRole("button", { name: "保存", exact: true }).click();
  await expect(editDialog).toBeHidden({ timeout: 10_000 });
  // 重新打开:字段从权威快照回填
  await page.locator('.react-flow__node:has-text("分析")').first().click();
  await page.getByRole("button", { name: "编辑节点" }).click();
  await expect(editDialog.getByLabel(/验收说明/)).toHaveValue("覆盖目标、范围外与风险");
  await editDialog.getByRole("button", { name: "取消" }).click();

  await connectNodes(page, "分析", "实现");
  await connectNodes(page, "实现", "审查");
  await expect(page.locator(".editor-canvas .react-flow__edge")).toHaveCount(2);
});

test("自动布局按依赖分层(A→B→C 三层)", async ({ page }) => {
  await openWorkbench(page);
  await openEditor(page);
  await expect(page.locator(".editor-canvas .react-flow__node")).toHaveCount(3);

  await page.getByRole("button", { name: "⇅ 自动布局" }).click();
  await expect(page.getByRole("alert")).toContainText("已按依赖层级自动布局", { timeout: 30_000 });

  const yAnalysis = await nodeTop(page, "分析");
  const yImpl = await nodeTop(page, "实现");
  const yReview = await nodeTop(page, "审查");
  // key/handle 混用未修复时 B/C 同层;修复后三层各差一个层距
  expect(yImpl - yAnalysis).toBeGreaterThan(100);
  expect(yReview - yImpl).toBeGreaterThan(100);
});

test("重载后图与数据一致(节点/边/位置持久)", async ({ page }) => {
  await openWorkbench(page);
  await openEditor(page);
  await expect(page.locator(".editor-canvas .react-flow__node")).toHaveCount(3);
  await expect(page.locator(".editor-canvas .react-flow__edge")).toHaveCount(2);

  const yAnalysis = await nodeTop(page, "分析");
  const yImpl = await nodeTop(page, "实现");
  const yReview = await nodeTop(page, "审查");
  expect(yImpl - yAnalysis).toBeGreaterThan(100);
  expect(yReview - yImpl).toBeGreaterThan(100);

  await page.getByRole("button", { name: "关闭编辑器" }).click();
  await expect(
    page.locator(`.workflow-card:has-text("${WF_NAME}")`).getByTitle("打开 DAG 编辑器"),
  ).toBeVisible();
});

test("启动运行并逐节点 mock 结算到成功", async ({ page }) => {
  await openWorkbench(page);
  await page
    .locator(`.workflow-card:has-text("${WF_NAME}")`)
    .getByRole("button", { name: "启动运行" })
    .click();
  await page.getByPlaceholder("本次运行的目标(必填)").fill("T0 真实浏览器验收");
  await page.getByRole("button", { name: "启动", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("运行已启动", { timeout: 15_000 });

  await page.locator(".run-card").first().click();
  await waitForStepStatus(page, "分析", "待结算");
  // T2:冻结输入只读卡(已发送)随步骤展示
  await expect(page.locator(".node-input-card summary").first()).toContainText(
    "节点输入 · 已发送",
  );
  await page.locator(".node-input-card summary").first().click();
  await expect(page.locator(".node-input-section pre").first()).toContainText(
    "覆盖目标、范围外与风险",
  );
  // 分析节点带输出约束(T1):结算须提供满足 schema 的输出 JSON
  await settleStep(
    page,
    "分析",
    "分析完成",
    '{"report_path":"reports/analysis.md"}',
  );

  // T3:实现节点启用了人工检查——分析完成后不自动派发,等待确认
  const implItem = () => stepItem(page, "实现");
  await waitForStepStatus(page, "实现", "就绪");
  await expect(implItem().locator(".node-input-card summary")).toContainText("等待确认");
  // 重载(模拟断线/刷新):待确认输入仍在
  await page.reload();
  await expect(page.getByRole("status")).toContainText("已连接", { timeout: 20_000 });
  await page.locator(".run-card").first().click();
  await waitForStepStatus(page, "实现", "就绪");
  // 检查并编辑本次输入 → 确认发送(含修改)
  await implItem().getByRole("button", { name: /检查并编辑本次输入/ }).click();
  await implItem().locator(".gate-editor textarea").fill("实现节点:按用户修改后的输入执行,报告写入 reports/impl.md");
  await implItem().getByRole("button", { name: "确认发送(含修改)" }).click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
  // 确认后派发一次;实际发送内容含用户修改
  await waitForStepStatus(page, "实现", "待结算");
  await expect(implItem().locator(".node-input-card summary")).toContainText("已发送");
  await expect(implItem().locator(".node-input-section").first()).toContainText(
    "按用户修改后的输入执行",
  );
  await settleStep(page, "实现", "实现完成");

  await waitForStepStatus(page, "审查", "待结算");
  await settleStep(page, "审查", "审查通过");

  // 运行级状态徽章(inspector 头部 leaf-count)收敛到已成功
  await expect(page.locator(".inspector .leaf-count").first()).toContainText("已成功", {
    timeout: 90_000,
  });
});
