// 运行详情视图模型(#74):WorkflowRunSnapshotData wire → 展示 + 响应
// 动作构造(respond/settle/retry)。字段与 kernel projection.rs 逐一对齐
// (标量 revision 经 serialize_u64_decimal 字符串化)。

import type { CommandEnvelope, CommandType } from "../api/protocol.ts";

export interface RunStepView {
  step: string;
  revision: string;
  key: string;
  title: string;
  instructions: string;
  agentInstanceRef: string;
  status: string;
}

export interface RunQuestionView {
  step: string | null;
  agentRun: string | null;
  question: string;
}

export interface RunAgentRunView {
  agentRun: string;
  step: string;
  agentSession: string | null;
  status: string;
  agentState: string;
  outcome: string | null;
}

export interface RunSessionView {
  agentSession: string;
  title: string;
  runtime: string;
  status: string;
}

export interface RunDetailView {
  workflowRun: string;
  revision: string;
  title: string;
  goal: string;
  status: string;
  needsYou: boolean;
  reasonCount: number;
  steps: RunStepView[];
  questions: RunQuestionView[];
  agentRuns: RunAgentRunView[];
  sessions: RunSessionView[];
  focusStep: string | null;
}

type Row = Record<string, unknown>;

export function runDetailViewOf(data: Row): RunDetailView {
  const str = (v: unknown): string => String(v ?? "");
  // ScalarRevision 序列化为 {revision: "3"} 对象形态
  const revisionRaw = data.revision as Row | number | string | undefined;
  const revision =
    typeof revisionRaw === "object" && revisionRaw !== null
      ? String(revisionRaw.revision ?? "0")
      : String(revisionRaw ?? "0");
  return {
    workflowRun: str(data.workflow_run),
    revision,
    title: str(data.title),
    goal: str(data.goal),
    status: str(data.status),
    needsYou: data.needs_you === true,
    reasonCount: Number(data.reason_count ?? 0),
    steps: (Array.isArray(data.steps) ? data.steps : []).map((raw) => {
      const row = raw as Row;
      const revisionRaw = row.revision as Row | number | string | undefined;
      const revision =
        typeof revisionRaw === "object" && revisionRaw !== null
          ? String(revisionRaw.revision ?? "0")
          : String(revisionRaw ?? "0");
      return {
        step: str(row.step),
        revision,
        key: str(row.key),
        title: str(row.title),
        instructions: str(row.instructions),
        agentInstanceRef: str(row.agent_instance_ref),
        status: str(row.status),
      };
    }),
    questions: (Array.isArray(data.open_questions) ? data.open_questions : []).map(
      (raw) => {
        const row = raw as Row;
        return {
          step: typeof row.step === "string" ? row.step : null,
          agentRun: typeof row.agent_run === "string" ? row.agent_run : null,
          question: str(row.question),
        };
      },
    ),
    agentRuns: (Array.isArray(data.agent_runs) ? data.agent_runs : []).map((raw) => {
      const row = raw as Row;
      return {
        agentRun: str(row.agent_run),
        step: str(row.step),
        agentSession: typeof row.agent_session === "string" ? row.agent_session : null,
        status: str(row.status),
        agentState: str(row.agent_state),
        outcome: typeof row.outcome === "string" ? row.outcome : null,
      };
    }),
    sessions: (Array.isArray(data.agent_sessions) ? data.agent_sessions : []).map(
      (raw) => {
        const row = raw as Row;
        return {
          agentSession: str(row.agent_session),
          title: str(row.title),
          runtime: str(row.runtime),
          status: str(row.status),
        };
      },
    ),
    focusStep: typeof data.focus_step === "string" ? data.focus_step : null,
  };
}

/** 该 step 最新的 agent run(settle 目标)。 */
export function agentRunOfStep(detail: RunDetailView, step: string): string | null {
  const candidates = detail.agentRuns.filter((run) => run.step === step);
  return candidates.length > 0 ? candidates[candidates.length - 1].agentRun : null;
}

/** run 级命令 envelope(target=run;project 经 payload——kernel_bridge
 * run 命令族契约;run/step 走 expected CAS;payload 与翻译层对齐)。 */
export function runActionCommand(input: {
  commandId: string;
  clientId: string;
  controllerLeaseEpoch: string;
  projectHandle: string;
  runHandle: string;
  runRevision: string;
  type: CommandType;
  payload: Record<string, unknown>;
}): CommandEnvelope {
  // kernel 快照序列化的 handle 是裸 UUIDv7;wire 校验(handle::parse)
  // 要求 wf_/run_/… 前缀形态,故统一补前缀(translate 层会 strip)。
  const wireRun = input.runHandle.startsWith("run_")
    ? input.runHandle
    : `run_${input.runHandle}`;
  return {
    schema: "mf.command.v1",
    command_id: input.commandId,
    client_id: input.clientId,
    controller_lease_epoch: input.controllerLeaseEpoch,
    target: { kind: "workflow_run", handle: wireRun },
    expected: [
      {
        aggregate: { kind: "workflow_run", handle: wireRun },
        semantic_revision: input.runRevision,
      },
    ],
    type: input.type,
    payload: { project_handle: input.projectHandle, ...input.payload },
  };
}
