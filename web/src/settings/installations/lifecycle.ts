// CLI Installation 生命周期 UI(T9c,Issue #57)。浏览器只提交冻结三元组
// (install_plan_handle + recipe_digest + catalog_revision),不能提交任意
// argv;成功必经 post-probe;失败不谎报 installed。
import type { CommandEnvelope, CommandType } from "../../api/protocol.ts";

export interface InstallPreviewUi {
  /** 冻结票据(提交只认三元组)。 */
  planHandle: string;
  recipeDigest: string;
  catalogRevision: string;
  source: string;
  /** argv 摘要(展示;不可回用作命令)。 */
  argvSummary: string[];
  targetDirectory: string;
  checksum: string;
  rollbackAvailable: boolean;
  /** 受影响 pinned Revision 数(0 = 无 pin 影响)。 */
  pinnedRevisions: number;
  requiresElevation: boolean;
}

export type JobStateUi =
  | "queued"
  | "resolving"
  | "downloading"
  | "executing"
  | "verifying"
  | "installed"
  | "failed"
  | "cancelled"
  | "repair_needed";

export interface JobProgressUi {
  state: JobStateUi;
  /** 脱敏日志尾(服务端已 redact)。 */
  logTail: string[];
  operationHandle: string | null;
}

/** 提交票据:冻结三元组之外的任何字段被剔除(不能提交解析后的 argv)。 */
export function installTicketCommand(
  ctx: { commandId: string; clientId: string; controllerLeaseEpoch: string },
  preview: InstallPreviewUi,
  action: "install" | "update" | "repair" | "uninstall",
): CommandEnvelope {
  const type: CommandType = ({
    install: "cli.install",
    update: "cli.update",
    repair: "cli.repair",
    uninstall: "cli.uninstall",
  } as const)[action];
  return {
    schema: "mf.command.v1",
    command_id: ctx.commandId,
    client_id: ctx.clientId,
    controller_lease_epoch: ctx.controllerLeaseEpoch,
    target: { kind: "installation_plan", handle: preview.planHandle },
    expected: [{ aggregate: { kind: "catalog", handle: "catalog" }, semantic_revision: preview.catalogRevision }],
    type,
    payload: {
      install_plan_handle: preview.planHandle,
      recipe_digest: preview.recipeDigest,
      catalog_revision: preview.catalogRevision,
    },
  };
}

/** 成功判定:仅 installed 状态(post-probe 由 Core 保证);失败不谎报。 */
export function isTerminalSuccess(state: JobStateUi): boolean {
  return state === "installed";
}

export function needsUserRepair(state: JobStateUi): boolean {
  return state === "repair_needed";
}

/** pin 冲突提示:活动 Run 引用的 pinned Revision 受影响时阻止破坏性
 *  操作(#43 语义),UI 显示受影响清单。 */
export function destructiveActionBlocked(pinnedRevisions: number): boolean {
  return pinnedRevisions > 0;
}
