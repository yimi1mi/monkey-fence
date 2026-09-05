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
import { storeSession, type BootstrapSession } from "./session.ts";

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
  constructor(private context: ClientContext) {}

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

  /** 写路径 401 自愈(#97):HttpOnly cookie 由浏览器全局共享,其它
   * tab 的 exchange 会覆盖它——本 tab 未刷新时持旧 csrf/client_id,
   * 写命令全部 csrf_rejected(读路径不受影响)。探活 /auth/session
   * (只认 cookie,同源安全)权威刷新 context 后重试一次;探活失败
   * (session 真失效)则抛原始错误。 */
  private async write(send: () => Promise<Response>): Promise<Response> {
    let response = await send();
    if (response.status === 401) {
      const problem = await problemOf(response);
      if (problem.code === "csrf_rejected" || problem.code === "unauthenticated") {
        const refreshed = await this.refreshFromServerSession();
        if (refreshed) {
          response = await send();
          if (response.ok) return response;
          throw new ApiError(await problemOf(response));
        }
      }
      throw new ApiError(problem);
    }
    if (!response.ok) throw new ApiError(await problemOf(response));
    return response;
  }

  /** 以 cookie session 权威刷新本 client 的 csrf/client/epoch/role。 */
  async refreshFromServerSession(): Promise<boolean> {
    try {
      const probe = await fetch("/auth/session");
      if (!probe.ok) return false;
      const data = (await probe.json()) as BootstrapSession;
      this.context = {
        csrfToken: data.csrf_token,
        clientId: data.client_id,
        controllerLeaseEpoch: String(
          data.controller?.lease_epoch ?? this.context.controllerLeaseEpoch,
        ),
        role: data.controller?.role === "controller" ? "controller" : "observer",
      };
      storeSession(data);
      return true;
    } catch {
      return false;
    }
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
    const response = await this.write(() =>
      fetch("/api/v1/commands", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-CSRF-Token": this.context.csrfToken,
          "X-Client-Id": this.context.clientId,
          "X-Controller-Lease-Epoch": this.context.controllerLeaseEpoch,
        },
        body: JSON.stringify(envelope),
      }),
    );
    return (await response.json()) as CommandOutcomeWire;
  }

  /** 单次运行权威详情(steps/questions/agent_runs/sessions;#74)。 */
  async workflowRunSnapshot(
    projectHandle: string,
    runHandle: string,
  ): Promise<Record<string, unknown>> {
    const response = await fetch(
      `/api/v1/snapshots/workflow-run/${encodeURIComponent(projectHandle)}/${encodeURIComponent(runHandle)}`,
      { headers: { "X-Client-Id": this.context.clientId } },
    );
    if (!response.ok) throw new ApiError(await problemOf(response));
    const envelope = (await response.json()) as { data: Record<string, unknown> };
    return envelope.data;
  }

  /** 设置项目自定义名字(#custom-name;空串清除)。 */
  async renameProject(projectHandle: string, displayName: string): Promise<void> {
    await this.write(() =>
      fetch(`/api/v1/projects/${encodeURIComponent(projectHandle)}/name`, {
        method: "PUT",
        headers: {
          "Content-Type": "application/json",
          "X-CSRF-Token": this.context.csrfToken,
          "X-Client-Id": this.context.clientId,
        },
        body: JSON.stringify({ display_name: displayName }),
      }),
    );
  }

  /** 发起 ad-hoc 会话(#92;Controller-only)。 */
  async adhocSession(input: {
    projectHandle: string;
    runHandle: string;
    instanceId: string;
    prompt?: string;
  }): Promise<{ title: string; display_session_handle: string | null }> {
    const response = await this.write(() =>
      fetch("/api/v1/sessions/adhoc", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-CSRF-Token": this.context.csrfToken,
          "X-Client-Id": this.context.clientId,
        },
        body: JSON.stringify({
          project_handle: input.projectHandle,
          run_handle: input.runHandle,
          instance_id: input.instanceId,
          prompt: input.prompt,
        }),
      }),
    );
    return await response.json();
  }

  /** CLI 安装 recipe(#93;含包管理器探测)。 */
  async cliRecipes(): Promise<{
    recipes: Array<{ agent_type: string; package: string; display: string }>;
    package_manager: string | null;
    install_available: boolean;
  }> {
    const response = await fetch("/api/v1/cli/recipes", {
      headers: { "X-Client-Id": this.context.clientId },
    });
    if (!response.ok) throw new ApiError(await problemOf(response));
    return await response.json();
  }

  /** 安装 CLI(#93;Controller-only;包管理器真实执行)。 */
  async cliInstall(agentType: string): Promise<{ outcome: string; version?: string; reason?: string }> {
    const response = await this.write(() =>
      fetch("/api/v1/cli/install", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-CSRF-Token": this.context.csrfToken,
          "X-Client-Id": this.context.clientId,
        },
        body: JSON.stringify({ agent_type: agentType }),
      }),
    );
    return await response.json();
  }

  /** 真实 catalog 实例列表(#87;只读)。 */
  async catalogInstances(): Promise<
    Array<{ id: string; name: string; agent_type: string; enabled: boolean }>
  > {
    const response = await fetch("/api/v1/catalog/instances", {
      headers: { "X-Client-Id": this.context.clientId },
    });
    if (!response.ok) throw new ApiError(await problemOf(response));
    const data = (await response.json()) as {
      instances: Array<{ id: string; name: string; agent_type: string; enabled: boolean }>;
    };
    return data.instances;
  }

  /** CLI 检测(#90):PATH 扫描常见 agent CLI(只读)。 */
  async cliDetect(): Promise<Array<{ agent_type_id: string; executable: string; source: string }>> {
    const response = await fetch("/api/v1/cli/detect", {
      headers: { "X-Client-Id": this.context.clientId },
    });
    if (!response.ok) throw new ApiError(await problemOf(response));
    const data = (await response.json()) as {
      detected: Array<{ agent_type_id: string; executable: string; source: string }>;
    };
    return data.detected;
  }

  /** 目录浏览:浏览起点(盘符/主目录;只读,已认证会话可用)。 */
  async fsRoots(): Promise<Array<{ path: string; name: string }>> {
    const response = await fetch("/api/v1/fs/roots", {
      headers: { "X-Client-Id": this.context.clientId },
    });
    if (!response.ok) throw new ApiError(await problemOf(response));
    const data = (await response.json()) as { roots: Array<{ path: string; name: string }> };
    return data.roots;
  }

  /** 目录浏览:列子目录(仅目录名;无权限时 error 字段说明)。 */
  async fsDirs(
    path: string,
  ): Promise<{ path: string; parent: string | null; dirs: Array<{ path: string; name: string }>; error?: string }> {
    const response = await fetch(
      `/api/v1/fs/dirs?path=${encodeURIComponent(path)}`,
      { headers: { "X-Client-Id": this.context.clientId } },
    );
    if (!response.ok) throw new ApiError(await problemOf(response));
    return await response.json();
  }

  /** 挂载项目目录(多项目入口;Controller-only)。 */
  async attachProject(path: string): Promise<{ project: string; display_name: string }> {
    const response = await this.write(() =>
      fetch("/api/v1/projects", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-CSRF-Token": this.context.csrfToken,
          "X-Client-Id": this.context.clientId,
        },
        body: JSON.stringify({ path }),
      }),
    );
    return (await response.json()) as { project: string; display_name: string };
  }

  /** 卸载项目(Controller-only)。 */
  async detachProject(projectHandle: string): Promise<void> {
    await this.write(() =>
      fetch(`/api/v1/projects/${encodeURIComponent(projectHandle)}`, {
        method: "DELETE",
        headers: {
          "X-CSRF-Token": this.context.csrfToken,
          "X-Client-Id": this.context.clientId,
        },
      }),
    );
  }

  /** Observer 显式 takeover(CAS:最后观察 epoch);成功返回新会话
   *  形态(角色升 Controller + 新 lease epoch),前端续存后生效。 */
  async takeover(lastObservedEpoch: string): Promise<BootstrapSession> {
    const response = await this.write(() =>
      fetch("/api/v1/controller/takeover", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-CSRF-Token": this.context.csrfToken,
          "X-Client-Id": this.context.clientId,
        },
        body: JSON.stringify({ last_observed_epoch: lastObservedEpoch }),
      }),
    );
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
