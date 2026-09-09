// 项目工作流与运行图共用的图语义。节点 key 是唯一的图内身份。
export interface GraphBinding {
  name: string;
  sourceNodeKey: string;
  fieldPath: string;
  required: boolean;
  defaultValue: string | null;
}

export interface WorkflowGraphNode {
  key: string;
  title: string;
  instructions: string;
  agentInstanceId: string;
  deps: string[];
  inputBindings: GraphBinding[];
  contextPolicy: string;
  position?: { x: number; y: number } | null;
  status?: string;
  locked?: boolean;
  inputState?: string;
}

export function ancestorsOf(nodes: Pick<WorkflowGraphNode, "key" | "deps">[], key: string): string[] {
  const deps = new Map(nodes.map((node) => [node.key, node.deps]));
  const seen = new Set<string>();
  const queue = [...(deps.get(key) ?? [])];
  while (queue.length) {
    const current = queue.shift()!;
    if (seen.has(current) || current === key) continue;
    seen.add(current);
    queue.push(...(deps.get(current) ?? []));
  }
  return [...seen];
}

export interface GraphTransfer {
  source: string;
  field: string;
  binding: string | null;
  required: boolean;
  automatic: boolean;
}

/** 该边上到达下游的输入，包含经由直接上游传递的祖先引用。 */
export function transfersAcrossEdge(
  nodes: WorkflowGraphNode[], source: string, target: string,
): GraphTransfer[] {
  const downstream = nodes.find((node) => node.key === target);
  if (!downstream?.deps.includes(source)) return [];
  const upstream = new Set([source, ...ancestorsOf(nodes, source)]);
  const transfers: GraphTransfer[] = [];
  for (const binding of downstream.inputBindings) {
    if (upstream.has(binding.sourceNodeKey)) {
      transfers.push({ source: binding.sourceNodeKey, field: binding.fieldPath,
        binding: binding.name, required: binding.required, automatic: false });
    }
  }
  for (const match of downstream.instructions.matchAll(/\$\{nodes\.([A-Za-z0-9_-]+)(?:\.([^}]+))?\}/g)) {
    if (upstream.has(match[1])) {
      transfers.push({ source: match[1], field: match[2] || "完整交接",
        binding: null, required: false, automatic: false });
    }
  }
  if (downstream.contextPolicy !== "explicit_only") {
    for (const key of upstream) {
      transfers.push({ source: key, field: "summary", binding: null, required: false, automatic: true });
    }
  }
  return transfers.filter((entry, index, all) => all.findIndex((other) =>
    other.source === entry.source && other.field === entry.field &&
    other.binding === entry.binding && other.automatic === entry.automatic) === index);
}

/** 可视化一项间接输入经过的全部合法路径。 */
export function pathKeys(nodes: WorkflowGraphNode[], source: string, target: string): Set<string> {
  const targets = new Set([target, ...ancestorsOf(nodes, target)]);
  return new Set(nodes.filter((node) => targets.has(node.key) &&
    (node.key === source || ancestorsOf(nodes, node.key).includes(source))).map((node) => node.key));
}
