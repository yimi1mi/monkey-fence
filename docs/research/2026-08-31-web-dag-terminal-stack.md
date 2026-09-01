# Web DAG 编辑器与原生 Agent 终端技术底座研究

> 后续决策覆盖：本文的 checkpoint 方案是研究候选，不是 v1 契约。`2026-09-01-web-api-terminal-protocol-v1.md` 已决定 Terminal v1 不发送 checkpoint；history gap 改为关闭 live WS、展示 durable read-only transcript，并由用户显式重启 Session。本文关于 React Flow、xterm、PTY、三数据面和性能验证的其余结论继续有效。

- 日期：2026-08-31
- 对应 ticket：[GitHub Issue #3](https://github.com/yimi1mi/monkey-fence/issues/3)
- 范围：Web 作为 MonkeyFence 的交互客户端；Rust 保留工作流、运行、智能体会话、PTY 与安全边界
- 来源规则：只使用官方文档、官方仓库/源码、标准或本仓库当前源码

## 结论先行

建议采用以下底座，并通过小型技术原型再锁定具体版本：

| 层 | 推荐 | 结论 |
| --- | --- | --- |
| Web UI | React + TypeScript | 与节点编辑器、终端、状态管理和浏览器测试生态匹配 |
| DAG 交互 | `@xyflow/react`（React Flow 12） | 首选；节点拖拽、端口连线、重连、双击、键盘与屏幕阅读器支持最贴近 MonkeyFence |
| 自动布局 | `@dagrejs/dagre`，保留 `LayoutEngine` 接口 | 当前项目工作流是普通 DAG，先用简单快速的 Dagre；出现复合节点、跨层边或正交路由需求时换 ELK |
| Web 终端 | `@xterm/xterm` + `@xterm/addon-fit`；WebGL 可选并带 DOM fallback | 首选；浏览器只做终端模拟，真实 Codex / Claude Code / GLM CLI 继续运行在 Rust 拥有的 PTY 中 |
| Rust Web 服务 | `axum` + Tokio | 提供 Snapshot/Command HTTP API、工作流事件 WebSocket、独立终端 WebSocket |
| PTY | 复用 MonkeyFence 当前 `pty_spawn` 与 `SessionRegistry` | 不引入另一套 PTY crate；补齐 resize、原始 VT 字节广播、顺序号、背压、重连和 writer lease |

关键原则：

1. Web 不解释 `/model`、`/skills`、`/compact` 或任何 Agent 命令。它把键盘产生的原始输入送入真实 CLI；Slash command、Skill、审批和 TUI 状态机全部由 CLI 自己处理。Codex 官方文档明确说明 Slash popup、命令排队和 `/skills` 都发生在 CLI composer 中；Claude Code 也把内置命令和技能统一放在终端内的 `/` 命令入口中。[Codex developer commands](https://learn.chatgpt.com/docs/developer-commands?surface=cli)；[Claude Code commands](https://code.claude.com/docs/en/commands)
2. React Flow 不是领域模型，也不是执行引擎。Rust 中的项目工作流、Pipeline Revision、工作流运行和 Settlement 仍是唯一事实源；Web 只持有带 revision 的编辑投影。
3. 浏览器刷新只 detach，不结束智能体会话。Rust 进程持有 PTY、子进程树和可重放输出；服务进程崩溃后的“继续同一 PTY”不应承诺，跨平台没有可移植的进程重附着语义。
4. 终端数据面与普通工作流事件面分离。高吞吐 VT 流不能阻塞“需要你”、步骤状态或结算事件。

## 1. Web DAG 技术比较

### 1.1 React Flow：推荐

React Flow 12 的包名是 `@xyflow/react`，官方仓库采用 MIT 许可；截至研究日，官方 Releases 显示 `@xyflow/react@12.11.0`，仓库仍持续维护。[官方仓库与许可](https://github.com/xyflow/xyflow)；[官方 Releases](https://github.com/xyflow/xyflow/releases)

与 MonkeyFence 直接相关的能力：

- `ReactFlow` 原生提供节点拖动、选择、平移缩放、端口连接和键盘快捷键；节点与边默认可聚焦。[ReactFlow API](https://reactflow.dev/api-reference/react-flow)
- `<Handle />` 提供 source/target 端口、端口级 connectability 和 `isValidConnection`；官方建议把全局合法性检查放到 `ReactFlow` 级别以降低重复计算成本。[Handle API](https://reactflow.dev/api-reference/components/handle)
- 边可重连，且有完整的 connect/reconnect start/end 事件，适合在 UI 侧做乐观反馈后把命令交给 Rust 权威校验。[Reconnect edge](https://reactflow.dev/examples/edges/reconnect-edge)；[Connection events](https://reactflow.dev/examples/interaction/connection-events)
- 节点库拖入画布需要自行接 Pointer Events 或 HTML DnD；官方明确提示 HTML DnD 对触屏支持不好，Pointer Events 可统一鼠标与触控。[Drag and Drop](https://reactflow.dev/examples/interaction/drag-and-drop)
- `onNodeDoubleClick` 是正式 API，恰好可承载“运行态双击 Step 打开对应智能体会话面板”。[ReactFlow node events](https://reactflow.dev/api-reference/react-flow)
- 默认支持 Tab 遍历、Enter/Space 选择、方向键移动、自动把焦点节点移入视口、ARIA live 更新和可本地化提示，官方把这些能力定位为帮助满足 WCAG 2.1 AA。[Accessibility](https://reactflow.dev/learn/advanced-use/accessibility)

限制与处理：

- React Flow 不内置布局引擎。官方布局指南把 Dagre 定位为简单、偏速度的 directed graph 布局，把 ELK 定位为更复杂但支持边路由、复合图的布局引擎。[Layouting overview](https://reactflow.dev/learn/layouting/layouting)
- DOM/SVG 节点在大图上会受到 React 重渲染和复杂 CSS 影响。官方要求 memoize 节点/回调、避免订阅完整 nodes/edges、折叠大树、减少阴影/渐变/动画。[Performance](https://reactflow.dev/learn/advanced-use/performance)
- 一些官方示例（undo/redo、helper lines、自动布局 hook 等）属于 React Flow Pro，而核心库仍是 MIT。实现时只能借鉴公开 API，不能把 Pro 示例源码当作开源依赖。[Examples index](https://reactflow.dev/examples)

结论：MonkeyFence 的图通常是几十到数百个富节点卡片，而不是十万级网络可视化；定制卡片、交互完整度和无障碍优先级高于极限节点数，React Flow 的取舍最合适。

### 1.2 Rete.js：能力强，但不推荐作为第一选择

Rete.js v2 是面向视觉编程的框架，除节点/连线渲染外还包含 dataflow/control-flow 处理概念，并通过多个插件组合编辑器；核心为 MIT，官方 core 最新 release 为 `2.0.6`（2025-06-30）。[官方介绍](https://retejs.org/docs/)；[官方仓库/Releases](https://github.com/retejs/rete/releases)

优点：

- 连接创建/删除、选择、缩放和自定义 React 节点已有插件。[Connection plugin](https://retejs.org/docs/api/rete-connection-plugin/)；[React renderer](https://retejs.org/docs/guides/renderers/react/)
- 官方 auto-arrange 插件直接使用 ELK，并支持动画应用布局结果。[Auto-arrange API](https://retejs.org/docs/api/rete-auto-arrange-plugin/)
- 官方性能指南提供按缩放级别降低节点细节（LOD）的方案。[Performance](https://retejs.org/docs/best-practices/performance/)

问题：

- Rete 的编辑器/引擎/plugin graph 容易形成第二套执行模型，与 Rust Orchestrator 的唯一事实源重叠；MonkeyFence 只需要视图编辑层，不需要浏览器执行 dataflow。
- 插件组合比 React Flow 更重，交互控件需要处理 area 捕获 pointer 事件等细节。[React controls](https://retejs.org/docs/guides/renderers/react/)
- 官方没有像 React Flow 那样给出明确的 WCAG、键盘遍历和 screen-reader 合约；必须自行做无障碍设计与验证。
- 许可并非所有官方插件都同为 MIT。官方许可页明确指出 `rete-structures` 和 `rete-scopes-plugin` 是 CC-BY-NC-SA-4.0，不能用于商业用途。[Rete licensing](https://retejs.org/docs/licensing/)

仅当未来决定在浏览器内也运行一套可扩展的视觉编程引擎时，再重新评估 Rete。

### 1.3 Cytoscape.js：适合大图分析，不适合富工作流编辑器

Cytoscape.js 是 MIT 许可的 graph analysis/visualization 库，使用 Canvas，官方仓库 2026 年仍连续发布，Releases 已显示 3.34 系列。[官方文档](https://js.cytoscape.org/)；[官方 Releases](https://github.com/cytoscape/cytoscape.js/releases)

优点是图算法、布局、批量更新与 Canvas 大图性能；官方文档也给出 pixel ratio、批处理、隐藏交互时的边、texture viewport 等性能选项。[Cytoscape factsheet/performance](https://js.cytoscape.org/)

但连接 handles、HTML 节点、edge editing、grid guide 等工作流编辑能力主要来自额外扩展；官方扩展列表本身就把这些列为独立 UI extensions。[Cytoscape UI extensions](https://js.cytoscape.org/)。Canvas 也无法天然获得每个富节点的 DOM 语义和焦点行为。

结论：若产品将来增加只读的超大运行依赖总览，可单独考虑 Cytoscape；不适合作为主编辑器。

### 1.4 JointJS：成熟但许可与商业功能边界更复杂

JointJS 4.3 是 SVG 图编辑框架，核心采用 MPL-2.0，官方 2026 年仍有 4.3.1 release；它支持 ports、links、drag/reconnect 和 React integration。[官方仓库与许可](https://github.com/clientIO/joint)；[官方 Releases](https://github.com/clientIO/joint/releases)；[Interactivity](https://docs.jointjs.com/react/features/interactivity/)

开源 directed graph layout 已可用于 React；但 element palette、完整 Diagram、virtual rendering、spatial index、undo/redo 等大量产品化能力位于 JointJS+ 商业层。[Automatic layouts](https://docs.jointjs.com/react/features/automatic-layouts/)；[JointJS 与 JointJS+ 边界](https://docs.jointjs.com/)

结论：它适合愿意购买商业套件的大型通用制图产品；对 MonkeyFence 当前范围，React Flow 的许可和心智成本更清楚。

### 1.5 DAG 选择矩阵

| 方案 | 节点拖拽/连线 | 自动布局 | 性能取向 | 无障碍 | 许可风险 | 结论 |
| --- | --- | --- | --- | --- | --- | --- |
| React Flow 12 | 原生且 API 完整 | 外接 Dagre/ELK | 中型富 DOM 图；需 memo/折叠 | 官方明确支持键盘、ARIA、screen reader | Core MIT；注意 Pro 示例 | **推荐** |
| Rete.js 2 | 插件完整 | 官方 ELK 插件 | DOM，可做 LOD | 官方合约不明确 | Core MIT；部分高级插件非商业 | 备选 |
| Cytoscape.js 3 | 基础拖动原生，工作流 handles 靠扩展 | 内置/扩展丰富 | Canvas 大图 | 富节点语义需另建 DOM | Core/第一方扩展 MIT | 只读大图备选 |
| JointJS 4 | 完整 | 有开源/商业多档 | SVG；Plus 有虚拟渲染 | 需专项验证 | Core MPL-2.0，Plus 商业 | 暂不选 |

## 2. 自动布局选择

### 2.1 首期用 Dagre

`@dagrejs/dagre` 是 MIT 许可、持续更新的 directed graph layout 库。官方仓库特别提醒应使用 DagreJS 组织下仍在更新的 npm 包。[Dagre official repository](https://github.com/dagrejs/dagre)

React Flow 官方将 Dagre 描述为配置少、偏速度、适合树/普通有向图的方案；它支持动态节点尺寸，但不负责 edge routing，复合 sub-flow 也存在已知限制。[React Flow layout comparison](https://reactflow.dev/learn/layouting/layouting)

这与当前项目工作流匹配：Step 是普通 DAG 节点，画布需要“自动排列”而不是在编辑过程中持续运行复杂布局。建议：

- 用户移动后的坐标属于 presentation metadata；只有显式点击“自动排列”才重算。
- 把布局实现藏在 `LayoutEngine.layout(nodes, edges, direction)` 后，不让 React Flow 组件直接依赖 Dagre 数据结构。
- Rust 只验证 DAG 合法性；坐标计算可以留在 Web，不进入 Pipeline Revision 的可执行语义。

### 2.2 ELK 作为升级路径

ELK Layered 支持 ports、复合图、跨层边、edge labels，以及 straight/orthogonal/spline routing；它会按层摆放并降低交叉。[ELK Layered reference](https://eclipse.dev/elk/reference/algorithms/org-eclipse-elk-layered.html)

`elkjs` 2026-03 发布 0.11.1，采用 EPL-2.0。[elkjs official repository](https://github.com/kieler/elkjs)；[license](https://github.com/kieler/elkjs/blob/master/LICENSE.md)

以下任一条件出现时升级到 ELK：

- 需要嵌套/分组工作流；
- 需要固定端口侧与正交折线；
- 大量 join/fan-out 导致 Dagre 交叉不可接受；
- 需要布局保留用户相对顺序或复合图边路由。

不要首期同时维护 Dagre 与 ELK 的交互细节；保留接口与一组布局快照测试即可。

## 3. Web 终端技术比较

### 3.1 xterm.js：推荐

xterm.js 是 MIT 许可的浏览器终端组件，官方明确说明它支持 bash、vim、tmux、curses、鼠标、CJK、emoji、IME、screen-reader mode 和 GPU renderer；它不是 shell，必须连接真实 PTY。[xterm.js official repository](https://github.com/xtermjs/xterm.js)

与真实 Agent CLI 兼容的关键能力：

- `Terminal.write` 接受 `Uint8Array`，按 UTF-8 流式解码，即使多字节字符跨 WebSocket chunk 也能组合；`onData` 用于普通 Unicode 输入，`onBinary` 保留非 UTF-8 mouse report 的原始 8-bit 数据。[Encoding guide](https://xtermjs.org/docs/guides/encoding/)
- `onData`、`onBinary`、`onResize` 都是正式 API；`write` 有解析完成 callback，可以作为应用层 ACK/背压的确认点。[Terminal API](https://xtermjs.org/docs/api/terminal/classes/terminal/)
- 支持 normal/alternate buffer；官方 VT 支持表列出 DECSET 47/1047/1049、application cursor、SGR mouse、focus events 与 bracketed paste。1049 的 set 路径仍标为 partial，必须进入兼容性测试。[Supported Terminal Sequences](https://xtermjs.org/docs/api/vtfeatures/)
- `@xterm/addon-fit` 负责根据容器算 cols/rows；`@xterm/addon-webgl` 可 GPU 渲染；`@xterm/addon-search` 和 `@xterm/addon-serialize` 可后续加入。[Official addons list](https://github.com/xtermjs/xterm.js)
- `screenReaderMode` 会为 Windows NVDA 与 macOS VoiceOver 暴露辅助 DOM；还可配置 minimum contrast ratio。[xterm typings](https://github.com/xtermjs/xterm.js/blob/master/typings/xterm.d.ts)

版本与维护状态：

- 官方最新稳定 release 是 6.0.0；官方同时持续发布 beta，并说明 VS Code 通常跟随 beta，但稳定性敏感的应用不应未经验证直接使用 beta。[6.0.0 release](https://github.com/xtermjs/xterm.js/releases)；[release policy](https://github.com/xtermjs/xterm.js)
- 建议锁定经过 MonkeyFence 回归矩阵验证的精确版本，不使用 `^` 漂移，也不在实现 ticket 里假定“最新 beta 必然更好”。

### 3.2 hterm：可用备选，但生态不如 xterm.js

hterm 是 ChromiumOS `libapps` 下的 JavaScript terminal emulator，官方描述为“reasonably fast/correct/portable”，当前包为 1.92.1、BSD-3-Clause，要求 ECMAScript 2021。[hterm HEAD](https://chromium.googlesource.com/apps/libapps/+/HEAD/hterm/)；[package metadata](https://chromium.googlesource.com/apps/libapps/+/HEAD/hterm/package.json)

它有 VT、keyboard 和 accessibility reader 实现，仍适合 ChromeOS/SSH 场景。但对 MonkeyFence 来说，xterm.js 的 TypeScript API、WebSocket/fit/WebGL/search/serialize 官方 addons、VS Code 级使用面和公开 flow-control 指南更直接。

结论：仅在 xterm.js 的关键 IME/TUI 缺陷无法绕过时做 hterm 对照原型，不同时维护两套终端。

### 3.3 libghostty-vt：观察，不作为首期 Web UI

Ghostty 官方仓库称 `libghostty-vt` 已可在 WebAssembly 上使用且解析器稳定，但 API 签名仍在变化、尚未 tagged version；它目前首先解决 VT parsing/state，并不是成熟的浏览器输入、无障碍、选择/剪贴板、IME 与 WebSocket 组件。[Ghostty official repository](https://github.com/ghostty-org/ghostty)

结论：可作为未来 server-side checkpoint parser 的候选研究对象，首期不能替代 xterm.js。

### 3.4 xterm.js 当前必须正视的风险

1. **中文/日文 IME 仍有活跃缺陷。** 官方 issue 中有 Windows Microsoft Pinyin composition 被截断，以及隐藏 textarea 导致历史输入被重新发射的 2026 年开放报告。[Windows Pinyin issue #6049](https://github.com/xtermjs/xterm.js/issues/6049)；[textarea re-emission issue #6078](https://github.com/xtermjs/xterm.js/issues/6078)
2. **稳定版生产 bundle 风险。** 官方仓库 issue #5800 报告 `@xterm/xterm@6.0.0` 在 Vite/esbuild 再压缩后会在 DCS/TUI 路径崩溃；这是报告而非已确认的发行说明，必须在 MonkeyFence 的 production build 中复现/排除，不能只跑 dev server。[xterm.js issue #5800](https://github.com/xtermjs/xterm.js/issues/5800)
3. **吞吐与背压不是 addon-attach 自动解决的。** xterm.js 官方指出 `write` 非阻塞、内部吞吐约束和有限 buffer 会导致高产出下卡顿/丢弃；跨 WebSocket 需要自定义 ACK watermark。[Flow control guide](https://xtermjs.org/docs/guides/flowcontrol/)
4. **XSS 等同于终端劫持。** 官方安全指南明确指出页面内任意 JavaScript 都可读键盘和操作终端；不能加载不受控 CDN/第三方脚本，必须有严格 CSP、依赖锁定和同源策略。[xterm.js security guide](https://xtermjs.org/docs/guides/security/)
5. **SerializeAddon 仍标为 experimental。** 不能把生产重连的唯一恢复机制交给浏览器端 addon snapshot。[Serialize addon](https://github.com/xtermjs/xterm.js/blob/master/addons/addon-serialize/README.md)

## 4. Rust 后端集成方案

### 4.1 复用现有 PTY 所有权

MonkeyFence 已经有比通用 PTY crate 更符合自身安全需求的实现：

- [`pty_spawn.rs`](../../crates/mf/src/pty_spawn.rs) 原生封装 Windows ConPTY / Unix openpty，并处理 Windows Job Object、Unix process group 与 Secret 环境块清零；
- [`runtime_host.rs`](../../crates/mf/src/runtime_host.rs) 的 `SessionRegistry` 已持有智能体会话，提供 snapshot、tail、普通输入和 raw bytes 输入；
- [`term.rs`](../../crates/mf/src/term.rs) 已解析颜色、光标、alternate screen、滚动区和 OSC title，可作为重连 checkpoint 的起点。

当前缺口（以研究日源码为准）：

- `PtyMaster` 尚未暴露 resize；
- 会话输出主要保留有限 `output_tail` 与渲染后 `Screen`，没有带单调序号的原始 VT journal；
- 尚无 per-client ACK、慢客户端隔离、attach/detach 协议和 writer lease；
- 当前 `Screen` 的 VT 覆盖不是 xterm.js 完整序列集，不能未经 conformance test 就作为无损重连状态。

因此不要替换 PTY，只把它从 GPUI 生命周期中抽到 Rust service，并补齐网络边界。

### 4.2 Web 服务选择

建议使用 `axum` 0.8 系列：官方 WebSocket extractor 支持 upgrade、subprotocol、frame/message/write buffer 限制，`Message` 原生区分 Text/Binary/Ping/Pong/Close。[WebSocketUpgrade](https://docs.rs/axum/latest/axum/extract/ws/struct.WebSocketUpgrade.html)；[Message](https://docs.rs/axum/latest/axum/extract/ws/enum.Message.html)

官方示例展示了 loopback listener、升级前读取 headers/peer address，以及 split socket 后并行 send/receive；这正好匹配每个 terminal attach 的两个方向。[axum websocket example](https://github.com/tokio-rs/axum/blob/main/examples/websockets/src/main.rs)

Tokio `broadcast` 可用于同一智能体会话的实时 fan-out，并能报告 `Lagged(n)`；但其满容量时会覆盖最老消息，新 subscriber 只收订阅之后的值，因此它只能做 live bus，不能当重连 journal。[Tokio broadcast semantics](https://docs.rs/tokio/latest/tokio/sync/broadcast/)

### 4.3 建议的接口分层

```text
Browser
├─ React Flow editor
│  ├─ HTTP Snapshot/Command
│  └─ WS workflow-events
└─ xterm.js Node Session Panel
   └─ WS terminal-session (binary data + JSON control)
                    │
Rust mf-server      │
├─ Application Services / Orchestrator / Store
├─ SessionRegistry
├─ Terminal Journal + Attach Registry
└─ native PTY → real Codex / Claude Code / GLM CLI
```

分开三个通道：

1. **HTTP Snapshot/Command**：低频、可重试、带 `base_revision` 与 idempotency key；修改项目工作流必须由 Rust 校验 DAG、Agent Instance 和版本冲突。
2. **工作流事件 WS**：JSON 事件，带 `stream_epoch + seq`；断线后可从 Snapshot + `after_seq` 恢复。它不承载终端字节。
3. **终端 WS**：每条连接只 attach 一个智能体会话。VT 输出和 raw input 用 binary；attach、resize、ack、exit、history-gap 用小型 JSON control frame。不要直接使用 `@xterm/addon-attach`，因为它没有 MonkeyFence 需要的 revision、ACK、writer lease 与恢复语义。

### 4.4 终端协议最小语义

建议协议版本化为 `mf-terminal.v1`，而不是把 WebSocket 当作无结构字节管道。

Server → Client：

- `hello { protocol, session_id, stream_epoch, cols, rows, first_seq, next_seq, writable }`
- binary `output { seq, bytes }`
- `exit { code, signal, final_seq }`
- `writer_changed { writable, lease_id }`
- `history_gap { requested_seq, first_available_seq, checkpoint_seq }`
- `reset_required { reason }`

Client → Server：

- binary `input { lease_id, bytes }`
- `resize { lease_id, cols, rows }`
- `ack { through_seq }`（必须在 xterm `write` callback 后发送，而不是 WebSocket 收到时）
- `request_writer` / `release_writer`
- `detach`

约束：

- 每个会话可有多个只读 attach，但同时只有一个 writer lease；否则两个浏览器标签会相互注入按键。
- 只有 writer 的尺寸能改变 PTY；其他观察者本地 fit/letterbox，避免多个 viewport 争抢 cols/rows。
- `seq` 对输出字节全序，`stream_epoch` 在服务重启或新 PTY 时变化；客户端不得跨 epoch 拼接。
- `MF_RUN_TOKEN`、Secret lease 和 CLI 凭据永远不进入浏览器。终端输入只绑定到一个明确的智能体会话 capability。
- 入站输入默认不持久化，避免密码/令牌进入历史；出站 journal 必须位于现有跨 chunk redaction 之后。

### 4.5 resize 必须到达真实 PTY

`FitAddon` 只改变 xterm 的 cols/rows；Rust 必须同步真实 PTY：

- Windows 调用 `ResizePseudoConsole(HPCON, COORD)`，微软说明这会让 CUI 应用读取到正确尺寸。[ResizePseudoConsole](https://learn.microsoft.com/en-us/windows/console/resizepseudoconsole)
- Unix 对 PTY master/slave 执行 `TIOCSWINSZ`；Linux 会向前台 process group 发送 `SIGWINCH`。[TIOCSWINSZ](https://www.man7.org/linux/man-pages/man2/TIOCSWINSZ.2const.html)

Windows ConPTY 的输入/输出是 UTF-8 文本与 VT sequences 的双向流，官方还警告输入/输出 channel 应分别处理，避免 buffer deadlock。[Microsoft Pseudoconsoles](https://learn.microsoft.com/en-us/windows/console/pseudoconsoles)；[Creating a Pseudoconsole Session](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session)

因此 resize 实现需要进入 `pty_spawn` 的平台抽象，而不是只调用 [`Screen::resize`](../../crates/mf/src/term.rs)。

### 4.6 重连与历史恢复

仅保存“最后 N 行文本”不能恢复 TUI。颜色、cursor、alternate buffer、滚动区、application cursor keys、mouse/focus tracking 和 bracketed paste 都会影响后续解释。

推荐两阶段：

**MVP：完整 raw journal（从会话开始）**

- Rust 在 redaction 后给每个 VT chunk 分配 seq，并保留有界完整 journal；浏览器刷新后从 seq 0 或 retained checkpoint 依次 replay 到 xterm。
- journal 容量未超限时可以精确恢复；超限时必须显式返回 `history_gap`，不能静默把任意 tail 喂给全新 xterm。
- journal 容量、内存/磁盘策略由压力测试确定，不在架构文档中拍脑袋固定。

**生产：server-side `TerminalCheckpoint` + raw delta**

- 扩展当前 Rust `Screen`，checkpoint 至少包含 normal/alternate buffer、active buffer、cells/style、cursor、title、scroll region 与会影响未来输入的 DEC modes。
- checkpoint 生成可重放的 VT reset/redraw，再追加 `checkpoint_seq` 后的 raw chunks。
- 用同一录制 corpus 分别喂给 Rust parser 与 xterm.js，对 Codex/Claude Code 的屏幕、cursor、active buffer 做差分测试。

xterm.js 官方说明 `@xterm/headless` + serialize 可在 Node 端维护状态并用于重连；但 MonkeyFence 的服务端目标是 Rust-only，因此这可作为正确性对照或应急 sidecar，不应成为默认生产依赖。[xterm headless/reconnect](https://github.com/xtermjs/xterm.js)

服务崩溃/升级是另一层问题：现有 Job Object/process group 所有权会使遗留进程不可安全跨进程重附着。浏览器断线恢复应保证；Rust 服务进程重启后只恢复转录与工作流状态，并按既有语义进入“需要你”，不要伪装成仍可交互的智能体会话。

### 4.7 背压与慢客户端

xterm.js 官方 WebSocket flow-control 方案要求把 `write` callback 的 ACK 送回服务端，再用 high/low watermark 控制生产者。[Flow control over WebSockets](https://xtermjs.org/docs/guides/flowcontrol/)

对 MonkeyFence 的落地建议：

- PTY reader 始终在独立线程/任务中 drain，避免 ConPTY pipe deadlock；
- raw chunk 进入 bounded journal 与 live `broadcast`；
- 每个 WebSocket 有 bounded send queue；慢客户端 lag 时暂停该客户端发送并尝试从 journal 补齐，不拖住整个 Agent；
- journal 也超限时断开慢客户端并返回可诊断的 `history_gap/reset_required`；
- 用 `yes`/大日志场景验证用户按 Ctrl+C 仍可在可接受时间内送达真实 PTY。

## 5. 安全边界

WebSocket 标准本身采用 browser Origin security model；浏览器握手会发送 `Origin`，服务端可以拒绝未授权 origin，但非浏览器客户端可能不发送该头。[RFC 6455](https://datatracker.ietf.org/doc/html/rfc6455)

本地 UI Gateway 至少需要：

- 仅监听 `127.0.0.1`/`::1` 的随机端口，不监听 LAN；
- UI 静态资源与 API 同源，使用启动期随机 UI session；
- 精确校验 `Host` 与 `Origin`，不能只依赖 CORS；
- 用 HttpOnly、SameSite=Strict cookie 或等价的启动期 capability 完成 WebSocket upgrade 鉴权；不要把令牌放 query string；
- 限制 frame/message 大小、输入速率、attach scope 与 writer lease；
- CSP 禁止外部脚本、`unsafe-eval` 和 CDN；前端 assets 随应用打包，依赖 lockfile 与完整性审计；
- 浏览器绝不获得 `MF_RUN_TOKEN`，也不能按任意 PID/路径 attach；只能通过 Rust 返回的 opaque session handle；
- 终端链接默认仅允许 `http/https`，其他协议必须显式 allowlist；
- output redaction 必须发生在 journal/fan-out 之前，且跨 chunk 工作。

xterm.js 官方安全文档的核心结论是：同一页面中的任意 JavaScript 都能读取 terminal keystrokes 和控制 terminal I/O，所以终端页面的 XSS 严重度等同于本机 shell access。[xterm.js security](https://xtermjs.org/docs/guides/security/)

## 6. 与 MonkeyFence 领域模型的结合

按照 `CONTEXT.md` 与 ADR 0004：

- 编辑页的节点是项目工作流中的 Step 定义；运行页的节点是工作流运行中的 Step 投影。
- 单击节点打开配置/状态 inspector。
- 双击**运行中且已有智能体会话**的 Step，attach 对应 Agent Session；React Flow 原生提供 `onNodeDoubleClick`。
- pending/ready Step 不创建假终端；显示“尚未创建智能体会话”。
- 已结束 Step 打开只读 checkpoint/transcript，是否允许 raw history 下载另行定义。
- 编辑态的“测试节点”创建独立 Preview Session，不能计入正式工作流运行或 Settlement。
- CLI 进程退出、终端 idle、浏览器 detach 都不等于 Step 成功；Settlement 与“需要你”语义保持 Rust 权威。

Rust 与 Web 的状态职责：

| 数据 | 权威来源 | Web 是否可乐观更新 |
| --- | --- | --- |
| Step/依赖/Agent Instance/运行策略 | Rust Project Workflow Store | 可，但失败必须 rollback |
| DAG 合法性与 cycle 检查 | Rust | Web 可预检，Rust 必须复检 |
| 节点坐标/viewport | presentation metadata | 可以 |
| Pipeline Revision / Agent Run / Settlement | Rust Orchestrator | 不可以伪造 |
| PTY 进程与智能体会话生命周期 | Rust Session Registry | 不可以 |
| xterm renderer buffer | Browser（可丢弃缓存） | 从 Rust journal/checkpoint 恢复 |

## 7. 必须通过的技术原型与验收矩阵

### 7.1 DAG 原型

用正式视觉稿复刻 5–10 个节点，并验证：

- 节点库 Pointer Events 拖入；节点移动、框选、缩放和平移；
- 端口 hover/合法/非法反馈；创建、删除、重连；
- cycle、本节点自连、重复边在 Web 预检和 Rust 权威校验结果一致；
- 单击 inspector、双击 terminal attach 不冲突；
- Dagre top-down / left-right 显式自动排列，手动坐标不会被后台悄悄覆盖；
- 键盘完成 focus、选择、移动和删除；ARIA 文案中文化；
- 100/500/1000 节点基准记录 FPS、drag latency、首次 fit/layout 时间与 heap，依据实测决定折叠/虚拟化阈值；
- 节点 component、callbacks、selectors 按 React Flow 官方性能指南 memoize。

### 7.2 真实 Agent CLI 终端原型

必须在 production build（非仅 dev server）中对真实 CLI 验证：

| 场景 | Codex | Claude Code | 预期 |
| --- | --- | --- | --- |
| Slash popup | 输入 `/`、`/model`、`/skills` | 输入 `/`、`/model`、自定义 skill | CLI 自己显示 popup/筛选/执行，Web 不解析 |
| 键盘 | Tab、Up/Down、Ctrl+R、Ctrl+C、Esc | Tab、Up/Down、Ctrl+R、Ctrl+C、Shift+Tab | 行为与系统终端一致 |
| TUI | model/permission/approval 菜单 | `/tui fullscreen`、permissions/tasks | alternate screen、cursor、颜色、mouse 正常 |
| 输入法 | Microsoft Pinyin、搜狗、日文 IME、macOS 中文 | 同左 | 不丢字、不重复、不提交半成品 composition |
| Unicode | CJK、emoji、组合字符、宽字符 | 同左 | cursor/换行宽度正确 |
| resize | 连续拖动面板宽高 | 同左 | xterm 与 PTY cols/rows 一致，TUI 收到 resize |
| output flood | 大量日志后 Ctrl+C | 同左 | UI 仍响应，输入不被输出饿死，无静默丢字节 |
| reconnect | 刷新、网络断开再 attach | 同左 | PTY 不死；seq replay 后屏幕/cursor 与断线前一致 |
| 多标签 | 第二标签 attach | 同左 | 仅 writer lease 可输入/resize，观察者只读 |
| exit | CLI 正常/异常退出 | 同左 | exit 事件正确；不自动等同 Settlement |

还必须覆盖 xterm.js 官方 issue #5800 的 Vite production minification 路径，以及 #6049/#6078 的 IME event shape。若选择的精确版本仍复现，不得以“多数输入正常”通过验收：要么使用官方已修复版本，要么维护最小、可删除且有回归测试的下游 patch。

### 7.3 协议与安全测试

- 非 loopback 绑定测试；错误 Host/Origin/cookie/subprotocol 全部拒绝；
- 普通网页对 localhost 发 WebSocket 请求不能 attach；
- 伪造 session id、跨项目 session handle、过期 writer lease 被拒绝；
- oversize binary frame、input flood、慢 client、ACK 丢失不会拖垮 Agent 或工作流事件；
- 浏览器获得的任何 response/event 中都不含 `MF_RUN_TOKEN`、Secret 环境值或真实 capability；
- redaction 命中跨 WebSocket chunk 的 Secret；
- 服务进程退出时按既有 Job/process-group 规则清理，不留孤儿 CLI。

## 8. 建议的实施顺序

1. 建立 Rust application service + loopback HTTP/WS shell，不迁移 UI。
2. 用 React Flow 实现只读项目工作流与工作流运行投影，验证状态边界。
3. 增加编辑命令、revision 冲突、Pointer DnD、连线与 Dagre 自动排列。
4. 为现有 PTY 增加跨平台 resize、raw redacted journal、seq 和 attach registry。
5. 接入 xterm.js，先做单 writer / 单 client；完成真实 Codex、Claude Code production-build 矩阵。
6. 增加 detach/reconnect、ACK/backpressure、history gap 与只读多 attach。
7. 完成 checkpoint conformance、安全测试和无障碍测试后，才把 GPUI 工作流界面标记为可退役。

## 9. 最终决策建议

可以进入后续 spec 的技术决策为：

- **DAG：React Flow 12 + Dagre，ELK 只作为接口后的升级路径。**
- **终端：xterm.js，但锁精确版本且 production/IME 测试是发布门槛。**
- **PTY：复用并服务化现有 `pty_spawn`/`SessionRegistry`；Web 绝不模拟 Agent。**
- **协议：Snapshot/Command + workflow-events WS + terminal-session WS 三分；terminal 用 binary + seq/ack/control。**
- **恢复：浏览器 detach/reconnect 必须支持；Rust 服务崩溃后的 live PTY reattach 明确不保证。**
- **安全：loopback、same-origin、随机 UI session、严格 Origin/Host/CSP、per-session writer lease；浏览器永远拿不到运行能力令牌。**

在进入大规模迁移前，仍需原型实证的开放项只有三个：

1. 选定 xterm.js 精确版本在 MonkeyFence production bundler 中是否触发 DCS/IME 缺陷；
2. 当前 Rust `Screen` 扩展为可靠 `TerminalCheckpoint` 的成本，是否低于引入 headless sidecar；
3. 真实项目工作流在 100/500/1000 节点下的 React Flow 性能阈值。
