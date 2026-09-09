import { build } from "esbuild";
import { chromium } from "@playwright/test";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const app = `
import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {NodeInputCard,SettleCard} from './src/workbench/run_node_forms.tsx';
const initial={status:'frozen',summary:'ready',createdAt:'input-one',agentRun:null,template:'template',resolvedInstructions:'do it',businessPrompt:'ORIGINAL_V1',protocolSegment:'',bindings:[],upstreamSummaries:[],missingRequired:[],contextPolicy:'explicit_only',reviewState:'awaiting_review',inputRevision:'1',overrides:null,effectiveBusinessPrompt:null};
function App(){const[input,setInput]=useState(initial);return <>
<button onClick={()=>setInput({...initial,inputRevision:'2',overrides:{businessPrompt:'REMOTE_V2',bindingValues:{}},effectiveBusinessPrompt:'REMOTE_V2'})}>remote update</button>
<NodeInputCard input={input} canEdit={true} stepHandle='step' stepRevision='1' onAct={async(type,payload)=>{window.sent.push({type,payload});return true;}}/>
<SettleCard disabled={false} onSettle={async()=>false}/></>}
window.sent=[];createRoot(document.getElementById('root')).render(<App/>);`;

const bundle = await build({ stdin: { contents: app, resolveDir: webRoot, sourcefile: "ui-tests.tsx", loader: "tsx" },
  bundle: true, write: false, format: "iife", jsx: "automatic", define: { "process.env.NODE_ENV": '"production"' } });
const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage();
  const pageErrors = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));
  page.setDefaultTimeout(10000);
  await page.setContent('<div id="root"></div>');
  await page.addScriptTag({ content: bundle.outputFiles[0].text });
  const card = page.locator(".node-input-card");
  await card.getByRole("button", { name: "✎ 检查并编辑本次输入" }).click();
  await card.locator("textarea").fill("DRAFT_V1");
  await page.getByRole("button", { name: "remote update" }).click();
  await page.waitForFunction(() => document.body.textContent.includes("旧草稿"));
  assert.equal(await card.getByRole("button", { name: "保存覆盖", exact: true }).isDisabled(), true);
  assert.equal(await card.getByRole("button", { name: "确认发送(含修改)" }).isDisabled(), true);
  assert.equal(await card.locator("textarea").inputValue(), "DRAFT_V1");
  assert.equal((await page.evaluate(() => window.sent)).length, 0);
  await card.getByRole("button", { name: "放弃草稿并重新加载" }).click();
  await card.getByRole("button", { name: "✎ 检查并编辑本次输入" }).click();
  assert.equal(await card.locator("textarea").inputValue(), "REMOTE_V2");
  await card.getByRole("button", { name: "保存覆盖", exact: true }).click();
  await page.waitForFunction(() => window.sent.length === 1);
  assert.equal((await page.evaluate(() => window.sent[0])).payload.input_revision, "2");
  console.log("PASS input draft conflict and explicit reload");

  const settle = page.getByRole("button", { name: "结算成功", exact: true });
  await settle.click();
  await page.waitForFunction(() => document.body.textContent.includes("结算未生效"));
  assert.equal(await settle.isEnabled(), true, "rejected settlement must remain editable");
  assert.deepEqual(pageErrors, []);
  console.log("PASS rejected settlement permits correction and resubmission");
} finally { await browser.close(); }
