// T0 真实验收 fixture:启动真实 mf-workbench(Core + Web 面)。
//
// - 端口由操作系统分配(MF_WEB_PORT=0),从 stdout 的 WEB_ENTRY= 行
//   解析真实入口 URL(带一次性 bootstrap nonce)。
// - 数据目录全部隔离到本次运行的临时目录:MF_SERVICE_DB /
//   MF_CATALOG_DB / MF_CATALOG_V2_DB 重定向;TEMP/TMP 重定向使验收
//   沙箱项目(std::env::temp_dir()/mf-workbench-acceptance-project,
//   项目库在其 .mf-agent/ 下)也落在同一临时目录。
// - MF_WEB_ACCEPTANCE=1:任意 agent_instance_id 解析为 mock echo CLI
//   (AcceptanceMockCatalog,不调用真实收费模型),并开放
//   POST /acceptance/new-nonce 为每个用例签发新 bootstrap nonce。
//
// 注意:CoreOwnerLock(机器级命名互斥)是设计不变量,测试运行期间
// 本机不能同时驻留另一个 Core 实例。
import { spawn } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

/** Core 进程输出落盘点(排查派发/结算失败)。 */
export const coreLogPath = join(repoRoot, "web", "test-results", "mf-core.log");

export interface CoreHandle {
  baseUrl: string;
  entryUrl: string;
  stop(): Promise<void>;
}

/** mf-workbench 可执行文件:MF_WORKBENCH_BIN 显式指定,否则用工作区构建产物。 */
export function mfWorkbenchBin(): string {
  const explicit = process.env.MF_WORKBENCH_BIN;
  if (explicit) return explicit;
  const exe = process.platform === "win32" ? "mf-workbench.exe" : "mf-workbench";
  return join(repoRoot, "target", "debug", exe);
}

/**
 * 启动真实 Core。解析到 WEB_ENTRY 后返回句柄;进程异常退出时抛出
 * 并附带 stdout/stderr 尾部,便于定位(owner lock 冲突、dist 缺失等)。
 */
export async function startCore(): Promise<CoreHandle> {
  const bin = mfWorkbenchBin();
  if (!existsSync(bin)) {
    throw new Error(
      `未找到 mf-workbench:${bin}(先 cargo build -p mf-web --bin mf-workbench,或设 MF_WORKBENCH_BIN)`,
    );
  }
  const dist = join(repoRoot, "web", "dist");
  if (!existsSync(join(dist, "index.html"))) {
    throw new Error(`未找到 ${dist}/index.html(先在 web/ 下 npm run build)`);
  }
  const dataDir = mkdtempSync(join(tmpdir(), "mf-e2e-"));
  const child = spawn(bin, [], {
    cwd: repoRoot,
    windowsHide: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      MF_WEB_DIST: dist,
      MF_WEB_PORT: "0",
      MF_WEB_ACCEPTANCE: "1",
      MF_SERVICE_DB: join(dataDir, "service-v1.db"),
      MF_CATALOG_DB: join(dataDir, "catalog-v1.db"),
      MF_CATALOG_V2_DB: join(dataDir, "catalog-v2.db"),
      MF_CORE_INSTANCE_DIR: dataDir,
      TEMP: dataDir,
      TMP: dataDir,
    },
  });
  let stdout = "";
  let stderr = "";
  mkdirSync(dirname(coreLogPath), { recursive: true });
  const tee = (chunk: string) => {
    stdout += chunk;
    try {
      appendFileSync(coreLogPath, chunk);
    } catch {
      /* 日志尽力而为 */
    }
  };
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk: string) => tee(chunk));
  child.stderr.on("data", (chunk: string) => {
    stderr += chunk;
    try {
      appendFileSync(coreLogPath, chunk);
    } catch {
      /* 日志尽力而为 */
    }
  });

  const entryUrl = await new Promise<string>((resolveEntry, rejectEntry) => {
    const startupTimeout = setTimeout(() => {
      rejectEntry(new Error(`mf-workbench 启动超时\nstdout:\n${stdout}\nstderr:\n${stderr}`));
    }, 120_000);
    let outBuffer = "";
    child.stdout.on("data", (chunk: string) => {
      const lines = (outBuffer + chunk).split(/\r?\n/);
      outBuffer = lines.pop() ?? "";
      for (const line of lines) {
        const match = /^WEB_ENTRY=(\S+)$/.exec(line.trim());
        if (match) {
          clearTimeout(startupTimeout);
          resolveEntry(match[1]);
        }
      }
    });
    child.on("exit", (code) => {
      clearTimeout(startupTimeout);
      rejectEntry(
        new Error(`mf-workbench 提前退出(code=${code})\nstdout:\n${stdout}\nstderr:\n${stderr}`),
      );
    });
  });
  child.removeAllListeners("exit");

  const baseUrl = new URL(entryUrl).origin;
  // HTTP 就绪探活:acceptance nonce 重签端点应答即说明路由已上线
  await newNonce(baseUrl);

  return {
    baseUrl,
    entryUrl,
    async stop() {
      if (child.exitCode !== null) return;
      if (process.platform === "win32") {
        spawn("taskkill", ["/pid", String(child.pid), "/T", "/F"], { stdio: "ignore" });
      } else {
        child.kill("SIGTERM");
      }
      await new Promise<void>((exited) => child.once("exit", () => exited()));
      rmSync(dataDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
    },
  };
}

/** 验收模式重签一个全新 bootstrap nonce(每个用例独立引导)。 */
export async function newNonce(baseUrl: string): Promise<string> {
  const response = await fetch(`${baseUrl}/acceptance/new-nonce`, {
    method: "POST",
    headers: { origin: baseUrl },
  });
  if (!response.ok) throw new Error(`acceptance/new-nonce → ${response.status}`);
  const body = (await response.json()) as { nonce: string };
  return body.nonce;
}
