// Provider Profile / write-only Secret / 模型下拉(T9b,Issue #56)。
// 明文只出现在 write-only 请求;提交后输入清空;不进 store/localStorage;
// 响应/事件/快照永不回显 Secret。
import type { CommandEnvelope, CommandType } from "../../api/protocol.ts";

export interface ProviderModel {
  id: string;
  displayName: string;
}

export interface ModelCatalogUi {
  models: ProviderModel[];
  source: "live" | "cache" | "manual";
  fetchedAt: string | null;
  /** 回退原因(明确错误;不含任何凭据)。 */
  fallbackError: string | null;
}

/** Secret 输入生命周期:submitted → 立即清空;store 永不持有明文。 */
export interface SecretInputState {
  plaintext: string;
  dirty: boolean;
}

export function secretInput(): SecretInputState {
  return { plaintext: "", dirty: false };
}

/** 提交后清空(唯一安全去向;双保险:组件 unmount/localStorage 都不落)。 */
export function secretSubmitted(input: SecretInputState): SecretInputState {
  return { plaintext: "", dirty: false };
}

/** 写入命令:明文只在请求 body(write-only 特例 §7.4);digest 内 HMAC。 */
export function providerSecretCommand(
  ctx: { commandId: string; clientId: string; controllerLeaseEpoch: string },
  input: { profileHandle: string; action: "create" | "replace" | "clear"; plaintext?: string },
): CommandEnvelope {
  return {
    schema: "mf.command.v1",
    command_id: ctx.commandId,
    client_id: ctx.clientId,
    controller_lease_epoch: ctx.controllerLeaseEpoch,
    target: { kind: "provider_profile", handle: input.profileHandle },
    expected: [],
    type: "catalog.provider_profile_upsert" as CommandType,
    payload:
      input.action === "clear"
        ? { handle: input.profileHandle, clear_secret: true }
        : { handle: input.profileHandle, api_secret: input.plaintext ?? "" },
  };
}

/** 模型下拉可见性:只有模型元数据+cache 状态;任何 Secret 字段剔除。 */
export function sanitizeModelPayload(catalog: ModelCatalogUi): ModelCatalogUi {
  return {
    models: catalog.models.map((m) => ({ id: m.id, displayName: m.displayName })),
    source: catalog.source,
    fetchedAt: catalog.fetchedAt,
    fallbackError: catalog.fallbackError,
  };
}

/** 手填模型校验(§9.8:允许手填合法模型 id)。 */
export function manualModelIdError(id: string): string | null {
  const trimmed = id.trim();
  if (trimmed === "") return "模型 id 不能为空";
  if (trimmed !== id) return "首尾含空白";
  if (/\s/.test(id)) return "含空白字符";
  if (!/^[A-Za-z0-9._:/-]+$/.test(id)) return "含非法字符(允许字母数字与 - _ . : /)";
  return null;
}
