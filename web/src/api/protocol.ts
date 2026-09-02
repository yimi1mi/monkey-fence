// MonkeyFence Web API v1 协议类型(T7b,Issue #39)。
// 与 crates/mf-web/src/api 的 Rust DTO 逐字段对齐;wire 冻结:
// v1 只允许 additive optional change(新可选字段/新资源/新非关键事件),
// 改字段语义必须升 v2。golden 对照:__tests__/protocol.golden.test.ts。

/** opaque handle:`wf_/run_/step_/sess_/inst_/op_/proj_` + 32 hex。 */
export type OpaqueHandle = string;

/** 字符串化 u64(JS Number 安全上限之上必须字符串)。 */
export type U64String = string;

export interface AggregateRef {
  kind: string;
  handle: OpaqueHandle;
}

export interface ExpectedRevision {
  aggregate: AggregateRef;
  semantic_revision?: U64String;
  presentation_revision?: U64String;
}

/** 封闭命令族(spec §7.4;与 Rust `CommandType` 的 serde rename 一致)。 */
export type CommandType =
  | "workflow.create" | "workflow.rename" | "workflow.delete"
  | "workflow.add_node" | "workflow.update_node" | "workflow.remove_node"
  | "workflow.move_node" | "workflow.connect" | "workflow.disconnect"
  | "workflow.viewport" | "workflow.set_unsafe_parallel_policy"
  | "workflow.run.start" | "workflow.run.cancel" | "workflow.run.retry_step"
  | "workflow.run.respond" | "workflow.run.settle"
  | "session.start_preview" | "session.stop_preview"
  | "session.start_adhoc" | "session.stop_adhoc"
  | "catalog.refresh_discovery" | "catalog.provider_model_probe"
  | "catalog.provider_profile_upsert" | "catalog.agent_instance_upsert"
  | "cli.install_preview" | "cli.install" | "cli.update" | "cli.repair"
  | "cli.uninstall" | "cli.cancel"
  | "root.enable" | "root.disable";

export interface CommandEnvelope {
  schema: "mf.command.v1";
  command_id: string;
  client_id: string;
  controller_lease_epoch: U64String;
  target: AggregateRef;
  expected: ExpectedRevision[];
  type: CommandType;
  payload: Record<string, unknown>;
}

export type CommandOutcomeWire =
  | { outcome: "applied"; revisions: ExpectedRevision[]; replayed: boolean }
  | { outcome: "accepted"; operation_handle: OpaqueHandle };

export interface SnapshotCursor {
  stream_epoch: string;
  through_seq: U64String;
}

export interface SnapshotEnvelope {
  schema: "mf.snapshot.v1";
  server_instance_id: string;
  cursor: SnapshotCursor;
  data: Record<string, unknown>;
}

export interface EventEnvelope {
  schema: "mf.event.v1";
  type: string;
  /** 未知 critical 事件 → 必须断开并 resync;非 critical 可忽略。 */
  critical: boolean;
  stream_epoch: string;
  seq: U64String;
  data: Record<string, unknown>;
}

export type ProblemCode =
  | "unsupported_api_version" | "unsupported_ws_subprotocol" | "invalid_envelope"
  | "unauthenticated" | "origin_rejected" | "csrf_rejected"
  | "controller_required" | "controller_lease_expired"
  | "resource_not_found" | "resource_scope_mismatch"
  | "revision_conflict" | "command_id_reused" | "command_in_progress"
  | "validation_failed" | "workflow_cycle" | "unknown_dependency"
  | "agent_instance_unavailable" | "plugin_version_unavailable" | "cli_version_mismatch"
  | "writer_required" | "writer_lease_expired" | "input_seq_conflict"
  | "terminal_epoch_mismatch" | "terminal_history_gap" | "frame_too_large" | "rate_limited"
  | "root_mode_required" | "root_epoch_expired" | "root_authorization_denied"
  | "broker_unavailable" | "elevation_required" | "installation_failed"
  | "resync_required" | "service_unavailable" | "internal_error" | "schema_future_version";

export type Retry =
  | "never" | "same_command_id" | "after_resync" | "after_reauth" | "after_retry_after";

export interface Problem {
  schema: "mf.problem.v1";
  code: ProblemCode;
  message: string;
  trace_id: string;
  command_id: string | null;
  retry: Retry | null;
  current?: Record<string, unknown>;
}

/** opaque handle 形态校验(浏览器不得提交任意 path/PID/argv)。 */
export function isValidHandle(handle: string): boolean {
  return /^(wf_|run_|step_|sess_|inst_|op_|proj_)[0-9a-f]{32}$/.test(handle);
}
