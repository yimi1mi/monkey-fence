// 协议 golden 对照(T7b):读取 Rust 侧生成的 fixtures,校验 TS 类型
// 形态一致(wire 冻结;node:test 运行,web 构建管线接入后自动执行)。
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { isValidHandle, type CommandEnvelope } from "../protocol.ts";

const fixtures = join(import.meta.dirname, "../../../crates/mf-web/tests/fixtures");

function readJson(kind: string, name: string): unknown {
  return JSON.parse(readFileSync(join(fixtures, kind, `${name}.json`), "utf8"));
}

test("golden command envelope matches TS shape", () => {
  const command = readJson("commands", "move_node") as CommandEnvelope;
  assert.equal(command.schema, "mf.command.v1");
  assert.equal(command.type, "workflow.move_node");
  assert.equal(typeof command.controller_lease_epoch, "string");
  assert.ok(isValidHandle(command.target.handle));
  assert.equal(command.expected.length, 1);
  assert.equal(command.expected[0].presentation_revision, "91");
});

test("golden problems keep stable codes and retry", () => {
  const problem = readJson("problems", "revision_conflict") as {
    code: string; retry: string;
  };
  assert.equal(problem.code, "revision_conflict");
  assert.equal(problem.retry, "after_resync");
});

test("every command fixture validates handles and string u64", () => {
  for (const file of readdirSync(join(fixtures, "commands"))) {
    const parsed = JSON.parse(
      readFileSync(join(fixtures, "commands", file), "utf8"),
    ) as Record<string, unknown>;
    const candidate = parsed as unknown as CommandEnvelope;
    if (candidate.schema === "mf.command.v1") {
      assert.ok(
        isValidHandle(candidate.target.handle),
        `${file}: target 必须是 opaque handle`,
      );
      assert.equal(
        typeof candidate.controller_lease_epoch,
        "string",
        `${file}: u64 必须字符串化`,
      );
    }
  }
});
