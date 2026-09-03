// Workbench 入口(T11 发布形态)。一次性 nonce 从 URL fragment 读取,
// 交换后立即清除;凭据永不进 URL/query/store。
import { createRoot } from "react-dom/client";
import { WorkbenchShell } from "../workbench/shell.tsx";
import { WorkbenchClient } from "../api/client.ts";

interface BootstrapResponse {
  client_id: string;
  csrf_token: string;
  controller: { role: string; lease_epoch?: string | number };
}

function showFatal(message: string): void {
  const mount = document.getElementById("workbench");
  if (mount) {
    mount.innerHTML = `<div role="alert" style="font-family:system-ui;padding:32px;line-height:1.6">${message}</div>`;
  }
}

async function bootstrap(): Promise<void> {
  const params = new URLSearchParams(location.hash.slice(1));
  const nonce = params.get("nonce");
  if (!nonce) {
    showFatal(
      "缺少一次性入口令牌。请使用 launcher 给出的 <code>#nonce=…</code> 入口 URL 打开本页。",
    );
    return;
  }
  let response: Response;
  try {
    response = await fetch("/auth/exchange", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ nonce }),
    });
  } catch (error) {
    showFatal(`无法连接 Core(${String(error)})。请确认 Core 正在运行后刷新。`);
    return;
  }
  if (!response.ok) {
    const detail = await response.text().catch(() => "");
    showFatal(
      `登录失败(${response.status})。${detail || ""}<br/>入口令牌是一次性的——请从 launcher 重新获取入口 URL。`,
    );
    return;
  }
  const data = (await response.json()) as BootstrapResponse;
  // fragment 立即清除(nonce 已消耗;不留在浏览器历史)
  history.replaceState(null, "", location.pathname);

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

void bootstrap();
