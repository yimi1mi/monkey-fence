import { useMemo, useState } from "react";
import type { WorkbenchClient } from "../api/client.ts";
import { ancestorsOf } from "../dag/workflow_graph.ts";
import { GraphCanvas, type GraphRemoval } from "./graph_canvas.tsx";
import { AddNodeButton, nodeDefinitionWire, nodeFromForm, toNodeForm, useNodeFormModal,
  type EditableNodeDefinition } from "./node_form.tsx";
import type { RunDetailView } from "./run_detail.ts";

export function runGraphNodes(detail: RunDetailView): EditableNodeDefinition[] {
  return detail.steps.map((step) => ({
    key: step.key, title: step.title, instructions: step.instructions,
    agentInstanceId: step.agentInstanceId, deps: [...step.dependencies],
    acceptanceCriteria: step.acceptanceCriteria, outputSchema: step.outputSchema,
    inputBindings: step.inputBindings.map((binding) => ({ ...binding })),
    contextPolicy: step.contextPolicy, requireInputReview: step.requireInputReview,
    status: step.status, locked: step.attempts > 0,
    inputState: step.input?.reviewState ?? undefined,
  }));
}

/** 与项目工作流共用画布、字段表单和试算；只把保存动作换为原子运行图补丁。 */
export function RunGraphEditor({ detail, client, projectHandle, agentOptions, onClose, onApply }: {
  detail: RunDetailView;
  client: WorkbenchClient;
  projectHandle: string;
  agentOptions: string[];
  onClose: () => void;
  onApply: (nodes: Array<Record<string, unknown>>, baseRevision: string) => Promise<boolean>;
}) {
  const [baseRevision] = useState(detail.pipelineRevision?.handle ?? "");
  const [initial] = useState(() => runGraphNodes(detail));
  const [nodes, setNodes] = useState(initial);
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const modal = useNodeFormModal(client, projectHandle, detail.workflowRun);
  const conflict = detail.pipelineRevision?.handle !== baseRevision || !detail.paused;
  const selectedNode = nodes.find((node) => node.key === selected);
  const choices = [...new Set([...agentOptions, ...nodes.map((node) => node.agentInstanceId)].filter(Boolean))];
  const changes = useMemo(() => {
    const old = new Map(initial.map((node) => [node.key, JSON.stringify(nodeDefinitionWire(node))]));
    const next = new Set(nodes.map((node) => node.key));
    return [
      ...nodes.filter((node) => !old.has(node.key)).map((node) => `新增 ${node.title}`),
      ...nodes.filter((node) => old.has(node.key) && old.get(node.key) !== JSON.stringify(nodeDefinitionWire(node))).map((node) => `修改 ${node.title}`),
      ...initial.filter((node) => !next.has(node.key)).map((node) => `删除 ${node.title}`),
    ];
  }, [initial, nodes]);

  const edit = async (key: string) => {
    const node = nodes.find((item) => item.key === key);
    if (!node) return;
    const form = await modal.ask({ title: node.locked ? "查看冻结节点" : "编辑节点", initial: toNodeForm(node),
      upstreamKeys: ancestorsOf(nodes, key), agentOptions: choices, readOnly: node.locked });
    if (form && !node.locked) {
      setNodes((current) => current.map((item) => item.key === key ? { ...item, ...nodeFromForm(form, item.deps) } : item));
      setError(null);
    }
  };
  const remove = ({ nodes: removedNodes, edges }: GraphRemoval) => {
    const removed = new Set(removedNodes);
    if (nodes.some((node) => node.locked && (removed.has(node.key) ||
      node.deps.some((dep) => removed.has(dep) || edges.some((edge) => edge.source === dep && edge.target === node.key))))) {
      setError("已启动节点及其输入依赖不能修改"); return false;
    }
    setNodes((current) => current.filter((node) => !removed.has(node.key)).map((node) => ({
      ...node, deps: node.deps.filter((dep) => !removed.has(dep) && !edges.some((edge) => edge.source === dep && edge.target === node.key)),
    })));
    setError(null); return true;
  };
  // 编辑器只经显式「关闭编辑器」退出(误触遮罩不丢失图编辑;busy 保护由按钮承担)
return <div className="scrim">
    <div className="modal run-graph-editor" role="dialog" aria-modal="true" aria-label="编辑运行图">
      <h3>编辑运行图</h3>
      <p className="hint">仅影响本次运行。已启动节点保留冻结配置；应用后仍暂停，确认新图后再恢复。</p>
      <div className="editor-toolbar">
        <AddNodeButton busy={busy || conflict} agentOptions={choices} client={client} projectHandle={projectHandle}
          workflowHandle={detail.workflowRun} onAdd={(form) => {
            if (nodes.some((node) => node.key === form.key)) { setError(`节点 key ${form.key} 已存在`); return; }
            setNodes((current) => [...current, { ...nodeFromForm(form, []), status: "pending", locked: false }]);
            setSelected(form.key); setError(null);
          }} />
        {selectedNode && <>
          <button className="mf-btn ghost" disabled={busy || conflict} onClick={() => void edit(selectedNode.key)}>
            {selectedNode.locked ? "查看冻结节点" : "编辑节点"}
          </button>
          {!selectedNode.locked && <button className="mf-btn ghost" disabled={busy || conflict}
            onClick={() => remove({ nodes: [selectedNode.key], edges: [] })}>删除节点</button>}
        </>}
      </div>
      <GraphCanvas graph={nodes} direction="LR" editable={!conflict} busy={busy} selectedKey={selected} onSelect={setSelected}
        onEdit={(key) => void edit(key)} onNotice={(message, kind) => setError(kind === "info" ? null : message)} onRemove={remove}
        onConnect={(source, target) => {
          setNodes((current) => current.map((node) => node.key === target && !node.deps.includes(source)
            ? { ...node, deps: [...node.deps, source] } : node)); setError(null); return true;
        }} />
      <div className="graph-change-summary" aria-label="运行图变更">
        {changes.length ? changes.join("；") : "尚未修改节点定义"}
      </div>
      {(error || conflict) && <div className="form-error" role="alert">
        {conflict ? "运行图或暂停状态已经变化，请关闭并重新打开编辑器。草稿没有自动套用新版本。" : error}
      </div>}
      <div className="actions">
        <button className="mf-btn ghost" disabled={busy} onClick={onClose}>取消</button>
        <button className="mf-btn primary" disabled={busy || conflict || nodes.length === 0 || changes.length === 0}
          onClick={async () => {
            setBusy(true);
            try {
              if (await onApply(nodes.map(nodeDefinitionWire), baseRevision)) onClose();
              else setError("新版本未应用。请根据错误提示修正节点与引用后重试。");
            } finally { setBusy(false); }
          }}>应用新版本</button>
      </div>
      {modal.modal}
    </div>
  </div>;
}
