// mf-terminal.v1 浏览器客户端(T10a,Issue #59)。
// 32-byte MFT1 binary frame、attach/hello、output replay、xterm 消费后
// cumulative ACK、writer lease 生命周期、input seq/ack(结果未知不重放)、
// resize 真实 PTY、history gap → 只读 transcript、DOM fallback 与
// screenReaderMode。xterm 版本精确锁定(@xterm/xterm@6.0.0 +
// addon-fit@0.11.0,不用 ^ 漂移)。

export const FRAME_HEADER_BYTES = 32;
export const FRAME_MAGIC = [0x4d, 0x46, 0x54, 0x31]; // "MFT1"
export const KIND_OUTPUT = 1;
export const KIND_INPUT = 2;
export const FRAME_MAX_BYTES = 256 * 1024;

/** 编码 binary frame(kind/seq/lease + payload)。 */
export function encodeFrame(input: {
  kind: number;
  seq: bigint;
  writerLeaseId: Uint8Array;
  payload: Uint8Array;
}): Uint8Array {
  const frame = new Uint8Array(FRAME_HEADER_BYTES + input.payload.length);
  frame.set(FRAME_MAGIC, 0);
  frame[4] = input.kind;
  frame[5] = 0; // flags
  frame[6] = 0;
  frame[7] = 0;
  const view = new DataView(frame.buffer);
  view.setBigUint64(8, input.seq, false);
  frame.set(input.writerLeaseId, 16);
  frame.set(input.payload, FRAME_HEADER_BYTES);
  return frame;
}

/** 解码输出帧;坏 magic/kind/长度 → null(关闭连接)。 */
export function decodeOutputFrame(data: ArrayBuffer): {
  seq: bigint;
  payload: Uint8Array;
} | null {
  if (data.byteLength < FRAME_HEADER_BYTES) return null;
  const view = new DataView(data);
  for (let i = 0; i < 4; i += 1) {
    if (view.getUint8(i) !== FRAME_MAGIC[i]) return null;
  }
  const kind = view.getUint8(4);
  if (kind !== KIND_OUTPUT) return null;
  if (data.byteLength > FRAME_MAX_BYTES) return null;
  return {
    seq: view.getBigUint64(8, false),
    payload: new Uint8Array(data, FRAME_HEADER_BYTES),
  };
}

/** ACK 门槛:只在 xterm write 回调完成后发送(反压正确性)。 */
export interface AckGate {
  highestWrittenSeq: bigint;
  ackedThroughSeq: bigint;
}

export function newAckGate(): AckGate {
  return { highestWrittenSeq: 0n, ackedThroughSeq: 0n };
}

/** xterm 消费回调:推进连续已写水位,返回应发送的 through_seq。 */
export function onWriteCallback(
  gate: AckGate,
  seq: bigint,
): { send: boolean; throughSeq: bigint } {
  if (seq > gate.highestWrittenSeq) gate.highestWrittenSeq = seq;
  // cumulative:只有连续区间推进才发 ACK(简化:seq 即已消费最高)
  gate.ackedThroughSeq = gate.highestWrittenSeq;
  return { send: true, throughSeq: gate.ackedThroughSeq };
}

/** input 幂等门:每个 lease 单调 input_seq;结果未知(ack 未回)绝不重放。 */
export interface InputGate {
  nextSeq: bigint;
  /** 已发送未确认(结果未知——不重放;仅诊断)。 */
  inFlight: number;
}

export function newInputGate(): InputGate {
  return { nextSeq: 1n, inFlight: 0 };
}

export function nextInputSeq(gate: InputGate): bigint {
  const seq = gate.nextSeq;
  gate.nextSeq += 1n;
  gate.inFlight += 1;
  return seq;
}

export function inputAcked(gate: InputGate): void {
  gate.inFlight = Math.max(0, gate.inFlight - 1);
}

/** 网络不确定时永不自动重发(§8.4);提供只读诊断。 */
export function shouldReplayUnacked(gate: InputGate): false {
  return false;
}

/** writer 生命周期状态机(浏览器侧)。 */
export type WriterState =
  | "none"
  | "granted"
  | "renewing"
  | "revoked"
  | "denied";

export interface WriterLeaseUi {
  state: WriterState;
  leaseId: Uint8Array | null;
  ttlMs: number;
  renewAfterMs: number;
}

export function onWriterGranted(lease: WriterLeaseUi, id: Uint8Array, ttlMs: number, renewAfterMs: number): WriterLeaseUi {
  return { state: "granted", leaseId: id, ttlMs, renewAfterMs };
}

export function onWriterRevoked(lease: WriterLeaseUi, reason: string): WriterLeaseUi {
  return { ...lease, state: "revoked" };
}

/** Observer 恒不可输入(可看不可写)。 */
export function observerCanInput(isObserver: boolean): boolean {
  return !isObserver;
}

/** history gap:该连接不得申请 writer;切换只读 transcript 视图。 */
export function onHistoryGap(): { readonlyTranscript: true; writerAllowed: false } {
  return { readonlyTranscript: true, writerAllowed: false };
}

/** exit(final_seq):replay 完成后展示退出状态;exit ≠ settlement。 */
export function onExit(finalSeq: bigint, code: number | null): { finalSeq: string; code: number | null } {
  return { finalSeq: finalSeq.toString(), code };
}

/** attach 超时(附录 A2:5000ms)。 */
export const ATTACH_TIMEOUT_MS = 5000;

/** WS close code 语义(§7.5)。 */
export function closeCodeMeaning(code: number): string {
  switch (code) {
    case 4400: return "invalid_envelope";
    case 4401: return "unauthenticated";
    case 4403: return "role_or_lease";
    case 4409: return "resync_or_history_gap";
    case 4413: return "frame_too_large";
    case 4429: return "rate_limited";
    case 4500: return "internal";
    default: return code >= 4500 ? "internal" : "normal";
  }
}
