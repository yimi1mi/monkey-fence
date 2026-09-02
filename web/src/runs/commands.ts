// Workflow Run / Needs You / Settlement / Session 控制命令(T8c,#54)。
// Run 命令 CAS 语义:workflow.run.start 只接受 Core 已确认的
// semantic_revision;run 级命令 CAS run revision;进程 exit/终端 idle/
// done 不等于 Settlement——必须显式 settle。
import type { CommandEnvelope, CommandType } from "../api/protocol.ts";

export interface RunCommandContext {
  commandId: string;
  clientId: string;
  controllerLeaseEpoch: string;
}

/** run 命令 envelope(目标 = workflow 或 run;expected = run revision)。 */
export function runCommand(
  ctx: RunCommandContext,
  input: {
    type: CommandType;
    workflowHandle?: string;
    runHandle: string;
    runRevision: string;
    payload: Record<string, unknown>;
  },
): CommandEnvelope {
  const isStart = input.type === "workflow.run.start";
  return {
    schema: "mf.command.v1",
    command_id: ctx.commandId,
    client_id: ctx.clientId,
    controller_lease_epoch: ctx.controllerLeaseEpoch,
    target: {
      kind: isStart ? "project_workflow" : "workflow_run",
      handle: isStart ? input.workflowHandle! : input.runHandle,
    },
    expected: [
      isStart
        ? {
            aggregate: { kind: "project_workflow", handle: input.workflowHandle! },
            semantic_revision: input.runRevision,
          }
        : {
            aggregate: { kind: "workflow_run", handle: input.runHandle },
            semantic_revision: input.runRevision,
          },
    ],
    type: input.type,
    payload: input.payload,
  };
}

/** Settlement 判定:exit/idle/done 均不是结算——必须用户显式动作。 */
export type RunSignal = "process_exit" | "terminal_idle" | "done_reported" | "user_settle";

export function requiresExplicitSettlement(signal: RunSignal): boolean {
  // 一切非用户显式信号都进入 Needs You 等待结算
  return signal !== "user_settle";
}

/** Needs You 过滤:reason 分桶(用户可采取动作)。 */
export type NeedsYouReason =
  | "awaiting_outcome"
  | "question"
  | "crash_incomplete"
  | "lost_session";

export function needsYouFilter(
  items: Array<{ run: string; reason: string }>,
  filter: "all" | NeedsYouReason,
): Array<{ run: string; reason: string }> {
  if (filter === "all") return items;
  return items.filter((item) => item.reason === filter);
}

/** 双击节点:只建立待附着 Agent Session 的 navigation intent(§6.4);
 *  terminal attach 不产生命令——T10 经 WS。 */
export interface NavigationIntent {
  kind: "attach-session";
  runHandle: string;
  stepHandle: string;
  /** pending:会话尚不存在(等待创建);created:可附着。 */
  state: "pending" | "created";
}

export function openNodeIntent(
  runHandle: string,
  stepHandle: string,
  sessionExists: boolean,
): NavigationIntent {
  return {
    kind: "attach-session",
    runHandle,
    stepHandle,
    state: sessionExists ? "created" : "pending",
  };
}

/** Session 控制命令族(start/stop preview 与 ad-hoc)。 */
export function sessionCommand(
  ctx: RunCommandContext,
  input: {
    type: CommandType;
    projectHandle: string;
    payload: Record<string, unknown>;
  },
): CommandEnvelope {
  return {
    schema: "mf.command.v1",
    command_id: ctx.commandId,
    client_id: ctx.clientId,
    controller_lease_epoch: ctx.controllerLeaseEpoch,
    target: { kind: "project", handle: input.projectHandle },
    expected: [],
    type: input.type,
    payload: input.payload,
  };
}
