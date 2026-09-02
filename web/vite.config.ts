import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // 同源 Core 服务:开发代理到 loopback(生产内嵌)
  server: { proxy: { "/api": "http://127.0.0.1:8765" } },
});
