// Playwright globalSetup:整个测试运行只启动一个真实 Core(mf-workbench),
// 把入口信息放进环境变量供各 worker 使用;结束时停进程、清临时目录。
import { startCore } from "./core.ts";

export default async function globalSetup() {
  const core = await startCore();
  process.env.MF_E2E_BASE = core.baseUrl;
  process.env.MF_E2E_ENTRY = core.entryUrl;
  return async () => {
    await core.stop();
  };
}
