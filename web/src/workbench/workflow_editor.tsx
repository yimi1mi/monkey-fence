// 工作流 DAG 编辑器(#76):React Flow 画布 + dagre 自动布局(graph.ts)。
// 全部编辑经 workflow.* 命令(双轴 CAS);快照是唯一数据源。
// #97:节点编辑/新增用表单弹窗,agent_instance_id 可从 catalog 实例与
// 本机检测 CLI 中选择(datalist,仍允许自由输入)。

import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  addEdge,
  Background,
  Controls,
  ReactFlow,
  useEdgesState,
  useNodesState,
  type Connection,
  type Edge,
  type Node,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { ApiError, type WorkbenchClient } from "../api/client.ts";
import { uuidv7 } from "../api/uuid.ts";
import { autoLayout, type DagGraph } from "../dag/graph.ts";

export interface WorkflowSnapshotView {
  workflow: string;
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
      return {
        handle: str(row.handle),
        key: str(row.key),
        title: str(row.title),
        instructions: str(row.instructions),
        agentInstanceId: str(row.agent_instance_id),
        deps: (Array.isArray(row.deps) ? row.deps : []).map((d) => str(d)),
        position:
          position && Number.isFinite(position.x) && Number.isFinite(position.y) ? position : null,
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

export function WorkflowEditor({
  client,
  projectHandle,
  workflowHandle,
  agentOptions,
  onDone,
  onClose,
}: {
  client: WorkbenchClient;
  projectHandle: string;
  workflowHandle: string;
  /** agent_instance_id 候选(catalog 实例 id + 本机检测 CLI;#97)。 */
  agentOptions: string[];
  onDone: (message: string) => void;
  onClose: () => void;
}) {
  const [snapshot, setSnapshot] = useState<WorkflowSnapshotView | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [nodes, setNodes, onNodesChange] = useNodesState<Node>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<Edge>([]);
  const [busy, setBusy] = useState(false);
  const nodeFormModal = useNodeFormModal();
  // #98 布局稳定性:位置以本地缓存优先(拖动过的位置不因快照刷新被
  // dagre 重排覆盖);快照持久位置次之;全图无位置(首次加载)才 dagre。
  const positionsRef = useRef<Map<string, { x: number; y: number }>>(new Map());

  const reload = useCallback(async () => {
    const response = await fetch(
      `/api/v1/snapshots/workflow/${encodeURIComponent(projectHandle)}/${encodeURIComponent(workflowHandle)}`,
      { headers: { "X-Client-Id": client.clientId } },
    );
    if (!response.ok) throw new ApiError(await (await response.json()) as never);
    const envelope = (await response.json()) as { data: Row };
    setSnapshot(workflowViewOf(envelope.data));
  }, [client.clientId, projectHandle, workflowHandle]);

  useEffect(() => {
    void reload().catch((error) => onDone(`快照拉取失败:${String(error)}`));
  }, [reload, onDone]);

  // 快照 → React Flow 节点/边。位置决策:本地缓存 > 快照持久位置;
  // 都没有(新节点)排到现有内容下方;全图首次加载才 dagre 自动布局。
  useEffect(() => {
    if (!snapshot) return;
    const known = new Map(positionsRef.current);
    for (const node of snapshot.nodes) {
      if (!known.has(node.handle) && node.position) {
        known.set(node.handle, node.position);
      }
    }
    const unknown = snapshot.nodes.filter((node) => !known.has(node.handle));
    if (unknown.length > 0) {
      if (known.size === 0) {
        // 首次加载:整图 dagre(TB 层次布局)
        const graph: DagGraph = {
          nodes: snapshot.nodes.map((node) => ({
            id: node.handle,
            title: node.title,
            instructions: node.instructions,
            agentInstanceId: node.agentInstanceId,
            deps: node.deps,
            x: 0,
            y: 0,
          })),
        };
        for (const p of autoLayout(graph, "TB")) {
          known.set(p.id, { x: p.x, y: p.y });
        }
      } else {
        // 新节点:排到当前最底部之下,居中
        const placed = [...known.values()];
        const maxY = Math.max(0, ...placed.map((p) => p.y + 160));
        const avgX = placed.length
          ? Math.max(0, ...placed.map((p) => p.x))
          : 0;
        for (const node of unknown) {
          known.set(node.handle, { x: avgX, y: maxY });
        }
      }
    }
    positionsRef.current = known;
    setNodes(
      snapshot.nodes.map((node) => ({
        id: node.handle,
        position: known.get(node.handle) ?? { x: 0, y: 0 },
        data: {
          label: `${node.title}\n${node.key} · ${node.agentInstanceId || "无实例"}`,
        },
        selected: node.handle === selected,
      })),
    );
    setEdges(
      snapshot.edges.map((edge) => ({
        id: edge.handle,
        source: edge.upstream,
        target: edge.downstream,
      })),
    );
  }, [snapshot, setNodes, setEdges, selected]);

  const editCommand = useCallback(
    async (
      type: "workflow.add_node" | "workflow.update_node" | "workflow.remove_node" | "workflow.connect" | "workflow.disconnect" | "workflow.move_node",
      payload: Record<string, unknown>,
      axis: "semantic" | "presentation" = "semantic",
    ) => {
      if (!snapshot) return;
      const revision =
        axis === "semantic" ? snapshot.semanticRevision : snapshot.presentationRevision;
      setBusy(true);
      try {
        await client.command({
          schema: "mf.command.v1",
          command_id: uuidv7(),
          client_id: client.clientId,
          controller_lease_epoch: client.leaseEpoch,
          target: { kind: "project_workflow", handle: `wf_${workflowHandle}` },
          expected: [
            {
              aggregate: { kind: "project_workflow", handle: `wf_${workflowHandle}` },
              ...(axis === "semantic"
                ? { semantic_revision: revision }
                : { presentation_revision: revision }),
            },
          ],
          type,
          payload: { project_handle: projectHandle, workflow_handle: `wf_${workflowHandle}`, ...payload },
        });
        await reload();
        setBusy(false);
      } catch (error) {
        setBusy(false);
        const code = error instanceof ApiError ? error.problem.code : null;
        if (code === "controller_required" || code === "controller_lease_expired") {
          location.reload();
          return;
        }
        onDone(`编辑失败:${error instanceof Error ? error.message : String(error)}`);
      }
    },
    [client, onDone, projectHandle, reload, snapshot, workflowHandle],
  );

  const onConnect = useCallback(
    (connection: Connection) => {
      if (!connection.source || !connection.target) return;
      void editCommand("workflow.connect", {
        upstream_node_handle: connection.source,
        downstream_node_handle: connection.target,
      });
    },
    [editCommand],
  );

  // #98 拖动结束:位置写入本地缓存(立即)并经 move_node 持久化
  // (presentation 轴,不与语义编辑撞 CAS)。失败静默——位置是非关键呈现。
  const onNodeDragStop = useCallback(
    (_event: MouseEvent | TouchEvent, node: Node) => {
      positionsRef.current.set(node.id, node.position);
      void editCommand(
        "workflow.move_node",
        { node_handle: node.id, x: node.position.x, y: node.position.y },
        "presentation",
      ).catch(() => undefined);
    },
    [editCommand],
  );

  // #98 自动布局(格式化):dagre 全量重排,更新本地并串行持久化全部位置。
  // 每条 move_node 会推进 presentation revision,按 +1 推算下一 CAS 值;
  // 冲突时中断并 reload(用户可重点一次)。
  const formatLayout = useCallback(async () => {
    if (!snapshot) return;
    const graph: DagGraph = {
      nodes: snapshot.nodes.map((node) => ({
        id: node.handle,
        title: node.title,
        instructions: node.instructions,
        agentInstanceId: node.agentInstanceId,
        deps: node.deps,
        x: 0,
        y: 0,
      })),
    };
    const laid = new Map(autoLayout(graph, "TB").map((p) => [p.id, { x: p.x, y: p.y }]));
    positionsRef.current = new Map(laid);
    setNodes((prev) => prev.map((n) => ({ ...n, position: laid.get(n.id) ?? n.position })));
    if (!client.isController) return;
    setBusy(true);
    try {
      let revision = Number(snapshot.presentationRevision) || 0;
      for (const node of snapshot.nodes) {
        const pos = laid.get(node.handle);
        if (!pos) continue;
        await client.command({
          schema: "mf.command.v1",
          command_id: uuidv7(),
          client_id: client.clientId,
          controller_lease_epoch: client.leaseEpoch,
          target: { kind: "project_workflow", handle: `wf_${workflowHandle}` },
          expected: [
            {
              aggregate: { kind: "project_workflow", handle: `wf_${workflowHandle}` },
              presentation_revision: String(revision),
            },
          ],
          type: "workflow.move_node",
          payload: {
            project_handle: projectHandle,
            workflow_handle: `wf_${workflowHandle}`,
            node_handle: node.handle,
            x: pos.x,
            y: pos.y,
          },
        });
        revision += 1;
      }
      await reload();
      onDone("已按依赖层级自动布局");
    } catch (error) {
      onDone(`布局保存中断:${error instanceof Error ? error.message : String(error)}`);
      await reload().catch(() => undefined);
    } finally {
      setBusy(false);
    }
  }, [snapshot, client, workflowHandle, projectHandle, setNodes, reload, onDone]);

  const selectedNode = useMemo(
    () => snapshot?.nodes.find((node) => node.handle === selected) ?? null,
    [snapshot, selected],
  );

  if (!snapshot) {
    return <div className="editor-loading">加载工作流…</div>;
  }

  return (
    <div className="workflow-editor">
      <div className="editor-toolbar">
        <span className="editor-title">{snapshot.name}</span>
        <span className="mono-dim">
          语义 rev {snapshot.semanticRevision} · 呈现 rev {snapshot.presentationRevision}
        </span>
        <span className="header-space" />
        <button
          className="mf-btn ghost"
          disabled={busy || !snapshot}
          title="按依赖层级(TB)重排全部节点并保存位置"
          onClick={() => void formatLayout()}
        >
          ⇅ 自动布局
        </button>
        <AddNodeButton
          busy={busy}
          agentOptions={agentOptions}
          onAdd={(key, title, instance, instructions) =>
            editCommand("workflow.add_node", {
              node: { key, title, instructions, agent_instance_id: instance, deps: [] },
            })
          }
        />
        {selectedNode && client.isController && (
          <>
            <button
              className="mf-btn ghost"
              disabled={busy}
              onClick={() => void editCommand("workflow.remove_node", { node_handle: selectedNode.handle })}
            >
              删除节点
            </button>
            <button
              className="mf-btn ghost"
              disabled={busy}
              onClick={() => {
                void (async () => {
                  const form = await nodeFormModal.ask({
                    title: "编辑节点",
                    agentOptions,
                    initial: {
                      key: selectedNode.key,
                      title: selectedNode.title,
                      instance: selectedNode.agentInstanceId,
                      instructions: selectedNode.instructions,
                    },
                  });
                  if (!form) return;
                  void editCommand("workflow.update_node", {
                    node_handle: selectedNode.handle,
                    title: form.title,
                    instructions: form.instructions,
                    agent_instance_id: form.instance,
                  });
                })();
              }}
            >
              编辑节点
            </button>
          </>
        )}
        <button className="mf-btn ghost" onClick={onClose}>
          关闭编辑器
        </button>
      </div>
      <div className="editor-canvas">
        <ReactFlow
          nodes={nodes}
          edges={edges}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          onConnect={onConnect}
          onNodeDragStop={onNodeDragStop}
          onNodeClick={(_, node) => setSelected(node.id)}
          onEdgeDoubleClick={(_, edge) => {
            void editCommand("workflow.disconnect", { edge_handle: edge.id });
          }}
          fitView
          proOptions={{ hideAttribution: true }}
        >
          <Background />
          <Controls />
        </ReactFlow>
      </div>
      <div className="editor-hint">
        点击节点选中(删除/编辑);拖出连线建立依赖;双击连线断开。所有编辑经内核命令(双轴 CAS)。
      </div>
      {nodeFormModal.modal}
    </div>
  );
}

// ── 节点表单弹窗(#97):标题 + agent 选择 + 指令 ───────────────────────

export interface NodeFormValue {
  key: string;
  title: string;
  instance: string;
  instructions: string;
}

interface NodeFormSpec {
  title: string;
  agentOptions: string[];
  initial: NodeFormValue;
  /** 新建节点时允许编辑 key(编辑节点时 key 不可变)。 */
  withKey?: boolean;
}

function useNodeFormModal(): {
  ask: (spec: NodeFormSpec) => Promise<NodeFormValue | null>;
  modal: ReactNode;
} {
  const [spec, setSpec] = useState<NodeFormSpec | null>(null);
  const resolver = useRef<((value: NodeFormValue | null) => void) | null>(null);

  const ask = useCallback((next: NodeFormSpec) => {
    setSpec(next);
    return new Promise<NodeFormValue | null>((resolve) => {
      resolver.current = resolve;
    });
  }, []);

  const settle = useCallback((value: NodeFormValue | null) => {
    resolver.current?.(value);
    resolver.current = null;
    setSpec(null);
  }, []);

  return {
    ask,
    modal: spec ? <NodeFormModal spec={spec} onSettle={settle} /> : null,
  };
}

function NodeFormModal({
  spec,
  onSettle,
}: {
  spec: NodeFormSpec;
  onSettle: (value: NodeFormValue | null) => void;
}) {
  const [value, setValue] = useState<NodeFormValue>(spec.initial);

  useEffect(() => {
    setValue(spec.initial);
  }, [spec]);

  const submit = () => {
    const key = value.key.trim();
    const title = value.title.trim() || key;
    const instance = value.instance.trim();
    if (!key && spec.withKey) return; // 新建必须有 key
    onSettle({ ...value, key, title, instance });
  };

  return (
    <div
      className="scrim"
      onClick={(event) => {
        if (event.target === event.currentTarget) onSettle(null);
      }}
    >
      <div className="modal node-form-modal" role="dialog" aria-modal="true" aria-label={spec.title}>
        <h3>{spec.title}</h3>
        {spec.withKey && (
          <div className="field">
            <label htmlFor="mf-node-key">节点 key(ASCII,创建后不可改)</label>
            <input
              id="mf-node-key"
              autoFocus
              value={value.key}
              placeholder="如 build / test / report"
              onChange={(event) => setValue({ ...value, key: event.target.value })}
              onKeyDown={(event) => {
                if (event.key === "Enter") submit();
                if (event.key === "Escape") onSettle(null);
              }}
            />
          </div>
        )}
        <div className="field">
          <label htmlFor="mf-node-title">标题</label>
          <input
            id="mf-node-title"
            autoFocus={!spec.withKey}
            value={value.title}
            placeholder="节点显示名"
            onChange={(event) => setValue({ ...value, title: event.target.value })}
            onKeyDown={(event) => {
              if (event.key === "Enter") submit();
              if (event.key === "Escape") onSettle(null);
            }}
          />
        </div>
        <div className="field">
          <label htmlFor="mf-node-agent">Agent 实例(每个节点可选不同 agent;下拉候选或自由输入)</label>
          <input
            id="mf-node-agent"
            list="mf-agent-options"
            value={value.instance}
            placeholder="如 codex / claude / agent-main"
            onChange={(event) => setValue({ ...value, instance: event.target.value })}
            onKeyDown={(event) => {
              if (event.key === "Enter") submit();
              if (event.key === "Escape") onSettle(null);
            }}
          />
          <datalist id="mf-agent-options">
            {spec.agentOptions.map((option) => (
              <option key={option} value={option} />
            ))}
          </datalist>
          {spec.agentOptions.length === 0 && (
            <span className="hint">暂无候选——在「设置 → Agent 与 CLI」安装/注册后可选</span>
          )}
        </div>
        <div className="field">
          <label htmlFor="mf-node-instructions">指令(节点任务说明;可引用上游输出)</label>
          <textarea
            id="mf-node-instructions"
            rows={4}
            value={value.instructions}
            placeholder="这个节点让 agent 做什么…"
            onChange={(event) => setValue({ ...value, instructions: event.target.value })}
            onKeyDown={(event) => {
              if (event.key === "Escape") onSettle(null);
            }}
          />
        </div>
        <div className="actions">
          <button className="mf-btn ghost" onClick={() => onSettle(null)}>
            取消
          </button>
          <button className="mf-btn primary" onClick={submit}>
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

function AddNodeButton({
  busy,
  agentOptions,
  onAdd,
}: {
  busy: boolean;
  agentOptions: string[];
  onAdd: (key: string, title: string, instance: string, instructions: string) => void;
}) {
  const nodeFormModal = useNodeFormModal();
  return (
    <>
      {nodeFormModal.modal}
      <button
        className="mf-btn primary"
        disabled={busy}
        onClick={() => {
          void (async () => {
            const form = await nodeFormModal.ask({
              title: "添加节点",
              agentOptions,
              withKey: true,
              initial: { key: "", title: "", instance: "agent-main", instructions: "" },
            });
            if (!form || !form.key) return;
            onAdd(form.key, form.title, form.instance, form.instructions);
          })();
        }}
      >
        ＋节点
      </button>
    </>
  );
}
