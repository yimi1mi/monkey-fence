// Workbench 入口(T11 发布形态)。一次性 nonce 从 URL fragment 读取,
// 交换后立即清除;凭据永不进 URL/query/store(sessionStorage 只保留
// 刷新续用所需的 client/CSRF 标识,不包含 nonce 本身)。
import { createRoot } from "react-dom/client";
import { WorkbenchShell } from "../workbench/shell.tsx";
import { WorkbenchClient } from "../api/client.ts";
import {
  clearStoredSession,
  readStoredSession,
  storeSession,
  type BootstrapSession,
} from "../api/session.ts";
import { installResizeObserverFallback } from "../api/resize_observer_fallback.ts";
import "../styles/global.css";

function showFatal(message: string): void {
  const mount = document.getElementById("workbench");
  if (mount) {
    mount.innerHTML = `<div class="mf-fatal" role="alert"><div class="card"><h1>◤ 无法进入工作台</h1>${message}</div></div>`;
  }
}

function mountWorkbench(data: BootstrapSession): void {
  storeSession(data);
  const client = new WorkbenchClient({
    csrfToken: data.csrf_token,
    clientId: data.client_id,
    controllerLeaseEpoch: String(data.controller?.lease_epoch ?? "1"),
    role: data.controller?.role === "controller" ? "controller" : "observer",
  });
  const mount = document.getElementById("workbench");
  if (!mount) return;
  createRoot(mount).render(<WorkbenchShell client={client} />);
}

async function exchange(nonce: string): Promise<Response> {
  return fetch("/auth/exchange", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ nonce }),
  });
}

/** 验收模式重签(生产 404 → 返回 null,不影响一次性 nonce 语义)。 */
async function reissueNonce(): Promise<string | null> {
  const response = await fetch("/acceptance/new-nonce", { method: "POST" }).catch(() => null);
  if (!response || !response.ok) return null;
  return ((await response.json()) as { nonce: string }).nonce;
}

async function bootstrapWithNonce(nonce: string): Promise<void> {
  let response = await exchange(nonce);
  // 浏览器预加载可能已消耗一次性 nonce:本机验收模式下自动重签重试一次
  if (response.status === 401) {
    const fresh = await reissueNonce();
    if (fresh) {
      response = await exchange(fresh);
    }
  }
  if (!response.ok) {
    const detail = await response.text().catch(() => "");
    showFatal(
      `登录失败(${response.status})。${detail || ""}<br/>入口令牌是一次性的——请从 launcher 重新获取入口 URL。`,
    );
    return;
  }
  // fragment 立即清除(nonce 已消耗;不留在浏览器历史)
  history.replaceState(null, "", location.pathname);
  mountWorkbench((await response.json()) as BootstrapSession);
}

async function bootstrap(): Promise<void> {
  // 先装 RO 兜底(#97):必须在 React Flow/xterm 创建任何 ResizeObserver
  // 之前完成——替换只影响之后 new 的实例。判定 ≤ 两帧,不拖慢首屏。
  await installResizeObserverFallback();
  const params = new URLSearchParams(location.hash.slice(1));
  const nonce = params.get("nonce");
  if (nonce) {
    try {
      await bootstrapWithNonce(nonce);
    } catch (error) {
      showFatal(`无法连接 Core(${String(error)})。请确认 Core 正在运行后刷新。`);
    }
    return;
  }
  // 无 fragment:刷新场景——服务端权威探活(/auth/session 回报当前
  // 角色与 epoch;存储的角色可能已过期——其它会话接管/重换后本会话
  // 已降 Observer)。探活失败则本机验收模式尝试重签,生产保持指引。
  const stored = readStoredSession();
  if (stored) {
    const probe = await fetch("/auth/session").catch(() => null);
    if (probe && probe.ok) {
      mountWorkbench((await probe.json()) as BootstrapSession);
      return;
    }
    clearStoredSession();
  }
  const fresh = await reissueNonce();
  if (fresh) {
    await bootstrapWithNonce(fresh);
    return;
  }
  showFatal(
    "缺少一次性入口令牌。请使用 launcher 给出的 <code>#nonce=…</code> 入口 URL 打开本页。",
  );
}

void bootstrap();
