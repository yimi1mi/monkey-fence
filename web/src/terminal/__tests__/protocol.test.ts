// T10a 契约:帧编解码、ACK 门槛、input 门、writer 生命周期、gap/exit。
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  closeCodeMeaning,
  decodeOutputFrame,
  encodeFrame,
  inputAcked,
  newAckGate,
  newInputGate,
  nextInputSeq,
  onExit,
  onHistoryGap,
  onWriteCallback,
  onWriterGranted,
  onWriterRevoked,
  observerCanInput,
  shouldReplayUnacked,
} from "../protocol.ts";

const lease = new Uint8Array(16).fill(9);

test("frame round trip preserves seq and payload", () => {
  const frame = encodeFrame({ kind: 2, seq: 7n, writerLeaseId: lease, payload: new Uint8Array([104, 105]) });
  // 解 input 帧经 output 解码应失败(kind 校验)
  assert.equal(decodeOutputFrame(frame.buffer), null);
  const output = encodeFrame({ kind: 1, seq: 7n, writerLeaseId: new Uint8Array(16), payload: new Uint8Array([104, 105]) });
  const decoded = decodeOutputFrame(output.buffer)!;
  assert.equal(decoded.seq, 7n);
  assert.equal(decoded.payload[0], 104);
});

test("bad magic and short frames rejected", () => {
  const bad = new Uint8Array(40);
  bad.set([0x58, 0x46, 0x54, 0x31], 0);
  assert.equal(decodeOutputFrame(bad.buffer), null);
  assert.equal(decodeOutputFrame(new Uint8Array(8).buffer), null);
});

test("ack gate only advances after write callback", () => {
  const gate = newAckGate();
  const first = onWriteCallback(gate, 1n);
  assert.equal(first.send, true);
  assert.equal(first.throughSeq, 1n);
  // 未消费不推进
  assert.equal(gate.ackedThroughSeq, 1n);
});

test("input seq monotonic; unacked never replayed", () => {
  const gate = newInputGate();
  assert.equal(nextInputSeq(gate), 1n);
  assert.equal(nextInputSeq(gate), 2n);
  assert.equal(gate.inFlight, 2);
  inputAcked(gate);
  assert.equal(gate.inFlight, 1);
  assert.equal(shouldReplayUnacked(gate), false, "网络不确定不自动重放");
});

test("writer lifecycle transitions", () => {
  let writer = onWriterGranted({ state: "none", leaseId: null, ttlMs: 0, renewAfterMs: 0 }, lease, 10_000, 6_000);
  assert.equal(writer.state, "granted");
  writer = onWriterRevoked(writer, "takeover");
  assert.equal(writer.state, "revoked");
});

test("observer sees but cannot input", () => {
  assert.equal(observerCanInput(true), false);
  assert.equal(observerCanInput(false), true);
});

test("history gap blocks writer and switches to transcript", () => {
  const gap = onHistoryGap();
  assert.equal(gap.readonlyTranscript, true);
  assert.equal(gap.writerAllowed, false);
});

test("exit reports final seq; never settlement", () => {
  const exit = onExit(42n, 0);
  assert.equal(exit.finalSeq, "42");
  assert.equal(exit.code, 0);
});

test("close codes map to stable meanings", () => {
  assert.equal(closeCodeMeaning(4409), "resync_or_history_gap");
  assert.equal(closeCodeMeaning(4413), "frame_too_large");
  assert.equal(closeCodeMeaning(4429), "rate_limited");
});
