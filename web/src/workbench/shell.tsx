// Workbench 壳(T8a,Issue #52):顶部「工作流 / 运行」、三栏布局与
// 窄屏 Inspector 下移;Observer 禁写 + 显式 takeover;全局 problem
// toast;断线重连(resume→4409→全量 resync)。数据全部来自 Core 权威
// snapshot(model.ts 映射),3s 轮询保持活性;事件流接入 WS 后经
// reducer 增量投影。

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, type WorkbenchClient } from "../api/client.ts";
import { EventSocket } from "../api/events.ts";
import { storeSession } from "../api/session.ts";
import { uuidv7 } from "../api/uuid.ts";
import { applyTheme, currentTheme, toggleTheme } from "../api/theme.ts";
import {
  reduceEvents,
  reduceSnapshot,
  PROJECTION_FEED_LIMIT,
  type ProjectionState,
} from "../state/reducer.ts";
import { workflowCreateCommand } from "../state/workflow_commands.ts";
import {
  agentRunOfStep,
  runDetailViewOf,
  runActionCommand,
  type RunDetailView,
} from "./run_detail.ts";
import type { CommandType } from "../api/protocol.ts";
import { WorkflowEditor } from "./workflow_editor.tsx";
import { TerminalPanel } from "./terminal_panel.tsx";
import { initialValues, renderFields, validate, type FormSchema, type FormValues } from "./form_schema.tsx";
import { CodeBrowserModal, VcsPanel } from "./code_browser.tsx";
import {
  activeRunsAcross,
  runIsActive,
  runStatusMeta,
  workspaceViewOf,
  type ProjectView,
  type RunView,
  type WorkflowView,
  type WorkspaceView,
} from "./model.ts";

export type WorkbenchTab = "workflows" | "runs" | "settings";

const POLL_FALLBACK_MS = 3000;
const POLL_LIVE_MS = 60000;

export function WorkbenchShell({ client }: { client: WorkbenchClient }) {
  const [tab, setTab] = useState<WorkbenchTab>(
    () => (localStorage.getItem("mf.defaultTab") as WorkbenchTab) ?? "workflows",
  );
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [view, setView] = useState<WorkspaceView | null>(null);
  const [projection, setProjection] = useState<ProjectionState | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  useEffect(() => {
    const onToastEvent = (event: Event) => setToast((event as CustomEvent<string>).detail);
    window.addEventListener("mf-toast", onToastEvent);
    return () => window.removeEventListener("mf-toast", onToastEvent);
  }, []);
  const [connection, setConnection] = useState<"live" | "reconnecting">("live");
  const [wsOpen, setWsOpen] = useState(false);
  const [selectedRun, setSelectedRun] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [theme, setTheme] = useState(currentTheme());
  const [editing, setEditing] = useState<{ project: ProjectView; workflow: WorkflowView } | null>(
    null,
  );
  const [terminalSession, setTerminalSession] = useState<string | null>(null);
  const [clis, setClis] = useState<Array<{ agent_type_id: string; executable: string }>>([]);
  const [instances, setInstances] = useState<Array<{ id: string; name: string; enabled: boolean }>>([]);
  const [recipes, setRecipes] = useState<Array<{ agent_type: string; package: string; display: string }>>([]);
  const [installing, setInstalling] = useState<string | null>(null);
  useEffect(() => {
    void client.cliDetect().then(setClis).catch(() => setClis([]));
    void client.catalogInstances().then(setInstances).catch(() => setInstances([]));
    void client.cliRecipes().then((data) => setRecipes(data.recipes)).catch(() => setRecipes([]));
  }, [client]);
  // #91 通知: needs-you 系统通知 + 提示音(设置开关;默认开)
  const notificationsOn = localStorage.getItem("mf.notify") !== "off";
  const [codeBrowser, setCodeBrowser] = useState<string | null>(null);
  const [vcsRoot, setVcsRoot] = useState<string | null>(null);
  const projectionRef = useRef<ProjectionState | null>(null);
  const socketStarted = useRef(false);

  const refresh = useCallback(async () => {
    try {
      const snapshot = await client.workspaceSnapshot();
      setView(workspaceViewOf(snapshot));
      setProjection((prev) => {
        const next = reduceSnapshot(prev ?? (null as never), snapshot);
        projectionRef.current = next;
        return next;
      });
      setConnection("live");
    } catch (error) {
      setToast(String(error));
      setConnection("reconnecting");
    }
  }, [client]);

  // #91 首次交互请求通知权限
  useEffect(() => {
    const requestOnce = () => {
      if ("Notification" in window && Notification.permission === "default") {
        void Notification.requestPermission();
        window.removeEventListener("click", requestOnce);
      }
    };
    window.addEventListener("click", requestOnce);
    return () => window.removeEventListener("click", requestOnce);
  }, []);

  // #84 命令面板:Ctrl+K
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((value) => !value);
      }
      if (event.key === "Escape") setPaletteOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // 初始 snapshot + 轮询兜底(WS 活跃时降频;页面隐藏暂停)
  useEffect(() => {
    void refresh();
  }, [refresh]);
  useEffect(() => {
    const timer = window.setInterval(
      () => {
        if (document.visibilityState === "visible") void refresh();
      },
      wsOpen ? POLL_LIVE_MS : POLL_FALLBACK_MS,
    );
    return () => window.clearInterval(timer);
  }, [refresh, wsOpen]);

  // 事件流(首份 snapshot 后订阅一次;cursor 续传;4409/未知 critical → 全量)
  useEffect(() => {
    if (!projection || socketStarted.current) return;
    socketStarted.current = true;
    const socket = new EventSocket({
      cursor: () =>
        projectionRef.current
          ? {
              streamEpoch: projectionRef.current.cursor.streamEpoch,
              throughSeq: projectionRef.current.cursor.throughSeq,
            }
          : { streamEpoch: "", throughSeq: "0" },
      onEvents: (events) => {
        // #91: needs-you → 系统通知 + 提示音(页面不可见时)
        if (notificationsOn) {
          const needsYou = events.find(
            (event) => event.type === "workflow_run.needs_you" && event.critical,
          );
          if (needsYou && document.visibilityState !== "visible") {
            void notifyNeedsYou(needsYou.data);
          }
        }
        const prev = projectionRef.current;
        if (!prev) return;
        const { state: next, resyncRequired } = reduceEvents(prev, events);
        // feed 是显示层"本页会话送达的事件":命令触发的快照刷新可能
        // 先于 WS 帧把 cursor 推进(reducer 去重正确跳过重复),送达的
        // 事件仍要展示一次。
        const feed = [
          ...events.map((event) => ({
            type: event.type,
            seq: event.seq,
            critical: event.critical,
            at: Date.now(),
          })),
          ...next.feed,
        ].slice(0, PROJECTION_FEED_LIMIT);
        const merged = { ...next, feed };
        projectionRef.current = merged;
        setProjection(merged);
        if (resyncRequired) void refresh();
      },
      onResync: () => {
        void refresh();
      },
      onStateChange: (state) => setWsOpen(state === "open"),
    });
    socket.connect();
    return () => socket.stop();
    // 仅在首个投影基线就绪时建立一次
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projection !== null, refresh]);

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
          <button aria-selected={tab === "workflows"} onClick={() => { setTab("workflows"); localStorage.setItem("mf.defaultTab", "workflows"); }}>
            工作流
          </button>
          <button aria-selected={tab === "runs"} onClick={() => { setTab("runs"); localStorage.setItem("mf.defaultTab", "runs"); }}>
            运行
          </button>
          <button aria-selected={tab === "settings"} onClick={() => { setTab("settings"); localStorage.setItem("mf.defaultTab", "settings"); }}>
            设置
          </button>
        </nav>
        <span className="header-space" />
        <button
          className="mf-btn ghost theme-toggle"
          title={theme === "dark" ? "切换到亮色" : "切换到深色"}
          onClick={() => {
            applyTheme(toggleTheme());
            setTheme(currentTheme());
          }}
        >
          {theme === "dark" ? "☀" : "☾"}
        </button>
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
                <p className="muted-note">尚无项目——到「设置」里添加项目目录。</p>
              )}
            </div>
          </div>
          <div className="pane-body" style={{ paddingTop: 2 }}>
            <button className="mf-btn ghost settings-entry" onClick={() => setTab("settings")}>
              项目管理 →
            </button>
          </div>
        </aside>

        {editing ? (
          <section className="pane center editor-pane" aria-label="工作流编辑器">
            <WorkflowEditor
              client={client}
              projectHandle={editing.project.handle}
              workflowHandle={editing.workflow.handle}
              onDone={(message) => {
                setToast(message);
                void refresh();
              }}
              onClose={() => setEditing(null)}
            />
          </section>
        ) : tab === "settings" ? (
          <section className="pane center" aria-label="设置">
            <SettingsPane
              client={client}
              view={view}
              clis={clis}
              instances={instances}
              recipes={recipes}
              installing={installing}
              onInstall={async (agentType) => {
                setInstalling(agentType);
                try {
                  const result = await client.cliInstall(agentType);
                  window.dispatchEvent(
                    new CustomEvent("mf-toast", {
                      detail:
                        result.outcome === "installed"
                          ? `已安装 ${agentType}${result.version ? `(v${result.version})` : ""}`
                          : `安装失败:${result.reason ?? result.outcome}`,
                    }),
                  );
                } catch (error) {
                  window.dispatchEvent(
                    new CustomEvent("mf-toast", {
                      detail: `安装失败:${error instanceof Error ? error.message : String(error)}`,
                    }),
                  );
                } finally {
                  setInstalling(null);
                  void client.cliDetect().then(setClis).catch(() => {});
                }
              }}
              onBrowse={(root) => setCodeBrowser(root)}
              onVcs={(root) => setVcsRoot(root)}
              onDone={(message) => {
                setToast(message);
                void refresh();
              }}
            />
          </section>
        ) : (
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
                project.runs.length === 0 && project.workflows.length === 0 ? null : (
                  <div key={project.handle} style={{ marginBottom: 14 }}>
                    <div className="group-head">
                      <span className="name">{project.name}</span>
                      <span className="meta">
                        {project.workflows.length} 工作流 · {project.runs.length} 运行 ·{" "}
                        {project.activeSessions} 活跃会话
                      </span>
                      <span className="group-actions">
                        <button className="mf-btn ghost card-action" onClick={() => setCodeBrowser(project.root ?? "")}>
                          代码
                        </button>
                      </span>
                    </div>
                    {project.workflows.length > 0 && (
                      <div className="workflow-list" style={{ marginBottom: 10 }}>
                        {project.workflows.map((workflow) => (
                          <WorkflowCard
                            key={workflow.handle}
                            workflow={workflow}
                            project={project}
                            client={client}
                            onEdit={() => setEditing({ project, workflow })}
                            onDone={(message) => {
                              setToast(message);
                              void refresh();
                            }}
                          />
                        ))}
                      </div>
                    )}
                    {project.runs.length > 0 && (
                      <div className="run-list">
                        {project.runs.map((run, index) => (
                          <RunCard
                            key={run.handle}
                            run={run}
                            selected={run.handle === selectedRun}
                            index={index}
                            cancellable={client.isController && runIsActive(run.status)}
                            onCancel={async () => {
                              try {
                                await client.command(
                                  runActionCommand({
                                    commandId: uuidv7(),
                                    clientId: client.clientId,
                                    controllerLeaseEpoch: client.leaseEpoch,
                                    projectHandle: run.projectHandle,
                                    runHandle: run.handle,
                                    runRevision: run.revision,
                                    type: "workflow.run.cancel",
                                    payload: {},
                                  }),
                                );
                                setToast("已请求取消");
                                void refresh();
                              } catch (error) {
                                setToast(
                                  `取消失败:${error instanceof Error ? error.message : String(error)}`,
                                );
                              }
                            }}
                            onSelect={() => setSelectedRun(run.handle)}
                          />
                        ))}
                      </div>
                    )}
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
        )}

        <aside className="pane inspector" aria-label="检视器">
          <div className="pane-head">
            <h2>检视器</h2>
            {selected && (
              <span className="leaf-count">{runStatusMeta(selected.status).label}</span>
            )}
          </div>
          <div className="inspector-body">
            {selected ? (
              <RunDetail
                run={selected}
                client={client}
                instances={instances}
                onTerminal={setTerminalSession}
                onAction={(message) => {
                  setToast(message);
                  void refresh();
                }}
              />
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

            <div className="pane-head" style={{ margin: "0 -14px", padding: 0 }}>
              <h2 style={{ fontSize: 11, padding: "8px 14px", width: "100%" }}>
                事件流 {wsOpen ? "●" : "○"}
              </h2>
            </div>
            {projection && projection.feed.length > 0 ? (
              <div className="feed">
                {projection.feed.slice(0, 12).map((item) => (
                  <div
                    key={item.seq}
                    className={`feed-item ${item.critical ? "critical" : ""}`}
                  >
                    <span className="seq">{item.seq}</span>
                    <span>{item.type}</span>
                  </div>
                ))}
              </div>
            ) : (
              <p className="muted-note">
                等待事件…{wsOpen ? "(已订阅)" : "(事件流未连接,轮询兜底)"}
              </p>
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

      {paletteOpen && (
        <CommandPalette
          onClose={() => setPaletteOpen(false)}
          actions={[
            { label: "工作流", run: () => { setTab("workflows"); localStorage.setItem("mf.defaultTab", "workflows"); } },
            { label: "运行", run: () => { setTab("runs"); localStorage.setItem("mf.defaultTab", "runs"); } },
            { label: "设置 / 项目管理", run: () => { setTab("settings"); localStorage.setItem("mf.defaultTab", "settings"); } },
            {
              label: `切换到${currentTheme() === "dark" ? "亮色" : "深色"}主题`,
              run: () => {
                applyTheme(toggleTheme());
                setTheme(currentTheme());
              },
            },
          ]}
        />
      )}
      {codeBrowser && codeBrowser !== "" && (
        <CodeBrowserModal
          client={client}
          startPath={codeBrowser}
          title={codeBrowser.split(/[\/]/).pop() ?? codeBrowser}
          onClose={() => setCodeBrowser(null)}
        />
      )}
      {vcsRoot && <VcsPanel root={vcsRoot} onClose={() => setVcsRoot(null)} />}
      {terminalSession && (
        <TerminalPanel sessionHandle={terminalSession} onClose={() => setTerminalSession(null)} />
      )}
      {toast && (
        <div role="alert" className="toast" onClick={() => setToast(null)}>
          {toast}
        </div>
      )}
      {createOpen && view && view.projects.length > 0 && (
        <CreateWorkflowModal
          client={client}
          projects={view.projects}
          cliOptions={[
            ...instances.filter((i) => i.enabled).map((i) => i.id),
            ...clis.map((cli) => cli.agent_type_id),
          ]}
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
        <span className="t">{project.name}</span>
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
  cancellable = false,
  onCancel,
  onSelect,
}: {
  run: RunView;
  selected: boolean;
  index: number;
  cancellable?: boolean;
  onCancel?: () => void;
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
        {cancellable && (
          <button
            className="mf-btn danger card-action"
            onClick={(event) => {
              event.stopPropagation();
              onCancel?.();
            }}
          >
            取消
          </button>
        )}
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

function RunDetail({
  run,
  client,
  instances,
  onTerminal,
  onAction,
}: {
  run: RunView;
  client: WorkbenchClient;
  instances: Array<{ id: string; name: string; enabled: boolean }>;
  onTerminal: (sessionHandle: string) => void;
  onAction: (message: string) => void;
}) {
  const meta = runStatusMeta(run.status);
  const [detail, setDetail] = useState<RunDetailView | null>(null);

  // 选中运行时拉取权威详情(轮询由外层 refresh 触发重拉)
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const data = await client.workflowRunSnapshot(run.projectHandle, run.handle);
        if (!cancelled) setDetail(runDetailViewOf(data));
      } catch {
        /* 详情拉取失败时保留摘要;动作面不可用 */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, run.handle, run.projectHandle, run.revision]);

  const act = useCallback(
    async (
      type: CommandType,
      payload: Record<string, unknown>,
      stepHandle?: string,
      stepRevision?: string,
    ) => {
      if (!detail) return;
      // RetryStep/Respond/Settle 要求 expected 携带目标 Step 语义 revision
      const stepExpectation =
        stepHandle && stepRevision
          ? [
              {
                aggregate: {
                  kind: "workflow_step",
                  handle: stepHandle.startsWith("step_") ? stepHandle : `step_${stepHandle}`,
                },
                semantic_revision: stepRevision,
              },
            ]
          : [];
      try {
        await client.command({
          ...runActionCommand({
            commandId: uuidv7(),
            clientId: client.clientId,
            controllerLeaseEpoch: client.leaseEpoch,
            projectHandle: run.projectHandle,
            runHandle: run.handle,
            runRevision: detail.revision,
            type,
            payload,
          }),
          expected: [
            {
              aggregate: {
                kind: "workflow_run",
                handle: run.handle.startsWith("run_") ? run.handle : `run_${run.handle}`,
              },
              semantic_revision: detail.revision,
            },
            ...stepExpectation,
          ],
        });
        onAction("已提交");
      } catch (error) {
        const code = error instanceof ApiError ? error.problem.code : null;
        if (code === "controller_required" || code === "controller_lease_expired") {
          location.reload();
          return;
        }
        onAction(`操作失败:${error instanceof Error ? error.message : String(error)}`);
      }
    },
    [client, detail, onAction, run.handle, run.projectHandle],
  );

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

      {detail && client.isController && instances.length > 0 && (
        <button
          className="mf-btn ghost"
          onClick={() => {
            const instanceId = window.prompt(
              "Agent 实例 ID(来自 catalog)",
              instances[0]?.id ?? "",
            );
            if (!instanceId) return;
            void client
              .adhocSession({
                projectHandle: run.projectHandle,
                runHandle: run.handle,
                instanceId,
              })
              .then((result) => {
                onAction(`会话「${result.title}」已发起${result.display_session_handle ? ";可在会话列表打开终端" : ""}`);
              })
              .catch((error) => {
                onAction(`发起失败:${error instanceof Error ? error.message : String(error)}`);
              });
          }}
        >
          ＋发起 ad-hoc 会话
        </button>
      )}

      {detail && detail.sessions.length > 0 && (
        <div className="session-row">
          {detail.sessions.map((session) => (
            <button
              key={session.agentSession}
              className="mf-btn ghost"
              onClick={() => onTerminal(session.agentSession)}
              title={`${session.title} · ${session.runtime}`}
            >
              ▶ 终端 {session.title.slice(0, 14)}
            </button>
          ))}
        </div>
      )}

      {detail && detail.pendingProposals.length > 0 && (
        <div className="proposal-card">
          <div className="question-text">
            Agent 提案了新的任务链({detail.pendingProposals.length} 个 draft revision):
          </div>
          {detail.pendingProposals.map((proposal) => (
            <div key={proposal.revisionHandle} className="proposal-item">
              <div className="proposal-steps">
                {proposal.steps.map((step) => (
                  <span key={step.key} className="badge tone-info">
                    {step.title} · {step.agent}
                  </span>
                ))}
              </div>
              <button
                className="mf-btn primary"
                disabled={!client.isController}
                title={client.isController ? undefined : "Observer 禁写"}
                onClick={() =>
                  act("workflow.confirm_proposal", { project_handle: run.projectHandle })
                }
              >
                确认激活
              </button>
            </div>
          ))}
        </div>
      )}

      {detail && <AgentRunsRow detail={detail} />}

      {detail && detail.steps.length > 0 && (
        <div className="step-timeline">
          {detail.steps.map((step) => {
            const question = detail.questions.find((q) => q.step === step.step) ?? null;
            const stepMeta = stepStatusMeta(step.status);
            return (
              <div key={step.step} className={`step-item tone-${stepMeta.tone}`}>
                <div className="step-head">
                  <span className={`badge tone-${stepMeta.tone}`}>{stepMeta.label}</span>
                  <span className="step-title">{step.title}</span>
                </div>
                {question && (
                  <QuestionCard
                    question={question}
                    onAnswer={(answer) =>
                      act("workflow.run.respond", { step_handle: step.step, answer }, step.step, step.revision)
                    }
                  />
                )}
                {step.status === "awaiting-outcome" && (
                  <SettleCard
                    disabled={!client.isController || agentRunOfStep(detail, step.step) === null}
                    onSettle={(kind, text) =>
                      act(
                        "workflow.run.settle",
                        {
                          step_handle: step.step,
                          agent_run_handle: agentRunOfStep(detail, step.step),
                          settlement:
                            kind === "complete"
                              ? { kind: "complete", summary: text }
                              : { kind: "fail", reason: text },
                        },
                        step.step,
                        step.revision,
                      )
                    }
                  />
                )}
                {step.status === "failed" && client.isController && (
                  <button
                    className="mf-btn ghost"
                    onClick={() =>
                      act(
                        "workflow.run.retry_step",
                        { step_handle: step.step, mode: "fresh_session" },
                        step.step,
                        step.revision,
                      )
                    }
                  >
                    重试(新会话)
                  </button>
                )}
              </div>
            );
          })}
        </div>
      )}
    </>
  );
}

/** 工作流卡片(#75):名称 + 双轴修订 + 启动运行(goal 输入)。 */
function WorkflowCard({
  workflow,
  project,
  client,
  onEdit,
  onDone,
}: {
  workflow: WorkflowView;
  project: ProjectView;
  client: WorkbenchClient;
  onEdit: () => void;
  onDone: (message: string) => void;
}) {
  const [goal, setGoal] = useState("");
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  return (
    <div className="workflow-card">
      <div className="row1">
        <span className="title">{workflow.name}</span>
        <span className="mono-dim">wf_{workflow.handle.slice(4, 14)}…</span>
        <button
          className="mf-btn ghost card-action"
          onClick={onEdit}
          title="打开 DAG 编辑器"
        >
          编辑
        </button>
        <button
          className="mf-btn primary card-action"
          disabled={!client.isController}
          title={client.isController ? undefined : "Observer 禁写"}
          onClick={() => setOpen((value) => !value)}
        >
          启动运行
        </button>
      </div>
      <div className="row2">
        <span>语义 rev {workflow.semanticRevision}</span>
        <span className="sep">|</span>
        <span>呈现 rev {workflow.presentationRevision}</span>
      </div>
      {open && (
        <div className="question-actions" style={{ marginTop: 8 }}>
          <input
            value={goal}
            placeholder="本次运行的目标(必填)"
            onChange={(event) => setGoal(event.target.value)}
          />
          <button
            className="mf-btn primary"
            disabled={goal.trim().length === 0 || busy}
            onClick={async () => {
              setBusy(true);
              try {
                await client.command({
                  schema: "mf.command.v1",
                  command_id: uuidv7(),
                  client_id: client.clientId,
                  controller_lease_epoch: client.leaseEpoch,
                  target: { kind: "project_workflow", handle: project.handle },
                  expected: [
                    {
                      aggregate: {
                        kind: "project_workflow",
                        handle: `wf_${workflow.handle}`,
                      },
                      semantic_revision: workflow.semanticRevision,
                    },
                  ],
                  type: "workflow.run.start",
                  payload: {
                    workflow_handle: `wf_${workflow.handle}`,
                    goal: goal.trim(),
                  },
                });
                setOpen(false);
                onDone("运行已启动");
              } catch (error) {
                setBusy(false);
                const code = error instanceof ApiError ? error.problem.code : null;
                if (code === "controller_required" || code === "controller_lease_expired") {
                  location.reload();
                  return;
                }
                onDone(`启动失败:${error instanceof Error ? error.message : String(error)}`);
              }
            }}
          >
            {busy ? "启动中…" : "启动"}
          </button>
        </div>
      )}
    </div>
  );
}

/** 命令面板(#84):Ctrl+K;过滤执行动作列表。 */
function CommandPalette({
  actions,
  onClose,
}: {
  actions: Array<{ label: string; run: () => void }>;
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const filtered = actions.filter((action) =>
    action.label.toLowerCase().includes(query.trim().toLowerCase()),
  );
  return (
    <div
      className="scrim"
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="modal command-palette" role="dialog" aria-modal="true" aria-label="命令面板">
        <div className="field" style={{ marginBottom: 8 }}>
          <input
            autoFocus
            value={query}
            placeholder="输入命令…(Ctrl+K 切换)"
            onChange={(event) => setQuery(event.target.value)}
          />
        </div>
        <div className="folder-list" style={{ minHeight: 0 }}>
          {filtered.map((action) => (
            <button
              key={action.label}
              className="folder-item"
              onClick={() => {
                action.run();
                onClose();
              }}
            >
              <span className="folder-icon">↳</span>
              {action.label}
            </button>
          ))}
          {filtered.length === 0 && <p className="muted-note">没有匹配命令。</p>}
        </div>
      </div>
    </div>
  );
}

/** Agent 运行状态行(#86:终端嗅探语义的 agent_state 可视化)。 */
function AgentRunsRow({ detail }: { detail: RunDetailView }) {
  if (detail.agentRuns.length === 0) return null;
  const stateIcon = (state: string): string => {
    if (state.includes("working")) return "◉";
    if (state.includes("waiting")) return "◐";
    return "○";
  };
  return (
    <div className="agent-runs-row">
      {detail.agentRuns.map((run) => (
        <span key={run.agentRun} className="mono-dim" title={`${run.status} · ${run.agentState}`}>
          {stateIcon(run.agentState)} {run.status}
        </span>
      ))}
    </div>
  );
}

/** 步骤状态元数据(StepStatus str_enum 的中文标签)。 */
function stepStatusMeta(status: string): { label: string; tone: string } {
  const table: Record<string, { label: string; tone: string }> = {
    pending: { label: "等待依赖", tone: "dim" },
    ready: { label: "就绪", tone: "info" },
    running: { label: "执行中", tone: "live" },
    "awaiting-outcome": { label: "待结算", tone: "warn" },
    "needs-input": { label: "等待输入", tone: "warn" },
    succeeded: { label: "成功", tone: "ok" },
    failed: { label: "失败", tone: "bad" },
    blocked: { label: "被阻塞", tone: "dim" },
    skipped: { label: "已跳过", tone: "dim" },
    cancelled: { label: "已取消", tone: "dim" },
  };
  return table[status] ?? { label: status, tone: "dim" };
}

function QuestionCard({
  question,
  onAnswer,
}: {
  question: { question: string };
  onAnswer: (answer: string) => void;
}) {
  const [answer, setAnswer] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <div className="question-card">
      <div className="question-text">{question.question}</div>
      <div className="question-actions">
        <input
          value={answer}
          placeholder="输入回答…"
          onChange={(event) => setAnswer(event.target.value)}
        />
        <button
          className="mf-btn primary"
          disabled={answer.trim().length === 0 || busy}
          onClick={() => {
            setBusy(true);
            onAnswer(answer.trim());
          }}
        >
          回答
        </button>
      </div>
    </div>
  );
}

function SettleCard({
  disabled,
  onSettle,
}: {
  disabled: boolean;
  onSettle: (kind: "complete" | "fail", text: string) => void;
}) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <div className="question-card">
      <div className="question-text">该步骤在等待结算(exit/idle 都不是结算——由你判定)。</div>
      <div className="question-actions">
        <input
          value={text}
          placeholder={text === "" ? "总结(可选)" : "总结"}
          onChange={(event) => setText(event.target.value)}
        />
        <button
          className="mf-btn primary"
          disabled={busy}
          onClick={() => {
            setBusy(true);
            onSettle("complete", text.trim() || "完成");
          }}
        >
          结算成功
        </button>
        <button
          className="mf-btn danger"
          disabled={busy}
          onClick={() => {
            setBusy(true);
            onSettle("fail", text.trim() || "未说明原因");
          }}
        >
          结算失败
        </button>
      </div>
    </div>
  );
}

/** 设置页:项目管理(多项目同时在线)+ 系统信息。 */
function SettingsPane({
  client,
  view,
  clis,
  instances,
  recipes,
  installing,
  onInstall,
  onBrowse,
  onVcs,
  onDone,
}: {
  client: WorkbenchClient;
  view: WorkspaceView | null;
  clis: Array<{ agent_type_id: string; executable: string }>;
  instances: Array<{ id: string; name: string; enabled: boolean }>;
  recipes: Array<{ agent_type: string; package: string; display: string }>;
  installing: string | null;
  onInstall: (agentType: string) => Promise<void>;
  onBrowse: (root: string) => void;
  onVcs: (root: string) => void;
  onDone: (message: string) => void;
}) {
  const [addOpen, setAddOpen] = useState(false);
  const isController = client.isController;
  const projects = view?.projects ?? [];

  return (
    <>
      <div className="pane-head">
        <h2>设置</h2>
        <span className="leaf-count">{projects.length} 项目</span>
      </div>
      <div className="pane-body">
        <div className="settings-section">
          <div className="settings-title">
            <span>项目管理</span>
            <button
              className="mf-btn primary"
              disabled={!isController}
              title={isController ? undefined : "Observer 禁写——接管为 Controller 后可操作"}
              onClick={() => setAddOpen(true)}
            >
              添加项目
            </button>
          </div>
          <p className="muted-note">
            挂载本机项目目录(在其中初始化/复用 .mf-agent 存储);多项目同时在线,快照与事件流覆盖全部项目。
          </p>
          {projects.length > 0 ? (
            <div className="project-table">
              <div className="project-table-row head">
                <span>项目</span>
                <span className="num">工作流</span>
                <span className="num">运行</span>
                <span className="num">会话</span>
                <span className="path">路径</span>
                <span className="ops" aria-hidden="true" />
              </div>
              {projects.map((project) => (
                <div key={project.handle} className="project-table-row">
                  <span className="pname" title={`proj_${project.handle.slice(5, 13)}`}>
                    {project.name}
                  </span>
                  <span className="num">{project.workflows.length}</span>
                  <span className="num">{project.runs.length}</span>
                  <span className="num">{project.activeSessions}</span>
                  <span
                    className="path"
                    title={project.root || "路径未知(重启后挂载的项目可见)"}
                  >
                    {project.root || "—"}
                  </span>
                  <span className="ops">
                    <button
                      className="icon-btn"
                      title="重命名(自定义显示名)"
                      disabled={!isController}
                      onClick={() => {
                        const name = window.prompt("项目自定义名字(留空恢复路径名)", project.name);
                        if (name === null) return;
                        void client
                          .renameProject(project.handle, name)
                          .then(() => onDone(name.trim() ? `已重命名为「${name.trim()}」` : "已恢复路径名"))
                          .catch((error) =>
                            onDone(`重命名失败:${error instanceof Error ? error.message : String(error)}`),
                          );
                      }}
                    >
                      ✎
                    </button>
                    <button
                      className="icon-btn"
                      onClick={() => onBrowse(project.root)}
                      disabled={!project.root}
                      title={project.root ? "浏览代码" : "路径未知(重启后挂载的项目可见)"}
                    >
                      {"</>"}
                    </button>
                    <button
                      className="icon-btn"
                      onClick={() => project.root && onVcs(project.root)}
                      disabled={!project.root}
                      title="查看 Git 变更"
                    >
                      ⎇
                    </button>
                    <button
                      className="icon-btn danger"
                      disabled={!isController}
                      title={isController ? "移除项目" : "Observer 禁写"}
                      onClick={async () => {
                        try {
                          await client.detachProject(project.handle);
                          onDone(`项目「${project.name}」已移除`);
                        } catch (error) {
                          onDone(
                            `移除失败:${error instanceof Error ? error.message : String(error)}`,
                          );
                        }
                      }}
                    >
                      ✕
                    </button>
                  </span>
                </div>
              ))}
            </div>
          ) : (
            <p className="muted-note">当前没有在线项目。</p>
          )}
        </div>

        <div className="settings-section">
          <div className="settings-title">
            <span>通知</span>
            <button
              className="mf-btn ghost"
              onClick={() => {
                const next = localStorage.getItem("mf.notify") === "off" ? "on" : "off";
                localStorage.setItem("mf.notify", next);
                onDone(next === "off" ? "通知已关闭(刷新生效)" : "通知已开启(刷新生效)");
              }}
            >
              {localStorage.getItem("mf.notify") === "off" ? "开启" : "关闭"}
            </button>
          </div>
          <p className="muted-note">
            运行进入「需要你」时发送系统通知与提示音(仅页面不在前台时打扰)。
          </p>
        </div>

        <div className="settings-section">
          <div className="settings-title">
            <span>CLI 管理</span>
          </div>
          <p className="muted-note">
            检测本机 PATH 中的 agent CLI;安装/更新/修复写面待内核命令族接管(深水区票)。
          </p>
          {clis.length > 0 ? (
            <div className="project-admin-list">
              {clis.map((cli) => (
                <div key={cli.agent_type_id} className="project-admin-row">
                  <div className="info">
                    <span className="name">{cli.agent_type_id}</span>
                    <span className="meta">{cli.executable}</span>
                  </div>
                  <span className="badge tone-ok">已检测</span>
                </div>
              ))}
            </div>
          ) : (
            <p className="muted-note">未在 PATH 中检测到常见 agent CLI。</p>
          )}
          {recipes.length > 0 && (
            <>
              <p className="muted-note" style={{ marginTop: 10 }}>
                安装({recipes.length} 个可用;包管理器全局安装,完成后自动检测):
              </p>
              <div className="project-admin-list">
                {recipes
                  .filter((recipe) => !clis.some((cli) => cli.agent_type_id === recipe.agent_type))
                  .map((recipe) => (
                    <div key={recipe.agent_type} className="project-admin-row">
                      <div className="info">
                        <span className="name">{recipe.display}</span>
                        <span className="meta">{recipe.package}</span>
                      </div>
                      <button
                        className="mf-btn primary"
                        disabled={!client.isController || installing !== null}
                        title={client.isController ? undefined : "Observer 禁写"}
                        onClick={() => void onInstall(recipe.agent_type)}
                      >
                        {installing === recipe.agent_type ? "安装中…" : "安装"}
                      </button>
                    </div>
                  ))}
              </div>
            </>
          )}
        </div>

        <div className="settings-section">
          <div className="settings-title">
            <span>Agent 实例(catalog)</span>
          </div>
          <p className="muted-note">
            真实目录只读列表;节点实例下拉优先使用这些 ID。注册/编辑经 launcher/CLI(web 不写目录)。
          </p>
          {instances.length > 0 ? (
            <div className="project-admin-list">
              {instances.map((instance) => (
                <div key={instance.id} className="project-admin-row">
                  <div className="info">
                    <span className="name">{instance.name}</span>
                    <span className="meta">{instance.id}</span>
                  </div>
                  <span className={`badge tone-${instance.enabled ? "ok" : "dim"}`}>
                    {instance.enabled ? "启用" : "停用"}
                  </span>
                </div>
              ))}
            </div>
          ) : (
            <p className="muted-note">目录中暂无实例。</p>
          )}
        </div>

        <div className="settings-section">
          <div className="settings-title">
            <span>系统信息</span>
          </div>
          <dl className="kv">
            <dt>Core 实例</dt>
            <dd>{view?.serverInstanceId ?? "—"}</dd>
            <dt>投影游标</dt>
            <dd>
              {view ? `${view.streamEpoch} @ ${view.throughSeq}` : "—"}
            </dd>
            <dt>本会话角色</dt>
            <dd>{isController ? "Controller(可写)" : "Observer(只读)"}</dd>
            <dt>客户端 ID</dt>
            <dd>{client.clientId}</dd>
            <dt>lease epoch</dt>
            <dd>{client.leaseEpoch}</dd>
          </dl>
        </div>
      </div>
      {addOpen && (
        <AddProjectModal
          client={client}
          onClose={() => setAddOpen(false)}
          onDone={(message) => {
            setAddOpen(false);
            onDone(message);
          }}
        />
      )}
    </>
  );
}

/** 添加项目弹层(#73):服务端目录浏览选择(浏览器沙箱拿不到真实
 * 路径,原生 showDirectoryPicker 不可用)。面包屑 + 目录列表 +
 * 上级 + 快速入口;选中仍走 POST /api/v1/projects。 */
function AddProjectModal({
  client,
  onDone,
  onClose,
}: {
  client: WorkbenchClient;
  onDone: (message: string) => void;
  onClose: () => void;
}) {
  const [current, setCurrent] = useState<{ path: string; parent: string | null } | null>(null);
  const [dirs, setDirs] = useState<Array<{ path: string; name: string }>>([]);
  const [roots, setRoots] = useState<Array<{ path: string; name: string }>>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [filter, setFilter] = useState("");

  const enter = useCallback(
    async (path: string) => {
      setLoading(true);
      setError(null);
      try {
        const listing = await client.fsDirs(path);
        setCurrent({ path: listing.path, parent: listing.parent });
        setDirs(listing.dirs);
        if (listing.error) setError(listing.error);
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setLoading(false);
      }
    },
    [client],
  );

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const list = await client.fsRoots();
        if (cancelled) return;
        setRoots(list);
        const home = list.find((r) => r.name === "主目录") ?? list[0];
        if (home) void enter(home.path);
      } catch {
        if (!cancelled) setError("无法获取浏览起点");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, enter]);

  const segments = current ? current.path.split(/[\\/]+/).filter(Boolean) : [];

  return (
    <div
      className="scrim"
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="modal folder-modal" role="dialog" aria-modal="true" aria-label="选择项目目录">
        <h3>
          <span className="mark">＋</span>添加项目
        </h3>

        <div className="folder-quick">
          {roots.map((root) => (
            <button
              key={root.path}
              className={"mf-btn ghost" + (current?.path === root.path ? " active" : "")}
              onClick={() => void enter(root.path)}
            >
              {root.name}
            </button>
          ))}
        </div>

        <div className="folder-breadcrumb" aria-label="当前路径">
          <button
            className="crumb"
            disabled={!current?.parent}
            onClick={() => current?.parent && void enter(current.parent)}
            title="上一级"
          >
            ↑
          </button>
          {segments.map((segment, index) => {
            const target = segments.slice(0, index + 1).join("/");
            const isLast = index === segments.length - 1;
            return (
              <span key={target} className="crumb-seg">
                <button
                  className="crumb"
                  onClick={() => !isLast && void enter(index === 0 ? `${segment}/` : target)}
                >
                  {segment}
                </button>
                {!isLast && <span className="crumb-sep">/</span>}
              </span>
            );
          })}
          {loading && <span className="crumb-loading">读取中…</span>}
        </div>

        {dirs.length > 12 && (
          <div className="field folder-filter">
            <input
              value={filter}
              placeholder="过滤当前目录…"
              onChange={(event) => setFilter(event.target.value)}
              aria-label="过滤目录名"
            />
          </div>
        )}

        <div className="folder-list" role="listbox" aria-label="子目录">
          {dirs.length === 0 && !loading && (
            <p className="muted-note">{error ?? "此目录下没有可浏览的子目录。"}</p>
          )}
          {dirs
            .filter((dir) => !filter || dir.name.toLowerCase().includes(filter.toLowerCase()))
            .map((dir) => (
            <button
              key={dir.path}
              className="folder-item"
              onDoubleClick={() => void enter(dir.path)}
              onClick={() => void enter(dir.path)}
            >
              <span className="folder-icon">▸</span>
              {dir.name}
            </button>
          ))}
        </div>
        {error && dirs.length > 0 && <p className="muted-note">{error}</p>}

        <p className="muted-note">
          选中「选择此文件夹」挂载当前目录;Core 将在其中初始化/复用 .mf-agent 存储,同一目录重复添加幂等。
        </p>

        <div className="actions">
          <button className="mf-btn ghost" onClick={onClose}>
            取消
          </button>
          <button
            className="mf-btn primary"
            disabled={!current || busy}
            onClick={async () => {
              if (!current) return;
              setBusy(true);
              try {
                const result = await client.attachProject(current.path);
                onDone(`项目「${result.display_name}」已挂载`);
              } catch (err) {
                setBusy(false);
                setError(err instanceof Error ? err.message : String(err));
              }
            }}
          >
            {busy ? "挂载中…" : "选择此文件夹"}
          </button>
        </div>
      </div>
    </div>
  );
}

/** 新建工作流弹层:真实 workflow.create 命令(collection CAS 来自
 *  快照;draft 首节点——空节点是 EmptyWorkflow 校验错误)。 */
function CreateWorkflowModal({
  client,
  projects,
  cliOptions,
  onDone,
  onClose,
}: {
  client: WorkbenchClient;
  projects: ProjectView[];
  cliOptions: string[];
  onDone: (message: string) => void;
  onClose: () => void;
}) {
  const schema: FormSchema = {
    fields: [
      { kind: "select", id: "project", label: "项目", required: true,
        options: projects.map((p) => ({ value: p.handle, label: p.name })) },
      { kind: "text", id: "name", label: "工作流名称", placeholder: "如:仓库巡检", required: true },
      { kind: "text", id: "node", label: "首节点标题", placeholder: "如:检查变更", required: true },
      { kind: "text", id: "agent", label: "Agent 实例 ID", placeholder: "选择或输入 CLI(如 codex)",
        required: true, datalist: cliOptions, hint: "实例存在性在运行时绑定;创建时仅要求非空。" },
    ],
  };
  const [values, setValues] = useState<FormValues>(() => initialValues({
    fields: schema.fields.map((f) => (f.id === "project" ? { ...f, options: [{ value: projects[0]?.handle ?? "", label: projects[0]?.name ?? "" }] } : f)),
  }));
  const [busy, setBusy] = useState(false);
  const errors = validate(schema, values);
  const name = values.name ?? "";
  const firstNodeTitle = values.node ?? "";
  const agentInstanceId = values.agent ?? "";
  const projectHandle = values.project ?? "";
  const valid = Object.keys(errors).length === 0;
  const set = (id: string, value: string) => setValues((prev) => ({ ...prev, [id]: value }));

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
        {renderFields(schema, values, errors, set)}
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
                const code = error instanceof ApiError ? error.problem.code : null;
                if (code === "controller_required" || code === "controller_lease_expired") {
                  // 角色已过期(其它会话接管):重新探活,UI 回到
                  // Observer + 接管入口
                  location.reload();
                  return;
                }
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

/** #91: needs-you 系统通知 + 提示音(页面不可见时才打扰)。 */
async function notifyNeedsYou(data: Record<string, unknown>): Promise<void> {
  try {
    if ("Notification" in window && Notification.permission === "granted") {
      const title = String(data?.title ?? data?.workflow_run ?? "运行需要你");
      new Notification("MonkeyFence · 需要你", {
        body: `${title} 正在等待你的处理`,
        tag: "mf-needs-you",
      });
    }
    // 提示音: 短促双音(880Hz → 660Hz, 各 120ms)
    const AudioCtor =
      window.AudioContext ??
      (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
    if (AudioCtor) {
      const audio = new AudioCtor();
      const beep = (frequency: number, at: number) => {
        const oscillator = audio.createOscillator();
        const gain = audio.createGain();
        oscillator.frequency.value = frequency;
        oscillator.connect(gain);
        gain.connect(audio.destination);
        oscillator.start(audio.currentTime + at);
        oscillator.stop(audio.currentTime + at + 0.12);
      };
      beep(880, 0);
      beep(660, 0.15);
    }
  } catch {
    /* 通知/音频失败不影响主流程 */
  }
}

// 窄屏:Inspector 下移为第四行(布局由 CSS grid 的媒体查询承载;
// 组件结构保持三栏语义 + inspector 可重排)。
export const NARROW_INSPECTOR_ORDER = "inspector-last";
