import test from "node:test";
import assert from "node:assert/strict";
import { ancestorsOf, transfersAcrossEdge, pathKeys, type WorkflowGraphNode } from "../workflow_graph.ts";

const node = (key: string, deps: string[] = []): WorkflowGraphNode => ({
  key, deps, title: key, instructions: "", agentInstanceId: "agent", inputBindings: [], contextPolicy: "explicit_only",
});

test("edge inspection includes indirect field references and their path", () => {
  const a=node("a"), b=node("b",["a"]), c=node("c",["b"]), other=node("other");
  c.inputBindings=[{ name:"report", sourceNodeKey:"a", fieldPath:"output.report",required:true,defaultValue:null }];
  const graph=[a,b,c,other];
  assert.deepEqual(ancestorsOf(graph,"c"),["b","a"]);
  assert.deepEqual(transfersAcrossEdge(graph,"b","c"),[
    {source:"a",field:"output.report",binding:"report",required:true,automatic:false},
  ]);
  assert.deepEqual([...pathKeys(graph,"a","c")],["a","b","c"]);
});

test("legacy summary transfer is not labelled a control-only edge", () => {
  const a=node("a"), b=node("b",["a"]);
  assert.equal(transfersAcrossEdge([a,b],"a","b").length,0);
  b.contextPolicy="";
  assert.deepEqual(transfersAcrossEdge([a,b],"a","b"),[
    {source:"a",field:"summary",binding:null,required:false,automatic:true},
  ]);
  b.contextPolicy="explicit_only";b.instructions="${nodes.a} ${nodes.a.output.report}";
  assert.deepEqual(transfersAcrossEdge([a,b],"a","b").map((item)=>item.field),["完整交接","output.report"]);
});
