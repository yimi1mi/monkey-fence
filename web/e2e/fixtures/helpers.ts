// 浏览器用例共享助手:真实 nonce 引导、React Flow 连线、运行步骤等待/结算。
// 选择器与 UI 文案对齐 web/src/workbench/(shell.tsx / workflow_editor.tsx)。
import { expect, type Page } from "@playwright/test";
import { newNonce } from "./core.ts";

export function baseUrl(): string {
  const value = process.env.MF_E2E_BASE;
  if (!value) {
    throw new Error("缺少 MF_E2E_BASE——用例必须经 playwright(由 globalSetup 启动真实 Core)");
  }
  return value;
}

/** 引导一个全新 Controller 页面(真实 nonce 交换 + fragment 清除)。 */
export async function openWorkbench(page: Page): Promise<void> {
  const nonce = await newNonce(baseUrl());
  await page.goto(`${baseUrl()}/#nonce=${nonce}`);
  await expect(page.getByRole("status")).toContainText("已连接", { timeout: 20_000 });
  await expect(page.getByText("Controller", { exact: true }).first()).toBeVisible();
}

export interface NodeDraftInput {
  key: string;
  title: string;
  instance?: string;
  instructions?: string;
  /** T3:派发前人工检查本次输入。 */
  review?: boolean;
}

/** 编辑器里新增节点(「＋节点」弹窗)。 */
export async function addNode(
  page: Page,
  { key, title, instance, instructions, review }: NodeDraftInput,
) {
  await page.getByRole("button", { name: "＋节点" }).click();
  const dialog = page.getByRole("dialog", { name: "添加节点" });
  await dialog.getByLabel(/节点 key/).fill(key);
  await dialog.getByLabel(/职责\(标题/).fill(title);
  if (instance) await dialog.getByLabel(/Agent 实例/).fill(instance);
  if (instructions) await dialog.getByLabel(/职责\(任务说明/).fill(instructions);
  if (review) await dialog.getByLabel(/派发前人工检查/).check();
  await dialog.getByRole("button", { name: "保存", exact: true }).click();
  const canvas = await editableCanvas(page);
  await expect(canvas.locator(`.react-flow__node[data-id="${key}"]`)).toBeVisible();
}

/** React Flow 拖拽连线:上游节点底部 source handle → 下游顶部 target handle。 */
export async function editableCanvas(page: Page) {
  const runtime = page.locator(".run-graph-editor .shared-graph-canvas");
  return await runtime.count() ? runtime : page.locator(".workflow-editor .shared-graph-canvas");
}

export async function connectNodes(page: Page, fromTitle: string, toTitle: string) {
  const canvas = await editableCanvas(page);
  await page.evaluate(() => document.fonts.ready);
  await canvas.locator(".react-flow__controls-fitview").click();
  await page.waitForTimeout(450);
  const titleNode = (title: string) => canvas.locator('.react-flow__node').filter({
    has: page.locator('.graph-node-label > strong').filter({ hasText: new RegExp("^" + title.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + "$") }),
  });
  const source = titleNode(fromTitle).locator('.react-flow__handle.source');
  const target = titleNode(toTitle).locator('.react-flow__handle.target');
  const from = await source.boundingBox(), to = await target.boundingBox();
  if (!from || !to) throw new Error(`连线句柄不可见:${fromTitle} → ${toTitle}`);
  const before = await canvas.locator('.react-flow__edge').count();
  await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
  await page.mouse.down();
  await page.mouse.move(to.x + to.width / 2, to.y + to.height / 2, { steps: 12 });
  await page.mouse.up();
  await expect(canvas.locator('.react-flow__edge')).toHaveCount(before + 1, { timeout: 15000 });
}
export function stepItem(page: Page, title: string) {
  return page.locator(".step-item").filter({ has: page.locator(".step-title").filter({ hasText: new RegExp("^" + title.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + "$") }) });
}

/** SVG 竖直边的 bounding box 可为零宽度；用路径真实中点点击命中区。 */
export async function clickGraphEdge(page: Page, source: string, target: string) {
  const canvas = await editableCanvas(page);
  await canvas.locator(".react-flow__controls-fitview").click();
  await page.waitForTimeout(250);
  const edge = canvas.locator(`.react-flow__edge[data-id="${source}->${target}"] .react-flow__edge-interaction`);
  const point = await edge.evaluate((element) => {
    const path = element as SVGPathElement;
    const midpoint = path.getPointAtLength(path.getTotalLength() / 2);
    const matrix = path.getScreenCTM();
    if (!matrix) throw new Error("edge has no screen transform");
    const screen = new DOMPoint(midpoint.x, midpoint.y).matrixTransform(matrix);
    return { x: screen.x, y: screen.y };
  });
  await page.mouse.click(point.x, point.y);
}

/** 结算当前 awaiting-outcome 步骤为成功(可带输出 JSON 满足输出约束)。 */
export async function settleStep(page: Page, title: string, summary?: string, outputJson?: string) {
  const item = stepItem(page, title);
  const complete = item.getByRole("button", { name: "结算成功" });
  await expect(complete).toBeEnabled({ timeout: 30_000 });
  if (summary) await item.getByPlaceholder("总结(可选)").fill(summary);
  if (outputJson) await item.getByPlaceholder(/输出 JSON/).fill(outputJson);
  await complete.click();
  await expect(page.getByRole("alert")).toContainText("已提交", { timeout: 15_000 });
}

/** 验证页面能自行响应 Core 变化，不通过反复重载掩盖刷新缺陷。 */
export async function waitForStepStatus(page: Page, title: string, status: string,
  { timeoutMs = 90000 }: { timeoutMs?: number } = {}) {
  await expect(stepItem(page, title).locator(".step-head .badge").first()).toHaveText(status, { timeout: timeoutMs });
}

export interface RunSnapshot {
  workflow_run: string;
  status: string;
  paused: boolean;
  needs_you: boolean;
  pipeline_revision: { handle: string; number: string };
  steps: Array<{ key: string; step: string; status: string; attempts: number;
    input: null | { status: string; review_state: string; business_prompt: string; agent_run: string | null } }>;
  agent_runs: Array<{ agent_run: string; step: string }>;
  handoffs: Array<{ handoff: { summary: string; output: Record<string, unknown> } }>;
}

/** 只读读取真实 Core 快照，以记录数/attempt 数验证幂等，不从界面文案推断。 */
export async function readRunSnapshot(page: Page, title: string): Promise<RunSnapshot> {
  return page.evaluate(async (goal) => {
    const session = JSON.parse(sessionStorage.getItem("mf.workbench.session") ?? "null");
    if (!session) throw new Error("missing browser session");
    const headers = { "X-Client-Id": session.client_id };
    const workspace = await (await fetch("/api/v1/snapshots/workspace", { headers })).json();
    for (const project of workspace.data.projects) {
      const run = project.workflow_runs.find((row: { title: string }) => row.title === goal);
      if (!run) continue;
      const response = await fetch(`/api/v1/snapshots/workflow-run/${encodeURIComponent(project.project)}/${encodeURIComponent(run.workflow_run)}`, { headers });
      if (!response.ok) throw new Error(`run snapshot ${response.status}`);
      return (await response.json()).data;
    }
    throw new Error(`run ${goal} missing`);
  }, title);
}
