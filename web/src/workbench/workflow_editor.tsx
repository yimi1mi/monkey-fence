import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, type WorkbenchClient } from "../api/client.ts";
import { uuidv7 } from "../api/uuid.ts";
import { ancestorsOf } from "../dag/workflow_graph.ts";
import { GraphCanvas, type GraphRemoval } from "./graph_canvas.tsx";
import { AddNodeButton, nodeWireExtras, nodeDefinitionWire, toNodeForm, useNodeFormModal,
  type InputBindingView, type ContextPolicyView } from "./node_form.tsx";
export interface WorkflowSnapshotView {
  workflow: string;
  key: string;
  name: string;
  allowUnsafeParallel: boolean;
  semanticRevision: string;
  presentationRevision: string;
  workflowCollectionRevision: string;
  nodes: Array<{
    handle: string;
    key: string;
    title: string;
    instructions: string;
    agentInstanceId: string;
    deps: string[];
    position: { x: number; y: number } | null;
    acceptanceCriteria: string;
    outputSchema: unknown | null;
    inputBindings: InputBindingView[];
    contextPolicy: ContextPolicyView;
    requireInputReview: boolean;
  }>;
  edges: Array<{
    handle: string;
    upstream: string;
    downstream: string;
  }>;
}

type Row = Record<string, unknown>;

export function workflowViewOf(data: Row): WorkflowSnapshotView {
  const str = (v: unknown): string => String(v ?? "");
  const rev = (v: unknown): string => {
    if (typeof v === "object" && v !== null) return String((v as Row).revision ?? "0");
    return String(v ?? "0");
  };
  return {
    workflow: str(data.workflow),
    key: str(data.key),
    name: str(data.name),
    allowUnsafeParallel: data.allow_unsafe_parallel === true,
    semanticRevision: rev((data.revisions as Row | undefined)?.semantic_revision),
    presentationRevision: rev((data.revisions as Row | undefined)?.presentation_revision),
    workflowCollectionRevision: rev(data.workflow_collection_revision),
    nodes: (Array.isArray(data.nodes) ? data.nodes : []).map((raw) => {
      const row = raw as Row;
      // wire 位置是 [x, y] 数组;旧快照/宽容路径也可能是 {x,y}
      const rawPosition = row.position as unknown;
      const position =
        Array.isArray(rawPosition) && rawPosition.length >= 2
          ? { x: Number(rawPosition[0]), y: Number(rawPosition[1]) }
          : rawPosition && typeof rawPosition === "object"
            ? (rawPosition as { x: number; y: number })
            : null;
      const policy = row.context_policy;
      return {
        handle: str(row.handle),
        key: str(row.key),
        title: str(row.title),
        instructions: str(row.instructions),
        agentInstanceId: str(row.agent_instance_id),
        deps: (Array.isArray(row.deps) ? row.deps : []).map((d) => str(d)),
        position:
          position && Number.isFinite(position.x) && Number.isFinite(position.y) ? position : null,
        acceptanceCriteria: str(row.acceptance_criteria),
        outputSchema: row.output_schema ?? null,
        inputBindings: (Array.isArray(row.input_bindings) ? row.input_bindings : []).map(
          (binding) => {
            const b = binding as Row;
            return {
              name: str(b.name),
              sourceNodeKey: str(b.source_node_key),
              fieldPath: str(b.field_path),
              required: b.required === true,
              defaultValue: b.default_value == null ? null : str(b.default_value),
            };
          },
        ),
        contextPolicy:
          policy === "legacy_ancestors" || policy === "explicit_only" ? policy : "",
        requireInputReview: row.require_input_review === true,
      };
    }),
    edges: (Array.isArray(data.edges) ? data.edges : []).map((raw) => {
      const row = raw as Row;
      return {
        handle: str(row.handle),
        upstream: str(row.upstream_node_handle),
        downstream: str(row.downstream_node_handle),
      };
    }),
  };
}

export function WorkflowEditor({ client, projectHandle, workflowHandle, agentOptions, onDone, onClose }: {
  client: WorkbenchClient; projectHandle: string; workflowHandle: string; agentOptions: string[];
  onDone: (message: string) => void; onClose: () => void;
}) {
  const [snapshot, setSnapshot] = useState<WorkflowSnapshotView | null>(null);
  const notice = useRef(onDone);
  useEffect(() => { notice.current = onDone; }, [onDone]);
  const current = useRef<WorkflowSnapshotView | null>(null);
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const wireWorkflow = workflowHandle.startsWith("wf_") ? workflowHandle : `wf_${workflowHandle}`;
  const modal = useNodeFormModal(client, projectHandle, wireWorkflow);
  const reload = useCallback(async () => {
    const response = await fetch(`/api/v1/snapshots/workflow/${encodeURIComponent(projectHandle)}/${encodeURIComponent(workflowHandle)}`,
      { headers: { "X-Client-Id": client.clientId } });
    if (!response.ok) throw new ApiError(await response.json());
    const view = workflowViewOf(((await response.json()) as { data: Row }).data);
    current.current = view;
    setSnapshot((previous) => previous?.semanticRevision === view.semanticRevision &&
      previous?.presentationRevision === view.presentationRevision && previous?.name === view.name ? previous : view);
  }, [client.clientId, projectHandle, workflowHandle]);
  useEffect(() => { void reload().catch((error) => notice.current(`加载工作流失败:${String(error)}`)); }, [reload]);

  const command = useCallback((type: "workflow.add_node" | "workflow.update_node" | "workflow.update_graph" | "workflow.connect" | "workflow.move_node",
    payload: (view: WorkflowSnapshotView) => Record<string, unknown>, presentation = false): Promise<boolean> => {
    const operation = queue.current.then(async () => {
      const view = current.current;
      if (!view || !client.isController) return false;
      setBusy(true);
      try {
        await client.command({ schema: "mf.command.v1", command_id: uuidv7(), client_id: client.clientId,
          controller_lease_epoch: client.leaseEpoch,
          target: { kind: "project_workflow", handle: wireWorkflow },
          expected: [{ aggregate: { kind: "project_workflow", handle: wireWorkflow },
            ...(presentation ? { presentation_revision: view.presentationRevision } : { semantic_revision: view.semanticRevision }) }],
          type, payload: { project_handle: projectHandle, workflow_handle: wireWorkflow, ...payload(view) } });
        await reload();
        return true;
      } catch (error) {
        onDone(`编辑失败:${error instanceof Error ? error.message : String(error)}`);
        await reload().catch(() => undefined);
        return false;
      } finally { setBusy(false); }
    });
    queue.current = operation.catch(() => undefined);
    return operation;
  }, [client, projectHandle, wireWorkflow, reload, onDone]);

  const edit = async (key: string) => {
    const view = current.current;
    const node = view?.nodes.find((item) => item.key === key);
    if (!view || !node) return;
    const form = await modal.ask({ title: client.isController ? "编辑节点" : "查看节点", agentOptions,
      initial: toNodeForm(node), upstreamKeys: ancestorsOf(view.nodes, key), readOnly: !client.isController });
    if (form) await command("workflow.update_node", () => ({ node_handle: node.handle,
      title: form.title, instructions: form.instructions, agent_instance_id: form.instance, ...nodeWireExtras(form) }));
  };
  const remove = (removal: GraphRemoval) => command("workflow.update_graph", (view) => {
    const removed = new Set(removal.nodes);
    return { draft: { key: view.key, name: view.name, allow_unsafe_parallel: view.allowUnsafeParallel,
      nodes: view.nodes.filter((node) => !removed.has(node.key)).map((node) => nodeDefinitionWire({
        ...node, deps: node.deps.filter((dep) => !removed.has(dep) && !removal.edges.some((edge) => edge.source === dep && edge.target === node.key)),
      })) } };
  });
  if (!snapshot) return <div className="editor-loading">加载工作流…</div>;
  const selectedNode = snapshot.nodes.find((node) => node.key === selected);
  return <div className="workflow-editor">
    <div className="editor-toolbar">
      <span className="editor-title">{snapshot.name}</span>
      <span className="hint">保存到项目工作流 · 不影响已有运行</span>
      <span className="header-space" />
      <AddNodeButton busy={busy || !client.isController} agentOptions={[...new Set([...agentOptions, ...snapshot.nodes.map((node) => node.agentInstanceId)])]} client={client}
        projectHandle={projectHandle} workflowHandle={wireWorkflow}
        onAdd={(form) => { void command("workflow.add_node", () => ({ node: {
          key: form.key, title: form.title, instructions: form.instructions, agent_instance_id: form.instance,
          deps: [], ...nodeWireExtras(form),
        } })); }} />
      {selectedNode && <>
        <button className="mf-btn ghost" disabled={busy} onClick={() => void edit(selectedNode.key)}>{client.isController ? "编辑节点" : "查看节点"}</button>
        {client.isController && <button className="mf-btn ghost" disabled={busy} onClick={() => void remove({ nodes: [selectedNode.key], edges: [] })}>删除节点</button>}
      </>}
      <button className="mf-btn ghost" disabled={busy} onClick={onClose}>关闭编辑器</button>
    </div>
    <div className="editor-canvas project-graph"><GraphCanvas graph={snapshot.nodes} editable={client.isController} busy={busy} selectedKey={selected}
      onSelect={setSelected} onEdit={(key) => void edit(key)} onNotice={onDone}
      onConnect={(source, target) => command("workflow.connect", (view) => ({
        upstream_node_handle: view.nodes.find((node) => node.key === source)?.handle,
        downstream_node_handle: view.nodes.find((node) => node.key === target)?.handle,
      }))} onRemove={remove}
      onMove={(key, position) => command("workflow.move_node", (view) => ({
        node_handle: view.nodes.find((node) => node.key === key)?.handle, ...position,
      }), true)} /></div>
    {modal.modal}
  </div>;
}
