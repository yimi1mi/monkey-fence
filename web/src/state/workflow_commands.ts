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
