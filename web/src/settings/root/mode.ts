// Root Mode UI(T9d,Issue #58)。仅 Controller 可开关;页面永不持有
// Broker capability/nonce/MAC;开启不等于插件可提权(缺 root_launch/
// capability 由 Core fail-closed);关闭后新请求拒、既有任务带徽标继续。
import type { CommandEnvelope, CommandType } from "../../api/protocol.ts";

export interface RootStateUi {
  enabled: boolean;
  rootEpoch: string | null;
  /** 授权等待中(UAC 对话);Core restart → 强制 off。 */
  authorizing: boolean;
}

export type RootProblemUi =
  | "root_authorization_denied"
  | "broker_unavailable"
  | "root_epoch_expired";

export function rootModeCommand(
  ctx: { commandId: string; clientId: string; controllerLeaseEpoch: string },
  action: "enable" | "disable",
): CommandEnvelope {
  return {
    schema: "mf.command.v1",
    command_id: ctx.commandId,
    client_id: ctx.clientId,
    controller_lease_epoch: ctx.controllerLeaseEpoch,
    target: { kind: "core", handle: "root-mode" },
    expected: [],
    type: (action === "enable" ? "root.enable" : "root.disable") as CommandType,
    payload: {},
  };
}

/** Core restart:Root Mode 强制 off(不持久化;§10.1)。 */
export function rootStateAfterCoreRestart(state: RootStateUi): RootStateUi {
  return { enabled: false, rootEpoch: null, authorizing: false };
}

/** 管理员徽标:Root Mode 开启期间,Run/Node/Session/Job 显示徽标;
 *  关闭后**既有**高权限对象仍带徽标(可完成/可取消)。 */
export interface AdminBadgeInput {
  rootModeEnabled: boolean;
  /** 该对象是否以高权限启动(Root epoch 内启动)。 */
  launchedUnderRoot: boolean;
}

export function adminBadge(input: AdminBadgeInput): boolean {
  return input.launchedUnderRoot;
}

/** 新高权限请求判定:关闭后拒绝(既有可完成/取消;新的全拒)。 */
export function newElevatedRequestAllowed(state: RootStateUi): boolean {
  return state.enabled;
}

/** 授权等待动画(全局红色状态持续)。 */
export function rootIndicatorVisible(state: RootStateUi): boolean {
  return state.enabled || state.authorizing;
}
