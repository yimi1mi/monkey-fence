// T0 真实验收配置:globalSetup 启动真实 mf-workbench(OS 端口 + 真实
// nonce + 隔离数据目录 + 验收 mock Agent),用例经 e2e/fixtures/helpers
// 引导。e2e/sketches/ 下是尚未接线的骨架,待对应阶段迁移。
import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  outputDir: "./test-results/main",
  testMatch: /.*\.spec\.ts$/,
  testIgnore: /.*[\\/]sketches[\\/].*|core-restart[.]spec[.]ts/,
  timeout: 180_000,
  expect: { timeout: 15_000 },
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  reporter: [["list"]],
  globalSetup: "./e2e/fixtures/global-setup.ts",
  use: {
    browserName: "chromium",
    headless: true,
    screenshot: { mode: "only-on-failure", fullPage: true },
    trace: "retain-on-failure",
  },
});
