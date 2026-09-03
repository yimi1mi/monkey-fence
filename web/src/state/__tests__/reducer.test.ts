// 事件 reducer 契约(T8a):cursor 单调/epoch 旋转 resync/重复倒退忽略/
// 未知 critical resync/已知非 critical 忽略/snapshot 基线重置。
import { test } from "node:test";
import assert from "node:assert/strict";
import { reduceEvents, reduceResync, reduceSnapshot } from "../reducer.ts";

function state(epoch: string, seq: string) {
  return { cursor: { streamEpoch: epoch, throughSeq: seq }, feed: [], needsYou: 0, activeRuns: 0 };
}

function event(type: string, seq: string, critical: boolean, epoch = "ep1") {
  return {
    schema: "mf.event.v1" as const,
    type,
    critical,
    stream_epoch: epoch,
    seq,
    data: {},
  };
}

test("snapshot resets the projection baseline", () => {
  const before = state("ep-old", "99");
  const snapshot = {
    schema: "mf.snapshot.v1",
    server_instance_id: "srv",
    cursor: { stream_epoch: "ep2", through_seq: "100" },
    data: {},
  };
  const after = reduceSnapshot(before, snapshot as never);
  assert.equal(after.cursor.streamEpoch, "ep2");
  assert.equal(after.cursor.throughSeq, "100");
  assert.equal(after.feed.length, 0);
});

test("snapshot keeps feed history (display layer survives refresh)", () => {
  const withFeed = { ...state("ep1", "0"), feed: [{ type: "workflow.replace", seq: "3", critical: true, at: 1 }] };
  const snapshot = {
    schema: "mf.snapshot.v1",
    server_instance_id: "srv",
    cursor: { stream_epoch: "ep2", through_seq: "10" },
    data: {},
  };
  const after = reduceSnapshot(withFeed, snapshot as never);
  assert.equal(after.feed.length, 1, "feed 是显示历史,不因快照清空");
});

test("snapshot seeds authoritative counters for event deltas", () => {
  const snapshot = {
    schema: "mf.snapshot.v1",
    server_instance_id: "srv",
    cursor: { stream_epoch: "ep2", through_seq: "100" },
    data: { needs_you_count: 2, active_workflow_runs: 3 },
  };
  const seeded = reduceSnapshot(state("ep1", "0"), snapshot as never);
  assert.equal(seeded.needsYou, 2);
  assert.equal(seeded.activeRuns, 3);
  // 事件增量叠加在快照计数之上(无双计;epoch 与快照一致)
  const { state: after } = reduceEvents(seeded, [
    event("workflow_run.started", "101", true, "ep2"),
  ]);
  assert.equal(after.activeRuns, 4);
  assert.equal(after.needsYou, 2);
});

test("seq must advance strictly; duplicates and regressions are ignored", () => {
  const { state: after } = reduceEvents(state("ep1", "5"), [
    event("workspace.resync", "5", true),
    event("workspace.resync", "4", true),
    event("workspace.resync", "6", true),
  ]);
  assert.equal(after.cursor.throughSeq, "6");
  assert.equal(after.feed.length, 1, "重复/倒退不产生 feed 项");
});

test("epoch rotation forces resync", () => {
  const { resyncRequired } = reduceEvents(state("ep1", "5"), [
    event("workspace.resync", "6", true, "ep2"),
  ]);
  assert.equal(resyncRequired, true, "跨 epoch 不拼接");
});

test("unknown critical events force resync; unknown optional ignored", () => {
  const critical = reduceEvents(state("ep1", "0"), [
    event("future.critical.thing", "1", true),
  ]);
  assert.equal(critical.resyncRequired, true);
  const optional = reduceEvents(state("ep1", "0"), [
    event("future.hint.thing", "1", false),
  ]);
  assert.equal(optional.resyncRequired, false);
  assert.equal(optional.state.feed.length, 1, "非 critical 未知事件可显示");
});

test("resync clears cursor awaiting fresh snapshot", () => {
  const after = reduceResync(state("ep1", "88"));
  assert.equal(after.cursor.streamEpoch, "");
  assert.equal(after.cursor.throughSeq, "0");
});

test("server v1 event whitelist does not force resync", () => {
  // workflow.create 实际发布 replace 全量投影 + collection 变更(critical)
  const { state: after, resyncRequired } = reduceEvents(state("ep1", "5"), [
    event("workflow.replace", "6", true),
    event("project.workflow_collection_changed", "7", true),
    event("workflow.rename", "8", true),
    event("workflow.delete", "9", true),
  ]);
  assert.equal(resyncRequired, false, "v1 白名单事件已知,无需全量");
  assert.equal(after.feed.length, 4);
});
