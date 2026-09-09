import { useEffect, useMemo, useRef, useState } from "react";
import {
  Background, Controls, MarkerType, Position, ReactFlow, useEdgesState, useNodesState,
  type Edge, type Node,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { autoLayout, wireGraph } from "../dag/graph.ts";
import { ancestorsOf, pathKeys, transfersAcrossEdge, type WorkflowGraphNode } from "../dag/workflow_graph.ts";

export type GraphRemoval = { nodes: string[]; edges: Array<{ source: string; target: string }> };
type Outcome = boolean | void | Promise<boolean | void>;

/** 所有工作流图共用布局、选择、连线、删除和交接说明；调用方只提交领域意图。 */
export function GraphCanvas({
  graph, editable = false, busy = false, selectedKey, onSelect, onEdit, onConnect,
  onRemove, onMove, onNotice, direction = "TB",
}: {
  graph: WorkflowGraphNode[];
  editable?: boolean;
  busy?: boolean;
  selectedKey?: string | null;
  onSelect?: (key: string) => void;
  onEdit?: (key: string) => void;
  onConnect?: (source: string, target: string) => Outcome;
  onRemove?: (removal: GraphRemoval) => Outcome;
  onMove?: (key: string, position: { x: number; y: number }) => Outcome;
  onNotice?: (message: string, kind?: "info" | "error") => void;
  direction?: "TB" | "LR";
}) {
  const [nodes, setNodes, onNodesChange] = useNodesState<Node>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<Edge>([]);
  const [selection, setSelection] = useState<string | null>(null);
  const [edgeSelection, setEdgeSelection] = useState<{ source: string; target: string } | null>(null);
  const [sourceFocus, setSourceFocus] = useState<string | null>(null);
  const [formatting, setFormatting] = useState(false);
  const positions = useRef(new Map<string, { x: number; y: number }>());
  const selected = selectedKey ?? selection;
  useEffect(() => {
    if (edgeSelection && !graph.find((node) => node.key === edgeSelection.target)?.deps.includes(edgeSelection.source)) {
      setEdgeSelection(null);
      setSourceFocus(null);
    }
  }, [graph, edgeSelection]);
  const path = useMemo(() => selected
    ? sourceFocus ? pathKeys(graph, sourceFocus, selected) : new Set([selected, ...ancestorsOf(graph, selected)])
    : new Set<string>(), [graph, selected, sourceFocus]);

  useEffect(() => {
    const known = positions.current;
    const keys = new Set(graph.map((node) => node.key));
    for (const key of known.keys()) if (!keys.has(key)) known.delete(key);
    const fallback = new Map(autoLayout(wireGraph(graph.map((node) => ({
      handle: node.key, key: node.key, deps: node.deps,
    }))), direction).map((p) => [p.id, { x: p.x, y: p.y }]));
    for (const node of graph) {
      if (known.has(node.key)) continue;
      let position = node.position ?? fallback.get(node.key) ?? { x: 0, y: 0 };
      const overlaps = (candidate: { x: number; y: number }) => [...known.values()].some((point) =>
        Math.abs(point.x - candidate.x) < 30 && Math.abs(point.y - candidate.y) < 30);
      // Core 给新节点的默认 (0,0) 不是用户主动堆叠；保持现有位置并为新节点留空间。
      if (overlaps(position)) position = fallback.get(node.key) ?? position;
      if (overlaps(position)) position = { x: position.x, y: Math.max(...[...known.values()].map((point) => point.y)) + 160 };
      known.set(node.key, position);
    }
    setNodes((previous) => graph.map((node) => ({
      ...previous.find((entry) => entry.id === node.key),
      id: node.key, position: known.get(node.key)!, selected: node.key === selected,
      sourcePosition: direction === "LR" ? Position.Right : Position.Bottom,
      targetPosition: direction === "LR" ? Position.Left : Position.Top,
      deletable: editable && !node.locked,
      className: `workflow-graph-node ${node.status ?? "draft"}${path.has(node.key) ? " graph-path" : ""}`,
      data: { label: <div className="graph-node-label">
        <strong>{node.title}</strong>
        <span>{node.key} · {node.agentInstanceId || "待选择 Agent"}</span>
        {node.status && <span className="graph-node-status">{statusLabel(node.status)}{node.locked ? " · 定义已冻结" : ""}</span>}
        {node.inputState === "awaiting_review" && <span className="graph-input-badge">等待检查输入</span>}
      </div> },
    })));
    setEdges(graph.flatMap((node) => node.deps.map((source) => ({
      id: `${source}->${node.key}`, source, target: node.key,
      deletable: editable && !node.locked,
      markerEnd: { type: MarkerType.ArrowClosed },
      className: path.has(source) && path.has(node.key) ? "run-dag-edge-highlight" : "",
    }))));
  }, [graph, editable, selected, path, direction, setNodes, setEdges]);

  const transfer = edgeSelection ? transfersAcrossEdge(graph, edgeSelection.source, edgeSelection.target) : [];
  const format = async () => {
    setFormatting(true);
    try {
      const layout = autoLayout(wireGraph(graph.map((node) => ({ handle: node.key, key: node.key, deps: node.deps }))), direction);
      for (const item of layout) {
        const next = { x: item.x, y: item.y };
        if (onMove && await onMove(item.id, next) === false) return;
        positions.current.set(item.id, next);
      }
      setNodes((current) => current.map((node) => ({ ...node, position: positions.current.get(node.id)! })));
      onNotice?.("已按依赖层级自动布局", "info");
    } finally { setFormatting(false); }
  };

  return <div className="graph-workspace">
    <div className="graph-canvas-tools">
      <span>{editable ? "拖动连线编排 · 双击节点编辑 · Delete 删除选中项" : "点击节点查看依赖 · 点击连线查看传递内容"}</span>
      {editable && <button type="button" className="mf-btn ghost tiny" disabled={busy || formatting} onClick={() => void format()}>⇅ 自动布局</button>}
    </div>
    <div className="shared-graph-canvas">
      <ReactFlow nodes={nodes} edges={edges} onNodesChange={onNodesChange} onEdgesChange={onEdgesChange}
        nodesConnectable={editable && !busy} nodesDraggable={editable && !busy}
        deleteKeyCode={editable && !busy ? ["Backspace", "Delete"] : null}
        onNodeClick={(_, node) => { setSelection(node.id); setSourceFocus(null); setEdgeSelection(null); onSelect?.(node.id); }}
        onNodeDoubleClick={(_, node) => onEdit?.(node.id)}
        onEdgeClick={(_, edge) => { setEdgeSelection({ source: edge.source, target: edge.target }); setSourceFocus(null); }}
        onConnect={(connection) => {
          const { source, target } = connection;
          if (!source || !target || busy) return;
          if (graph.find((node) => node.key === target)?.locked) { onNotice?.("已启动节点的输入依赖已冻结"); return; }
          if (source === target || ancestorsOf(graph, source).includes(target)) { onNotice?.("该连线会形成依赖环，请选择其他节点"); return; }
          void onConnect?.(source, target);
        }}
        onBeforeDelete={async ({ nodes: removedNodes, edges: removedEdges }) => {
          if (!editable || busy) return false;
          if (removedNodes.some((node) => graph.find((item) => item.key === node.id)?.locked) ||
              removedEdges.some((edge) => graph.find((item) => item.key === edge.target)?.locked)) {
            onNotice?.("已启动节点及其输入依赖不能删除"); return false;
          }
          await onRemove?.({ nodes: removedNodes.map((node) => node.id),
            edges: removedEdges.map((edge) => ({ source: edge.source, target: edge.target })) });
          // 领域成功后的新 graph 才改变图，失败不产生本地幽灵删除。
          return false;
        }}
        onNodeDragStop={async (_, node) => {
          const before = positions.current.get(node.id)!;
          positions.current.set(node.id, node.position);
          if (onMove && await onMove(node.id, node.position) === false) {
            positions.current.set(node.id, before);
            setNodes((current) => current.map((item) => item.id === node.id ? { ...item, position: before } : item));
          }
        }}
        fitView proOptions={{ hideAttribution: true }}>
        <Background /><Controls />
      </ReactFlow>
    </div>
    {edgeSelection && <div className="graph-transfer-panel" aria-label="连线传递内容">
      <strong>{edgeSelection.source} → {edgeSelection.target}</strong>
      {transfer.length === 0 ? <p>仅等待完成，不自动传递内容。</p> : <ul>
        {transfer.map((item, index) => <li key={index}>
          <button type="button" className="mf-btn ghost tiny" onClick={() => {
            setSelection(edgeSelection.target); onSelect?.(edgeSelection.target); setSourceFocus(item.source);
          }}>{item.source}.{item.field}</button>
          {item.binding ? ` → ${item.binding}` : ""}{item.required ? " · 必填" : ""}
          {item.automatic ? " · 祖先摘要策略自动传递" : ""}
          {item.source !== edgeSelection.source ? ` · 经由 ${edgeSelection.source}` : ""}
        </li>)}
      </ul>}
      {editable && <button type="button" className="mf-btn ghost tiny" disabled={busy}
        onClick={() => {
          if (graph.find((node) => node.key === edgeSelection.target)?.locked) { onNotice?.("该输入依赖已冻结"); return; }
          void onRemove?.({ nodes: [], edges: [edgeSelection] });
        }}>断开连线</button>}
    </div>}
  </div>;
}

function statusLabel(status: string): string {
  return ({ pending: "等待上游", ready: "就绪", running: "执行中", "awaiting-outcome": "待结算",
    "needs-input": "等待回答", succeeded: "已成功", failed: "失败", blocked: "被阻塞", skipped: "已跳过", cancelled: "已取消" } as Record<string, string>)[status] ?? status;
}
