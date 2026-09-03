// UUIDv7 生成(kernel CommandId 强校验 v7:crypto.randomUUID 的 v4
// 会被 invalid_envelope 拒绝)。48-bit unix 毫秒前缀,其余 crypto 随机。

export function uuidv7(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  const view = new DataView(bytes.buffer);
  const now = Date.now();
  view.setUint32(0, Math.floor(now / 2 ** 16));
  view.setUint16(4, now % 2 ** 16);
  view.setUint16(6, (0x7 << 12) | (view.getUint16(6) & 0x0fff));
  view.setUint16(8, (0b10 << 14) | (view.getUint16(8) & 0x3fff));
  const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
