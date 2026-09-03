// 跨平台测试发现:npm script 在 Windows 走 cmd(find/glob 不可靠),
// 这里显式收集 src 下全部 *.test.ts 交给 node --test。
import { readdirSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

function collect(dir) {
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...collect(full));
    } else if (entry.name.endsWith(".test.ts")) {
      out.push(full);
    }
  }
  return out;
}

const files = collect("src");
if (files.length === 0) {
  console.error("未发现任何 *.test.ts");
  process.exit(1);
}
const result = spawnSync(process.execPath, ["--test", ...files], { stdio: "inherit" });
process.exit(result.status ?? 1);
