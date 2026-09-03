// Snapshot → 展示模型(T8a 数据接线)。只做字段映射与状态元数据,
// 不解释领域状态机;wire 冻结语义见 api/protocol.ts。
// 字段与 crates/mf-kernel/src/projection.rs 的
// WorkspaceSnapshotData/WorkspaceProjectSnapshot/WorkflowRunSummarySnapshot
// 逐字段对齐(serde 默认命名 = snake_case 原样)。

import type { SnapshotEnvelope } from "../api/protocol.ts";

/** TaskStatus str_enum(mf-agent/src/model.rs):v1 冻结取值。 */
export type RunStatus =
  | "draft"
  | "ready"
  | "running"
  | "needs-you"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "archived";

export interface RunStatusMeta {
  label: string;
  tone: "dim" | "info" | "live" | "warn" | "ok" | "bad";
  /** 活动态(呼吸点):running / needs-you。 */
  pulsing: boolean;
}

export const RUN_STATUS_META: Record<RunStatus, RunStatusMeta> = {
  draft: { label: "草稿", tone: "dim", pulsing: false },
  ready: { label: "就绪", tone: "info", pulsing: false },
  running: { label: "运行中", tone: "live", pulsing: true },
  "needs-you": { label: "需要你", tone: "warn", pulsing: true },
  succeeded: { label: "已成功", tone: "ok", pulsing: false },
  failed: { label: "已失败", tone: "bad", pulsing: false },
  cancelled: { label: "已取消", tone: "dim", pulsing: false },
  archived: { label: "已归档", tone: "dim", pulsing: false },
};

export function runStatusMeta(status: string): RunStatusMeta {
  return RUN_STATUS_META[status as RunStatus] ?? { label: status, tone: "dim", pulsing: false };
}

export interface RunView {
  handle: string;
  projectHandle: string;
  projectName: string;
  revision: string;
  title: string;
  status: string;
  paused: boolean;
  unread: boolean;
  needsYou: boolean;
  reasonCount: number;
  focusStep: string | null;
  activeAgentRuns: number;
}

export interface ProjectView {
  handle: string;
  name: string;
  /** workflow.create/delete 的 collection CAS 轴。 */
  collectionRevision: string;
  activeSessions: number;
  runs: RunView[];
}

export interface WorkspaceView {
  serverInstanceId: string;
  streamEpoch: string;
  throughSeq: string;
  projects: ProjectView[];
  activeRuns: number;
  needsYou: number;
  fetchedAt: number;
}

/** 活动态:kernel workspace_projection 的同一口径。 */
export function runIsActive(status: string): boolean {
  return status === "ready" || status === "running" || status === "needs-you";
}

/** snapshot.data(Record<string,unknown>) → 类型化视图;字段缺失时回退。 */
export function workspaceViewOf(snapshot: SnapshotEnvelope): WorkspaceView {
  const data = (snapshot.data ?? {}) as {
    projects?: Array<Record<string, unknown>>;
    active_workflow_runs?: number;
    needs_you_count?: number;
  };
  const projects: ProjectView[] = (data.projects ?? []).map((project) => {
    const handle = String(project.project ?? "");
    const name = String(project.display_name ?? "未命名项目");
    const collectionRevision = String(project.workflow_collection_revision ?? "0");
    const runs = (Array.isArray(project.workflow_runs) ? project.workflow_runs : []).map(
      (run): RunView => {
        const row = run as Record<string, unknown>;
        const revision = row.revision;
        return {
          handle: String(row.workflow_run ?? ""),
          projectHandle: handle,
          projectName: name,
          revision:
            typeof revision === "number"
              ? String(revision)
              : revision && typeof revision === "object"
                ? String((revision as Record<string, unknown>).revision ?? "?")
                : String(revision ?? "?"),
          title: String(row.title ?? "未命名运行"),
          status: String(row.status ?? "draft"),
          paused: row.paused === true,
          unread: row.unread === true,
          needsYou: row.needs_you === true,
          reasonCount: Number(row.reason_count ?? 0),
          focusStep: typeof row.focus_step === "string" ? row.focus_step : null,
          activeAgentRuns: Number(row.active_agent_runs ?? 0),
        };
      },
    );
    return {
      handle,
      name,
      collectionRevision,
      activeSessions: Number(project.active_agent_sessions ?? 0),
      runs,
    };
  });
  return {
    serverInstanceId: snapshot.server_instance_id,
    streamEpoch: snapshot.cursor.stream_epoch,
    throughSeq: snapshot.cursor.through_seq,
    projects,
    activeRuns: Number(data.active_workflow_runs ?? 0),
    needsYou: Number(data.needs_you_count ?? 0),
    fetchedAt: Date.now(),
  };
}

/** 运行 tab:全部活动 run(跨项目,needs-you 优先)。 */
export function activeRunsAcross(projects: ProjectView[]): RunView[] {
  return projects
    .flatMap((project) => project.runs)
    .filter((run) => runIsActive(run.status))
    .sort((a, b) => {
      if (a.needsYou !== b.needsYou) return a.needsYou ? -1 : 1;
      return a.status.localeCompare(b.status);
    });
}
