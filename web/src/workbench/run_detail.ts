// 运行详情视图模型(#74):WorkflowRunSnapshotData wire → 展示 + 响应
// 动作构造(respond/settle/retry)。字段与 kernel projection.rs 逐一对齐
// (标量 revision 经 serialize_u64_decimal 字符串化)。

import type { CommandEnvelope, CommandType } from "../api/protocol.ts";

export interface RunStepView {
  step: string;
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
  return {
    workflowRun: str(data.workflow_run),
    revision: str(data.revision),
    title: str(data.title),
    goal: str(data.goal),
    status: str(data.status),
    needsYou: data.needs_you === true,
    reasonCount: Number(data.reason_count ?? 0),
    steps: (Array.isArray(data.steps) ? data.steps : []).map((raw) => {
      const row = raw as Row;
      return {
        step: str(row.step),
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

/** run 级命令 envelope(target=run;CAS run revision;payload 与
 * kernel_bridge 读取字段逐一对齐)。 */
export function runActionCommand(input: {
  commandId: string;
  clientId: string;
  controllerLeaseEpoch: string;
  runHandle: string;
  runRevision: string;
  type: CommandType;
  payload: Record<string, unknown>;
}): CommandEnvelope {
  return {
    schema: "mf.command.v1",
    command_id: input.commandId,
    client_id: input.clientId,
    controller_lease_epoch: input.controllerLeaseEpoch,
    target: { kind: "workflow_run", handle: input.runHandle },
    expected: [
      {
        aggregate: { kind: "workflow_run", handle: input.runHandle },
        semantic_revision: input.runRevision,
      },
    ],
    type: input.type,
    payload: input.payload,
  };
}
