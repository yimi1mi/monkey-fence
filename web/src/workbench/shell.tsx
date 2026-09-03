// Workbench 壳(T8a,Issue #52):顶部「工作流 / 运行」、三栏布局与
// 窄屏 Inspector 下移;Observer 禁写 + 显式 takeover;全局 problem
// toast;断线重连(resume→4409→全量 resync)。数据全部来自 Core 权威
// snapshot(model.ts 映射),3s 轮询保持活性;事件流接入 WS 后经
// reducer 增量投影。

import { useCallback, useEffect, useMemo, useState } from "react";
import { ApiError, type WorkbenchClient } from "../api/client.ts";
import { storeSession } from "../api/session.ts";
import { uuidv7 } from "../api/uuid.ts";
import { workflowCreateCommand } from "../state/workflow_commands.ts";
import {
  activeRunsAcross,
  runIsActive,
  runStatusMeta,
  workspaceViewOf,
  type ProjectView,
  type RunView,
  type WorkspaceView,
} from "./model.ts";

export type WorkbenchTab = "workflows" | "runs";

const POLL_INTERVAL_MS = 3000;

export function WorkbenchShell({ client }: { client: WorkbenchClient }) {
  const [tab, setTab] = useState<WorkbenchTab>("workflows");
  const [view, setView] = useState<WorkspaceView | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [connection, setConnection] = useState<"live" | "reconnecting">("live");
  const [selectedRun, setSelectedRun] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const snapshot = await client.workspaceSnapshot();
      setView(workspaceViewOf(snapshot));
      setConnection("live");
    } catch (error) {
      setToast(String(error));
      setConnection("reconnecting");
    }
  }, [client]);

  // 初始 snapshot + 轮询(页面隐藏时暂停;快照本机便宜)
  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") {
        void refresh();
      }
    }, POLL_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  // 自动选中的 run 失效时回退到第一个活动 run
  const runs = useMemo(() => (view ? activeRunsAcross(view.projects) : []), [view]);
  useEffect(() => {
    if (!view) return;
    if (selectedRun && runs.some((run) => run.handle === selectedRun)) return;
    const explicit = view.projects.flatMap((p) => p.runs).find((r) => r.handle === selectedRun);
    if (!explicit) setSelectedRun(runs[0]?.handle ?? view.projects[0]?.runs[0]?.handle ?? null);
  }, [view, runs, selectedRun]);

  const selected = useMemo(() => {
    if (!view) return null;
    for (const project of view.projects) {
      const hit = project.runs.find((run) => run.handle === selectedRun);
      if (hit) return hit;
    }
    return null;
  }, [view, selectedRun]);

  const isController = client.isController;
  const needsYouRuns = useMemo(
    () => (view ? view.projects.flatMap((p) => p.runs).filter((r) => r.needsYou) : []),
    [view],
  );

  return (
    <div className="mf-workbench">
      <header>
        <div className="brand">
          <span className="mark">◤</span>
          <span className="name">MonkeyFence</span>
          <span className="sub">WORKBENCH</span>
        </div>
        <nav className="tabs" aria-label="主视图">
          <button aria-selected={tab === "workflows"} onClick={() => setTab("workflows")}>
            工作流
          </button>
          <button aria-selected={tab === "runs"} onClick={() => setTab("runs")}>
            运行
          </button>
        </nav>
        <span className="header-space" />
        <span role="status" className={`chip ${connection === "live" ? "live" : "reconnecting"}`}>
          <span className="dot" />
          {connection === "live" ? "已连接" : "重连中…"}
        </span>
        <span className={`chip ${isController ? "role-controller" : ""}`}>
          {isController ? "Controller" : "Observer"}
        </span>
        {!isController && (
          <TakeoverButton
            client={client}
            onTaken={() => location.reload()}
            onFailed={(message) => setToast(`接管失败:${message}`)}
          />
        )}
      </header>

      <main className="panes">
        <aside className="pane sidebar" aria-label="工作区概览">
          <div className="pane-head">
            <h2>哨戒概览</h2>
            <span className="leaf-count">{view ? `${view.projects.length} 项目` : "…"}</span>
          </div>
          <div className="pane-body">
            <div className="stat-grid">
              <div className="stat live">
                <span className="k">
                  <span className="badge tone-live">
                    <span className="dot" />
                  </span>
                  活动运行
                </span>
                <span className="v">{view?.activeRuns ?? "—"}</span>
              </div>
              <div className="stat warn">
                <span className="k">
                  <span className="badge tone-warn">
                    <span className="dot" />
                  </span>
                  需要你
                </span>
                <span className="v">{view?.needsYou ?? "—"}</span>
              </div>
              <div className="stat">
                <span className="k">Agent 会话</span>
                <span className="v">
                  {view ? view.projects.reduce((sum, p) => sum + p.activeSessions, 0) : "—"}
                </span>
              </div>
            </div>
          </div>
          <div className="pane-head">
            <h2>项目</h2>
          </div>
          <div className="pane-body">
            <div className="projects">
              {(view?.projects ?? []).map((project) => (
                <ProjectRow
                  key={project.handle}
                  project={project}
                  active={runs.some((run) => run.projectHandle === project.handle)}
                  onClick={() => {
                    const first = project.runs.find((r) => runIsActive(r.status)) ?? project.runs[0];
                    setSelectedRun(first?.handle ?? null);
                    setTab("workflows");
                  }}
                />
              ))}
              {view && view.projects.length === 0 && (
                <p className="muted-note">尚未注册项目。用 mfctl 或 launcher 接入后此处会出现。</p>
              )}
            </div>
          </div>
        </aside>

        <section className="pane center" aria-label={tab === "workflows" ? "工作流运行" : "活动运行"}>
          <div className="pane-head">
            <h2>{tab === "workflows" ? "工作流运行" : "活动运行"}</h2>
            <span className="leaf-count">
              {tab === "workflows"
                ? `${view?.projects.reduce((sum, p) => sum + p.runs.length, 0) ?? 0} 条`
                : `${runs.length} 活跃`}
            </span>
          </div>
          <div className="pane-body">
            {tab === "workflows" ? (
              (view?.projects ?? []).map((project) =>
                project.runs.length === 0 ? null : (
                  <div key={project.handle} style={{ marginBottom: 14 }}>
                    <div className="group-head">
                      <span className="name">{project.name}</span>
                      <span className="meta">
                        {project.runs.length} 运行 · {project.activeSessions} 活跃会话
                      </span>
                    </div>
                    <div className="run-list" style={{ marginTop: 8 }}>
                      {project.runs.map((run, index) => (
                        <RunCard
                          key={run.handle}
                          run={run}
                          selected={run.handle === selectedRun}
                          index={index}
                          onSelect={() => setSelectedRun(run.handle)}
                        />
                      ))}
                    </div>
                  </div>
                ),
              )
            ) : runs.length > 0 ? (
              <div className="run-list">
                {runs.map((run, index) => (
                  <RunCard
                    key={run.handle}
                    run={run}
                    selected={run.handle === selectedRun}
                    index={index}
                    onSelect={() => setSelectedRun(run.handle)}
                  />
                ))}
              </div>
            ) : null}
            {view &&
              ((tab === "workflows" &&
                view.projects.every((p) => p.runs.length === 0)) ||
                (tab === "runs" && runs.length === 0)) && (
                <div className="empty-state">
                  <div className="fence">┼┼┼┼┼┼┼┼┼┼┼┼┼┼┼</div>
                  <div className="title">
                    {tab === "runs" ? "当前没有活动运行" : "围栏内一片安宁"}
                  </div>
                  <div className="hint">mfctl workflow start 发起第一次运行</div>
                </div>
              )}
            {!view && connection === "reconnecting" && (
              <div className="empty-state">
                <div className="fence">┼┼┼┼┼┼┼┼┼┼┼┼┼┼┼</div>
                <div className="title">正在连接 Core…</div>
                <div className="hint">确认 monkeyfence-core 正在运行</div>
              </div>
            )}
          </div>
        </section>

        <aside className="pane inspector" aria-label="检视器">
          <div className="pane-head">
            <h2>检视器</h2>
            {selected && (
              <span className="leaf-count">{runStatusMeta(selected.status).label}</span>
            )}
          </div>
          <div className="inspector-body">
            {selected ? (
              <RunDetail run={selected} />
            ) : (
              <p className="muted-note">选择一个运行查看详情。</p>
            )}

            <div className="pane-head" style={{ margin: 0 -14, padding: 0 }}>
              <h2 style={{ fontSize: 11, padding: "8px 14px", width: "100%" }}>
                需要你 {needsYouRuns.length > 0 && `(${needsYouRuns.length})`}
              </h2>
            </div>
            {needsYouRuns.length > 0 ? (
              <div className="feed">
                {needsYouRuns.slice(0, 12).map((run) => (
                  <button
                    key={run.handle}
                    className="needs-row"
                    onClick={() => setSelectedRun(run.handle)}
                  >
                    <span className="arrow">▲</span>
                    <span className="title">{run.title}</span>
                    <span>{run.reasonCount} 项</span>
                  </button>
                ))}
              </div>
            ) : (
              <p className="muted-note">没有等待你处理的运行。</p>
            )}

            <button
              className="mf-btn primary"
              style={{ width: "100%" }}
              disabled={!isController || !view || view.projects.length === 0}
              title={
                !isController
                  ? "Observer 禁写——接管为 Controller 后可操作"
                  : view && view.projects.length === 0
                    ? "尚无已注册项目"
                    : undefined
              }
              onClick={() => setCreateOpen(true)}
            >
              新建工作流
            </button>
            {view && (
              <dl className="kv">
                <dt>Core 实例</dt>
                <dd>{view.serverInstanceId}</dd>
                <dt>投影游标</dt>
                <dd>
                  {view.streamEpoch} @ {view.throughSeq}
                </dd>
                <dt>刷新于</dt>
                <dd>{new Date(view.fetchedAt).toLocaleTimeString("zh-CN")}</dd>
              </dl>
            )}
          </div>
        </aside>
      </main>

      {toast && (
        <div role="alert" className="toast" onClick={() => setToast(null)}>
          {toast}
        </div>
      )}
      {createOpen && view && view.projects.length > 0 && (
        <CreateWorkflowModal
          client={client}
          projects={view.projects}
          onDone={(message) => {
            setCreateOpen(false);
            setToast(message);
            void refresh();
          }}
          onClose={() => setCreateOpen(false)}
        />
      )}
    </div>
  );
}

function ProjectRow({
  project,
  active,
  onClick,
}: {
  project: ProjectView;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <div
      className={`project-row ${active ? "selected" : ""}`}
      onClick={onClick}
      role="button"
      tabIndex={0}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") onClick();
      }}
    >
      <span className="name">
        {active && (
          <span className="badge tone-live" style={{ padding: "0 6px" }}>
            <span className="dot" />
          </span>
        )}
        {project.name}
      </span>
      <span className="meta">
        proj {project.handle.slice(4, 12)} · {project.runs.length} 运行
      </span>
    </div>
  );
}

function RunCard({
  run,
  selected,
  index,
  onSelect,
}: {
  run: RunView;
  selected: boolean;
  index: number;
  onSelect: () => void;
}) {
  const meta = runStatusMeta(run.status);
  return (
    <button
      className={`run-card tone-${meta.tone} ${selected ? "selected" : ""} ${run.unread ? "unread" : ""}`}
      style={{ animationDelay: `${Math.min(index * 40, 240)}ms` }}
      onClick={onSelect}
    >
      <div className="row1">
        <span className="title">{run.title}</span>
        {run.paused && <span className="badge tone-dim">已暂停</span>}
        <span className={`badge tone-${meta.tone} ${meta.pulsing ? "pulsing" : ""}`}>
          <span className="dot" />
          {meta.label}
        </span>
      </div>
      <div className="row2">
        <span>run_{run.handle.slice(4, 14)}…</span>
        <span className="sep">|</span>
        <span>rev {run.revision}</span>
        {run.activeAgentRuns > 0 && (
          <>
            <span className="sep">|</span>
            <span className="agents">◉ {run.activeAgentRuns} agent</span>
          </>
        )}
        {run.needsYou && (
          <>
            <span className="sep">|</span>
            <span className="reasons">{run.reasonCount} 项待你处理</span>
          </>
        )}
        {run.focusStep && (
          <>
            <span className="sep">|</span>
            <span>焦点 {run.focusStep.slice(5, 13)}…</span>
          </>
        )}
      </div>
    </button>
  );
}

function RunDetail({ run }: { run: RunView }) {
  const meta = runStatusMeta(run.status);
  return (
    <>
      <div>
        <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 6 }}>
          <span className={`badge tone-${meta.tone} ${meta.pulsing ? "pulsing" : ""}`}>
            <span className="dot" />
            {meta.label}
          </span>
          {run.paused && <span className="badge tone-dim">已暂停</span>}
          {run.unread && <span className="badge tone-warn">未读</span>}
        </div>
        <div style={{ fontSize: 15, fontWeight: 600, lineHeight: 1.4 }}>{run.title}</div>
        <div style={{ fontSize: 12, color: "var(--text-faint)", marginTop: 2 }}>
          {run.projectName}
        </div>
      </div>
      <dl className="kv">
        <dt>运行句柄</dt>
        <dd>{run.handle}</dd>
        <dt>项目</dt>
        <dd>{run.projectHandle}</dd>
        <dt>修订</dt>
        <dd>{run.revision}</dd>
        <dt>活跃 Agent</dt>
        <dd>{run.activeAgentRuns}</dd>
        <dt>待处理项</dt>
        <dd>{run.needsYou ? `${run.reasonCount} 项` : "—"}</dd>
        {run.focusStep && (
          <>
            <dt>焦点步骤</dt>
            <dd>{run.focusStep}</dd>
          </>
        )}
      </dl>
    </>
  );
}

/** 新建工作流弹层:真实 workflow.create 命令(collection CAS 来自
 *  快照;draft 首节点——空节点是 EmptyWorkflow 校验错误)。 */
function CreateWorkflowModal({
  client,
  projects,
  onDone,
  onClose,
}: {
  client: WorkbenchClient;
  projects: ProjectView[];
  onDone: (message: string) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState("");
  const [firstNodeTitle, setFirstNodeTitle] = useState("");
  const [agentInstanceId, setAgentInstanceId] = useState("");
  const [projectHandle, setProjectHandle] = useState(projects[0]?.handle ?? "");
  const [busy, setBusy] = useState(false);

  const valid =
    name.trim().length > 0 &&
    firstNodeTitle.trim().length > 0 &&
    agentInstanceId.trim().length > 0 &&
    projectHandle !== "";

  return (
    <div
      className="scrim"
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="modal" role="dialog" aria-modal="true" aria-label="新建工作流">
        <h3>
          <span className="mark">◤</span>新建工作流
        </h3>
        <div className="field">
          <label htmlFor="wf-project">项目</label>
          <select
            id="wf-project"
            value={projectHandle}
            onChange={(event) => setProjectHandle(event.target.value)}
          >
            {projects.map((project) => (
              <option key={project.handle} value={project.handle}>
                {project.name}
              </option>
            ))}
          </select>
        </div>
        <div className="field">
          <label htmlFor="wf-name">工作流名称</label>
          <input
            id="wf-name"
            value={name}
            placeholder="如:仓库巡检"
            onChange={(event) => setName(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor="wf-node">首节点标题</label>
          <input
            id="wf-node"
            value={firstNodeTitle}
            placeholder="如:检查变更"
            onChange={(event) => setFirstNodeTitle(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor="wf-agent">Agent 实例 ID</label>
          <input
            id="wf-agent"
            value={agentInstanceId}
            placeholder="Agent Instance 稳定 ID(mfctl agent 查看)"
            onChange={(event) => setAgentInstanceId(event.target.value)}
          />
          <span className="hint">实例存在性在运行时绑定;创建时仅要求非空。</span>
        </div>
        <div className="actions">
          <button className="mf-btn ghost" onClick={onClose}>
            取消
          </button>
          <button
            className="mf-btn primary"
            disabled={!valid || busy}
            onClick={async () => {
              setBusy(true);
              const project = projects.find((entry) => entry.handle === projectHandle);
              if (!project) return;
              try {
                await client.command(
                  workflowCreateCommand(
                    {
                      commandId: uuidv7(),
                      clientId: client.clientId,
                      controllerLeaseEpoch: client.leaseEpoch,
                      projectHandle,
                      name,
                      firstNodeTitle,
                      agentInstanceId,
                    },
                    project.collectionRevision,
                  ),
                );
                onDone(`工作流「${name.trim()}」已创建`);
              } catch (error) {
                setBusy(false);
                onDone(
                  `创建失败:${error instanceof Error ? error.message : String(error)}`,
                );
              }
            }}
          >
            {busy ? "创建中…" : "创建"}
          </button>
        </div>
      </div>
    </div>
  );
}

function TakeoverButton({
  client,
  onTaken,
  onFailed,
}: {
  client: WorkbenchClient;
  onTaken: () => void;
  onFailed: (message: string) => void;
}) {
  const [busy, setBusy] = useState(false);
  return (
    <button
      className="mf-btn ghost"
      disabled={busy}
      onClick={async () => {
        setBusy(true);
        try {
          // CAS:本会话最后观察的 controller lease epoch;若已被其它
          // bootstrap 前移,服务端在 problem.current 里给出当前值,
          // 以当前值重试一次(仍失败才报错)。
          const session = await client.takeover(client.leaseEpoch).catch(async (error) => {
            const current =
              error instanceof ApiError ? error.problem.current?.controller_epoch : undefined;
            if (current === undefined) throw error;
            return client.takeover(String(current));
          });
          storeSession(session);
          onTaken();
        } catch (error) {
          setBusy(false);
          onFailed(error instanceof Error ? error.message : String(error));
        }
      }}
    >
      {busy ? "接管中…" : "接管为 Controller"}
    </button>
  );
}

// 窄屏:Inspector 下移为第四行(布局由 CSS grid 的媒体查询承载;
// 组件结构保持三栏语义 + inspector 可重排)。
export const NARROW_INSPECTOR_ORDER = "inspector-last";
