// 运行详情视图模型(#74):WorkflowRunSnapshotData wire → 展示 + 响应
// 动作构造(respond/settle/retry)。字段与 kernel projection.rs 逐一对齐
// (标量 revision 经 serialize_u64_decimal 字符串化)。

import type { CommandEnvelope, CommandType } from "../api/protocol.ts";

export interface RunStepInputBindingView {
  name: string;
  sourceNodeKey: string;
  fieldPath: string;
  required: boolean;
  value: string | null;
  defaultValue: string | null;
  missingReason: string | null;
}

export interface RunStepInputView {
  status: string;
  /** R4:输入版本轴(保存覆盖推进;确认/保存 CAS 绑定它)。 */
  inputRevision: string;
  summary: string;
  createdAt: string;
  agentRun: string | null;
  template: string;
  resolvedInstructions: string;
  businessPrompt: string;
  protocolSegment: string;
  bindings: RunStepInputBindingView[];
  upstreamSummaries: Array<{ nodeKey: string; summary: string; agentRun: string | null }>;
  missingRequired: string[];
  contextPolicy: string;
  reviewState: string;
  /** 用户覆盖(awaiting_review 期间可编辑)。 */
  overrides: {
    bindingValues: Record<string, string>;
    businessPrompt: string | null;
  } | null;
  /** 应用覆盖后的业务 prompt(确认实际发送的内容)。 */
  effectiveBusinessPrompt: string | null;
}

export interface RunStepView {
  step: string;
  revision: string;
  key: string;
  title: string;
  instructions: string;
  agentInstanceRef: string;
  status: string;
  attempts: number;
  /** 依赖步骤句柄 → 面板内映射回 key。 */
  dependencies: string[];
  input: RunStepInputView | null;
  /** R1:活动 Revision 冻结的完整节点定义(真实实例 id + 扩展字段)。 */
  agentInstanceId: string;
  acceptanceCriteria: string;
  outputSchema: unknown | null;
  inputBindings: Array<{
    name: string;
    sourceNodeKey: string;
    fieldPath: string;
    required: boolean;
    defaultValue: string | null;
  }>;
  contextPolicy: "legacy_ancestors" | "explicit_only" | "";
  requireInputReview: boolean;
}

export interface RunQuestionView {
  step: string | null;
  agentRun: string | null;
  question: string;
}

export interface RunAgentRunView {
  agentRun: string;
  revision: string;
  step: string;
  agentSession: string | null;
  status: string;
  agentState: string;
  outcome: string | null;
}

export interface RunSessionView {
  agentSession: string;
  revision: string;
  title: string;
  runtime: string;
  status: string;
}

export interface PendingProposalView {
  revisionHandle: string;
  revision: string;
  steps: Array<{ key: string; title: string; agent: string }>;
}

export interface RunHandoffView {
  step: string;
  summary: string;
  status: string;
  changedFiles: string[];
  artifacts: string[];
  blockers: string[];
  recommendations: string[];
  /** 自定义结构化输出(mfctl --output-json);空对象(null)省略显示。 */
  outputJson: string | null;
}

export interface RunDetailView {
  workflowRun: string;
  revision: string;
  /** 活动流水线版本(T4 图补丁的基线句柄)。 */
  pipelineRevision: { handle: string; number: string } | null;
  paused: boolean;
  title: string;
  goal: string;
  status: string;
  needsYou: boolean;
  reasonCount: number;
  steps: RunStepView[];
  questions: RunQuestionView[];
  agentRuns: RunAgentRunView[];
  sessions: RunSessionView[];
  handoffs: RunHandoffView[];
  focusStep: string | null;
  pendingProposals: PendingProposalView[];
}

type Row = Record<string, unknown>;

/** 节点输入冻结记录(T2;dispatched = 已发送,只读)。 */
function stepInputOf(raw: unknown): RunStepInputView | null {
  if (typeof raw !== "object" || raw === null) return null;
  const row = raw as Row;
  const str = (v: unknown): string => String(v ?? "");
  return {
    status: str(row.status),
    inputRevision: str(row.input_revision) || "1",
    summary: str(row.summary),
    createdAt: str(row.created_at),
    agentRun: typeof row.agent_run === "string" ? row.agent_run : null,
    template: str(row.template),
    resolvedInstructions: str(row.resolved_instructions),
    businessPrompt: str(row.business_prompt),
    protocolSegment: str(row.protocol_segment),
    bindings: (Array.isArray(row.bindings) ? row.bindings : []).map((entry) => {
      const binding = entry as Row;
      return {
        name: str(binding.name),
        sourceNodeKey: str(binding.source_node_key),
        fieldPath: str(binding.field_path),
        required: binding.required === true,
        value: binding.value == null ? null : str(binding.value),
        defaultValue: binding.default_value == null ? null : str(binding.default_value),
        missingReason:
          typeof binding.missing_reason === "string" ? binding.missing_reason : null,
      };
    }),
    upstreamSummaries: (Array.isArray(row.upstream_summaries) ? row.upstream_summaries : []).map(
      (entry) => {
        const upstream = entry as Row;
        return {
          nodeKey: str(upstream.node_key),
          summary: str(upstream.summary),
          agentRun: typeof upstream.agent_run_handle === "string" ? upstream.agent_run_handle : null,
        };
      },
    ),
    missingRequired: (Array.isArray(row.missing_required) ? row.missing_required : []).map(
      (entry) => str(entry),
    ),
    contextPolicy: str(row.context_policy),
    reviewState: str(row.review_state) || "none",
    overrides: stepOverridesOf(row.overrides),
    effectiveBusinessPrompt:
      typeof row.effective_business_prompt === "string" ? row.effective_business_prompt : null,
  };
}

function stepOverridesOf(raw: unknown): RunStepInputView["overrides"] {
  if (typeof raw !== "object" || raw === null) return null;
  const row = raw as Row;
  const str = (v: unknown): string => String(v ?? "");
  const values = (typeof row.binding_values === "object" && row.binding_values !== null
    ? row.binding_values
    : {}) as Record<string, unknown>;
  const bindingValues: Record<string, string> = {};
  for (const [key, value] of Object.entries(values)) {
    bindingValues[key] = str(value);
  }
  return {
    bindingValues,
    businessPrompt: typeof row.business_prompt === "string" ? row.business_prompt : null,
  };
}

export function runDetailViewOf(data: Row): RunDetailView {
  const str = (v: unknown): string => String(v ?? "");
  // ScalarRevision 序列化为 {revision: "3"} 对象形态
  const revisionRaw = data.revision as Row | number | string | undefined;
  const revision =
    typeof revisionRaw === "object" && revisionRaw !== null
      ? String(revisionRaw.revision ?? "0")
      : String(revisionRaw ?? "0");
  const pipelineRevisionRaw = data.pipeline_revision as Row | null | undefined;
  const pipelineRevision =
    pipelineRevisionRaw && typeof pipelineRevisionRaw === "object"
      ? {
          handle: str(pipelineRevisionRaw.handle),
          number: str(pipelineRevisionRaw.number),
        }
      : null;
  const handleToKey = new Map<string, string>();
  for (const raw of Array.isArray(data.steps) ? data.steps : []) {
    const row = raw as Row;
    handleToKey.set(str(row.step), str(row.key));
  }
  return {
    workflowRun: str(data.workflow_run),
    revision,
    pipelineRevision,
    paused: data.paused === true,
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
        attempts: Number(row.attempts ?? 0),
        dependencies: (Array.isArray(row.dependencies) ? row.dependencies : []).map(
          (dep) => handleToKey.get(str(dep)) ?? str(dep),
        ),
        input: stepInputOf(row.input),
        agentInstanceId: str(row.agent_instance_id),
        acceptanceCriteria: str(row.acceptance_criteria),
        outputSchema: row.output_schema ?? null,
        inputBindings: (Array.isArray(row.input_bindings) ? row.input_bindings : []).map(
          (entry) => {
            const binding = entry as Row;
            return {
              name: str(binding.name),
              sourceNodeKey: str(binding.source_node_key),
              fieldPath: str(binding.field_path),
              required: binding.required === true,
              defaultValue:
                binding.default_value == null ? null : str(binding.default_value),
            };
          },
        ),
        contextPolicy:
          row.context_policy === "legacy_ancestors" || row.context_policy === "explicit_only"
            ? row.context_policy
            : "",
        requireInputReview: row.require_input_review === true,
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
      const agentRevision = row.revision as Row | number | string | undefined;
      return {
        agentRun: str(row.agent_run),
        revision:
          typeof agentRevision === "object" && agentRevision !== null
            ? String(agentRevision.revision ?? "0")
            : String(agentRevision ?? "0"),
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
        const sessionRevision = row.revision as Row | number | string | undefined;
        return {
          agentSession: str(row.agent_session),
          revision:
            typeof sessionRevision === "object" && sessionRevision !== null
              ? String(sessionRevision.revision ?? "0")
              : String(sessionRevision ?? "0"),
          title: str(row.title),
          runtime: str(row.runtime),
          status: str(row.status),
        };
      },
    ),
    focusStep: typeof data.focus_step === "string" ? data.focus_step : null,
    handoffs: (Array.isArray(data.handoffs) ? data.handoffs : [])
      .map((raw) => {
        const row = raw as Row;
        const handoff = (row.handoff ?? {}) as Row;
        const output = handoff.output;
        const outputJson =
          output !== null && typeof output === "object" && Object.keys(output).length > 0
            ? JSON.stringify(output, null, 2)
            : null;
        const listOf = (v: unknown): string[] =>
          Array.isArray(v) ? v.map((item) => String(item)) : [];
        return {
          step: typeof row.step === "string" ? row.step : "",
          summary: String(handoff.summary ?? ""),
          status: String(handoff.status ?? ""),
          changedFiles: listOf(handoff.changed_files),
          artifacts: listOf(handoff.artifacts),
          blockers: listOf(handoff.blockers),
          recommendations: listOf(handoff.recommendations),
          outputJson,
        };
      }),
    pendingProposals: (Array.isArray(data.pending_proposals) ? data.pending_proposals : []).map(
      (raw) => {
        const row = raw as Row;
        return {
          revisionHandle: str(row.revision_handle),
          revision: str(row.revision),
          steps: (Array.isArray(row.steps) ? row.steps : []).map((step) => {
            const s = step as Row;
            return { key: str(s.key), title: str(s.title), agent: str(s.agent) };
          }),
        };
      },
    ),
  };
}

/** 该 step 最新的 agent run(settle 目标)。 */
export function agentRunOfStep(detail: RunDetailView, step: string): string | null {
  const view = agentRunViewOfStep(detail, step);
  return view ? view.agentRun : null;
}

/** 该 step 最新的 agent run 完整视图(settle 需要 handle+revision)。 */
export function agentRunViewOfStep(
  detail: RunDetailView,
  step: string,
): RunAgentRunView | null {
  const candidates = detail.agentRuns.filter((run) => run.step === step);
  return candidates.length > 0 ? candidates[candidates.length - 1] : null;
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
