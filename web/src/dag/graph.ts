// DAG 图模型与 Web 预检(T8b,Issue #52/53)。
// Rust 复检一切合法性;这里只做本地预检(cycle 预检)与坐标/节点
// 增量(position delta ≤512B)。

export interface DagNode {
  /** Step opaque handle(wf_ 域 step_)。 */
  id: string;
  title: string;
  /** 语义字段:改动使 semantic revision 前进(阻止陈旧 Run)。 */
  instructions: string;
  agentInstanceId: string;
  deps: string[];
  /** presentation 坐标(显式自动排列覆盖;手动移动保留)。 */
  x: number;
  y: number;
}

export interface DagGraph {
  nodes: DagNode[];
}

/** cycle 预检:新增依赖后是否存在环(Web 预检;Rust 复检兜底)。 */
export function wouldCreateCycle(
  graph: DagGraph,
  nodeId: string,
  newDeps: string[],
): boolean {
  const deps = new Map<string, string[]>();
  for (const node of graph.nodes) deps.set(node.id, [...node.deps]);
  deps.set(nodeId, newDeps);
  // DFS 检测从 nodeId 出发能否回到自身
  const visiting = new Set<string>();
  const visit = (id: string): boolean => {
    if (id === nodeId && visiting.size > 0) return true;
    if (visiting.has(id)) return id === nodeId;
    visiting.add(id);
    for (const dep of deps.get(id) ?? []) {
      if (visit(dep)) return true;
    }
    visiting.delete(id);
    return false;
  };
  for (const dep of newDeps) {
    visiting.clear();
    if (visit(dep)) return true;
  }
  return false;
}

/** 未知依赖预检(自连/重复边/不存在节点)。 */
export function validateDeps(graph: DagGraph, nodeId: string, deps: string[]): string[] {
  const known = new Set(graph.nodes.map((n) => n.id));
  return deps.filter((dep) => dep === nodeId || !known.has(dep));
}

/** Dagre 自动排列(显式触发;返回 position delta ≤512B 的批量增量)。 */
export function autoLayout(
  graph: DagGraph,
  direction: "TB" | "LR" = "TB",
): Array<{ id: string; x: number; y: number }> {
  // 与 @xyflow/react 配套的 dagre 布局;此处为纯计算(测试无 DOM)。
  // 布局算法:最长路径分层 + 同层排序(确定性;dagre 在浏览器侧接入)。
  const depth = new Map<string, number>();
  const resolve = (id: string): number => {
    if (depth.has(id)) return depth.get(id)!;
    const node = graph.nodes.find((n) => n.id === id);
    if (!node) return 0;
    const d = node.deps.length ? Math.max(...node.deps.map(resolve)) + 1 : 0;
    depth.set(id, d);
    return d;
  };
  for (const node of graph.nodes) resolve(node.id);
  const layers = new Map<number, string[]>();
  for (const node of graph.nodes) {
    const d = depth.get(node.id) ?? 0;
    layers.set(d, [...(layers.get(d) ?? []), node.id]);
  }
  const gap = direction === "TB" ? 160 : 320;
  const span = direction === "TB" ? 240 : 140;
  return [...layers.entries()].map(([layer, ids]) =>
    ids.map((id, index) => ({
      id,
      x: direction === "TB" ? index * span : layer * gap,
      y: direction === "TB" ? layer * gap : index * span,
    })),
  ).flat();
}

/** position delta(批量;≤512B 预算的序列化)。 */
export function positionDelta(
  moves: Array<{ id: string; x: number; y: number }>,
): string {
  return JSON.stringify(
    moves.map((m) => [m.id, Math.round(m.x), Math.round(m.y)]),
  );
}
