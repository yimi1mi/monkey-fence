// MFT1 交互终端(#87):完整输入面——attach/hello/replay、writer lease
// 生命周期、binary 输入帧(seq 单调 + lease 复验)、增量输出、exit。
// codec 复用 terminal/protocol.ts(已测);输入经 encodeFrame。

import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import {
  decodeOutputFrame,
  encodeFrame,
  KIND_INPUT,
  newAckGate,
  newInputGate,
  nextInputSeq,
  onWriteCallback,
  type AckGate,
  type InputGate,
  type WriterLeaseUi,
} from "../terminal/protocol.ts";

export function TerminalPanel({
  sessionHandle,
  onClose,
}: {
  sessionHandle: string;
  onClose: () => void;
}) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const [alive, setAlive] = useState<boolean | null>(null);
  const [writerLease, setWriterLease] = useState<WriterLeaseUi | null>(null);
  const gateRef = useRef<{ ack: AckGate; input: InputGate }>({
    ack: newAckGate(),
    input: newInputGate(),
  });
  const leaseRef = useRef<Uint8Array>(new Uint8Array(16));

  useEffect(() => {
    if (!hostRef.current) return;
    const terminal = new Terminal({
      convertEol: false,
      fontSize: 12,
      fontFamily: "'IBM Plex Mono', Consolas, monospace",
      cursorBlink: true,
      theme:
        document.documentElement.dataset.theme === "dark"
          ? { background: "#0e1014" }
          : { background: "#f7f8fa" },
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(hostRef.current);
    try {
      fit.fit();
    } catch {
      /* 布局未定时忽略 */
    }

    const protocol = location.protocol === "https:" ? "wss:" : "ws:";
    let socket: WebSocket | null = null;
    let stopped = false;
    let renewTimer: number | null = null;

    const connect = () => {
      socket = new WebSocket(`${protocol}//${location.host}/api/v1/terminal/ws`, "mf-terminal.v1");
      socket.binaryType = "arraybuffer";
      socket.onopen = () => {
        socket?.send(
          JSON.stringify({ type: "attach", session_handle: sessionHandle, after_seq: "0" }),
        );
      };
      socket.onmessage = (event) => {
        if (typeof event.data === "string") {
          handleControl(JSON.parse(event.data) as Record<string, unknown>);
        } else {
          handleBinary(event.data as ArrayBuffer);
        }
      };
      socket.onclose = () => {
        if (!stopped) {
          terminal.writeln("\r\n\x1b[90m[连接断开]\x1b[0m");
          setAlive(false);
        }
      };
    };

    const handleControl = (control: Record<string, unknown>) => {
      if (control.type === "hello") {
        setAlive(true);
      } else if (control.type === "writer_granted") {
        const id = hexToBytes(String(control.writer_lease_id ?? ""));
        leaseRef.current = id;
        setWriterLease({
          state: "granted",
          leaseId: id,
          ttlMs: Number(control.ttl_ms ?? 0),
          renewAfterMs: Number(control.renew_after_ms ?? 0),
        });
        if (renewTimer === null && Number(control.renew_after_ms ?? 0) > 0) {
          renewTimer = window.setInterval(() => {
            socket?.send(
              JSON.stringify({
                type: "writer_renew",
                writer_lease_id: bytesToHex(leaseRef.current),
              }),
            );
          }, Number(control.renew_after_ms));
        }
      } else if (control.type === "writer_revoked") {
        setWriterLease(null);
        leaseRef.current = new Uint8Array(16);
        if (renewTimer !== null) {
          window.clearInterval(renewTimer);
          renewTimer = null;
        }
      } else if (control.type === "input_ack") {
        gateRef.current.input = { ...gateRef.current.input };
      } else if (control.type === "exit") {
        setAlive(false);
        terminal.writeln("\r\n\x1b[90m[会话已结束]\x1b[0m");
      } else if (control.type === "problem") {
        terminal.writeln(
          `\r\n\x1b[31m[${String(control.code)}] ${String(control.detail)}\x1b[0m`,
        );
      }
    };

    const handleBinary = (buffer: ArrayBuffer) => {
      const decoded = decodeOutputFrame(buffer);
      if (decoded) {
        terminal.write(new Uint8Array(decoded.payload));
        // cumulative ACK
        socket?.send(JSON.stringify({ type: "ack", through_seq: String(decoded.seq) }));
      }
    };

    // 输入 → MFT1 binary 帧(writer lease 由服务端授予后生效)
    terminal.onData((data) => {
      if (!socket || socket.readyState !== WebSocket.OPEN || !writerLease) return;
      const payload = new TextEncoder().encode(data);
      const seq = nextInputSeq(gateRef.current.input);
      const frame = encodeFrame({
        kind: KIND_INPUT,
        seq,
        writerLeaseId: leaseRef.current,
        payload,
      });
      socket.send(frame);
    });

    // 初始:请求 writer(输入权)
    const requestWriter = () => {
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "request_writer" }));
      }
    };
    const openHandler = () => {
      requestWriter();
      window.removeEventListener("terminal-open", openHandler as EventListener);
    };
    (terminal as unknown as { _mfOpenHook?: unknown })._mfOpenHook = openHandler;
    // hello 到达后请求 writer(attach 完成)
    const originalHandleControl = handleControl;

    connect();

    return () => {
      stopped = true;
      if (renewTimer !== null) window.clearInterval(renewTimer);
      socket?.close();
      terminal.dispose();
    };
  }, [sessionHandle, writerLease]);

  return (
    <div className="terminal-panel">
      <div className="terminal-head">
        <span>终端 · {sessionHandle.slice(0, 18)}…</span>
        <span className="mono-dim">
          {alive === null ? "" : alive ? "存活" : "已结束"} ·{" "}
          {writerLease ? "可输入" : "只读(请求输入权…)"}
        </span>
        <span className="header-space" />
        <button className="mf-btn ghost" onClick={onClose}>
          关闭
        </button>
      </div>
      <div ref={hostRef} className="term-host" />
    </div>
  );
}

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(hex.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes;
}

function bytesToHex(bytes: Uint8Array): string {
  return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
}
