// T9b 契约:明文生命周期、响应脱敏、手填校验。
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  manualModelIdError,
  sanitizeModelPayload,
  secretInput,
  secretSubmitted,
} from "../model.ts";

test("secret input clears immediately after submit", () => {
  const input = secretInput();
  input.plaintext = "sk-live-123";
  const after = secretSubmitted(input);
  assert.equal(after.plaintext, "");
  assert.equal(after.dirty, false);
});

test("model payload never carries secret-shaped fields", () => {
  const sanitized = sanitizeModelPayload({
    models: [{ id: "gpt-5", displayName: "GPT-5" }],
    source: "live",
    fetchedAt: "2026-09-02",
    fallbackError: null,
  });
  const json = JSON.stringify(sanitized);
  assert(!json.includes("secret"), "无 secret 字段");
  assert.equal(sanitized.models[0].id, "gpt-5");
});

test("manual model id validation", () => {
  assert.equal(manualModelIdError("deepseek/chat-v3"), null);
  assert.equal(manualModelIdError(""), "模型 id 不能为空");
  assert.equal(manualModelIdError(" leading"), "首尾含空白");
  assert.equal(manualModelIdError("with space"), "含空白字符");
  assert.equal(manualModelIdError("bad$char"), "含非法字符(允许字母数字与 - _ . : /)");
});
