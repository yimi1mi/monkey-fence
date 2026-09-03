// Project Workflow 命令构建与双 revision CAS(T8b,Issue #53)。
// 全部写入走 Core 命令(wire 冻结枚举);semantic/presentation 分轴:
// 语义字段改动使 semantic revision 前进(阻止陈旧 Run),presentation
// (坐标/viewport)不阻止。乐观更新失败回滚。

import type { CommandEnvelope, CommandType, ExpectedRevision } from "../api/protocol.ts";

export interface RevisionPair {
  semantic: string;
  presentation: string;
  /** Project collection revision(创建/删除 CAS)。 */
  collection: string;
}

/** 各命令的语义轴(semantic=true 的命令 CAS semantic revision)。 */
const SEMANTIC_COMMANDS = new Set<string>([
  "workflow.create",
  "workflow.delete",
  "workflow.add_node",
  "workflow.update_node",
  "workflow.remove_node",
  "workflow.connect",
  "workflow.disconnect",
]);

export function isSemanticCommand(type: CommandType): boolean {
  return SEMANTIC_COMMANDS.has(type);
}

/** 工作流命令 envelope 构造(双轴 expected:语义命令带 semantic,
 *  presentation 命令(move/viewport)只带 presentation)。 */
export function workflowCommand(input: {
  commandId: string;
  clientId: string;
  controllerLeaseEpoch: string;
  projectHandle: string;
  workflowHandle: string;
  type: CommandType;
  payload: Record<string, unknown>;
  revisions: RevisionPair;
}): CommandEnvelope {
  const semantic = isSemanticCommand(input.type);
  const expected: ExpectedRevision[] = [
    {
      aggregate: { kind: "project_workflow", handle: input.workflowHandle },
      ...(semantic ? { semantic_revision: input.revisions.semantic } : {}),
      ...(!semantic ? { presentation_revision: input.revisions.presentation } : {}),
    },
  ];
  // 创建/删除额外 CAS collection(创建父 CAS;删除双 CAS)
  if (input.type === "workflow.create" || input.type === "workflow.delete") {
    expected.push({
      aggregate: { kind: "project", handle: input.projectHandle },
      semantic_revision: input.revisions.collection,
    });
  }
  return {
    schema: "mf.command.v1",
    command_id: input.commandId,
    client_id: input.clientId,
    controller_lease_epoch: input.controllerLeaseEpoch,
    target: { kind: "project_workflow", handle: input.workflowHandle },
    expected,
    type: input.type,
    payload: input.payload,
  };
}

/** workflow.create envelope(target=project handle;collection CAS 走
 *  payload 字段,与 kernel_bridge 的读取逐字对齐;draft 至少一个节点
 *  ——空节点是 EmptyWorkflow 校验错误)。 */
export function workflowCreateCommand(
  input: {
    commandId: string;
    clientId: string;
    controllerLeaseEpoch: string;
    projectHandle: string;
    name: string;
    /** 首节点标题。 */
    firstNodeTitle: string;
    agentInstanceId: string;
  },
  collectionRevision: string,
): CommandEnvelope {
  // key 仅允许 ASCII 字母数字/-/_(is_valid_key);非 ASCII 一律丢弃。
  // 后缀取 command_id 末 6 位(uuidv7 尾部是随机段;前缀是时间戳,
  // 短时间多次创建会碰撞)。
  const base =
    input.name
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "") || "workflow";
  const suffix = input.commandId.replaceAll("-", "").slice(-6);
  const key = `${base}-${suffix}`;
  return {
    schema: "mf.command.v1",
    command_id: input.commandId,
    client_id: input.clientId,
    controller_lease_epoch: input.controllerLeaseEpoch,
    target: { kind: "project", handle: input.projectHandle },
    expected: [],
    type: "workflow.create",
    payload: {
      draft: {
        key,
        name: input.name.trim(),
        nodes: [
          {
            key: "start",
            title: input.firstNodeTitle.trim(),
            instructions: "",
            agent_instance_id: input.agentInstanceId.trim(),
            deps: [],
          },
        ],
        allow_unsafe_parallel: false,
      },
      expected_collection_revision: collectionRevision,
    },
  };
}

/** 乐观更新模型:提交前记录回滚态;revision_conflict → 回滚 + 提示刷新。 */
export interface OptimisticUpdate<T> {
  pending: T;
  rollback: T;
}

export function beginOptimistic<T>(current: T, next: T): OptimisticUpdate<T> {
  return { pending: next, rollback: current };
}

export function settleOptimistic<T>(
  update: OptimisticUpdate<T>,
  outcome: { ok: true } | { ok: false; code: string },
): { state: T; conflict: boolean } {
  if (outcome.ok) return { state: update.pending, conflict: false };
  // 失败回滚(任何错误);revision_conflict/command_id_reused 额外提示
  return {
    state: update.rollback,
    conflict: outcome.code === "revision_conflict",
  };
}
