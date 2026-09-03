// 会话续用(T11):bootstrap 结果存 sessionStorage(不含 nonce),
// 刷新时先探测 HttpOnly 会话再决定是否需要新入口令牌。

export interface BootstrapSession {
  client_id: string;
  csrf_token: string;
  controller: { role: string; lease_epoch?: string | number };
}

export const SESSION_KEY = "mf.workbench.session";

export function readStoredSession(): BootstrapSession | null {
  try {
    const raw = sessionStorage.getItem(SESSION_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as BootstrapSession;
    return parsed.client_id && parsed.csrf_token ? parsed : null;
  } catch {
    return null;
  }
}

export function storeSession(session: BootstrapSession): void {
  sessionStorage.setItem(SESSION_KEY, JSON.stringify(session));
}

export function clearStoredSession(): void {
  sessionStorage.removeItem(SESSION_KEY);
}
