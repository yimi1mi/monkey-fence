# Wayfinder → /to-spec 交接：Web Interaction Client + Rust Core Service

- 日期：2026-09-01
- Wayfinder map：[Web 交互客户端与 Rust 核心服务重构](https://github.com/yimi1mi/monkey-fence/issues/1)
- 目标读者：下一阶段 `/to-spec` agent（可交给 GLM 5.3）
- 范围：汇总已验证决策，不重新讨论产品方向，不直接实施

## Destination

把 MonkeyFence 从 GPUI 工作台迁移为：

- 默认系统浏览器中的 Web Interaction Client；
- 每 OS 用户一个、跨 Project 的无界面 Rust Core Service；
- 以 Project Workflow DAG 的可视化编辑、Workflow Run 监控、Needs You 和人在环引导为核心；
- 节点双击进入真实 Agent CLI 的 PTY Session，原生支持 `/xxx`、Skill command、审批与 TUI；
- Agent Plugin 驱动 CLI 发现/安装/更新/修复/卸载，Provider Profile 提供 CC Switch 类 API Key/Endpoint/模型配置；
- 可选 Root Mode 让新安装和新 Agent Session 同时获得 OS elevation 与 Agent 自身 full-access；
- 代码编辑器、Git/P4 UI 和通用终端不迁移。

## 权威输入

下一阶段必须完整读取并引用：

1. `CONTEXT.md`：统一语言；
2. `docs/adr/0001`–`0005`，尤其 ADR 0005 对旧 VCS UI、plugin 权限/词汇和 GPUI 渲染条款的逐项 supersede；
3. [#1 Wayfinder map](https://github.com/yimi1mi/monkey-fence/issues/1) 及全部 Decisions；
4. `docs/research/2026-08-31-web-dag-terminal-stack.md`：技术底座研究；
5. `docs/research/2026-09-01-node-session-preview-prototype-results.md`：真实 CLI/PTY/性能验证；
6. `docs/research/2026-09-01-web-gateway-root-security.md`：Web、Secret、Root/Broker 安全；
7. `docs/research/2026-09-01-agent-plugin-cli-install-contract.md`：Manifest v3、CLI 安装与 Provider；
8. `docs/research/2026-09-01-web-api-terminal-protocol-v1.md`：HTTP/WS/Terminal 契约；
9. `docs/research/2026-09-01-gpui-web-migration-retirement.md`：迁移 DAG、owner handoff、rollback。

旧 GPUI 设计/计划只能作为当前代码历史，不得覆盖以上新决策。

## 不可重新打开的决策

### 产品与生命周期

- 独立默认浏览器；不使用 Electron/WebView/PWA 作为必需壳；
- Core 每用户 singleton、跨项目；关闭标签/tray 不停止；无自动空闲退出；
- tray 存在但不拥有 Core、不承载第二 UI、不自启动；
- Web bundle 与 Core 同版本内嵌、hash asset、离线、无 CDN/Node runtime；
- 顶层只有工作流/运行，Needs You 是运行过滤器；设置是 overlay；
- 三栏 Workbench：Agent 库 / DAG / Inspector，窄屏 Inspector 下移。

### 状态与并发

- Rust 是唯一事实源；Web 不离线编辑、不持久化权威业务状态；
- 每用户一个 Controller + 多 Observer；新 bootstrap client 成为 Controller，旧端降只读但不断开；
- 所有写入走封闭 command、Controller lease、expected revision/CAS、idempotency；
- Project Workflow 分 semantic/presentation revision，Project 有 workflow collection revision；
- 运行只认 Rust 已确认 semantic revision；
- Store 不是 Event Sourcing，使用一致 Snapshot + 当前 epoch 有界 projection journal。

### Web/API/Terminal

- 三数据面：HTTP JSON、`mf-workflow.v1`、`mf-terminal.v1`；不使用 GraphQL/SSE/JSON Patch；
- Workflow WS 是当前 OS 用户全部已登记 Project 的全局流，不过滤 seq；
- API target 使用 opaque handle，不接受 rowid/PID/任意 path/command；
- Terminal 每 WS 一个 Session，真实 PTY 每次一个 terminal epoch；
- output 在跨 chunk redaction 后分配 seq，xterm callback 后累计 ACK；慢客户端不能反压 PTY；
- writer lease 绑定 client/controller epoch/connection/session，TTL/renew/release；input 不跨断线重放；
- v1 不发送伪 VT checkpoint；history gap 关闭 live WS并显示 durable read-only transcript；
- exit 必须 durable transcript/metadata 后再通知；进程退出不等于 Settlement。

### 安全与 Root

- loopback IP literal + random port；严格 Host/Origin；fragment bootstrap → HttpOnly session + memory CSRF；
- Secret write-only，响应/事件/日志/journal/receipt 不含 API Key/MF_RUN_TOKEN；
- Browser 永不持 Broker capability、OS token、arbitrary path/PID/command；
- Root Mode 默认关、不持久化、Core 重启后关；Core 本身普通权限；
- Elevated Broker 只授权/启动；Root PTY 与安装分别由 session/job-scoped elevated host 物理持有；
- full-access = OS elevation + Agent adapter 的结构化最高权限启动映射；缺失 fail-closed；
- Root 关闭后拒绝新任务，既有 host 可继续/取消；Core crash 使用 read-only reattach + orphan grace，最终终止不可控 Root process group。

### Agent/Provider/CLI

- Plugin Package、CLI Installation、Provider Profile、Agent Instance、Agent Session 五个生命周期分离；
- Manifest v3 声明 Agent Type、Provider Type、discovery/version、root launch、installer recipes；
- installer：package-manager、verified-download、custom-command；预览后冻结 exact plan；
- external CLI 默认只 launch，可信 hash/signature匹配才可 adopt，否则重装 managed；
- Provider Type 负责 Endpoint/API Key/远端模型目录；Agent Type 负责本地 CLI 能力和模型启动映射；
- CLI Installation Receipt 与 Plugin lock 分离；活动 Run/replay lease pin，历史 Revision 不永久 pin；
- uninstall 只删 receipt-owned 内容，不删用户配置/Secret/外部 CLI。

### 迁移

- 不允许 GPUI/Web 双 owner、双写或 UI mutation bypass；
- Terminal v1 必须在 Core 拆进程前完成；
- 不能发布自动抢 Controller 但无写能力的 read-only Web；
- Core owner 切换必须 freeze/drain/epoch/Store close/OS lock/handoff/acquire 线性化；
- whole versioned bundle side-by-side 更新/rollback，不能只换 UI；
- durable feature reader-before-writer，滚动 Bridge anchor；pre-Bridge 当前二进制不是 rollback target；
- Project Store v6→v7 expand；Catalog 迁到新 `catalog-v2.db`，不让旧代码降版本标记；
- command 使用 service intent + target-local receipt/outbox，跨 Store 使用 Operation saga；
- Windows 真实包先行；macOS/Linux 真实包可后置，但三平台 fake installer/broker/host contracts 前置。

## 当前代码事实

- `crates/mf/src/main.rs` 直接启动 GPUI `Workspace`；`Workspace::new` 创建 `AppCtx`；
- `AppCtx` 公开 Registry/Plugin/Catalog/Overview 等可变子系统，UI 仍有旁路；
- `ProjectOverviewHub` 携带 path/rowid、整表 rebuild，不符合 v1 projection；
- `PtySession` 是固定 Screen + 256 KiB tail + raw writer；Registry key 使用 project path + rowid；
- legacy PTY reader 有未脱敏 Screen/tail 路径，现有 redactor 未覆盖后注入的 MF_RUN_TOKEN；
- Project schema 当前 v6，Catalog schema v1；Catalog init 当前缺 future-version guard；
- 根 Cargo 直接依赖 `D:/workspace/zed/crates/gpui`，发布工程当前 Windows-only；
- 没有 tray；当前 UI 进程退出会带走 Core。

这些是实现起点，不是目标约束。

## /to-spec 必须产出的设计

生成一个单一、可构建的主 spec，建议章节：

1. Goals / non-goals / release support matrix；
2. CoreKernel deep module 与 crate/process boundaries；
3. storage v7/catalog-v2/service-v1 schema、migration 与 future guards；
4. command intent + target receipt/outbox + Operation recovery；
5. ProjectionHub、Snapshot barrier、event journal；
6. Web Gateway/auth/security headers/bootstrap；
7. Web API v1 DTO、command/event/problem schemas；
8. SessionRuntime、TerminalJournal/Transcript、Terminal v1 frames/limits；
9. Manifest v3、Provider/Agent/CLI Installation；
10. Root Broker/Runtime/Install Host protocol 与 platform traits；
11. launcher/tray/picker/singleton/discovery/safe shutdown；
12. React Workbench/React Flow/xterm UI state and journeys；
13. Bridge/owner handoff/side-by-side bundle/rollback；
14. observability, tests, capacity budgets and rollout gates；
15. exact file/crate changes and deletion plan。

Spec 不得只写架构口号；每个模块必须给接口、state machine、ownership、failure recovery、migration、tests 和 acceptance。

## 必须在 spec 中量化的参数

这些不是开放架构问题，但必须选择具体默认值/上限：

- workflow event journal 条数/字节、per-client queue、resume window；
- terminal frame/input rate/outstanding bytes/replay ring/transcript cap/flush period/retention；
- writer lease TTL/renew interval、resize/input bounds；
- bootstrap/session/CSRF/install-plan nonce TTL；
- command receipt/Operation/audit retention 与 GC；
- Root orphan grace、Broker/host heartbeat、install timeout/download/archive limits；
- 100/500/1000 node Snapshot/event/perf budgets；
- Core shutdown/update drain timeouts与failure UX；
- provider model cache TTL/retry/backoff；
- supported Windows version/architecture与首发包格式。

参数必须出现在 config/default/constants 和 contract tests 中，不能散落 magic number。

## 实施 DAG（/to-tickets 的主骨架）

```text
T0 Baseline fixtures + existing behavior characterization
  ↓
T1 Storage guards, public IDs, revisions, service intent/outbox, owner lock
  ↓
T2 CoreKernel extraction; remove GPUI mutation bypass
  ↓
T3 Terminal pipeline/protocol + Transcript + Root host seam
  ↓
T4 Manifest v3 / Provider / CLI Installation / Root runtime readers
  ↓
T5 In-process Bridge A + reader-before-writer + whole-bundle manager
  ↓
T6 Atomic standalone Core split + legacy GPUI client
  ↓
T7 WebGateway/auth/API/event implementation + embedded assets
  ↓
T8 Web Workbench + Workflow/Run writes + Needs You
  ↓
T9 Agent settings/Provider/models/CLI install/Root UI
  ↓
T10 Node/Preview xterm Terminal v1 + real CLI matrix
  ↓
T11 Web default + tray/launcher/picker + rollout/rollback
  ↓
T12 GPUI/Zed/editor/VCS/general-terminal deletion
```

允许并行：

- T2 contract 冻结后，Web component、Gateway、安全 header、Windows packaging 可并行；
- T1+T2 后，Manifest/Installation 数据结构可并行；Root host integration 仍依赖 T3；
- 各 lane 在 T5/T6/T8 gate 必须重新汇合，不得跳过 hard blocker。

每个 ticket 是 tracer bullet，必须声明 blockers、可独立验收、迁移/回滚和测试。不要创建一个“重写整个前端”或“迁移 Core”巨型 ticket。

## 实现与 review 约定

- 实现阶段可在适合的独立前端/机械迁移 ticket 使用 `computer-use:computer-use` 唤起 Zcode；
- Zcode/GLM 不得直接改变上述架构决策；发现矛盾时回到 ticket/Spec，而不是静默自创协议；
- 每个实现 ticket 使用 TDD/contract fixtures，完成后由 Codex 做 Standards + Spec review；
- 任何 UI 可用性验收都在实际浏览器进行；Terminal 必须运行真实 Codex/Claude/GLM，而不是 mock 代替；
- 不在同一实现 ticket 顺手迁移代码编辑器、VCS 或通用终端。

## Completion definition

`/to-spec` 完成时，下一位 `/to-tickets` agent 不需要再决定：

- 应用壳与生命周期；
- Core/UI ownership；
- Controller/CAS/idempotency/event recovery；
- Web/Terminal transport；
- Secret/Root/Broker boundary；
- Agent Plugin/CLI/Provider model；
- GPUI bridge/owner handoff/rollback/deletion order。

它只需要把 spec 拆成有明确 blocking edges 的实现 tickets。
