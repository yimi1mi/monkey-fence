// Agent Catalog UI 模型(T9a,Issue #55)。状态与数据全部来自 Core
// 权威 snapshot/discovery(#40 的 DiscoveredInstallation/CliInstallation);
// 前端只做展示分组与动作构建,不自行探测文件系统。
import type { CommandEnvelope, CommandType } from "../../api/protocol.ts";

export type InstallationState =
  | "absent"
  | "detected"
  | "external"
  | "managed"
  | "update_available"
  | "repair_needed";

export interface CatalogInstallation {
  /** opaque handle(inst_ 域)。 */
  handle: string;
  agentTypeId: string;
  state: InstallationState;
  version: string | null;
  /** external = PATH / managed = 受管根。 */
  source: "external" | "managed";
  scope: "user" | "machine";
  /** canonical executable + identity 摘要(路径+hash 前 8 位)。 */
  canonicalPath: string;
  identityDigest: string;
}

export interface CatalogCard {
  agentTypeId: string;
  displayName: string;
  installations: CatalogInstallation[];
}

/** 卡片主状态:未安装的显示安装动作而非置灰(§9.4)。 */
export function cardStatus(card: CatalogCard): InstallationState {
  if (card.installations.length === 0) return "absent";
  if (card.installations.some((i) => i.state === "repair_needed")) return "repair_needed";
  if (card.installations.some((i) => i.state === "update_available")) return "update_available";
  if (card.installations.some((i) => i.source === "managed")) return "managed";
  return "external";
}

/** 多安装选择:同 Type 的 external+managed 并存;canonical 去重由
 *  Core 已做(每 canonical 一条),UI 只按 scope/sourced 排序展示。 */
export function sortInstallations(items: CatalogInstallation[]): CatalogInstallation[] {
  return [...items].sort((a, b) => {
    const rank = (i: CatalogInstallation) =>
      (i.source === "managed" ? 0 : 1) * 10 + (i.scope === "user" ? 0 : 1);
    return rank(a) - rank(b);
  });
}

/** Agent Instance 草稿(引用固定 plugin/installation/profile)。 */
export interface AgentInstanceDraftUi {
  name: string;
  agentTypeId: string;
  installationHandle: string | null;
  providerProfileHandle: string | null;
}

/** availability:executable identity 变化(目标被替换)→ unavailable,
 *  Agent Run 启动拒绝(#40 verify_launch_identity 契约)。 */
export function instanceAvailability(
  draft: AgentInstanceDraftUi,
  catalog: CatalogCard[],
  pinnedIdentity: string | null,
): "available" | "unavailable" | "incomplete" {
  const card = catalog.find((c) => c.agentTypeId === draft.agentTypeId);
  if (!card || !draft.installationHandle) return "incomplete";
  const installation = card.installations.find((i) => i.handle === draft.installationHandle);
  if (!installation) return "unavailable";
  if (pinnedIdentity && pinnedIdentity !== installation.identityDigest) {
    return "unavailable"; // identity 已被替换:拒绝启动并提示重装
  }
  return "available";
}

export function catalogCommand(
  ctx: { commandId: string; clientId: string; controllerLeaseEpoch: string },
  type: CommandType,
  payload: Record<string, unknown>,
): CommandEnvelope {
  return {
    schema: "mf.command.v1",
    command_id: ctx.commandId,
    client_id: ctx.clientId,
    controller_lease_epoch: ctx.controllerLeaseEpoch,
    target: { kind: "catalog", handle: "catalog" },
    expected: [],
    type,
    payload,
  };
}
