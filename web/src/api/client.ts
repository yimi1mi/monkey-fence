// MonkeyFence 前端 API client(T8a,Issue #52)。
// 只消费 Core 权威状态:snapshot + 事件 resume;不复制 Rust 领域状态机。
// 所有 mutation 经 POST /api/v1/commands(wire envelope 由 protocol.ts
// 冻结);Observer 禁写在 UI 与服务端双重生效(服务端 lease 拒绝伪造)。

import type {
  CommandEnvelope,
  CommandOutcomeWire,
  Problem,
  SnapshotEnvelope,
} from "./protocol.ts";
import type { BootstrapSession } from "./session.ts";

export interface ClientContext {
  csrfToken: string;
  clientId: string;
  controllerLeaseEpoch: string;
  /** Controller 才发起 mutation;Observer 由 UI 禁用 + 服务端拒绝。 */
  role: "controller" | "observer";
}

export class ApiError extends Error {
  constructor(
    readonly problem: Problem,
  ) {
    super(problem.message);
  }
}

/** 同源 API client(bootstrap cookie 自动携带;凭据永不进 URL)。 */
export class WorkbenchClient {
  constructor(private readonly context: ClientContext) {}

  get role(): "controller" | "observer" {
    return this.context.role;
  }

  get isController(): boolean {
    return this.context.role === "controller";
  }

  /** 本会话的 controller lease epoch(takeover CAS 观察值)。 */
  get leaseEpoch(): string {
    return this.context.controllerLeaseEpoch;
  }

  /** 本会话 client id(envelope 构造)。 */
  get clientId(): string {
    return this.context.clientId;
  }

  /** Workspace Snapshot(权威;刷新后以此为基线再 resume 事件)。 */
  async workspaceSnapshot(): Promise<SnapshotEnvelope> {
    const response = await fetch("/api/v1/snapshots/workspace", {
      method: "GET",
      headers: { "X-Client-Id": this.context.clientId },
    });
    if (!response.ok) throw new ApiError(await problemOf(response));
    return (await response.json()) as SnapshotEnvelope;
  }

  /** 命令提交(Controller;服务端复验 lease——伪造写入被拒)。 */
  async command(envelope: CommandEnvelope): Promise<CommandOutcomeWire> {
    if (!this.isController) {
      throw new Error("observer_forbidden: Observer 不可提交命令");
    }
    const response = await fetch("/api/v1/commands", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-CSRF-Token": this.context.csrfToken,
        "X-Client-Id": this.context.clientId,
        "X-Controller-Lease-Epoch": this.context.controllerLeaseEpoch,
      },
      body: JSON.stringify(envelope),
    });
    if (!response.ok) throw new ApiError(await problemOf(response));
    return (await response.json()) as CommandOutcomeWire;
  }

  /** 挂载项目目录(多项目入口;Controller-only)。 */
  async attachProject(path: string): Promise<{ project: string; display_name: string }> {
    const response = await fetch("/api/v1/projects", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-CSRF-Token": this.context.csrfToken,
        "X-Client-Id": this.context.clientId,
      },
      body: JSON.stringify({ path }),
    });
    if (!response.ok) throw new ApiError(await problemOf(response));
    return (await response.json()) as { project: string; display_name: string };
  }

  /** 卸载项目(Controller-only)。 */
  async detachProject(projectHandle: string): Promise<void> {
    const response = await fetch(`/api/v1/projects/${encodeURIComponent(projectHandle)}`, {
      method: "DELETE",
      headers: {
        "X-CSRF-Token": this.context.csrfToken,
        "X-Client-Id": this.context.clientId,
      },
    });
    if (!response.ok) throw new ApiError(await problemOf(response));
  }

  /** Observer 显式 takeover(CAS:最后观察 epoch);成功返回新会话
   *  形态(角色升 Controller + 新 lease epoch),前端续存后生效。 */
  async takeover(lastObservedEpoch: string): Promise<BootstrapSession> {
    const response = await fetch("/api/v1/controller/takeover", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-CSRF-Token": this.context.csrfToken,
        "X-Client-Id": this.context.clientId,
      },
      body: JSON.stringify({ last_observed_epoch: lastObservedEpoch }),
    });
    if (!response.ok) throw new ApiError(await problemOf(response));
    return (await response.json()) as BootstrapSession;
  }
}

async function problemOf(response: Response): Promise<Problem> {
  const body = (await response.json().catch(() => null)) as Problem | null;
  return (
    body ?? {
      schema: "mf.problem.v1",
      code: "internal_error",
      message: `HTTP ${response.status}`,
      trace_id: "",
      command_id: null,
      retry: null,
    }
  );
}
