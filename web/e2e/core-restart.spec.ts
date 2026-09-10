import { test, expect, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { RestartableCore } from "./fixtures/restartable_core.ts";
import { addNode, connectNodes, clickGraphEdge, editableCanvas, readRunSnapshot, settleStep, stepItem, waitForStepStatus } from "./fixtures/helpers.ts";

test.describe.configure({ mode: "serial" });

async function openCore(page: Page, core: RestartableCore, goal?: string) {
  await page.goto(core.entryUrl);
  await expect(page.getByRole("status")).toContainText("已连接", { timeout: 20000 });
  if (goal) await page.locator(".run-card", { hasText: goal }).first().click();
}

async function createChain(page: Page, goal: string) {
  await expect(page.locator(".project-row").first()).toBeVisible();
  await page.getByRole("button", { name: "新建工作流" }).click();
  const create = page.getByRole("dialog", { name: "新建工作流" });
  await create.getByLabel("工作流名称").fill(goal);
  await create.getByLabel("首节点标题").fill("分析");
  await create.getByLabel("Agent 实例 ID").fill("codex");
  await create.getByRole("button", { name: "创建", exact: true }).click();
  await expect(page.locator(".workflow-card", { hasText: goal })).toBeVisible();
  await page.locator(".workflow-card", { hasText: goal }).getByTitle("打开 DAG 编辑器").click();
  await addNode(page, { key: "b", title: "实现", instance: "codex", review: true,
    instructions: "读取 ${nodes.start.output.report_path} 后实施" });
  await connectNodes(page, "分析", "实现");
  await page.getByRole("button", { name: "关闭编辑器" }).click();
  await page.locator(".workflow-card", { hasText: goal }).getByRole("button", { name: "启动运行" }).click();
  await page.getByPlaceholder("本次运行的目标(必填)").fill(goal);
  await page.getByRole("button", { name: "启动", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("运行已启动");
  await expect(page.locator(".run-card", { hasText: goal })).toBeVisible();
  await page.locator(".run-card", { hasText: goal }).first().click();
  await waitForStepStatus(page, "分析", "待结算");
  await settleStep(page, "分析", "分析完成", '{"report_path":"reports/original.md"}');
  await expect(stepItem(page,"实现").locator(".node-input-card summary")).toContainText("等待确认");
}

async function expectAttempts(page: Page, goal: string, counts: Record<string, number>, total: number) {
  await expect.poll(async () => {
    const snapshot = await readRunSnapshot(page, goal);
    return { counts: Object.fromEntries(snapshot.steps.map((step) => [step.key, step.attempts])), total: snapshot.agent_runs.length };
  }).toEqual({ counts, total });
}

test("entry.url 引导文件每次交换后重签：无会话的新浏览器随时直达（托盘场景）", async ({ browser }) => {
  const core = new RestartableCore();
  const entryFile = () => readFileSync(join(core.dataDir, "entry.url"), "utf8").trim();
  try {
    await core.start();
    // 启动即写入,内容与 stdout WEB_ENTRY 一致
    const bootUrl = entryFile();
    expect(bootUrl).toBe(core.entryUrl);

    // 第一个浏览器消耗启动 nonce 交换进入
    const first = await browser.newContext();
    const pageA = await first.newPage();
    await pageA.goto(core.entryUrl);
    await expect(pageA.getByRole("status")).toContainText("已连接", { timeout: 20000 });

    // 交换后文件被 Core 重签:nonce 不再是启动时那个
    await expect.poll(entryFile).not.toBe(bootUrl);
    const refreshedUrl = entryFile();
    expect(refreshedUrl).toMatch(/^http:\/\/127\.0\.0\.1(:\d+)?\/#nonce=\S+$/);

    // 托盘场景:无任何既有会话的新浏览器,用 entry.url 直达工作台
    const second = await browser.newContext();
    const pageB = await second.newPage();
    await pageB.goto(refreshedUrl);
    await expect(pageB.getByRole("status")).toContainText("已连接", { timeout: 20000 });
    await second.close();

    // 保新鲜:此后无任何交换发生,只有后台线程会写文件——等待文件
    // 换成新 nonce(短 TTL 30s → 约 10s 节奏),第三个无会话浏览器直达
    const afterB = entryFile();
    let renewedUrl = "";
    await expect.poll(() => entryFile(), { timeout: 90000 }).not.toBe(afterB);
    renewedUrl = entryFile();
    expect(renewedUrl).not.toBe(bootUrl);
    const third = await browser.newContext();
    const pageC = await third.newPage();
    await pageC.goto(renewedUrl);
    await expect(pageC.getByRole("status")).toContainText("已连接", { timeout: 20000 });
    await first.close(); await third.close();
  } finally { await core.dispose(); }
});

test("待确认和已派发待结算跨真实 Core 重启，不重复创建 attempt", async ({ browser }) => {
  const core = new RestartableCore();
  const context = await browser.newContext(); const page = await context.newPage();
  const goal = "重启-待确认与已派发";
  try {
    await core.start(); await openCore(page, core); await createChain(page, goal);
    await expectAttempts(page, goal, { start: 1, b: 0 }, 1);
    await core.restart(); await openCore(page, core, goal);
    await expectAttempts(page, goal, { start: 1, b: 0 }, 1);
    await expect(stepItem(page,"实现").locator(".node-input-card summary")).toContainText("等待确认");
    await stepItem(page,"实现").getByRole("button",{name:"确认发送(自动值)"}).click();
    await waitForStepStatus(page,"实现","待结算");
    await expectAttempts(page, goal, { start: 1, b: 1 }, 2);
    const before = await readRunSnapshot(page, goal);
    await core.restart(); await openCore(page, core, goal);
    await waitForStepStatus(page,"实现","待结算");
    await expectAttempts(page, goal, { start: 1, b: 1 }, 2);
    const after = await readRunSnapshot(page, goal);
    expect(after.agent_runs.map((run) => run.agent_run)).toEqual(before.agent_runs.map((run) => run.agent_run));
    expect(after.steps.find((step)=>step.key==="b")?.input?.business_prompt).toContain("reports/original.md");
    await settleStep(page,"实现","完成");
    await expect.poll(async()=> (await readRunSnapshot(page,goal)).status).toBe("succeeded");
  } finally { await context.close(); await core.dispose(); }
});

test("已确认但未派发：暂停期间确认，重启后仍为零 attempt", async ({ browser }) => {
  const core = new RestartableCore();
  const context = await browser.newContext(); const page = await context.newPage();
  const goal = "重启-确认未派发";
  try {
    await core.start(); await openCore(page,core); await createChain(page,goal);
    await page.getByRole("button",{name:/暂停派发/}).click();
    await expect.poll(async()=> (await readRunSnapshot(page,goal)).paused).toBe(true);
    await stepItem(page,"实现").getByRole("button",{name:"确认发送(自动值)"}).click();
    await expect.poll(async()=> (await readRunSnapshot(page,goal)).steps.find((step)=>step.key==="b")?.input?.review_state).toBe("confirmed");
    await expectAttempts(page,goal,{start:1,b:0},1);
    await core.restart(); await openCore(page,core,goal);
    const restored=await readRunSnapshot(page,goal);
    expect(restored.paused).toBe(true);
    expect(restored.steps.find((step)=>step.key==="b")?.input?.review_state).toBe("confirmed");
    await expectAttempts(page,goal,{start:1,b:0},1);
    await page.getByRole("button",{name:"恢复派发"}).click();
    await waitForStepStatus(page,"实现","待结算");
    await expectAttempts(page,goal,{start:1,b:1},2);
    await settleStep(page,"实现","完成");
    await expect.poll(async()=> (await readRunSnapshot(page,goal)).status).toBe("succeeded");
  } finally { await context.close(); await core.dispose(); }
});

test("A→C→B 图补丁提交后硬重启，继承 A 输出且只执行 C/B", async ({ browser }) => {
  const core = new RestartableCore();
  const context = await browser.newContext(); const page = await context.newPage();
  const goal = "重启-图补丁";
  try {
    await core.start(); await openCore(page,core); await createChain(page,goal);
    await page.getByRole("button",{name:/暂停派发/}).click();
    await expect.poll(async()=> (await readRunSnapshot(page,goal)).paused).toBe(true);
    const before=await readRunSnapshot(page,goal);
    await page.getByRole("button",{name:/编辑运行图/}).click();
    const editor=page.getByRole("dialog",{name:"编辑运行图"});
    await addNode(page,{key:"c",title:"检查",instance:"codex",instructions:"检查 ${nodes.start.output.report_path}"});
    await connectNodes(page,"分析","检查");
    await connectNodes(page,"检查","实现");
    const canvas=await editableCanvas(page);
    await clickGraphEdge(page,"start","b");
    await editor.getByRole("button",{name:"断开连线"}).click();
    await expect(canvas.locator('.react-flow__edge')).toHaveCount(2);
    await editor.getByRole("button",{name:"⇅ 自动布局"}).click();
    await canvas.locator(".react-flow__controls-fitview").click();
    await page.waitForTimeout(300);
    const graphScreenshot = test.info().outputPath("run-graph.png");
    await editor.screenshot({path:graphScreenshot});
    await test.info().attach("运行图编辑", { path:graphScreenshot, contentType:"image/png" });
    await editor.getByRole("button",{name:"应用新版本"}).click();
    await expect(editor).toBeHidden();
    const patched=await readRunSnapshot(page,goal);
    expect(patched.pipeline_revision.handle).not.toBe(before.pipeline_revision.handle);
    expect(patched.paused).toBe(true);
    await expectAttempts(page,goal,{start:1,b:0,c:0},1);
    await core.restart(); await openCore(page,core,goal);
    const recovered=await readRunSnapshot(page,goal);
    expect(recovered.pipeline_revision.handle).toBe(patched.pipeline_revision.handle);
    expect(recovered.paused).toBe(true);
    expect(recovered.handoffs.some((row)=>row.handoff.output.report_path==="reports/original.md")).toBe(true);
    await expectAttempts(page,goal,{start:1,b:0,c:0},1);
    await page.getByRole("button",{name:"恢复派发"}).click();
    await waitForStepStatus(page,"检查","待结算");
    const checking=await readRunSnapshot(page,goal);
    expect(checking.steps.find((step)=>step.key==="c")?.input?.business_prompt).toContain("reports/original.md");
    await expectAttempts(page,goal,{start:1,b:0,c:1},2);
    await settleStep(page,"检查","检查通过");
    await expect(stepItem(page,"实现").locator(".node-input-card summary")).toContainText("等待确认");
    await stepItem(page,"实现").getByRole("button",{name:"确认发送(自动值)"}).click();
    await waitForStepStatus(page,"实现","待结算");
    expect((await readRunSnapshot(page,goal)).steps.find((step)=>step.key==="b")?.input?.business_prompt).toContain("reports/original.md");
    await settleStep(page,"实现","完成");
    await expect.poll(async()=> (await readRunSnapshot(page,goal)).status).toBe("succeeded");
    await expectAttempts(page,goal,{start:1,b:1,c:1},3);
    const final=await readRunSnapshot(page,goal);
    expect(final.agent_runs.some((run)=>run.agent_run===before.agent_runs[0].agent_run)).toBe(true);
  } finally { await context.close(); await core.dispose(); }
});
