import { test, expect, type Page } from "@playwright/test";
import { addNode, connectNodes, readRunSnapshot, settleStep, stepItem, waitForStepStatus } from "./fixtures/helpers.ts";
import { RestartableCore } from "./fixtures/restartable_core.ts";

// 独立场景使用独立 Core，避免整套测试的新登录挤占同一来源的正常登录限流。
let core: RestartableCore;
test.beforeAll(async () => { core = new RestartableCore(); await core.start(); });
test.afterAll(async () => { await core?.dispose(); });

async function workflowSnapshot(page: Page, name: string) {
  return page.evaluate(async (name) => {
    const session = JSON.parse(sessionStorage.getItem("mf.workbench.session") ?? "null");
    const headers = { "X-Client-Id": session.client_id };
    const workspace = await (await fetch("/api/v1/snapshots/workspace", { headers })).json();
    for (const project of workspace.data.projects) {
      const workflow = project.workflows.find((row: { name: string }) => row.name === name);
      if (workflow) {
        const response = await fetch(`/api/v1/snapshots/workflow/${project.project}/${workflow.workflow}`, { headers });
        if (!response.ok) throw new Error(`workflow snapshot ${response.status}`);
        return { workflow: (await response.json()).data, runs: project.workflow_runs.length };
      }
    }
    throw new Error("workflow missing");
  }, name);
}

test("未保存节点可用嵌套示例试算，正式运行使用真实交接且允许修正结算", async ({ page }) => {
  const name = "模板试算与真实交接";
  await page.goto(core.entryUrl);
  await expect(page.getByRole("status")).toContainText("已连接");
  await page.getByRole("button", { name: "新建工作流" }).click();
  const create = page.getByRole("dialog", { name: "新建工作流" });
  await create.getByLabel("工作流名称").fill(name);
  await create.getByLabel("首节点标题").fill("分析样本");
  await create.getByLabel("Agent 实例 ID").fill("codex");
  await create.getByRole("button", { name: "创建", exact: true }).click();
  await page.locator(".workflow-card", { hasText: name }).getByTitle("打开 DAG 编辑器").click();
  await addNode(page, { key: "b", title: "消费报告", instance: "codex", review: true });
  await connectNodes(page, "分析样本", "消费报告");
  await page.locator('.workflow-editor .react-flow__node').filter({ hasText: "消费报告" }).click();
  await page.getByRole("button", { name: "编辑节点", exact: true }).click();
  const edit = page.getByRole("dialog", { name: "编辑节点" });
  await edit.getByLabel(/职责\(任务说明/).fill("读取 ${inputs.report}；上游摘要 ${nodes.start.summary}");
  await edit.getByRole("button", { name: "＋映射" }).click();
  await edit.getByLabel("映射名", { exact: true }).fill("report");
  await edit.getByLabel("来源节点", { exact: true }).selectOption("start");
  await edit.getByLabel("字段路径", { exact: true }).fill("output.nested.path");
  await edit.locator("#mf-node-schema").fill('{"type":"object","properties":{"verdict":{"type":"string"}},"required":["verdict"]}');
  const before = await workflowSnapshot(page, name);
  await edit.locator(".template-preview > summary").click();
  await edit.getByLabel("任务目标（示例）").fill("只用于试算的目标");
  await edit.getByLabel("示例上游交接（JSON）").fill('{"start":{"summary":"示例摘要","output":{"nested":{"path":"SAMPLE_ONLY.md"}}}}');
  await edit.getByRole("button", { name: "试算", exact: true }).click();
  await expect(edit.getByLabel("模板试算结果")).toContainText("SAMPLE_ONLY.md");
  await expect(edit.getByLabel("模板试算结果")).toContainText("示例摘要");
  await expect(edit.getByLabel("模板试算结果")).toContainText("verdict");
  expect(await workflowSnapshot(page, name)).toEqual(before);
  await edit.getByLabel("示例上游交接（JSON）").fill('{"start":{"summary":"缺少字段","output":{}}}');
  await edit.getByRole("button", { name: "试算", exact: true }).click();
  await expect(edit.getByLabel("模板试算结果")).toContainText("缺失必填:report");
  await edit.getByRole("button", { name: "保存", exact: true }).click();
  await expect(edit).toBeHidden();
  await page.getByRole("button", { name: "关闭编辑器" }).click();
  await page.locator(".workflow-card", { hasText: name }).getByRole("button", { name: "启动运行" }).click();
  await page.getByPlaceholder("本次运行的目标(必填)").fill(name);
  await page.getByRole("button", { name: "启动", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("运行已启动");
  await page.locator(".run-card", { hasText: name }).first().click();
  await waitForStepStatus(page, "分析样本", "待结算");
  await settleStep(page, "分析样本", "真实摘要", '{"nested":{"path":"ACTUAL_REPORT.md"}}');
  await expect(stepItem(page, "消费报告").locator(".node-input-card summary")).toContainText("等待确认");
  const prepared = (await readRunSnapshot(page, name)).steps.find((step) => step.key === "b")!;
  expect(prepared.attempts).toBe(0);
  expect(prepared.input?.business_prompt).toContain("ACTUAL_REPORT.md");
  expect(prepared.input?.business_prompt).not.toContain("SAMPLE_ONLY.md");
  await stepItem(page, "消费报告").getByRole("button", { name: "确认发送(自动值)" }).click();
  await waitForStepStatus(page, "消费报告", "待结算");
  await stepItem(page, "消费报告").getByRole("button", { name: "结算成功", exact: true }).click();
  await expect(stepItem(page, "消费报告")).toContainText("结算未生效");
  await expect(stepItem(page, "消费报告").getByRole("button", { name: "结算成功", exact: true })).toBeEnabled();
  await settleStep(page, "消费报告", "审查通过", '{"verdict":"pass"}');
  await expect.poll(async () => (await readRunSnapshot(page, name)).status).toBe("succeeded");
  expect((await readRunSnapshot(page, name)).agent_runs).toHaveLength(2);
});
