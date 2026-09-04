// 只读终端面板(#77 v1):xterm 展示 + HTTP 增量轮询(MFT1 writer
// 输入面待完整 WS 票;输出侧 journal 是无损事实源)。

import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

export function TerminalPanel({
  sessionHandle,
  onClose,
}: {
  sessionHandle: string;
  onClose: () => void;
}) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const seqRef = useRef(0);
  const [alive, setAlive] = useState<boolean | null>(null);

  useEffect(() => {
    if (!hostRef.current) return;
    const terminal = new Terminal({
      convertEol: true,
      fontSize: 12,
      fontFamily: "'IBM Plex Mono', Consolas, monospace",
      theme: document.documentElement.dataset.theme === "dark"
        ? { background: "#0e1014" }
        : { background: "#f7f8fa" },
    });
    terminal.open(hostRef.current);
    let stopped = false;
    const poll = async () => {
      try {
        const url = `/api/v1/terminal/${encodeURIComponent(sessionHandle)}/output?after=${seqRef.current}`;
        const response = await fetch(url);
        if (!response.ok) {
          terminal.writeln(`\r\n\x1b[31m[输出读取失败 HTTP ${response.status}]\x1b[0m`);
          return;
        }
        const data = (await response.json()) as {
          alive: boolean;
          last_seq: string | null;
          frames: Array<[number, string]>;
        };
        for (const [seq, base64] of data.frames) {
          const text = atob(base64);
          terminal.write(text);
          seqRef.current = Math.max(seqRef.current, seq);
        }
        setAlive(data.alive);
        if (!data.alive) {
          terminal.writeln("\r\n\x1b[90m[会话已结束]\x1b[0m");
        }
      } catch {
        /* 轮询失败保留现场,下轮重试 */
      }
    };
    const timer = window.setInterval(() => {
      if (!stopped) void poll();
    }, 800);
    void poll();
    return () => {
      stopped = true;
      window.clearInterval(timer);
      terminal.dispose();
    };
  }, [sessionHandle]);

  return (
    <div className="terminal-panel">
      <div className="terminal-head">
        <span>终端 · {sessionHandle.slice(0, 18)}…</span>
        <span className="mono-dim">{alive === null ? "" : alive ? "存活" : "已结束"} · 只读(输入面待 MFT1)</span>
        <span className="header-space" />
        <button className="mf-btn ghost" onClick={onClose}>
          关闭
        </button>
      </div>
      <div ref={hostRef} className="term-host" />
    </div>
  );
}
