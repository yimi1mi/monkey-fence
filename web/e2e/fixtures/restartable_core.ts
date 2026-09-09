import { spawn, execFile, type ChildProcess } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve, dirname, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { mfWorkbenchBin } from "./core-bin.ts";

/** 只拥有本用例的进程与数据；重启不改变用户的默认 Core。 */
export class RestartableCore {
  proc: ChildProcess | null = null;
  readonly dataDir = mkdtempSync(join(tmpdir(), "mf-restart-"));
  baseUrl = "";
  entryUrl = "";

  async start(): Promise<void> {
    if (this.proc) throw new Error("fixture Core is already started");
    const root = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
    const dist = join(root, "web/dist");
    if (!existsSync(join(dist, "index.html"))) throw new Error("build web before restart E2E");
    const proc = spawn(mfWorkbenchBin(), [], { cwd: root, windowsHide: true, stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, MF_WEB_DIST: dist, MF_WEB_PORT: "0", MF_WEB_ACCEPTANCE: "1",
        MF_CORE_INSTANCE_DIR: this.dataDir, MF_SERVICE_DB: join(this.dataDir, "service.db"),
        MF_CATALOG_DB: join(this.dataDir, "catalog.db"), MF_CATALOG_V2_DB: join(this.dataDir, "catalog-v2.db"),
        TEMP: this.dataDir, TMP: this.dataDir } });
    this.proc = proc;
    let output = "";
    proc.stdout?.setEncoding("utf8"); proc.stderr?.setEncoding("utf8");
    try {
      this.entryUrl = await new Promise<string>((ok, fail) => {
        const timeout = setTimeout(() => fail(new Error("Core startup timeout")), 60000);
        const exited = (code: number | null) => { clearTimeout(timeout); fail(new Error(`Core exited ${code}: ${output.replace(/WEB_ENTRY=\S+/g, "WEB_ENTRY=<redacted>").slice(-3000)}`)); };
        proc.once("exit", exited);
        proc.once("error", (error) => { clearTimeout(timeout); fail(error); });
        proc.stderr?.on("data", (chunk: string) => { output = (output + chunk).slice(-10000); });
        let pending = "";
        proc.stdout?.on("data", (chunk: string) => {
          output = (output + chunk).slice(-10000); pending += chunk;
          const match = /^WEB_ENTRY=(\S+)\r?$/m.exec(pending);
          if (match) { clearTimeout(timeout); proc.removeListener("exit", exited); ok(match[1]); }
        });
      });
      this.baseUrl = new URL(this.entryUrl).origin;
      const deadline = Date.now() + 15000;
      while (true) {
        try { if ((await fetch(`${this.baseUrl}/`, { signal: AbortSignal.timeout(2000) })).ok) break; } catch { /* startup */ }
        if (Date.now() > deadline) throw new Error("Core HTTP did not become ready");
        await new Promise((done) => setTimeout(done, 100));
      }
    } catch (error) { await this.kill(); throw error; }
  }

  async kill(): Promise<void> {
    const proc = this.proc;
    if (!proc) return;
    this.proc = null;
    if (proc.exitCode !== null || proc.signalCode !== null) return;
    const exited = new Promise<void>((done, fail) => {
      const timeout = setTimeout(() => fail(new Error("fixture Core did not exit")), 15000);
      proc.once("exit", () => { clearTimeout(timeout); done(); });
    });
    if (process.platform === "win32") {
      await new Promise<void>((done, fail) => execFile("taskkill", ["/pid", String(proc.pid), "/T", "/F"],
        { windowsHide: true }, (error) => error && proc.exitCode === null ? fail(error) : done()));
    } else proc.kill("SIGKILL");
    await exited;
  }

  async restart(): Promise<void> { await this.kill(); await this.start(); }
  async dispose(): Promise<void> {
    await this.kill();
    const real = realpathSync(this.dataDir), temp = realpathSync(tmpdir());
    if (!real.startsWith(temp + sep) || !real.split(sep).pop()?.startsWith("mf-restart-")) throw new Error("fixture cleanup outside owned temp root");
    rmSync(real, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  }
}
