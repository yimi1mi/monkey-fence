// 事件流 WS 客户端(T7c 语义):Resume(cursor 恢复)→ hello → 事件
// 增量;断线指数退避重连;4409/未知 critical → 全量 resync(调用方
// 重拉 snapshot)。凭据走 HttpOnly cookie,不进 URL。

import type { EventEnvelope } from "./protocol.ts";

export interface EventsHello {
  schema: "mf-workflow-events.hello.v1";
  stream_epoch: string;
  first_available_seq: string;
  last_seq: string;
  resync_required: boolean;
}

export interface EventCursorView {
  streamEpoch: string;
  throughSeq: string;
}

export interface EventSocketOptions {
  cursor: () => EventCursorView;
  onEvents: (events: EventEnvelope[]) => void;
  onResync: (reason: string) => void;
  onStateChange?: (state: "open" | "down") => void;
}

const RESYNC_CLOSE_CODE = 4409;

export class EventSocket {
  private socket: WebSocket | null = null;
  private retry = 0;
  private timer: number | null = null;
  private stopped = false;

  constructor(private readonly options: EventSocketOptions) {}

  connect(): void {
    if (this.stopped || typeof WebSocket === "undefined") return;
    const protocol = location.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(
      `${protocol}//${location.host}/api/v1/events`,
      "mf-workflow.v1",
    );
    this.socket = socket;
    socket.onopen = () => {
      this.retry = 0;
      this.options.onStateChange?.("open");
      const cursor = this.options.cursor();
      socket.send(
        JSON.stringify({
          type: "resume",
          stream_epoch: cursor.streamEpoch,
          through_seq: cursor.throughSeq,
        }),
      );
    };
    socket.onmessage = (message) => this.handle(message.data);
    socket.onclose = (event) => {
      this.options.onStateChange?.("down");
      this.socket = null;
      if (this.stopped) return;
      if (event.code === RESYNC_CLOSE_CODE) {
        this.options.onResync(`事件流要求全量同步(close ${event.code})`);
      }
      this.scheduleReconnect();
    };
    socket.onerror = () => {
      socket.close();
    };
  }

  private handle(raw: string): void {
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(raw) as Record<string, unknown>;
    } catch {
      return;
    }
    if (parsed.schema === "mf-workflow-events.hello.v1") {
      const hello = parsed as unknown as EventsHello;
      if (hello.resync_required) {
        this.options.onResync("resume 超出保留窗口");
      }
      return;
    }
    if (parsed.schema === "mf.event.v1") {
      this.options.onEvents([parsed as unknown as EventEnvelope]);
      return;
    }
    if (parsed.schema === "mf.problem.v1" && parsed.code === "resync_required") {
      this.options.onResync(String(parsed.message ?? "resync_required"));
    }
  }

  private scheduleReconnect(): void {
    const delay = Math.min(1000 * 2 ** this.retry, 8000);
    this.retry += 1;
    this.timer = window.setTimeout(() => this.connect(), delay);
  }

  stop(): void {
    this.stopped = true;
    if (this.timer !== null) window.clearTimeout(this.timer);
    this.socket?.close();
    this.socket = null;
  }
}
