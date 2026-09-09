// 真实进程重启验收:无全局 Core(globalSetup)——该 spec 自行启停隔离的
// mf-workbench 进程(机器级 owner 互斥不允许两实例并存)。
import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  outputDir: "./test-results/restart",
  testMatch: /core-restart\.spec\.ts$/,
  timeout: 240_000,
  expect: { timeout: 15_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  use: {
    browserName: "chromium",
    headless: true,
    screenshot: { mode: "only-on-failure", fullPage: true },
    trace: "retain-on-failure",
  },
});
