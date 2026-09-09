const path = require('node:path');
const repoRoot = path.resolve(__dirname, '../../..');
const fs = require('node:fs');
const esbuild = require(path.join(repoRoot,'web/node_modules/esbuild'));
const ts = require(path.join(repoRoot,'web/node_modules/typescript'));
const { chromium } = require(path.join(repoRoot,'web/node_modules/@playwright/test'));
const source = fs.readFileSync(path.join(repoRoot,'web/src/workbench/shell.tsx'),'utf8');
const ast = ts.createSourceFile('shell.tsx',source,ts.ScriptTarget.Latest,true,ts.ScriptKind.TSX);
const component = ast.statements.find(n=>ts.isFunctionDeclaration(n)&&n.name?.text==='NodeInputCard').getFullText(ast);
const app = `
import React, {useState,useRef,useEffect,useMemo} from 'react';
import {createRoot} from 'react-dom/client';
${component}
const initial={status:'frozen',summary:'ready',createdAt:'',agentRun:null,template:'base template',resolvedInstructions:'base instruction',businessPrompt:'ORIGINAL_V1',protocolSegment:'',bindings:[],upstreamSummaries:[],missingRequired:[],contextPolicy:'explicit_only',reviewState:'awaiting_review',inputRevision:'1',overrides:null,effectiveBusinessPrompt:null};
function App(){const [input,setInput]=useState(initial);return <><button onClick={()=>setInput({...initial,inputRevision:'2',overrides:{businessPrompt:'REMOTE_SERVER_PROMPT',bindingValues:{}},effectiveBusinessPrompt:'REMOTE_SERVER_PROMPT'})}>remote update</button><NodeInputCard input={input} canEdit={true} stepHandle="test-step" stepRevision="3" onAct={async(type,payload)=>{window.sent.push({type,payload});return true;}}/></>}
window.sent=[];createRoot(document.getElementById('root')).render(<App/>);
`;
(async()=>{
 const build=await esbuild.build({stdin:{contents:app,resolveDir:path.join(repoRoot,'web'),sourcefile:'probe.tsx',loader:'tsx'},bundle:true,write:false,format:'iife',jsx:'automatic',define:{'process.env.NODE_ENV':'"production"'}});
 const browser=await chromium.launch({headless:true});
 try{
  const page=await browser.newPage();page.setDefaultTimeout(5000);page.on("pageerror",e=>console.error("PAGE_ERROR: "+e.message));
  await page.setContent('<div id="root"></div>');await page.addScriptTag({content:build.outputFiles[0].text});
  await page.getByRole('button',{name:'✎ 检查并编辑本次输入'}).click();
  await page.locator('textarea').fill('DRAFT_BASED_ON_V1');
  await page.getByRole('button',{name:'remote update'}).click();
  await page.waitForFunction(()=>document.body.textContent.includes('REMOTE_SERVER_PROMPT'));
  // S2 修复后的更强断言:冲突态禁用保存(旧草稿不得伪装成新版本修改),
  // 必须显式「放弃草稿并重新加载」才能基于服务器内容继续
  const saveDisabled = await page.getByRole('button',{name:'保存覆盖',exact:true}).isDisabled();
  const confirmDisabled = await page.getByRole('button',{name:'确认发送(含修改)'}).isDisabled();
  const conflictShown = (await page.locator('.node-input-body').innerText()).includes('旧草稿');
  console.log(JSON.stringify({saveDisabled,confirmDisabled,conflictShown,draft:await page.locator('textarea').inputValue()},null,2));
  if(!(saveDisabled&&confirmDisabled&&conflictShown)){console.error('FAILED: stale draft not explicitly conflicted (saveDisabled=%s confirmDisabled=%s conflictShown=%s)',saveDisabled,confirmDisabled,conflictShown);process.exitCode=1;}
  // 显式解决:放弃草稿重新加载 → 编辑器回填服务器内容,基于新版本
  await page.getByRole('button',{name:'放弃草稿并重新加载'}).click();
  await page.getByRole('button',{name:'✎ 检查并编辑本次输入'}).click();
  const reloaded = await page.locator('textarea').inputValue();
  if(reloaded!=='REMOTE_SERVER_PROMPT'){console.error('FAILED: reload did not adopt server content: %s',reloaded);process.exitCode=1;}
  await page.getByRole('button',{name:'保存覆盖',exact:true}).click();
  await page.waitForFunction(()=>window.sent.length===1);
  const sent=await page.evaluate(()=>window.sent[0]);
  if(sent.payload.input_revision!=='2'){console.error('FAILED: post-resolution save must carry server revision 2');process.exitCode=1;}
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=2});
