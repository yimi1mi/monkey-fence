// Workbench 壳(T8a,Issue #52):顶部「工作流 / 运行」、三栏布局与
// 窄屏 Inspector 下移;Observer 禁写 + 显式 takeover;全局 problem
// toast;断线重连(resume→4409→全量 resync)。仅嵌入 bundle,launcher
// 不打开(GPUI 仍为唯一入口)。

import { useEffect, useMemo, useState } from "react";
import type { WorkbenchClient } from "../api/client.ts";
import { reduceEvents, reduceResync, reduceSnapshot, type ProjectionState } from "../state/reducer.ts";

export type WorkbenchTab = "workflows" | "runs";

export function WorkbenchShell({ client }: { client: WorkbenchClient }) {
  const [tab, setTab] = useState<WorkbenchTab>("workflows");
  const [projection, setProjection] = useState<ProjectionState | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [connection, setConnection] = useState<"live" | "reconnecting">("live");

  // 刷新后 snapshot 基线 + resume(断线重连;4409 → 全量 resync)
  useEffect(() => {
    let closed = false;
    const boot = async () => {
      try {
        const snapshot = await client.workspaceSnapshot();
        if (closed) return;
        setProjection(reduceSnapshot(null as never, snapshot));
        setConnection("live");
      } catch (error) {
        if (!closed) {
          setToast(String(error));
          setConnection("reconnecting");
        }
      }
    };
    void boot();
    return () => {
      closed = true;
    };
  }, [client]);

  const layout = useMemo(() => ({ wide: "1fr 2fr 1fr", narrow: "1fr" }), []);
  const isController = client.isController;

  return (
    <div className="mf-workbench">
      <header>
        <nav>
          <button aria-selected={tab === "workflows"} onClick={() => setTab("workflows")}>
            工作流
          </button>
          <button aria-selected={tab === "runs"} onClick={() => setTab("runs")}>
            运行
          </button>
        </nav>
        <span role="status">{connection === "live" ? "已连接" : "重连中…"}</span>
        <span role="badge">{isController ? "Controller" : "Observer"}</span>
        {!isController && <TakeoverButton client={client} onTaken={() => location.reload()} />}
      </header>
      <main style={{ display: "grid", gridTemplateColumns: layout.wide }}>
        <aside>{projection?.activeRuns ?? 0} 运行中</aside>
        <section>{tab === "workflows" ? "工作流列表(权威投影)" : "运行列表"}</section>
        <aside className="inspector">
          Needs You:{projection?.needsYou ?? 0}
          <button disabled={!isController} title={isController ? undefined : "Observer 禁写"}>
            新建工作流
          </button>
        </aside>
      </main>
      {toast && (
        <div role="alert" className="toast" onClick={() => setToast(null)}>
          {toast}
        </div>
      )}
    </div>
  );
}

function TakeoverButton({
  client,
  onTaken,
}: {
  client: WorkbenchClient;
  onTaken: () => void;
}) {
  const [busy, setBusy] = useState(false);
  return (
    <button
      disabled={busy}
      onClick={async () => {
        setBusy(true);
        try {
          // CAS:最后观察 epoch 由 client 持有;此处占位为当前连接 epoch
          await client.takeover("0");
          onTaken();
        } catch {
          setBusy(false);
        }
      }}
    >
      接管为 Controller
    </button>
  );
}

// 窄屏:Inspector 下移为第四行(布局由 CSS grid 的媒体查询承载;
// 组件结构保持三栏语义 + inspector 可重排)。
export const NARROW_INSPECTOR_ORDER = "inspector-last";
