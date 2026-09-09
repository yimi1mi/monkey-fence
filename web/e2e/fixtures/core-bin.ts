// mf-workbench 可执行文件定位(供独立启停的进程重启用例复用)。
import { existsSync } from "node:fs";
import { join, resolve } from "node:path";

export function mfWorkbenchBin(): string {
  const explicit = process.env.MF_WORKBENCH_BIN;
  if (explicit) return explicit;
  const repoRoot = resolve(import.meta.dirname ?? ".", "..", "..", "..");
  const exe = process.platform === "win32" ? "mf-workbench.exe" : "mf-workbench";
  const path = join(repoRoot, "target", "debug", exe);
  if (!existsSync(path)) {
    throw new Error(
      `未找到 mf-workbench:${path}(先 cargo build -p mf-web --bin mf-workbench)`,
    );
  }
  return path;
}
