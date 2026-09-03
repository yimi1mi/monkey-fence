// UUIDv7 契约:version/variant 位正确、时间前缀单调不减。
import { test } from "node:test";
import assert from "node:assert/strict";
import { uuidv7 } from "../uuid.ts";

test("uuidv7 carries version 7 and RFC variant nibbles", () => {
  for (let index = 0; index < 32; index += 1) {
    const value = uuidv7();
    assert.match(value, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  }
});

test("uuidv7 timestamp prefix is monotonic non-decreasing", () => {
  let previous = 0;
  for (let index = 0; index < 8; index += 1) {
    const hex = uuidv7().replaceAll("-", "").slice(0, 12);
    const millis = parseInt(hex, 16);
    assert.ok(millis >= previous, `毫秒前缀单调:${millis} >= ${previous}`);
    previous = millis;
  }
});
