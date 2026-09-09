import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { WorkbenchClient } from "../api/client.ts";
import type { WorkflowGraphNode } from "../dag/workflow_graph.ts";

export interface InputBindingView {
  name: string;
  sourceNodeKey: string;
  fieldPath: string;
  required: boolean;
  defaultValue: string | null;
}

export type ContextPolicyView = "legacy_ancestors" | "explicit_only" | "";

export interface EditableNodeDefinition extends WorkflowGraphNode {
  acceptanceCriteria: string;
  outputSchema: unknown | null;
  contextPolicy: ContextPolicyView;
  requireInputReview: boolean;
}

export function toNodeForm(node: EditableNodeDefinition): NodeFormValue {
  return { key: node.key, title: node.title, instance: node.agentInstanceId,
    instructions: node.instructions, acceptanceCriteria: node.acceptanceCriteria,
    outputSchemaText: node.outputSchema == null ? "" : JSON.stringify(node.outputSchema, null, 2),
    inputBindings: node.inputBindings.map((binding) => ({ ...binding })),
    contextPolicy: node.contextPolicy, requireInputReview: node.requireInputReview };
}

export function nodeFromForm(form: NodeFormValue, deps: string[]): EditableNodeDefinition {
  return { key: form.key, title: form.title, instructions: form.instructions,
    agentInstanceId: form.instance, acceptanceCriteria: form.acceptanceCriteria,
    outputSchema: form.outputSchemaText.trim() ? JSON.parse(form.outputSchemaText) : null,
    inputBindings: form.inputBindings, contextPolicy: form.contextPolicy,
    requireInputReview: form.requireInputReview, deps };
}

export function nodeDefinitionWire(node: EditableNodeDefinition): Record<string, unknown> {
  return { key: node.key, title: node.title, instructions: node.instructions,
    agent_instance_id: node.agentInstanceId, deps: node.deps, ...nodeWireExtras(toNodeForm(node)) };
}

export interface NodeFormValue {
  key: string;
  title: string;
  instance: string;
  instructions: string;
  acceptanceCriteria: string;
  /** JSON Schema 文本;空串 = 无约束。 */
  outputSchemaText: string;
  inputBindings: InputBindingView[];
  /** "" = 未设置(旧数据,按遗留祖先摘要语义解释)。 */
  contextPolicy: ContextPolicyView;
  requireInputReview: boolean;
}

/** 三个职责预设:选择后复制为普通节点配置,可自由修改。 */
const NODE_PRESETS: Array<{ label: string; apply: (value: NodeFormValue) => NodeFormValue }> = [
  {
    label: "预设:需求分析",
    apply: (value) => ({
      ...value,
      title: value.title || "需求分析",
      instructions:
        value.instructions ||
        "阅读目标与上游材料,梳理需求与边界,产出结构化分析结论供下游实现使用。",
      acceptanceCriteria:
        value.acceptanceCriteria ||
        "覆盖目标、范围外事项、风险与开放问题;结论可被实现节点直接使用。",
      outputSchemaText:
        value.outputSchemaText ||
        JSON.stringify(
          {
            type: "object",
            properties: {
              report_path: { type: "string", description: "分析报告路径" },
              risks: { type: "array", items: { type: "string" } },
            },
            required: ["report_path"],
          },
          null,
          2,
        ),
    }),
  },
  {
    label: "预设:实现",
    apply: (value) => ({
      ...value,
      title: value.title || "实现",
      instructions:
        value.instructions ||
        "根据上游分析结论实现变更;完成后自检并通过现有测试,汇总改动点与验证结果。",
      acceptanceCriteria:
        value.acceptanceCriteria || "变更可构建、定向测试通过;改动点与验证结果如实汇总。",
      outputSchemaText:
        value.outputSchemaText ||
        JSON.stringify(
          {
            type: "object",
            properties: {
              changed_files: { type: "array", items: { type: "string" } },
              test_command: { type: "string" },
            },
            required: ["changed_files"],
          },
          null,
          2,
        ),
    }),
  },
  {
    label: "预设:审查",
    apply: (value) => ({
      ...value,
      title: value.title || "审查",
      instructions:
        value.instructions ||
        "审查上游实现:对照需求与验收说明检查正确性、边界与测试覆盖,给出明确结论。",
      acceptanceCriteria: value.acceptanceCriteria || "逐条对照验收说明;结论为通过/驳回并给出理由。",
      outputSchemaText:
        value.outputSchemaText ||
        JSON.stringify(
          {
            type: "object",
            properties: {
              verdict: { type: "string", description: "pass | reject" },
              issues: { type: "array", items: { type: "string" } },
            },
            required: ["verdict"],
          },
          null,
          2,
        ),
    }),
  },
];

/** NodeFormValue → wire payload(节点草稿/update 字段共用形态)。 */
export function nodeWireExtras(form: NodeFormValue): Record<string, unknown> {
  let outputSchema: unknown = null;
  if (form.outputSchemaText.trim() !== "") {
    try {
      outputSchema = JSON.parse(form.outputSchemaText);
    } catch {
      outputSchema = form.outputSchemaText.trim(); // 交 Core 校验拒绝(权威)
    }
  }
  return {
    acceptance_criteria: form.acceptanceCriteria,
    output_schema: outputSchema,
    input_bindings: form.inputBindings.map((binding) => ({
      name: binding.name,
      source_node_key: binding.sourceNodeKey,
      field_path: binding.fieldPath,
      required: binding.required,
      default_value: binding.defaultValue === "" ? null : binding.defaultValue,
    })),
    context_policy: form.contextPolicy === "" ? null : form.contextPolicy,
    require_input_review: form.requireInputReview,
  };
}

export interface NodeFormSpec {
  title: string;
  agentOptions: string[];
  initial: NodeFormValue;
  /** 新建节点时允许编辑 key(编辑节点时 key 不可变)。 */
  withKey?: boolean;
  /** 编辑节点时该节点的传递祖先 key(变量引用可见范围;#99)。 */
  upstreamKeys?: string[];
  readOnly?: boolean;
}

/** T2/4.2 模板试算面板:与正式派发共用 Core 编译语义(workflow-template/preview)。
 * 示例值仅本地传给预览端点,不进入覆盖/发送记录。 */
export function TemplatePreviewPanel({ value, upstreamKeys, client }: {
  value: NodeFormValue; upstreamKeys: string[]; client: WorkbenchClient;
  projectHandle?: string; workflowHandle?: string;
}) {
  const [goal, setGoal] = useState("");
  const example = useMemo(() => {
    const data: Record<string, { summary: string; output: Record<string, unknown> }> = Object.create(null);
    for (const key of upstreamKeys) data[key] = { summary: `${key} 的示例摘要`, output: Object.create(null) };
    for (const binding of value.inputBindings) {
      const source = data[binding.sourceNodeKey];
      if (!source || !binding.fieldPath.startsWith("output.")) continue;
      const segments = binding.fieldPath.slice(7).split(".");
      let current = source.output;
      for (const segment of segments.slice(0, -1)) {
        if (!Object.hasOwn(current, segment) || typeof current[segment] !== "object" || current[segment] === null) current[segment] = Object.create(null);
        current = current[segment] as Record<string, unknown>;
      }
      current[segments[segments.length - 1]] = binding.defaultValue ?? `示例_${binding.name}`;
    }
    return JSON.stringify(data, null, 2);
  }, [upstreamKeys.join("\n"), value.inputBindings]);
  const [upstreamJson, setUpstreamJson] = useState<string | null>(null);
  const [result, setResult] = useState<{ business_prompt: string; resolved_instructions: string;
    missing_required: string[]; protocol_segment: string; preview_only: boolean } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // 草稿或示例改变后，旧结果不再冒充当前试算。
  useEffect(() => { setResult(null); }, [value, goal, upstreamJson, example]);
  const runPreview = async () => {
    setBusy(true); setError(null);
    try {
      const upstream: unknown = JSON.parse(upstreamJson ?? example);
      if (typeof upstream !== "object" || upstream === null || Array.isArray(upstream)) throw new Error("示例上游交接须为 JSON 对象");
      const response = await fetch("/api/v1/workflow-template/preview", {
        method: "POST", headers: { "Content-Type": "application/json", "X-Client-Id": client.clientId,
          "X-CSRF-Token": client.csrfToken },
        body: JSON.stringify({ goal, allowed_upstream_keys: upstreamKeys, upstream,
          node: { key: value.key || "preview", title: value.title || value.key || "节点试算",
            agent_instance_id: value.instance || "preview", instructions: value.instructions,
            deps: upstreamKeys, ...nodeWireExtras(value) } }),
      });
      const body = await response.json();
      if (!response.ok) throw new Error(body.message ?? `试算失败 (${response.status})`);
      setResult(body);
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(false); }
  };
  return <details className="field template-preview">
    <summary>模板试算预览</summary>
    <p className="hint">使用示例值检查实际输入格式。试算不启动 Agent，也不会修改工作流或本次发送内容。</p>
    <div className="field">
      <label>任务目标（示例）<input value={goal} onChange={(event) => setGoal(event.target.value)} /></label>
    </div>
    {upstreamKeys.length > 0 && <div className="field">
      <label>示例上游交接（JSON）<textarea rows={7} className="mono-input" value={upstreamJson ?? example}
        onChange={(event) => setUpstreamJson(event.target.value)} /></label>
      <span className="hint">可以填写 summary、output、artifacts 等实际字段；嵌套 output 保留原结构。</span>
    </div>}
    <button type="button" className="mf-btn ghost" disabled={busy} onClick={() => void runPreview()}>{busy ? "试算中…" : "试算"}</button>
    {error && <p className="form-error" role="alert">{error}</p>}
    {result && <div className="node-input-section" aria-label="模板试算结果">
      {result.missing_required.length > 0 && <p className="handoff-meta warn">缺失必填:{result.missing_required.join("、")}</p>}
      <h4>业务 prompt（示例）</h4><pre>{result.business_prompt}</pre>
      <details><summary>只读结算协议</summary><pre>{result.protocol_segment}</pre></details>
    </div>}
  </details>;
}
function emptyBinding(): InputBindingView {
  return { name: "", sourceNodeKey: "", fieldPath: "summary", required: true, defaultValue: null };
}

export function useNodeFormModal(client: WorkbenchClient, projectHandle: string, workflowHandle: string): {
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
    modal: spec ? (
      <NodeFormModal
        spec={spec}
        onSettle={settle}
        client={client}
        projectHandle={projectHandle}
        workflowHandle={workflowHandle}
      />
    ) : null,
  };
}

export function NodeFormModal({
  spec,
  onSettle,
  client,
  projectHandle,
  workflowHandle,
}: {
  spec: NodeFormSpec;
  onSettle: (value: NodeFormValue | null) => void;
  client: WorkbenchClient;
  projectHandle: string;
  workflowHandle: string;
}) {
  const [value, setValue] = useState<NodeFormValue>(spec.initial);
  const [formError, setFormError] = useState<string | null>(null);
  const instructionsRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    setValue(spec.initial);
  }, [spec]);

  const patch = (next: Partial<NodeFormValue>) => setValue((prev) => ({ ...prev, ...next }));

  const insertIntoInstructions = (snippet: string) => {
    const el = instructionsRef.current;
    if (!el) {
      patch({ instructions: value.instructions + snippet });
      return;
    }
    const start = el.selectionStart ?? value.instructions.length;
    const end = el.selectionEnd ?? start;
    const next = value.instructions.slice(0, start) + snippet + value.instructions.slice(end);
    patch({ instructions: next });
    requestAnimationFrame(() => {
      el.focus();
      const caret = start + snippet.length;
      el.setSelectionRange(caret, caret);
    });
  };

  const submit = () => {
    if (spec.readOnly) return;
    const key = value.key.trim();
    const title = value.title.trim() || key;
    const instance = value.instance.trim();
    if (!key && spec.withKey) return; // 新建必须有 key
    // 前端预检只做反馈;Core 校验是权威(非法 schema/映射会被拒绝且不部分写入)
    if (value.outputSchemaText.trim() !== "") {
      try {
        const parsed: unknown = JSON.parse(value.outputSchemaText);
        if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
          setFormError("输出约束必须是 JSON 对象(type: object)");
          return;
        }
      } catch (error) {
        setFormError(`输出约束不是合法 JSON:${error instanceof Error ? error.message : String(error)}`);
        return;
      }
    }
    const upstream = new Set(spec.upstreamKeys ?? []);
    for (const binding of value.inputBindings) {
      if (!binding.name.trim() || !binding.fieldPath.trim()) {
        setFormError("输入映射需要本地名与字段路径");
        return;
      }
      if (!upstream.has(binding.sourceNodeKey)) {
        setFormError(`输入映射来源 ${binding.sourceNodeKey || "(空)"} 不是本节点的上游`);
        return;
      }
    }
    setFormError(null);
    onSettle({ ...value, key, title, instance });
  };

  const contextPolicyHint =
    value.contextPolicy === ""
      ? "未设置(旧语义):prompt 自动附带全部祖先摘要"
      : value.contextPolicy === "explicit_only"
        ? "只传显式选择的内容(输入映射 + 显式引用)"
        : "遗留语义:prompt 自动附带全部祖先摘要";

  return (
    <div
      className="scrim"
      onClick={(event) => {
        if (event.target === event.currentTarget) onSettle(null);
      }}
    >
      <div className="modal node-form-modal" role="dialog" aria-modal="true" aria-label={spec.title}>
        <h3>{spec.title}</h3>
        {spec.readOnly && <p className="hint">该节点已经启动，配置保留为只读；可以继续试算或查看实际发送记录。</p>}
        <fieldset className="node-config-fields" disabled={spec.readOnly}>
        <div className="field-row">
          {NODE_PRESETS.map((preset) => (
            <button
              key={preset.label}
              type="button"
              className="mf-btn ghost"
              onClick={() => setValue(preset.apply(value))}
            >
              {preset.label}
            </button>
          ))}
        </div>

        {spec.withKey && (
          <div className="field">
            <label htmlFor="mf-node-key">节点 key(ASCII,创建后不可改)</label>
            <input
              id="mf-node-key"
              autoFocus
              value={value.key}
              placeholder="如 build / test / report"
              onChange={(event) => patch({ key: event.target.value })}
              onKeyDown={(event) => {
                if (event.key === "Enter") submit();
                if (event.key === "Escape") onSettle(null);
              }}
            />
          </div>
        )}

        <div className="field">
          <label htmlFor="mf-node-title">职责(标题)</label>
          <input
            id="mf-node-title"
            autoFocus={!spec.withKey}
            value={value.title}
            placeholder="节点显示名"
            onChange={(event) => patch({ title: event.target.value })}
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
            onChange={(event) => patch({ instance: event.target.value })}
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
          <label htmlFor="mf-node-instructions">职责(任务说明;可引用上游输出与输入映射)</label>
          <textarea
            id="mf-node-instructions"
            ref={instructionsRef}
            rows={5}
            value={value.instructions}
            placeholder={"这个节点让 agent 做什么…\n例:读取 ${inputs.report} 后汇总,引用 ${nodes.build.summary}"}
            onChange={(event) => patch({ instructions: event.target.value })}
            onKeyDown={(event) => {
              if (event.key === "Escape") onSettle(null);
            }}
          />
          <span className="hint">
            {"引用语法 ${nodes.<上游key>.<字段>} 与 ${inputs.<映射名>};可用字段 .summary / .status / .changed_files / .artifacts / .blockers / .recommendations / .output.<嵌套键>"}
            {spec.upstreamKeys && spec.upstreamKeys.length > 0
              ? `。本节点可引用上游:${spec.upstreamKeys.join("、")}`
              : spec.upstreamKeys
                ? "。本节点暂无上游(先连线才能引用)"
                : ""}
          </span>
          {spec.upstreamKeys && spec.upstreamKeys.length > 0 && (
            <div className="field-row">
              <select
                aria-label="插入上游引用"
                defaultValue={spec.upstreamKeys[0]}
              >
                {spec.upstreamKeys.map((key) => (
                  <option key={key} value={key}>
                    {`上游 ${key}`}
                  </option>
                ))}
              </select>
              <button
                type="button"
                className="mf-btn ghost"
                onClick={(event) => {
                  const select = (event.currentTarget.previousSibling as HTMLSelectElement | null);
                  const key = select?.value ?? spec.upstreamKeys?.[0] ?? "";
                  if (key) insertIntoInstructions(`\${nodes.${key}.summary}`);
                }}
              >
                插入 .summary
              </button>
              <button
                type="button"
                className="mf-btn ghost"
                onClick={(event) => {
                  const select = (event.currentTarget.previousSibling as HTMLSelectElement | null);
                  const previous = select?.previousSibling as HTMLSelectElement | null;
                  const key =
                    previous?.value ?? select?.value ?? spec.upstreamKeys?.[0] ?? "";
                  if (key) insertIntoInstructions(`\${nodes.${key}.output.字段}`);
                }}
              >
                插入 .output.字段
              </button>
            </div>
          )}
        </div>

        <div className="field">
          <label htmlFor="mf-node-acceptance">验收说明(进入 prompt;可执行约束写在输出约束里)</label>
          <textarea
            id="mf-node-acceptance"
            rows={3}
            value={value.acceptanceCriteria}
            placeholder="怎样算完成…(文字要求不由程序验证;成功仍来自显式结算)"
            onChange={(event) => patch({ acceptanceCriteria: event.target.value })}
          />
        </div>

        <div className="field">
          <label htmlFor="mf-node-schema">输出约束(Handoff.output 的 JSON Schema;object 形态,支持 type/properties/required/items/description)</label>
          <textarea
            id="mf-node-schema"
            rows={4}
            className="mono-input"
            value={value.outputSchemaText}
            placeholder={'{"type":"object","properties":{"report_path":{"type":"string"}},"required":["report_path"]}'}
            onChange={(event) => patch({ outputSchemaText: event.target.value })}
          />
        </div>

        <div className="field">
          <label>输入映射(显式选择上游字段进入本次输入;经 {"${inputs.<名>}"} 引用)</label>
          {value.inputBindings.length === 0 && (
            <span className="hint">未配置映射;explicit_only 策略下节点只收到显式引用的内容</span>
          )}
          {value.inputBindings.map((binding, index) => (
            <div className="field-row binding-row" key={index}>
              <input
                aria-label="映射名"
                value={binding.name}
                placeholder="映射名"
                onChange={(event) => {
                  const next = [...value.inputBindings];
                  next[index] = { ...binding, name: event.target.value };
                  patch({ inputBindings: next });
                }}
              />
              <select
                aria-label="来源节点"
                value={binding.sourceNodeKey}
                onChange={(event) => {
                  const next = [...value.inputBindings];
                  next[index] = { ...binding, sourceNodeKey: event.target.value };
                  patch({ inputBindings: next });
                }}
              >
                <option value="">来源…</option>
                {(spec.upstreamKeys ?? []).map((key) => (
                  <option key={key} value={key}>
                    {key}
                    {(spec.upstreamKeys ?? []).indexOf(key) >= 0 ? "" : ""}
                  </option>
                ))}
              </select>
              <input
                aria-label="字段路径"
                value={binding.fieldPath}
                placeholder="summary / output.report_path"
                onChange={(event) => {
                  const next = [...value.inputBindings];
                  next[index] = { ...binding, fieldPath: event.target.value };
                  patch({ inputBindings: next });
                }}
              />
              <label className="inline-check">
                <input
                  type="checkbox"
                  checked={binding.required}
                  onChange={(event) => {
                    const next = [...value.inputBindings];
                    next[index] = { ...binding, required: event.target.checked,
                      defaultValue: event.target.checked ? null : binding.defaultValue };
                    patch({ inputBindings: next });
                  }}
                />
                必填
              </label>
              <input
                aria-label="默认值(可选)"
                value={binding.defaultValue ?? ""}
                placeholder="默认值(可选)"
                disabled={binding.required}
                onChange={(event) => {
                  const next = [...value.inputBindings];
                  next[index] = { ...binding, defaultValue: event.target.value };
                  patch({ inputBindings: next });
                }}
              />
              <button
                type="button"
                className="mf-btn ghost"
                onClick={() =>
                  patch({ inputBindings: value.inputBindings.filter((_, i) => i !== index) })
                }
              >
                删除
              </button>
            </div>
          ))}
          <div className="field-row">
            <button
              type="button"
              className="mf-btn ghost"
              onClick={() => patch({ inputBindings: [...value.inputBindings, emptyBinding()] })}
            >
              ＋映射
            </button>
            {value.inputBindings.length > 0 && (
              <button
                type="button"
                className="mf-btn ghost"
                onClick={() =>
                  insertIntoInstructions(
                    `\${inputs.${value.inputBindings[0]?.name || "映射名"}}`,
                  )
                }
              >
                插入 {"${inputs.…}"}
              </button>
            )}
          </div>
        </div>

        <div className="field">
          <label htmlFor="mf-node-policy">上下文策略(哪些内容进入 prompt)</label>
          <select
            id="mf-node-policy"
            value={value.contextPolicy}
            onChange={(event) =>
              patch({
                contextPolicy: event.target.value as ContextPolicyView,
              })
            }
          >
            <option value="">未设置(旧语义)</option>
            <option value="explicit_only">explicit_only(只传显式选择)</option>
            <option value="legacy_ancestors">legacy_ancestors(祖先摘要)</option>
          </select>
          <span className="hint">{contextPolicyHint}</span>
        </div>

        <div className="field">
          <label className="inline-check">
            <input
              type="checkbox"
              checked={value.requireInputReview}
              onChange={(event) => patch({ requireInputReview: event.target.checked })}
            />
            派发前人工检查本次输入(下游就绪后暂停,等确认再启动)
          </label>
        </div>

        </fieldset>
        <TemplatePreviewPanel
          value={value}
          upstreamKeys={spec.upstreamKeys ?? []}
          client={client}
          projectHandle={projectHandle}
          workflowHandle={workflowHandle}
        />

        {formError && <div className="form-error">{formError}</div>}

        <div className="actions">
          <button className="mf-btn ghost" onClick={() => onSettle(null)}>
            {spec.readOnly ? "关闭" : "取消"}
          </button>
          {!spec.readOnly && <button className="mf-btn primary" onClick={submit}>
            保存
          </button>}
        </div>
      </div>
    </div>
  );
}

export function AddNodeButton({
  busy,
  agentOptions,
  onAdd,
  client,
  projectHandle,
  workflowHandle,
}: {
  busy: boolean;
  agentOptions: string[];
  onAdd: (form: NodeFormValue) => void;
  client: WorkbenchClient;
  projectHandle: string;
  workflowHandle: string;
}) {
  const nodeFormModal = useNodeFormModal(client, projectHandle, workflowHandle);
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
              initial: {
                key: "",
                title: "",
                instance: agentOptions[0] ?? "",
                instructions: "",
                acceptanceCriteria: "",
                outputSchemaText: "",
                inputBindings: [],
                // 新节点默认显式上下文策略(只传显式选择内容)
                contextPolicy: "explicit_only",
                requireInputReview: false,
              },
            });
            if (!form || !form.key) return;
            onAdd(form);
          })();
        }}
      >
        ＋节点
      </button>
    </>
  );
}
