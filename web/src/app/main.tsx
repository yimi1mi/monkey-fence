// Workbench 入口(T8a)。仅嵌入 bundle;launcher 不打开(GPUI 唯一入口)。
import { createRoot } from "react-dom/client";
import { WorkbenchShell } from "../workbench/shell.tsx";

const mount = document.getElementById("workbench");
if (mount) {
  // bootstrap 上下文由首屏 nonce exchange 注入(此前入口隐藏)
  createRoot(mount).render(<WorkbenchShell client={placeholderClient()} />);
}

function placeholderClient(): never {
  throw new Error("bootstrap 未开放(web_workbench feature 关闭;T8 gate)");
}
