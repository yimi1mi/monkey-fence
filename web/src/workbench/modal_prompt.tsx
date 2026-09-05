// 模态输入框(#95):替代 window.prompt——内嵌 webview(如 ZCode IAB)不支持
// 原生对话框,prompt() 直接抛 "prompt() is not supported",事件处理器静默崩溃,
// 表现为"按钮点了没反应"。全站统一走 ask():确定→输入值,取消/遮罩/Esc→null。
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";

export type PromptSpec = {
  title: string;
  label: string;
  initial?: string;
  placeholder?: string;
  confirmText?: string;
};

export function useModalPrompt(): {
  ask: (spec: PromptSpec) => Promise<string | null>;
  modal: ReactNode;
} {
  const [spec, setSpec] = useState<PromptSpec | null>(null);
  const resolver = useRef<((value: string | null) => void) | null>(null);

  const ask = useCallback((next: PromptSpec) => {
    setSpec(next);
    return new Promise<string | null>((resolve) => {
      resolver.current = resolve;
    });
  }, []);

  const settle = useCallback((value: string | null) => {
    resolver.current?.(value);
    resolver.current = null;
    setSpec(null);
  }, []);

  return {
    ask,
    // spec 为 null 时组件卸载,下次 ask 重新挂载拿到干净初值
    modal: spec ? <ModalPrompt spec={spec} onSettle={settle} /> : null,
  };
}

function ModalPrompt({
  spec,
  onSettle,
}: {
  spec: PromptSpec;
  onSettle: (value: string | null) => void;
}) {
  const [value, setValue] = useState(spec.initial ?? "");

  // 同一挂载期内 spec 被替换(极端时序)时重置初值
  useEffect(() => {
    setValue(spec.initial ?? "");
  }, [spec]);

  return (
    <div
      className="scrim"
      onClick={(event) => {
        if (event.target === event.currentTarget) onSettle(null);
      }}
    >
      <div className="modal prompt-modal" role="dialog" aria-modal="true" aria-label={spec.title}>
        <h3>{spec.title}</h3>
        <div className="field">
          <label htmlFor="mf-prompt-input">{spec.label}</label>
          <input
            id="mf-prompt-input"
            autoFocus
            value={value}
            placeholder={spec.placeholder}
            onChange={(event) => setValue(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") onSettle(value);
              if (event.key === "Escape") onSettle(null);
            }}
          />
        </div>
        <div className="actions">
          <button className="mf-btn ghost" onClick={() => onSettle(null)}>
            取消
          </button>
          <button className="mf-btn primary" onClick={() => onSettle(value)}>
            {spec.confirmText ?? "确定"}
          </button>
        </div>
      </div>
    </div>
  );
}
