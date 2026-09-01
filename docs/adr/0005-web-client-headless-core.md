# ADR 0005: Web 交互客户端与无界面 Core Service

状态: 已接受

MonkeyFence 的产品交互统一迁移到系统默认浏览器中的 Web Interaction Client；每个操作系统用户只运行一个跨项目、普通权限、无界面的 Rust Core Service，后者独占工作流、运行、插件、Secret 与 Agent Session 的权威状态。Web bundle 与 Core 同版本内嵌发布，浏览器与托盘都不拥有服务生命周期；托盘只做打开、摘要和安全退出，默认不自启动。

选择该边界是因为可视化 DAG、响应式 Inspector 与原生 CLI 终端在 Web 技术栈中更成熟，而继续扩展 GPUI 会把产品重点稀释到编辑器/VCS/桌面框架。我们拒绝继续以 GPUI 作为产品主界面、用 Electron/WebView 作为必需壳、让浏览器成为第二事实源，或在 GPUI/Web 并行期保留任一绕过 Core 命令/CAS 的写路径。

本 ADR 保留 ADR 0001 的 Task/VCS 生命周期解耦，但废止其中继续提供 Change Set、VCS panel 与 Delivery UI 的条款；这些能力不迁移到 Web。它保留 ADR 0002 的统一插件扩展缝隙，但以 Manifest v3 的 Agent Type、Provider Type、installer contributions 和 Root/elevated host 契约取代旧 `agents/pipelines`、Agent Profile 兼容词与“CLI 永远当前普通用户权限”。它只取代 ADR 0003 中“声明式 UI 由 GPUI 渲染”的技术表述：插件仍只能贡献版本化 Schema，由宿主 Web 控件渲染，不得注入任意 JavaScript/Web UI。ADR 0004 的工作流优先交互，以及上述 ADR 中未被逐条取代的 Task、微内核、Settlement、插件 pin 与 VCS 解耦语义继续有效。
