// 事件 reducer(T8a,Issue #52):snapshot 基线 + mf-workflow.v1 事件
// 的 cursor/epoch/resync 语义。不复制领域状态机——只维护投影位置与
// 待显示摘要;未知 critical 事件 → 全量 resync(4409 语义)。

import type { EventEnvelope, SnapshotEnvelope } from "../api/protocol.ts";

export interface ProjectionState {
  /** 当前投影基线(snapshot 的 cursor)。 */
  cursor: { streamEpoch: string; throughSeq: string };
  /** 已应用的领域事件摘要(显示层;最新在前,有界)。 */
  feed: Array<{ type: string; seq: string; critical: boolean; at: number }>;
  needsYou: number;
  activeRuns: number;
}

export type ReducerAction =
  | { kind: "snapshot"; snapshot: SnapshotEnvelope }
  | { kind: "events"; events: EventEnvelope[] }
  | { kind: "resync" };

export const PROJECTION_FEED_LIMIT = 200;

/** snapshot 基线(刷新后的起点;丢弃旧投影)。 */
export function reduceSnapshot(
  state: ProjectionState,
  snapshot: SnapshotEnvelope,
): ProjectionState {
  return {
    cursor: {
      streamEpoch: snapshot.cursor.stream_epoch,
      throughSeq: snapshot.cursor.through_seq,
    },
    feed: [],
    needsYou: 0,
    activeRuns: 0,
  };
}

/** 事件应用:seq 单调、epoch 一致;倒退/跨 epoch 忽略;未知 critical
 *  事件要求调用方全量 resync(返回标记)。 */
export function reduceEvents(
  state: ProjectionState,
  events: EventEnvelope[],
): { state: ProjectionState; resyncRequired: boolean } {
  let resyncRequired = false;
  let { streamEpoch, throughSeq } = state.cursor;
  let { needsYou, activeRuns } = state;
  let feed = state.feed;

  for (const event of events) {
    // epoch 旋转:跨 epoch 不拼接(全量 resync)
    if (streamEpoch !== "" && event.stream_epoch !== streamEpoch) {
      resyncRequired = true;
      continue;
    }
    // seq 必须严格前进;倒退/重复忽略(无重复或倒退)
    if (BigInt(event.seq) <= BigInt(throughSeq === "" ? "0" : throughSeq)) {
      continue;
    }
    streamEpoch = event.stream_epoch;
    throughSeq = event.seq;
    // 领域摘要(不解释合法性,只投影已知形态)
    if (event.type === "workflow_run.needs_you") needsYou += 1;
    if (event.type === "workflow_run.started") activeRuns += 1;
    if (event.type === "workflow_run.settled" || event.type === "workflow_run.cancelled") {
      activeRuns = Math.max(0, activeRuns - 1);
      if (event.type === "workflow_run.settled") needsYou = Math.max(0, needsYou - 1);
    }
    // 未知 critical → 必须 resync;未知非 critical 可忽略(v1 additive)
    if (event.critical && !KNOWN_CRITICAL_EVENTS.has(event.type)) {
      resyncRequired = true;
    }
    feed = [
      { type: event.type, seq: event.seq, critical: event.critical, at: Date.now() },
      ...feed,
    ].slice(0, PROJECTION_FEED_LIMIT);
  }

  return {
    state: {
      cursor: { streamEpoch, throughSeq },
      feed,
      needsYou,
      activeRuns,
    },
    resyncRequired,
  };
}

/** 已知 critical 事件集(v1 冻结;新增必须 additive optional)。 */
const KNOWN_CRITICAL_EVENTS = new Set([
  "workflow_run.needs_you",
  "workflow_run.started",
  "workflow_run.settled",
  "workflow_run.cancelled",
  "workspace.resync",
]);

/** resync:丢弃投影,等待下一次 snapshot 基线。 */
export function reduceResync(state: ProjectionState): ProjectionState {
  return {
    ...state,
    cursor: { streamEpoch: "", throughSeq: "0" },
  };
}
