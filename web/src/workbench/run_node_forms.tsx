import { useEffect, useRef, useState } from "react";
import type { RunStepInputView } from "./run_detail.ts";
import type { CommandType } from "../api/protocol.ts";

export type RunAct = (
  type: CommandType,
  payload: Record<string, unknown>,
  stepHandle?: string,
  stepRevision?: string,
) => Promise<boolean>;

/** T2 节点输入卡:指令模板 / 解析后的输入 / 本次发送内容。
 * T3:awaiting_review 时提供覆盖编辑与确认发送(持久门控)。 */
export function NodeInputCard({
  input,
  canEdit,
  stepHandle,
  stepRevision,
  onAct,
}: {
  input: RunStepInputView;
  canEdit: boolean;
  stepHandle: string;
  stepRevision: string;
  onAct: RunAct;
}) {
  const awaiting = input.reviewState === "awaiting_review";
  // S2:草稿锁定创建时的输入版本。编辑中不因新快照推进版本/内容——
  // 否则旧草稿会伪装成新版本上的修改绕过服务端 CAS;冲突显式提示,
  // 由用户选择放弃草稿重新加载。
  const draftBaseRevision = useRef(input.inputRevision);
  const [conflict, setConflict] = useState(false);
  const [editing, setEditing] = useState(false);
  useEffect(() => {
    if (!editing) {
      draftBaseRevision.current = input.inputRevision;
    } else if (input.inputRevision !== draftBaseRevision.current) {
      setConflict(true);
    }
  }, [input.inputRevision, editing]);
  const [promptText, setPromptText] = useState(
    input.overrides?.businessPrompt ?? input.effectiveBusinessPrompt ?? input.businessPrompt,
  );
  const [bindingValues, setBindingValues] = useState<Record<string, string>>(
    input.overrides?.bindingValues ?? {},
  );
  useEffect(() => {
    if (!editing) {
      setPromptText(
        input.overrides?.businessPrompt ??
          input.effectiveBusinessPrompt ??
          input.businessPrompt,
      );
      setBindingValues(input.overrides?.bindingValues ?? {});
    }
  }, [editing, input]);
  const [busy, setBusy] = useState(false);
  const dispatchState =
    input.status === "dispatched"
      ? "已发送(只读)"
      : input.missingRequired.length > 0
        ? "未启动(必填缺失)"
        : "已冻结(未发送)";
  const copy = (text: string) => {
    void navigator.clipboard?.writeText(text);
  };
  const gateState = awaiting
    ? "等待确认(未派发)"
    : input.reviewState === "confirmed" && input.status !== "dispatched"
      ? "已确认(待派发)"
      : dispatchState;
  // R4/S2:保存/确认携带草稿创建时的 input_revision(服务端 CAS);
  // 保存失败不得继续确认(act 返回布尔,错误已由 toast 呈现)。
  // 冲突时禁止保存——旧草稿不得伪装成新版本上的修改。
  const saveOverrides = async (): Promise<boolean> => {
    if (conflict) return false;
    setBusy(true);
    try {
      const ok = await onAct(
        "workflow.run.save_input_overrides",
        {
          step_handle: stepHandle,
          input_revision: draftBaseRevision.current,
          overrides: {
            ...(Object.keys(bindingValues).length > 0
              ? { binding_values: bindingValues }
              : {}),
            ...(promptText.trim() !== "" && promptText !== input.businessPrompt
              ? { business_prompt: promptText }
              : {}),
          },
        },
        stepHandle,
        stepRevision,
      );
      if (ok) {
        // 以服务端接受后的下一版本为新草稿基线
        draftBaseRevision.current = String(BigInt(draftBaseRevision.current) + 1n);
      }
      return ok;
    } finally {
      setBusy(false);
    }
  };
  const confirmInput = async (withEdits: boolean) => {
    setBusy(true);
    try {
      // 含修改:先保存覆盖(仅 awaiting_review 可写),失败即中止
      if (conflict) return;
      if (withEdits && !(await saveOverrides())) return;
      await onAct(
        "workflow.run.confirm_input",
        { step_handle: stepHandle, input_revision: draftBaseRevision.current },
        stepHandle,
        stepRevision,
      );
    } finally {
      setBusy(false);
    }
  };
  return (
    <details className="node-input-card" open={awaiting}>
      <summary>
        节点输入 · {gateState} · {input.summary}
      </summary>
      <div className="node-input-body">
        {conflict && (
          <p className="handoff-meta warn">
            输入已在新版本上更新(草稿基于 rev {draftBaseRevision.current},当前 rev{" "}
            {input.inputRevision})。旧草稿不会被静默套用到新版本——请「放弃草稿并
            重新加载」后基于新内容重新编辑。
          </p>
        )}
        {awaiting && canEdit && (
          <div className="node-input-gate">
            {!editing ? (
              <div className="field-row">
                <button className="mf-btn ghost" onClick={() => setEditing(true)} disabled={busy}>
                  ✎ 检查并编辑本次输入
                </button>
                <button
                  className="mf-btn primary"
                  onClick={() => void confirmInput(false)}
                  disabled={busy}
                >
                  确认发送(自动值)
                </button>
              </div>
            ) : (
              <div className="gate-editor">
                <div className="field">
                  <label>业务 prompt(覆盖自动生成值;协议段只读不可改)</label>
                  <textarea
                    rows={6}
                    className="mono-input"
                    value={promptText}
                    onChange={(event) => setPromptText(event.target.value)}
                  />
                </div>
                {input.missingRequired.length > 0 && (
                  <div className="field">
                    <label>必填缺失补值(来源缺失时由用户补值)</label>
                    {input.missingRequired.map((name) => (
                      <input
                        key={name}
                        aria-label={`补值 ${name}`}
                        placeholder={`补值 ${name}`}
                        value={bindingValues[name] ?? ""}
                        onChange={(event) =>
                          setBindingValues((prev) => ({ ...prev, [name]: event.target.value }))
                        }
                      />
                    ))}
                  </div>
                )}
                <div className="field-row">
                  <button
                    className="mf-btn ghost"
                    onClick={() => void saveOverrides()}
                    disabled={busy || conflict}
                  >
                    保存覆盖
                  </button>
                  <button
                    className="mf-btn primary"
                    onClick={() => void confirmInput(true)}
                    disabled={busy || conflict}
                  >
                    确认发送(含修改)
                  </button>
                  {conflict ? (
                    <button
                      className="mf-btn ghost"
                      onClick={() => {
                        setConflict(false);
                        setEditing(false);
                        draftBaseRevision.current = input.inputRevision;
                        setPromptText(
                          input.overrides?.businessPrompt ??
                            input.effectiveBusinessPrompt ??
                            input.businessPrompt,
                        );
                        setBindingValues(input.overrides?.bindingValues ?? {});
                      }}
                    >
                      放弃草稿并重新加载
                    </button>
                  ) : (
                    <button
                      className="mf-btn ghost"
                      onClick={() => setEditing(false)}
                      disabled={busy}
                    >
                      取消编辑
                    </button>
                  )}
                </div>
              </div>
            )}
            {promptText !== input.businessPrompt && (
              <p className="handoff-meta warn">已修改自动生成的业务 prompt(差异随记录保存)</p>
            )}
          </div>
        )}
        {input.missingRequired.length > 0 && (
          <p className="handoff-meta warn">
            必填缺失:{input.missingRequired.join("、")}
          </p>
        )}
        {input.bindings.length > 0 && (
          <div className="node-input-section">
            <h4>输入映射解析</h4>
            <ul className="binding-list">
              {input.bindings.map((binding) => (
                <li key={binding.name}>
                  <span className="mono-dim">{binding.name}</span> ←{" "}
                  <span className="mono-dim">
                    {binding.sourceNodeKey}.{binding.fieldPath}
                  </span>
                  {binding.required ? "(必填)" : "(可选)"}
                  {binding.value != null ? `:${binding.value}` : ""}
                  {binding.value == null && binding.defaultValue != null
                    ? `(默认:${binding.defaultValue})`
                    : ""}
                  {binding.missingReason ? (
                    <span className="warn"> ⚠ {binding.missingReason}</span>
                  ) : null}
                  {binding.sourceNodeKey && input.agentRun ? (
                    <span className="mono-dim"> · 来源 {input.agentRun}</span>
                  ) : null}
                </li>
              ))}
            </ul>
          </div>
        )}
        {input.template && (
          <div className="node-input-section">
            <h4>
              指令模板{" "}
              <button className="mf-btn ghost tiny" onClick={() => copy(input.template)}>
                复制
              </button>
            </h4>
            <pre>{input.template}</pre>
          </div>
        )}
        {input.upstreamSummaries.length > 0 && (
          <div className="node-input-section">
            <h4>上游摘要(随输入注入)</h4>
            <ul className="binding-list">
              {input.upstreamSummaries.map((upstream) => (
                <li key={upstream.nodeKey}>
                  <span className="mono-dim">{upstream.nodeKey}</span>:{upstream.summary || "(无摘要)"}
                </li>
              ))}
            </ul>
          </div>
        )}
        <div className="node-input-section">
          <h4>
            本次发送内容(业务 prompt{input.effectiveBusinessPrompt != null &&
            input.effectiveBusinessPrompt !== input.businessPrompt
              ? ",含用户修改"
              : ""}
            ){" "}
            <button
              className="mf-btn ghost tiny"
              onClick={() => copy(input.effectiveBusinessPrompt ?? input.businessPrompt)}
            >
              复制
            </button>
          </h4>
          <pre>{input.effectiveBusinessPrompt ?? input.businessPrompt}</pre>
        </div>
        <div className="node-input-section">
          <h4>
            结算协议段(只读){" "}
            <button className="mf-btn ghost tiny" onClick={() => copy(input.protocolSegment)}>
              复制
            </button>
          </h4>
          <pre>{input.protocolSegment}</pre>
        </div>
      </div>
    </details>
  );
}

export function SettleCard({
  disabled,
  onSettle,
}: {
  disabled: boolean;
  onSettle: (kind: "complete" | "fail", text: string, outputJson: string | null) => Promise<boolean>;
}) {
  const [text, setText] = useState("");
  const [outputText, setOutputText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const submit = async (kind: "complete" | "fail") => {
    const trimmed = outputText.trim();
    if (kind === "complete" && trimmed !== "") {
      try {
        const parsed: unknown = JSON.parse(trimmed);
        if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
          setError("输出必须是 JSON 对象");
          return;
        }
      } catch (e) {
        setError(`输出不是合法 JSON:${e instanceof Error ? e.message : String(e)}`);
        return;
      }
    }
    setError(null);
    setBusy(true);
    try {
      const applied = await onSettle(
        kind,
        text.trim() || (kind === "complete" ? "完成" : "未说明原因"),
        kind === "complete" && trimmed !== "" ? trimmed : null,
      );
      if (!applied) setError("结算未生效，请检查输出要求或刷新后的状态再试。");
    } finally { setBusy(false); }
  };
  return (
    <div className="question-card">
      <div className="question-text">该步骤在等待结算(exit/idle 都不是结算——由你判定)。</div>
      <div className="question-actions">
        <input
          value={text}
          placeholder={text === "" ? "总结(可选)" : "总结"}
          onChange={(event) => setText(event.target.value)}
        />
        <input
          className="mono-input"
          value={outputText}
          placeholder={'输出 JSON(可选;下游 ${nodes.<key>.output.…} 引用)'}
          onChange={(event) => setOutputText(event.target.value)}
        />
        <button className="mf-btn primary" disabled={busy} onClick={() => submit("complete")}>
          结算成功
        </button>
        <button className="mf-btn danger" disabled={busy} onClick={() => submit("fail")}>
          结算失败
        </button>
      </div>
      {error && <p className="handoff-meta warn">{error}</p>}
    </div>
  );
}

/** 设置页:项目管理(多项目同时在线)+ 系统信息。 */
