# Web 交互客户端与无界面 Core Service 主规格（canonical spec）

- 日期：2026-09-01
- 状态：已批准，canonical；GitHub Spec Issue [#11](https://github.com/yimi1mi/monkey-fence/issues/11) 以 `ready-for-agent` 追踪 `/to-tickets`，本文件承载完整 spec 内容
- 基线 commit：`e027770`（docs(wayfinder): define web core migration）
- 权威输入：`CONTEXT.md`；`docs/adr/0001`–`0005`（ADR 0005 的逐项 supersede 生效）；Wayfinder map [#1](https://github.com/yimi1mi/monkey-fence/issues/1) 全部 Decisions；`docs/research/2026-08-31-web-dag-terminal-stack.md`、`2026-09-01-node-session-preview-prototype-results.md`、`2026-09-01-web-gateway-root-security.md`、`2026-09-01-agent-plugin-cli-install-contract.md`、`2026-09-01-web-api-terminal-protocol-v1.md`、`2026-09-01-gpui-web-migration-retirement.md`
- 历史地位：`docs/superpowers/specs/` 下 2026-08 的 GPUI 设计文档与一切旧 GPUI 计划只是代码历史，本文取代它们；冲突时以本文为准

## 阅读规则

1. 本文所有「决策」条目均为不可重开决策，实现阶段（包括 Zcode/GLM）不得改变；发现矛盾回到 ticket/spec 修订，不得静默自创协议。
2. 附录 A 是全量参数总表：每个容量、TTL、retention、超时、性能预算都有默认值、允许范围、hard cap（或 fixed/budget 标记）、常量落点与测试落点，无 TBD。可配置项仅能通过 `~/.monkeyfence/config.toml` 的 `[limits]` 段在允许范围内覆盖默认值，且不得超过 hard cap；标 `fixed` 的安全上限与标 `budget` 的性能预算不可配置（budget 是验收阈值而非运行参数）。正文只引用参数名，数值以附录 A 为准。
3. 术语一律使用 `CONTEXT.md` 统一语言（Project Workflow、Workflow Run、Agent Run、Agent Session、Preview Session、Controller Lease、Terminal Writer Lease、Root Mode、Installation Receipt、Needs You 等）。

---

## 问题陈述（Problem Statement）

用户在 MonkeyFence 中编排和监控多项目 AI 工作流，但当前产品是绑定在单进程 GPUI 工作台里的：关闭窗口即结束 Core，多标签/多屏/跨项目监控受桌面窗口模型限制；DAG 可视化、响应式 Inspector 与原生 Agent CLI 终端在 GPUI 中持续扩写成本高，而产品还把精力稀释在代码编辑器、Git/P4 UI 与通用终端上。用户需要的是：随时在默认浏览器中打开的、以项目工作流编辑—运行—介入为中心的交互面；真实 Codex/Claude/GLM CLI 的 PTY 终端；以及一个不随标签页关闭而死的、每用户唯一的后台核心服务。

## 解决方案（Solution）

MonkeyFence 迁移为「默认系统浏览器中的 Web Interaction Client + 每 OS 用户一个跨项目的无界面 Rust Core Service」。Core Service 独占工作流、运行、插件、Secret 与 Agent Session 的权威状态；Web 只投影权威状态并提交用户意图，经 loopback 上经过认证的三条数据面（HTTP JSON、`mf-workflow.v1`、`mf-terminal.v1`）通信。节点双击进入真实 Agent CLI 的 PTY 会话；Agent Plugin 驱动 CLI 发现/安装/更新/修复/卸载（Manifest v3），Provider Profile 提供 CC Switch 式 API Key/Endpoint/模型配置；可选 Root Mode 通过最小 Elevated Broker 授权、由会话级 Elevated Host 物理持有高权限进程。代码编辑器、Git/P4 UI 和通用终端不迁移，随 GPUI 一并退役。

## 用户故事（User Stories）

1. 作为多项目开发者，我想在系统默认浏览器打开 MonkeyFence，以便不需要安装专用桌面壳就能编排工作流。
2. 作为多项目开发者，我想关闭浏览器标签后 Core、工作流运行与会话继续，以便长时间 Agent 运行不被误打断。
3. 作为多项目开发者，我想在一个跨项目的页面看到所有已登记 Project 的工作流与运行，以便不逐窗口切换。
4. 作为工作流作者，我想在 DAG 画布上创建、拖动、连接、重连、删除节点，以便直观编排项目工作流。
5. 作为工作流作者，我想让节点坐标、viewport、折叠只影响展示（presentation），以便改布局不触发语义版本冲突、不阻止运行。
6. 作为工作流作者，我想单击节点在 Inspector 查看配置/状态，双击运行中的节点直接附着它的 Agent Session 终端，以便编辑与介入零切换。
7. 作为工作流作者，我想在编辑态双击节点启动 Preview Session 试验配置，以便试错不污染正式运行和结算。
8. 作为工作流作者，我想点击「自动排列」用 Dagre 重排布局，以便大图快速整洁；手动坐标不被后台悄悄覆盖。
9. 作为工作流作者，我想在窄屏下 Inspector 自动下移，以便笔记本半屏也能用。
10. 作为运行发起者，我想从项目工作流一键运行，以便系统自动创建 Task、冻结 Pipeline Revision 并调度。
11. 作为运行发起者，我想取消运行、重试 Step、响应询问、手工结算，以便人在环决策始终可用。
12. 作为运行监控者，我想通过「需要你（Needs You）」过滤召回所有等待我的运行，以便不错过审批与询问。
13. 作为运行监控者，我想看到进程退出/终端空闲/`done` 不被自动当作成功而是进入 Needs You，以便结算语义可信。
14. 作为第二标签页用户，我想以 Observer 身份查看同一终端输出与运行状态但不能输入，以便监控不打扰操作者。
15. 作为第二标签页用户，我想显式接管 Controller（takeover），以便换设备/换窗口继续操作。
16. 作为终端用户，我想在 xterm 中使用 `/model`、`/skills`、Skill command、审批与 TUI，且全部由真实 CLI 原生处理，以便行为与系统终端一致。
17. 作为终端用户，我想用中文 IME 输入不丢字、不重复、不提交半成品 composition，以便日常中文使用可靠。
18. 作为终端用户，我想刷新/断线重连后从 seq 恢复屏幕与光标，以便不丢上下文。
19. 作为终端用户，我想输出洪泛期间 UI 仍可交互、Ctrl+C 立即送达 PTY，以便随时中断失控输出。
20. 作为终端用户，历史超出保留时我想看到只读 transcript 与明确的重启会话入口，而不是被喂入错误状态，以便知道该做什么。
21. 作为 Agent 使用者，我想在 Agent Catalog 看到本机已检测的 CLI（版本、来源、scope），以便直接使用已有安装。
22. 作为 Agent 使用者，我想从未安装的 Agent Type 直接 Install 并看到冻结的安装预览，以便明确授权不可变计划。
23. 作为 Agent 使用者，我想对受管安装执行 Update/Repair/Uninstall 并查看 Installation Receipt，以便生命周期可控。
24. 作为 Agent 使用者，我想配置 Provider Profile（Endpoint、API Key、模型获取下拉），且 API Key 写入后不可读取，以便安全复用凭据。
25. 作为 Agent 使用者，我想模型下拉显示 live probe 或 cache 来源并可显式刷新、失败时回退缓存与手填，以便离线也能工作。
26. 作为 Agent 使用者，我不想 MonkeyFence 改写真实 CLI 的全局配置，以便两侧互不干扰。
27. 作为 Root Mode 用户，我想在当前 Core 生命周期内开启 Root Mode（一次 OS 授权）后，新安装与新 Agent Session 自动获得管理员权限 + Agent full-access，以便高权限任务不被逐次 UAC 打断。
28. 作为 Root Mode 用户，我想关闭 Root Mode 后既有高权限任务可继续/取消但不能新增，且 UI 持续标记，以便权限状态始终可见。
29. 作为 Root Mode 用户，我想 Core 崩溃后高权限进程不会永久无人控制（orphan grace 终止），以便安全与可审计。
30. 作为插件作者，我想用 Manifest v3 声明 Agent Type、Provider Type、discovery、root launch 与 installer recipe，以便第三方 Agent 获得与内置一致的能力。
31. 作为插件作者，我不想浏览器或 worker 拿到安装能力或 Broker capability，以便安全边界清晰。
32. 作为插件作者，我想权限/recipe/内容变化触发重新授权指纹，以便用户始终知情。
33. 作为故障恢复用户，我想 Core 重启后工作流、运行、收据、transcript 完整恢复、终端进入只读 + Needs You，以便重启不等于事故。
34. 作为故障恢复用户，我想相同 command id 重试得到相同结果而不是重复执行，以便网络抖动无害。
35. 作为升级用户，我想活动 Workflow Run / Agent Session / 安装任务存在时升级自动延期，以便不强杀 Agent。
36. 作为升级用户，我想 whole-bundle side-by-side 回滚到上一个兼容版本，以便新版本有问题快速止损。
37. 作为升级用户，我想旧数据（v6 项目库、v1 Catalog、session.json）幂等迁移且保留备份，以便升级零手工。
38. 作为 Observer，我想查看安装进度、日志、收据、运行与终端输出但不能写，以便监督而不越权。
39. 作为安全敏感用户，我想恶意网页无法借浏览器访问我的 loopback Core（Host/Origin/CSRF/nonce 全拦截），以便本地服务不被远程滥用。
40. 作为安全敏感用户，我想 API Key、MF_RUN_TOKEN 绝不出现在响应、事件、日志、journal、receipt 中，以便凭据不外泄。
41. 作为 tray 用户，我想 tray 提供「打开 Web、跨项目运行/Needs You 摘要、安全退出」，以便不开浏览器也知道状态。
42. 作为 tray 用户，我想安全退出前列出全部活动 Workflow Run / Agent Session / 安装任务并要求确认，以便退出不误伤。
43. 作为命令行用户，我想 `monkeyfence status/open/stop` 与 mfctl Settlement 语义不变，以便脚本与 Agent 流程兼容。
44. 作为 Windows 首发用户，我想 per-user MSI 安装、无需管理员、UAC 只在 Broker 启动时出现，以便安装与提权体验顺滑。
45. 作为无障碍用户，我想 DAG 支持键盘遍历/选择/移动、xterm 支持 screen-reader 模式，以便辅助技术可用。

---

## 1. 目标 / 非目标 / 发布支持矩阵

### 1.1 目标

- 产品交互全部迁移到默认系统浏览器中的 Web Interaction Client；Rust 成为每 OS 用户唯一、跨 Project、普通权限、无界面的 Core Service。
- 以 Project Workflow 的可视化编辑、Workflow Run 监控、Needs You 与人在环引导为核心。
- 节点双击进入真实 Agent CLI 的 PTY 会话；`/xxx`、Skill、审批、TUI 由 CLI 原生处理。
- Agent Plugin 驱动 CLI 发现/安装/更新/修复/卸载；Provider Profile 提供 CC Switch 式配置。
- 可选 Root Mode：OS elevation + Agent full-access 的结构化组合，缺失映射 fail-closed。
- 全链路安全边界：loopback、同源、一次性 nonce、HttpOnly session、内存 CSRF、write-only Secret。
- 迁移期唯一 CoreKernel 写路径；GPUI 走同一内核后整体退役删除。

### 1.2 非目标（Out of Scope）

- 不迁移代码编辑器、文件树、搜索、Git/P4/diff、通用终端到 Web；这些 UI 与编辑器/通用终端代码随 GPUI 直接退役，不做 Web 对等实现（headless Git runtime 保留，见 §15.3）。
- 不使用 Electron/WebView/PWA 作为必需壳；不实现浏览器离线编辑或第二事实源。
- 不做 Event Sourcing、GraphQL、SSE、JSON Patch。
- Terminal v1 不发送伪 VT checkpoint；不承诺普通 PTY 的跨进程 live 重附着——Core crash 后普通 live 会话即 lost/Needs You；既定例外是 §10.3 Root host 的窄化 read-only reattach（仅恢复只读输出，不恢复 writer/control）。
- 不构建通用软件商店或自动后台更新器；不自动迁移 CC Switch 数据；不修改 Claude/Codex 全局配置；不接管任意外部包管理器安装。
- 不允许插件注入任意 Web UI/JavaScript；插件只能贡献版本化 Schema。
- 首版不做空闲自动退出、开机自启动、Windows Service。
- 不承诺防御同 OS 用户下已有任意代码执行的恶意软件。

### 1.3 发布支持矩阵

| 平台 | 首发状态 | 交付物 | 声明边界 |
| --- | --- | --- | --- |
| Windows x64 | 首发真实产品包 | per-user WiX MSI + bootstrapper exe；含 core、launcher、tray/picker、mfctl、Web assets、Broker/Runtime/Install hosts | 支持声明以真实包验收为门槛 |
| Windows arm64 | 后置 | 无 | 不得声明支持 |
| macOS | 契约先行 | CI 编译 + fake installer/broker/host 契约测试 + 平台 trait | 真实签名/notarize 包后置；不得声明支持 |
| Linux | 契约先行 | 同上（UDS/polkit trait） | 同上 |

- Windows 最低版本、架构与包格式见附录 A8（fixed）：Windows 10 20H2（build 19042）与 Windows 11、x64、per-user WiX MSI + bootstrapper、side-by-side 版本目录 + `current.json` 指针、无 Service/自启动。
- 浏览器：Edge / Chrome / Firefox 当前版本与往前两个大版本（附录 A8，fixed）；WebGL 渲染可选，DOM renderer 兜底。
- 权限模型：Core/launcher/tray/picker 全部 asInvoker；仅 Broker（及由其拉起的 hosts）带 `requireAdministrator`；不注册 Service、不写自启动。

---

## 2. CoreKernel 深模块与 crate/进程边界

### 2.1 进程拓扑

```text
默认浏览器（Web Interaction Client）
    │ HTTP / Workflow WS / Terminal WS（loopback only）
    ▼
monkeyfence-core                 每 OS 用户唯一、普通权限、无产品 UI
    ├─ CoreKernel                唯一命令/状态接口（§2.2）
    ├─ Project Registry（service-v1.db）
    ├─ per-project Store + Orchestrator（Project v7）
    ├─ Catalog / Plugin / Secret / CLI Installation（catalog-v2 + 现有 Secret Store）
    ├─ SessionRegistry（所有 Agent Session 的逻辑 owner）
    ├─ SessionRuntime + PTY + TerminalJournal/Transcript（mf-terminal）
    ├─ ProjectionHub + event journal
    ├─ WebGateway + embedded hashed assets
    └─ RunControl IPC（mfctl，Named Pipe / UDS）

monkeyfence-launcher             短生命周期：start/open/status/stop
monkeyfence-tray                 极薄 companion：打开、摘要、安全退出（不拥有 Core、无第二 UI、不自启动）
monkeyfence-picker               按需短生命周期原生目录选择 helper

按需独立进程
    ├─ mf-broker（Elevated Broker，Root Mode 生命周期内）
    ├─ mf-root-host（session-scoped，Root PTY/进程组物理 owner）
    ├─ mf-install-host（job-scoped，安装进程物理 owner）
    ├─ Plugin Worker（NDJSON 协议）
    └─ Agent CLI（普通或 elevated）
```

`mfctl` 继续作为 Agent Run 内部 Settlement/控制工具，不与 launcher 合并。关闭浏览器标签或 tray 不停止 Core；只有显式安全退出（§11.4）才终止。

### 2.2 CoreKernel 接口（唯一深模块缝隙）

所有字段私有；WebGateway、legacy GPUI adapter、launcher/tray 本地 IPC、测试 harness 都只能调用：

```rust
pub trait CoreKernel: Send + Sync {
    fn dispatch(&self, cmd: CommandEnvelope) -> Result<CommandOutcome, KernelProblem>;
    // CommandOutcome = Applied { revisions } | Accepted { operation_handle }

    fn snapshot(&self, q: SnapshotQuery) -> Result<SnapshotEnvelope, KernelProblem>;
    fn subscribe_events(&self, cursor: EventCursor) -> Result<EventSubscription, KernelProblem>;
    fn attach_terminal(&self, session: SessionHandle, attach: TerminalAttach)
        -> Result<TerminalChannel, KernelProblem>;
    fn shutdown(&self, intent: ShutdownIntent) -> ShutdownAssessment;
}
```

「后台操作（Operation）」在本文件中指 `dispatch` 返回 `202 accepted` 的长任务句柄（如 CLI 安装、Workflow Run 启动、模型探测）：结果经 `GET /api/v1/operations/{operation_handle}` 与 workflow events 投影，跨 Core 重启由 target receipt 恢复（§4），不阻塞命令响应。

进入 CoreKernel 后共享（transport 只差认证方式）：Controller/Observer role 与 lease、aggregate revision/CAS、command idempotency 与后台操作（Operation）、opaque handle 与 Project scope、Secret/capability/redaction、audit 与 projection publication barrier。本地 legacy GPUI 不是「可信内置调用」：拆进程后经当前用户 ACL 保护的 versioned local transport（`mf.legacy-transport.v1`，NDJSON over Named Pipe/UDS）注册为普通 Client。

### 2.3 状态机（Core 所有权）

```text
starting → acquiring_owner_lock → owning（服务客户端）
owning → freezing（拒绝新 command / Agent Session / Installation Job / Root Mode enable；旋转 Controller/Root/writer epoch）
freezing → draining（publication barrier 上等已线性化命令、PTY input queue、outbox、可中断 Operation drain）
draining → stores_closed（flush Transcript/outbox/receipt；关闭全部 Store 句柄）
stores_closed → handed_off（写 handoff manifest、释放 CoreOwnerLock；永不再自行 reopen）
```

新 Core：`acquiring_owner_lock → validating(handoff/DB owner epoch/schema/bundle) → owning`；校验失败保持停止并输出恢复诊断，仅在 `handoff_reacquire_window_ms` 窗口内允许 Bridge A 或 schema-compatible 前一 bundle 以更高 owner epoch 重取。

### 2.4 所有权与线性化点

- 所有权：Core 独占全部业务 Store/调度/PTY/插件/Secret；`SessionRegistry` 是全部 Agent Session handle、Workflow Run 关联、writer lease、事件与 transcript 的逻辑 owner；普通 PTY 物理 owner 是 Core 内 SessionRuntime，Root PTY 物理 owner 是 session-scoped Elevated Runtime Host（§10）。
- 线性化点（全局枚举，后文引用）：
  - **L-CMD**：目标 Store 事务提交（业务效果 + target-local receipt + outbox 同一事务；事务内复验 lease/CAS/capability）。
  - **L-TAKEOVER**：service DB Controller epoch 单调递增提交。
  - **L-INPUT**：字节进入该 Agent Session 单线程有序 PTY 写队列时的原子复验（Controller epoch + writer lease）。
  - **L-PUBLISH**：publication barrier 上的 journal seq 分配与 append；Snapshot cursor 同 barrier 读取。
  - **L-ROOT**：Broker/host 启动高权限进程时的 root epoch 复验；下游每个未启动节点在 launch 时复验 active root epoch。
  - **L-OWNER**：CoreOwnerLock 原子 acquire + discovery 更新。
  - **L-SWITCH**：安装 receipt 写入 + staging→受管目录原子切换。

### 2.5 崩溃恢复

Core crash：生成新 `server_instance_id` 与 `stream_epoch`；session/nonce/CSRF/controller epoch 全部失效；Root Mode 强制关闭；未终结 Operation → `reconciling`；transcript 恢复到 `durable_through_seq` 且 `complete=false`；live PTY → lost/Needs You。恢复细节见各模块与附录 B。

### 2.6 安全

Core 不接受任意 path/PID/command 作为授权目标；全部资源经 opaque handle。认证差异（Web cookie/CSRF、本地 transport OS ACL）在 transport 层完成，进入 kernel 后能力判定一致。

### 2.7 可观测性

结构化日志（脱敏）至 `~/.monkeyfence/logs/core.log`，按 `log_rotate_max_bytes`/`log_rotate_keep` 轮转（附录 A10）；metrics 至少：command p95/p99、journal append 延迟、outbox 深度、ws 客户端数与队列水位、pty_drain_ms、ack outstanding bytes、install 时长、snapshot p95；全部 problem 带 `trace_id`。

### 2.8 验收

- 仓库级检查（CI grep/clippy lint 或独立 audit 测试）：GPUI/Companion/测试 harness 之外不存在对 Store/Orchestrator/SessionRegistry/raw PTY 的直接 mutation 引用；全部写路径经过 `CoreKernel::dispatch`。
- `attach_terminal` 的调用者拿不到 `PtyMaster`/raw writer；`send_prompt_raw` 类旁路删除。
- CoreKernel contract tests（§14.2）通过。

### 2.9 落点

新 crate `crates/mf-kernel`：`src/kernel.rs`（facade）、`src/handles.rs`、`src/command.rs`、`src/operation.rs`、`src/projection.rs`、`src/journal.rs`、`src/lease.rs`、`src/singleton.rs`、`src/shutdown.rs`、`src/run_control.rs`（自 `crates/mf/src/pipe_server.rs` 迁入）、`src/legacy_transport.rs`、`src/config.rs`、`src/limits.rs`；bin `src/bin/monkeyfence-core.rs`；`tests/contract/*`。

---

## 3. 存储：Project Store v7、Catalog v2、service-v1——schema、迁移与 future guards

### 3.1 总原则

- 权威数据不重建、不双写：`.mf-agent/workflow-v1.db`（每 Project）、Task/Pipeline Revision/Step/Agent Run/Settlement/Handoff、Agent Instance snapshot、Plugin pin、Execution Lease、加密 Secret 原样保留。
- Project v6→v7 只 expand（加列/加表），不 contract；Catalog 一次性迁入新文件 `catalog-v2.db`，v1 保留只读备份；旧代码（pre-Bridge）不得把 v2 的 `user_version` 降回。
- 所有 schema 升级前用 SQLite Backup API 生成一致备份 + manifest；不裸拷贝打开中的数据库。
- future guard：任一 Store 打开时 `user_version > 已知版本` → 拒绝打开（`schema_future_version`），各库都要有测试。

### 3.2 Project Store v7（`user_version` 6→7，expand-only）

当前 Project DB 没有 `projects` 表；也不得引入「table + rowid → handle」映射（handoff 明确禁止易漂移 rowid 映射，且 `graph_json` 内嵌节点没有 rowid）。持久 opaque handle 一律落在所属表的 `public_handle` 列，内嵌节点/边用独立 identity 表：

| 对象 | 形态 | 内容 |
| --- | --- | --- |
| `project_meta` | 新表（singleton） | `id INTEGER CHECK(id=1)`、`workflow_collection_revision INTEGER NOT NULL DEFAULT 1`；工作流 create/delete 的 collection CAS 与目标工作流行在同一 Project Store 事务（L-CMD）内完成 |
| `project_workflows` | ALTER 加列 | `public_handle TEXT NOT NULL UNIQUE`（UUIDv7，持久 aggregate handle，永不复用）、`semantic_revision INTEGER NOT NULL DEFAULT 1`、`presentation_revision INTEGER NOT NULL DEFAULT 1` |
| 其他持久 aggregate 表（Workflow Run、Agent Session、Task 等现有表） | ALTER 加列 | `public_handle TEXT NOT NULL UNIQUE`；缺 `revision` 的补 `revision INTEGER NOT NULL DEFAULT 1` |
| `workflow_node_identity` | 新表 | `(workflow_handle, node_key, node_handle UNIQUE)`——`graph_json` 内嵌节点没有 rowid，`node_key` 直接使用既有稳定 `WorkflowNodeDraft.key`；节点删除/工作流删除在同一事务清理 identity 行 |
| `workflow_edge_identity` | 新表 | `(workflow_handle, upstream_node_key, downstream_node_key, edge_handle UNIQUE)`，并对前三列加 UNIQUE；现有 DAG 以 downstream `deps` 中的每个 upstream/downstream 键对回填身份，新建连线时创建、断开时删除，重新连接生成新 handle（handle 永不复用） |
| `workflow_presentation` | 新表 | workflow handle → viewport、折叠、布局元数据（JSON） |
| `node_position` | 新表 | node handle → `(x REAL, y REAL)`；旧 GPUI 无坐标，Web 首次打开用确定性 Dagre 布局，随后由 presentation command 写入，不伪造迁移前坐标 |
| `command_receipt` | 新表 | target-local receipt：`command_id UNIQUE、semantic_digest、aggregate_handle、result_revisions、state、created_at、finalized_at` |
| `projection_outbox` | 新表 | `outbox_id INTEGER PRIMARY KEY AUTOINCREMENT、event_json、published_at NULL`——只有 store-local `outbox_id`，不预存全局 seq；全局 stream seq 仅在 publication barrier（L-PUBLISH，§5.2）分配 |
| `terminal_transcript` | 新表 | `session_handle PK、terminal_epoch、final_state(live|complete|crash_incomplete|lost)、durable_through_seq、exit_code、exit_signal、as_of_seq` |
| `terminal_transcript_segment` | 新表 | `(session_handle, seq_start, seq_end, bytes BLOB)`——已脱敏输出；输入永不持久化 |

迁移与 GC：v6 库打开 → Backup → 单事务 CREATE/ALTER + 为现存工作流图回填 node/edge identity（节点使用既有 `WorkflowNodeDraft.key`；边使用每个 downstream `deps` 的 upstream/downstream 键对；两者生成 handle）+ `user_version=7`；幂等（中断重跑无害）；v6 业务数据零重写。Task/Pipeline Revision/Step/Agent Run/Settlement/Handoff/Plugin pin 迁移前后等价由 T0 golden fixtures 证明。identity 行随节点/边/工作流删除同事务清理，不留孤儿 handle；handle 永不复用。`SessionRegistry` 不再使用 `project path + rowid` 组合 key，改用持久 `public_handle`。

### 3.3 Catalog Store v2（新文件 `~/.monkeyfence/catalog-v2.db`）

- schema 版本常量 `CATALOG_V2_SCHEMA_VERSION = 1`（新库新版本链），含 future guard。
- 表：`agent_type_catalog`、`cli_installations`（`installation_handle PK、agent_type_id、executable_path、canonical_path UNIQUE、actual_version、source(external|managed)、scope、health、receipt_handle?、detected_at`）、`installation_receipts`（不可变收据，§9.6）、`installation_jobs`、`provider_profiles`（含 `secret_ref`、`revision`）、`provider_model_cache`、`plugin_pins`（沿用语义，与 receipt 分表）、`command_receipt`/`projection_outbox`（catalog scope 镜像，同样只有 store-local `outbox_id`）。
- 迁移：从 `catalog-v1.db` 幂等导入（Agent Instance、模板、Secret 引用、插件锁），v1 只读保留并写 migration marker；Secret ciphertext 继续留在现有 Secret Store（keyring/AES-GCM），不复制。
- 当前 `crates/mf-agent/src/catalog_store.rs` 的 Catalog init 缺 future-version guard——T1 修复并补测试。

### 3.4 service-v1.db（新增，`~/.monkeyfence/service-v1.db`）

跨项目协调状态：

| 表 | 内容 |
| --- | --- |
| `meta` | instance identity、schema 版本、owner epoch 低位水位 |
| `project_registry` | `project_handle PK、public_id、canonical_root UNIQUE、display_path、registered_at、status(registered|missing)` |
| `command_intent` | `command_id PK、semantic_digest、target_store、aggregate、principal、client_id、controller_epoch、root_epoch?、state(reserved|applied|failed|cancelled)、created_at、resolved_at` |
| `operation` | `operation_handle PK、command_id FK、kind、state、saga_state、progress_json、created_at、updated_at` |
| `audit` | append-only：Root 开关、授权结果、安装 provenance、owner handoff、强制终止等；只存脱敏摘要 |
| `root_state` | 当前行 `id=1`：mode、root_epoch、enabled_at；Core 启动时强制 `mode=off`（历史 epoch 进 audit） |
| `durable_feature` | `feature PK、min_reader_version、writer_enabled_at`——reader-before-writer 注册表（§13.4） |
| `migration_marker` | session.json→Project Registry、catalog v1→v2 等幂等标记 |

`stream_epoch` 与 workflow event journal 是进程内状态，不从 service DB 恢复；Core 重启生成新 epoch。持久 opaque id 放在其所属 Project/Catalog Store 的 `public_handle` 列，不建易漂移 rowid 映射表。

### 3.5 session.json 迁移

只导入 Project 列表与可用 foreground Project 到 `project_registry`；`open_files`、active file、GPUI panel/layout 不迁移；原文件保留并写 marker，导入幂等；缺失路径保留为 `missing` 状态供用户清理，不删真实目录。

### 3.6 状态机（迁移）

```text
未迁移 → backup_ok → migrated(marker) ；任意步失败 → 保持未迁移 + 诊断（旧数据不动）
```

### 3.7 崩溃恢复 / GC

- 多库打开顺序：service-v1 → Project Stores → Catalog v2；任一 future-version 拒绝则 Core 启动失败并输出诊断，不部分服务。
- receipt/Operation/audit GC 按 §4.6 与附录 A4；transcript GC 按 §8 与附录 A2（`transcript_retention_days`、`transcript_project_cap_bytes`）；活动 Workflow Run / replay lease pin 的不清理。

### 3.8 安全 / 可观测性 / 验收 / 落点

- 安全：数据库文件当前用户 ACL；备份与日志不含 Secret 明文（Secret 只在 Secret Store ciphertext）。
- 可观测性：每库 schema 版本、迁移耗时、identity 回填数量、outbox 深度、GC 收益记入日志与 metrics。
- 验收：v6→v7、catalog v1→v2、session.json 三条迁移幂等（跑两遍结果一致）；future-version guard 拒绝测试；迁移前后 Task/Revision/Agent Run/Settlement golden 等价；identity 回填与删除 GC 测试；Backup API 路径测试。
- 落点：`crates/mf-agent/src/schema.rs`（版本常量与 guard）、`crates/mf-agent/src/store.rs`（v7 DDL/identity/receipt/outbox/transcript 表）、`crates/mf-agent/src/catalog_store.rs`（v2 新文件与迁移）、`crates/mf-kernel/src/project_registry.rs`（service DB 访问）；契约测试 `crates/mf-agent/tests/contract/schema_guards.rs`、`crates/mf-kernel/tests/contract/migration_idempotency.rs`。

---

## 4. 命令：service intent + target-local receipt/outbox + Operation 恢复

### 4.1 接口

`POST /api/v1/commands` 收 `mf.command.v1` envelope（§7.4），kernel `dispatch` 内执行两阶段 intent：

```text
1) service DB：按 command_id + semantic_digest 原子保留 intent（target store/aggregate、
   principal、client_id、controller_epoch、可选 root_epoch）；同 id 不同 digest/target → command_id_reused
2) 目标 Store 事务（L-CMD）：复验 principal/Controller lease/expected revision/root epoch；
   失效且未线性化 → intent 终结 controller_lease_expired / root_epoch_expired / cancelled；
   成功 → 同一领域事务写业务效果 + target-local receipt + projection outbox
3) coordinator 依 target receipt 完成 service intent/Operation；ProjectionHub 从 outbox 发布事件
4) crash 恢复：step1 后无 receipt → reconciler 终结 intent（epoch 已失效，不执行旧命令）；
   step2 后有 receipt → 只补 service 结果与事件发布，绝不重放业务写
5) 跨多 Store 的动作 = 带幂等 step receipt 的 Operation saga；失败进入可观察 compensation/Needs You
```

canonical request digest 只覆盖 `schema/type/target/expected/payload` 规范编码，排除 cookie/CSRF、client id、lease epoch、trace id、时间；每次重试先复验当前认证与 lease，再查幂等 receipt（旧 client 不能借 command id 绕过新 lease）。Secret 创建/替换命令的特例（write-only 明文输入、HMAC 幂等、禁 access-log/cache/回显）见 §7.4。

### 4.2 状态机（command intent / Operation saga）

```text
intent: reserved → applied | failed | cancelled | revoked(lease/epoch 失效)
operation: accepted → running(step receipts…) → completed | compensating | needs_you
```

长 Operation 只在 step2 目标事务创建成功后算 `202 accepted`；Root Operation / Root Workflow Run 的未启动 step 每次 launch 复验 active root epoch（§10.1）。

### 4.3 线性化点

L-CMD（§2.4）。HTTP 请求在进入 Store 事务的线性化点复验 lease，而不是只在路由入口。

### 4.4 崩溃恢复

见 4.1 步骤 4；同 command id 跨断线/跨重启幂等；Operation handle、receipt、install plan execution record 跨重启稳定，重启进入 `reconciling` 而不是换 handle。Root 等进程生命周期能力的旧结果携带原 `server_instance_id/root_epoch`，重启后不可解释为仍有效；需要新语义动作必须新 command id。

### 4.5 Schema 与迁移

`command_intent`/`operation`（service-v1）、`command_receipt`/`projection_outbox`（Project v7 与 Catalog v2）。无旧表对应，T1 新建。

### 4.6 Retention 与 GC

`receipt_retention_days`、`receipt_max_rows_per_store`、`operation_retention_days`、`audit_retention_days`、`gc_interval_ms` 见附录 A4；未终结 Operation 或被审计引用的 receipt 不清理。

### 4.7 安全 / 可观测性 / 验收 / 落点

- 安全：digest 排除凭据；receipt 只存 HMAC 与脱敏结果；审计不含 Secret/argv 原文。
- 可观测：intent 队列深度、outbox 深度、reconcile 数、saga 补偿数。
- 验收：crash 注入（step1 后/step2 后以 fault harness 终止 Core，§14.2）恢复正确；同 id 同 digest 幂等、异 digest 拒绝；跨 Store saga 有补偿测试；retention/GC 契约测试。
- 落点：`crates/mf-kernel/src/command.rs`、`src/operation.rs`；常量在 `src/limits.rs`；测试 `crates/mf-kernel/tests/contract/{command_idempotency,intent_recovery,operation_saga,retention_gc,limits_defaults}.rs`。

---

## 5. ProjectionHub、Snapshot barrier 与 workflow event journal

### 5.1 接口

- `snapshot(SnapshotQuery) -> SnapshotEnvelope`：`mf.snapshot.v1` 统一 envelope（§7.3）；多资源 Workspace Snapshot 不得伪造顶层统一 revision，每个 aggregate 自带 revision；单资源同构。
- `subscribe_events(EventCursor) -> EventSubscription`：`mf-workflow.v1` 全局跨项目流。

### 5.2 Publication barrier（线性化）

一次领域事务顺序：`validate/CAS → Store commit → append event journal → fan-out`。CAS、Controller/Root lease 复验、Store commit、journal seq 分配/append 与 epoch rotate 位于同一串行 publication barrier（L-PUBLISH）；Snapshot cursor 同 barrier 读取，不可能观察到「Store 已提交但 through_seq 仍指旧投影」。事务提交后 journal publication 失败 → 旋转 `stream_epoch` 并强制全部客户端 resync。

### 5.3 Journal 语义、容量与 fail-closed

- journal 是有界进程内投影恢复流（不是 Event Sourcing；权威状态在 Store）。
- 容量由 `journal_max_events`/`journal_max_bytes` 定义（默认/hard cap 见附录 A1，hard cap 不可逾越）。GC 在容量内驱逐最旧事件；容量允许时不驱逐 `journal_min_age_secs` 窗口内事件（**目标窗口**，非无条件承诺）。
- **fail-closed**：当事件洪泛使「保留目标窗口」与「上限」不可兼得时，journal 到达上限立即旋转 `stream_epoch` 并对所有客户端发 `resync_required` 后关闭——绝不超上限、绝不静默丢弃事件。
- resume window 定义为「journal 当前覆盖范围」（hello 暴露 `first_available_seq`）：覆盖内可 `resume(after_seq)` 增量恢复；`journal_min_age_secs` 只是容量允许时的目标下限。
- 单事件上限 `journal_event_max_bytes`：超限投影不得整包塞事件，改发 `projection_critical` 的 resync 指引。
- 每客户端 send queue：`client_event_queue_max_events`/`client_event_queue_max_bytes`；超限 → `resync_required` + close 4409，绝不阻塞其他客户端、Scheduler 或 PTY reader。
- WS keepalive：server ping `events_ws_ping_interval_ms`，空闲超时 `events_ws_idle_timeout_ms`；ping 不消耗领域 seq。
- 恢复协议：auth exchange → Snapshot(保存 stream_epoch+through_seq) → WS `events.resume.v1(stream_epoch, after_seq)` → hello → 补发 `seq > through_seq`；epoch 不匹配或 gap → `resync_required` 关闭，客户端重取 Snapshot。

### 5.4 事件 envelope（`mf.event.v1`）

```json
{
  "schema": "mf.event.v1",
  "stream_epoch": "opaque",
  "seq": "1843",
  "occurred_at": "2026-09-01T08:00:00Z",
  "aggregate": { "kind": "project_workflow", "handle": "opaque" },
  "base_revision": { "semantic_revision": "12", "presentation_revision": "91" },
  "aggregate_revision": { "semantic_revision": "12", "presentation_revision": "92" },
  "caused_by_command_id": "uuid-or-null",
  "type": "workflow.node_position_set",
  "projection_critical": true,
  "projection": {
    "mode": "typed_delta",
    "delta_type": "workflow.node_position_set",
    "data": { "node_handle": "opaque", "x": 420, "y": 180 }
  }
}
```

- `projection.mode` ∈ `replace | tombstone | typed_delta(封闭白名单)`；禁止 JSON Patch。typed delta 携带完整新 revision 与客户端必须已持有的 `base_revision`；未知 delta_type、base 不匹配或 revision 不连续 → 立即 resync。
- Project Workflow 事件携带 semantic+presentation 双 revision vector；普通 aggregate 单 `revision`。tombstone 带删除前最终 revision；handle 永不复用。
- 超过 JS safe integer 的 seq/epoch/revision 在 JSON 用十进制字符串。
- v1 可见性域 = 该 OS 用户全部已登记 Project 的单一全局流，不过滤隐藏事件、不制造 seq 空洞；未来 Project ACL 必须引入 per-visibility stream 或 watermark 协议（非本版）。
- 未识别且 `projection_critical=true` 的事件必须 resync，不得忽略后显示旧状态。
- Web 收到 `run.step_changed` 只应用权威投影，不自行解锁下游。

### 5.5 状态机（journal/client）

```text
journal: appending → rotated(epoch+)   // commit 后 publication 失败或容量 fail-closed
client: snapshot_ok → resumed(live) | resync_required(→snapshot_ok)
```

### 5.6 崩溃恢复

Core crash → 新 epoch；恢复器以 target receipt/outbox 为权威完成旧 epoch 未发布事件的 reconcile（标 `reconciled`），不向新 epoch 重放 Snapshot 已包含的陈旧 delta。

### 5.7 安全 / 可观测性 / 验收 / 落点

- 安全：事件不含 Secret/MF_RUN_TOKEN/capability；跨项目猜测 handle 只得到统一 `resource_not_found`。
- 可观测：journal 深度/驱逐速率、first_available_seq、每客户端队列水位与 resync 率、epoch 旋转次数（按原因分类）。
- 验收：并发命令下 barrier 不丢更新；commit 后 journal fault 旋转 epoch；同 epoch replay / epoch mismatch / journal gap / 慢客户端均确定结果；fan-out 附加延迟与 journal append p99 达标（附录 A9）；**事件洪泛把 journal 推到上限时 fail-closed（rotate + 全客户端 resync），任意时刻容量从未超过上限**（故障/洪泛注入测试）；容量允许时 `journal_min_age_secs` 目标窗口成立（目标测试）。
- 落点：`crates/mf-kernel/src/projection.rs`、`src/journal.rs`、常量 `src/limits.rs`；测试 `crates/mf-kernel/tests/contract/{journal_recovery,barrier_consistency,journal_limits,journal_overflow,limits_defaults}.rs`；WS 端点实现在 `crates/mf-web/src/ws/events.rs`，golden 事件 fixtures 在 `crates/mf-web/tests/fixtures/events/*.json`。

---

## 6. Web Gateway：bootstrap、auth、安全头

### 6.1 Loopback 与同源

- 仅绑定随机端口的 `127.0.0.1` 与 `::1`；不监听 `0.0.0.0`/LAN；发行版只打开 IP literal URL，不依赖可重绑定 hostname。
- 每个 HTTP 请求严格校验 `Host` = 当前绑定 IP literal + port；UI WebSocket 精确匹配 `Origin`，缺 Origin 的浏览器入口拒绝；不开放宽泛 CORS、不接受公共站 preflight；Local Network Access 浏览器权限不作为认证。
- 非浏览器本地 CLI（mfctl/launcher/tray）使用独立本地 IPC，不复用 UI WebSocket。

### 6.2 Bootstrap 流程

```text
1) launcher 从用户级 discovery 文件读 instance identity + port（文件仅当前 OS 用户可读）
2) 每次打开 Web UI 生成一次性 128-bit bootstrap nonce，放在 URL fragment（不进 HTTP 请求/日志/Referer）
3) 首屏 JS 从 fragment 取 nonce → POST same-origin /auth/exchange → 立即 history API 清除 fragment
4) Core 消耗 nonce 一次 → 设置 HttpOnly、SameSite=Strict、Path=/ 的 mf_session cookie
   → 返回仅存页面内存的 CSRF token + client id + 协议元数据
5) 写 HTTP 命令要求 cookie + 精确 Origin/Host + CSRF header + client id + Controller Lease Epoch
6) WS upgrade 要求同一 cookie、Origin、client id 归属与精确 subprotocol；token 不放 query string
7) Core 重启：session、nonce、CSRF、client id、lease epoch 全部失效
```

`mf.auth-bootstrap.v1` 响应：`client_id`、`csrf_token`、`controller{role,lease_epoch}`、`api_versions:["v1"]`、`ws_subprotocols:["mf-workflow.v1","mf-terminal.v1"]`、`core_build`、`ui_asset_build`。Web 先求版本交集；无交集不发起 WS upgrade（浏览器在 upgrade 失败时读不到 problem body，升级提示由 bootstrap/meta 驱动）。

### 6.3 响应头（发行版最低集）

```text
Content-Security-Policy:
  default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:;
  font-src 'self'; connect-src 'self' ws://127.0.0.1:<port> ws://[::1]:<port>;
  object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Resource-Policy: same-origin
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Permissions-Policy: camera=(), microphone=(), geolocation=(), payment=()
Cache-Control: no-store          # index/auth/API；hash asset 可 immutable
```

Web bundle 与 Core 同版本内嵌、内容哈希命名、离线可用；无 CDN、无 Node runtime、依赖 lockfile 锁定。

### 6.4 Controller/Observer 模型

- 每用户一个 Controller + 多 Observer；新 bootstrap client 成为 Controller（旧端降只读但不断开、不关闭旧连接）；同 client 重连仅当 Core 仍记录其为 Controller 才恢复写权限。
- Observer 可显式 `POST /api/v1/controller/takeover`：需该已认证 observer 的 CSRF + 最后观察到的 controller lease epoch，CAS 成功 epoch+1；失败 `controller_lease_expired`。takeover 使旧 HTTP 写、Terminal Writer Lease、尚未启动的手工 Root Agent Session / Installation Job 立即失效（L-INPUT/L-CMD 之后的不受影响）；已授权 Root Workflow Run 的后续节点按 §10.1 的 active root epoch 复验处理。
- Observer 可读 Snapshot/events/terminal output/进度/收据；不可写 DAG、终端输入/resize、Settlement、安装、Root Mode。

### 6.5 状态机（web session）

```text
nonce_issued → exchanged(session+CSRF+client) → (controller|observer) → closed
```

### 6.6 数值

见附录 A3：`bootstrap_nonce_ttl_secs`（单次）、`web_session_ttl_secs`（滑动/绝对，内存态，Core 重启失效）、`csrf_entropy_bits`（fixed）、`auth_exchange_rate_per_minute`、`install_plan_ttl_secs`（§9）。

### 6.7 安全 / 可观测性 / 验收 / 落点

- 验收（安全矩阵测试）：public origin、错误 Host/Origin、无 cookie/CSRF、Observer、旧 lease/epoch、DNS rebinding、LNA permission 全部不可绕过；nonce 重放/过期拒绝；Core 重启后旧 session 全部 401。
- 可观测：auth 失败分类计数、session 数、takeover 事件审计。
- 落点：`crates/mf-web/src/{gateway,auth,headers,assets}.rs`、常量 `src/limits.rs`；测试 `crates/mf-web/tests/contract/{security_matrix,auth_limits,headers,limits_defaults}.rs`。

---

## 7. Web API v1：DTO、命令、事件、problem 契约

### 7.1 路由表（最低集）

```text
POST /auth/exchange
GET  /api/v1/meta
GET  /api/v1/snapshots/workspace
GET  /api/v1/projects/{project_handle}/workflows/{workflow_handle}
GET  /api/v1/runs/{run_handle}
GET  /api/v1/agent-catalog
GET  /api/v1/installations/{installation_handle}
GET  /api/v1/operations/{operation_handle}
GET  /api/v1/sessions/{session_handle}/terminal-transcript
POST /api/v1/commands
POST /api/v1/controller/takeover
WS   /api/v1/events?client_id={opaque_client_id}        （mf-workflow.v1）
WS   /api/v1/terminal?client_id={opaque_client_id}      （mf-terminal.v1）
```

除静态 asset 与 `/auth/exchange` 外全部要求已认证 session（meta 不是匿名探测口）。`client_id` 非凭据，仅映射 cookie session↔bootstrap client；URL/query 不含 nonce/CSRF/lease/Secret/Broker capability。Terminal WS 的 URL 不含 session handle，live attach 目标只在升级后首个 control frame 中发送。所有资源引用为 Core 生成的 opaque handle（`wf_/run_/step_/sess_/inst_/op_/proj_` + UUIDv7 前缀风格），不接受 rowid/PID/任意 path/command；展示用 `display_path`/argv 摘要不可回用作命令目标；存在性差异对外统一 404 `resource_not_found`（内部可审计 `resource_scope_mismatch`）。

### 7.2 版本协商

HTTP major 在 `/api/v1`，WS major 在 subprotocol。v1 内只加可选字段/新资源/新非关键事件；改字段语义、幂等/CAS、terminal binary header 或恢复规则必须升 v2。Core/UI 通常同版本；协议保留至少一个迁移发布周期的明确拒绝/升级提示。

### 7.3 Snapshot envelope（`mf.snapshot.v1`）

```json
{
  "schema": "mf.snapshot.v1",
  "server_instance_id": "opaque",
  "cursor": { "stream_epoch": "opaque", "through_seq": "1842" },
  "data": {}
}
```

Workspace Snapshot 含项目列表、工作流摘要、活动 Workflow Run/Needs You、Agent Catalog 摘要；完整 DAG 由 per-workflow Snapshot 获取（大小/延迟预算见附录 A9）。

### 7.4 Command envelope（`mf.command.v1`）

```json
{
  "schema": "mf.command.v1",
  "command_id": "uuid-v7",
  "client_id": "opaque",
  "controller_lease_epoch": "17",
  "target": { "kind": "project_workflow", "handle": "opaque" },
  "expected": [
    { "aggregate": { "kind": "project_workflow", "handle": "opaque" },
      "revisions": { "presentation_revision": "91" } }
  ],
  "type": "workflow.move_node",
  "payload": { "node_handle": "opaque", "x": 420, "y": 180 }
}
```

- 命令族（封闭 Rust enum）：Project Workflow（create/rename/delete、add/update/remove/move node、connect/disconnect、viewport、unsafe-parallel policy）；Workflow Run（start/cancel、retry Step、respond、settle）；Agent Session（start/stop Preview Session / Ad-hoc CLI Session；terminal attach/input 不走此入口）；Catalog（refresh discovery、Provider model probe、Provider Profile、Agent Instance）；CLI（preview/install/update/repair/uninstall/cancel）；Root Mode（enable/disable）；Controller（takeover，特殊认证操作）。
- `expected` 恒为 aggregate 列表；会原子修改或以 Workflow Run / Step / Agent Session 为并发前提的命令必须列出每个已存在 aggregate 的 expected revision，遗漏或不匹配整条拒绝；新建对象无 expected，但父 aggregate 仍 CAS（如创建工作流 CAS Project `workflow_collection_revision`；删除同时 CAS collection 与目标 Workflow；rename 归入 presentation 轴）。
- `workflow.run` 只接受 Core 已确认的 `semantic_revision`；presentation 变化不阻止运行。
- 响应：`200 applied`（带最新 resource revision）/ `202 accepted`（返回 `operation_handle`）/ `409`（revision/command reuse/operation conflict）。浏览器超时不等于命令失败。
- 幂等：同 `command_id + canonical digest` 返回原结果；异 digest → `command_id_reused`；安装、运行、Settlement、Root enable 结果不确定时只能用原 command id 重试。
- write-only Secret 特例：创建/替换 Secret 的请求 body 可含明文（唯一例外）；请求禁 access-log/cache/回显，解析后立即交 Secret Store；semantic digest 中 Secret 字段替换为持久 service idempotency key 的 HMAC；receipt 只存 HMAC；响应/事件/日志/journal/Snapshot/receipt 永不含明文或可复用 secret ref。
- CLI 执行命令只能引用 Core 预览生成的短期 `install_plan_handle + recipe_digest + catalog_revision`（`install_plan_ttl_secs`，附录 A5），浏览器不能提交解析后的 argv；Root 命令只引用领域对象。

### 7.5 Problem（`mf.problem.v1`）

```json
{
  "schema": "mf.problem.v1",
  "code": "revision_conflict",
  "message": "工作流已被更新",
  "trace_id": "opaque",
  "command_id": "uuid-or-null",
  "retry": "after_resync",
  "current": { "semantic_revision": "13" }
}
```

`retry` ∈ `never | same_command_id | after_resync | after_reauth | after_retry_after`。稳定错误码全集：协议 `unsupported_api_version/unsupported_ws_subprotocol/invalid_envelope`；认证 `unauthenticated/origin_rejected/csrf_rejected`；角色 `controller_required/controller_lease_expired`；资源 `resource_not_found`（内部 `resource_scope_mismatch`）；CAS `revision_conflict/command_id_reused/command_in_progress`；DAG `validation_failed/workflow_cycle/unknown_dependency`；Agent `agent_instance_unavailable/plugin_version_unavailable/cli_version_mismatch`；Terminal `writer_required/writer_lease_expired/input_seq_conflict/terminal_epoch_mismatch/terminal_history_gap/frame_too_large/rate_limited`；Root/安装 `root_mode_required/root_epoch_expired/root_authorization_denied/broker_unavailable/elevation_required/installation_failed`；服务 `resync_required/service_unavailable/internal_error/schema_future_version`。HTTP status 表达大类，`code+retry` 才是客户端分支依据。

WS close code：4400 invalid envelope、4401 unauthenticated、4403 role/lease、4409 resync/history gap、4413 frame too large、4429 rate limited、4500 internal。命令速率 `command_rate_per_client`（附录 A1）→ 429 `rate_limited`。

### 7.6 安全 / 可观测性 / 验收 / 落点

- 验收：envelope/错误码/状态码/close code golden contract tests；命令族枚举与 `expected` 语义全覆盖；opaque handle 猜测统一 404；1000 节点 Snapshot/事件风暴/终端洪泛互不饿死（附录 A9 预算）。
- 落点：`crates/mf-web/src/api/{mod,snapshot,commands,operations,catalog,terminal_transcript}.rs`、`src/problem.rs`、`src/ws/{events,terminal}.rs`；golden fixtures `crates/mf-web/tests/fixtures/{commands,events,problems}/*.json`；前端协议类型 `web/src/api/*.ts` 与 golden 对齐测试 `web/src/api/__tests__/protocol.golden.test.ts`。

---

## 8. SessionRuntime、TerminalJournal/Transcript 与 Terminal v1

### 8.1 接口与数据面

每 WS 一个 Agent Session attach；真实 PTY 每次一个 `terminal_epoch`。连接顺序：

```text
Client → JSON attach(session_handle, terminal_epoch?, after_seq)
Server → JSON hello(terminal_epoch, first_available_seq, next_seq, alive, writer_state, cols, rows, limits)
Server → binary output replay
Client → JSON ack(through_seq)              # 仅在 xterm.write callback 完成后
Client → JSON request_writer
Server → JSON writer_granted(writer_lease_id, ttl_ms, renew_after_ms)
Client → JSON resize + binary input
Client → JSON writer_renew
Client → JSON release_writer?
Server → JSON input_ack / writer_renewed / writer_revoked / exit
```

binary frame 固定 32-byte network-order header：

```text
0..4    magic = "MFT1"
4       kind: 1=output, 2=input；3..255 保留（v1 不发送 checkpoint）
5       flags
6..8    reserved = 0
8..16   seq: u64 big-endian
16..32  writer_lease_id: UUID bytes；output 全 0
32..    raw bytes
```

control frame（UTF-8 JSON，统一含 `schema/type`）最低集合：`terminal.attach.v1 / hello.v1 / ack.v1 / request_writer.v1 / writer_granted.v1 / writer_denied.v1 / writer_revoked.v1 / release_writer.v1 / writer_renew.v1 / writer_renewed.v1 / input_ack.v1 / resize.v1 / exit.v1 / problem.v1`。未知 control type → `invalid_envelope` 关闭。Server 必须拒绝 permessage-deflate。Server 发起并统计 ping/pong。attach 必须在升级后 `attach_timeout_ms` 内到达，否则 4400 关闭；Server 先收到合法 attach 才发送 PTY 数据。

### 8.2 输出管线、seq 与 ACK

```text
raw PTY → streaming redactor(跨 chunk) → seq 分配 → 内存 replay ring + live fan-out
                                     → 周期 durable flush → Transcript Store
```

- output seq 属于 `terminal_epoch`，从 1 开始，在跨 chunk redaction 之后、journal/fan-out 之前分配；每条 binary output 消息占一个连续 seq。空会话 `next_seq=1、last_seq=0、first_available_seq=1`。
- ACK cumulative，只确认已发送且 xterm 已消费的连续 seq；`after_seq > last_seq` 或 ACK 高于该 client 已发最高 seq → 协议错误关闭；重复/旧 ACK 幂等忽略；ACK 释放至 through_seq 的 outstanding byte budget（`outstanding_output_max_bytes`，pause/resume 水位为其派生比例，附录 A2；超限暂停该 client，`slow_client_grace_ms` 未排空 → 4409 关闭）。
- PTY reader 永远独立任务持续 drain（单次因客户端反压阻塞受 `pty_drain_max_block_ms` budget 约束）；ACK 只限单客户端 send queue，绝不反压 ConPTY/PTY reader 或 Scheduler。

### 8.3 恢复与 history gap

- 同 terminal epoch 且 ring 覆盖 → 增量 replay；刷新/断线重连必须支持。
- `after_seq < first_available_seq - 1` → `terminal_history_gap`（含 first/last/as_of）+ close 4409；该连接不得申请 writer。Web 改读 `GET /terminal-transcript`（脱敏、UTF-8/行边界安全的只读投影 + `as_of_seq` + `complete`），并提示用户显式重启 Agent Session（新 PTY/新 epoch）恢复可写终端。
- 新 PTY 必须新 `terminal_epoch`，不得跨进程复用 seq。Core 重启后：普通 PTY 一律标记 lost/Needs You（无跨进程 live 重附着路径，§1.2）；唯一既定例外是 §10.3 Root host 的窄化 read-only reattach 明确成功时，保留只读输出，不伪装 live writer。
- v1 不发送 checkpoint/kind 3；只有未来协议定义并验证完整 VT state format、能力协商与 ACK 语义后才能启用。

### 8.4 Writer lease、input、resize

- lease 绑定 `client_id + controller_lease_epoch + WS 连接 + session_handle`，不可转移；仅当前 Controller 可申请。`writer_lease_ttl_ms`/`writer_lease_renew_after_ms`（附录 A2），续租复验 Controller epoch；新 Controller、连接关闭、显式 release、超时未续 → 撤销。`release_writer` 幂等，重复 release 返回相同终态不误伤新 lease。
- input：每 lease `input_seq` 从 1 单调；同 seq 同 payload digest 重发幂等返回原 `input_ack`；同 seq 异 payload → `input_seq_conflict` 撤销并关闭；乱序返回 expected seq 不写入。L-INPUT：进入 Agent Session 单线程有序写队列时原子复验 Controller epoch + writer lease；takeover 不撤销此前已线性化字节。
- 断线/网络不确定时**绝不自动重放未确认 input**（防重复执行命令/审批/`/xxx`）。
- `input_ack` 仅在底层 PTY writer 完整 `write_all` 成功后发送；部分写/错误不 ACK、撤销 writer、返回 terminal problem。
- resize 仅 writer 可发：单调 resize seq、cols/rows 边界与速率上限见附录 A2（fixed 边界；服务端在合并窗口内取最新）；陈旧值丢弃。attach/Observer 的浏览器尺寸不改 PTY（本地 fit/letterbox）。resize 实现进入 `pty_spawn` 平台抽象（Windows `ResizePseudoConsole`；Unix `TIOCSWINSZ`），不是只改 Screen。
- `/model`、`/skills`、审批、TUI、IME、Unicode 都是原始输入/VT 输出，Web 不解释命令。

### 8.5 Exit 与 durable-before-notify

PTY EOF/exit → redactor `finish()` → 最后脱敏字节分配 seq 写 journal → transcript through `final_seq` + exit metadata 原子 durable commit → 成功后才 fan-out 最后输出并串行发送 `exit(final_seq, code, signal)`；无输出 `final_seq=0`；持久化失败不发可恢复正常 exit，进入 terminal/session failure。退出后的重连先 replay 到 final_seq 再发相同 exit。exit ≠ Settlement：对应 Workflow Run 按既有状态机进入 awaiting-outcome/Needs You。

### 8.6 状态机

```text
session: provisioning → live(epoch N) → exiting(redactor finish + durable commit)
       → complete | crash_incomplete(恢复到 durable_through_seq) | lost(Core crash 且不可重附)
writer lease: granted(ttl) → renewed* → revoked(reason: released|timeout|takeover|connection_closed)
```

### 8.7 数值

全部见附录 A2（`frame_max_bytes`(fixed)、`input_rate_bytes_per_sec`、`outstanding_output_max_bytes`、`slow_client_grace_ms`、`replay_ring_max_bytes`、`transcript_flush_interval_ms`/`transcript_flush_batch_bytes`、`transcript_session_max_bytes`、`transcript_retention_days`、`transcript_project_cap_bytes`、`writer_lease_ttl_ms`/`writer_lease_renew_after_ms`、resize 边界/`resize_max_rate`、`attach_timeout_ms`、terminal WS ping/idle）。输入永不持久化。

### 8.8 迁移现状（T3 必须完成）

当前 `crates/mf/src/runtime_host.rs` 的 `PtySession`（固定 Screen、256 KiB tail、raw writer、`project path + rowid` key）升级为上述管线；legacy 未脱敏 `screen.feed/output_tail` 路径删除；redactor 必须覆盖后注入的 `MF_RUN_TOKEN`（当前两条 redactor 从 `plan.secret_env` 构造，token 在其后注入，echo 会泄漏——统一为 `raw PTY → redactor(all Secret/capability values) → seq/journal → Screen/transcript → fan-out` 并删除全部旁路）；SessionRegistry 增加持久随机 public handle（§3.2）。

### 8.9 安全 / 可观测性 / 验收 / 落点

- 安全：redaction 跨 chunk 且在 journal 之前；`MF_RUN_TOKEN`/Secret/CLI 凭据永不进入浏览器；终端链接默认仅 `http/https`；同页面 XSS 等同终端劫持 → 严格 CSP + 依赖锁定（§6.3）。
- 可观测：per-session seq/ack 水位、outstanding bytes、replay 命中、redaction 命中数、exit 时延。
- 验收：seq/ACK、input dedupe/不重放、writer revoke、resize、exit final seq 顺序全覆盖；`yes` 洪泛下 Ctrl+C 在 `flood_ctrlc_delivery_ms` budget 内送达 PTY；history gap 关闭 live WS + 行边界 transcript；crash-incomplete/durable-before-notify 故障注入；真实 Codex/Claude/GLM 的 slash/skill/TUI/IME/Unicode/resize/reconnect/多标签矩阵在 production build 通过；xterm.js 锁定精确版本（`@xterm/xterm@6.0.0`、`@xterm/addon-fit@0.11.0`，不用 `^` 漂移）并复现/排除官方 issue #5800（Vite minify）、#6049/#6078（IME）路径。
- 落点：`crates/mf-terminal/src/{pty/{mod,windows,unix}.rs, redactor.rs, journal.rs, transcript.rs, session.rs, channel.rs, term_screen.rs, limits.rs}`；GPUI 侧改用 `TerminalChannel`；测试 `crates/mf-terminal/tests/contract/{seq_ack,writer_lease,redaction_cross_chunk,gap_and_exit,crash_incomplete,limits_defaults}.rs` + 真实 CLI 矩阵 `crates/mf-terminal/tests/matrix/`（本机 gated）。

---

## 9. Manifest v3、Provider 与 CLI Installation

### 9.1 五个生命周期

Plugin Package、CLI Installation、Provider Profile、Agent Instance、Agent Session 分离。安装 CLI 不读写 API Key；配置 Provider 不改真实 CLI 全局配置；同一 Agent Type 可发现多份安装；同一安装可被多 Agent Instance 引用；只有拥有可信收据的受管安装能无歧义 update/repair/uninstall。

### 9.2 Manifest v3（`crates/mf-plugins/src/manifest.rs` 升级）

```toml
[manifest]
version = 3

[capabilities]
spawn = true
net = true
package_install = true
privileged_install = false
agent_full_access = false

[[agent_types]]
id = "codex"
name = "Codex"
adapter = "codex"
command = "codex"
modes = ["interactive", "oneshot"]
supports_isolated_config = true

[agent_types.discovery]
commands = ["codex"]
version_argv = ["--version"]
version_parser = "semver-first"

[agent_types.models]
local_probe = "adapter"

[agent_types.root_launch]
permission_mode = "full-access"
argv = ["--dangerously-bypass-approvals-and-sandbox"]

[[provider_types]]
id = "openai-compatible"
protocol = "openai"
config_schema = "schemas/openai-provider.json"
model_probe = "remote-catalog"
cache_ttl_seconds = 300

[[agent_types.installers]]
id = "npm-global"
platforms = ["windows-x64", "linux-x64", "macos-arm64"]
kind = "package-manager"
manager = "npm"
package = "@openai/codex"
argv = ["install", "--global", "@openai/codex@{version}"]
scope = "user"
post_install_probe = true
```

语义不可变项：

- Agent Type：稳定 id/adapter/默认命令/运行模式；discovery 候选命令（浏览器不能提交任意路径）；结构化 version probe + 解析器；可选本地 model probe/resume/attach；`root_launch` 结构化 argv/env 映射——缺失时 Root Agent 启动 fail-closed（不能只 OS 提权后宣称最高权限）；无内部权限层的 Generic Command 必须显式 `passthrough-full-access`；平台/架构/CLI 版本上下限；0..n 稳定 installer id；Provider Schema 与 CLI 安装分开声明。
- Provider Type：Endpoint/API 协议、Provider Profile Schema、远端模型目录探测、模型 id 归一化；`/models` 获取不挂在 CLI discovery 上。远端探测由 Core 内建协议适配器执行；若 worker 参与，只发一次性 Provider-scoped probe capability，输出脱敏后进缓存/事件/日志。
- 权限：新增 `package_install`、`privileged_install`、`agent_full_access`；下载仍需 `net`、启动仍需 `spawn`、shell recipe 仍需 `shell`。能力/recipe/worker/内容哈希任一变化改变授权指纹并要求重新授权。插件启用 ≠ 允许安装（用户仍需显式 Install/Update/Repair/Uninstall）。
- v2 不做安全敏感字段的静默兼容推断；旧 v2 安装贡献明确报版本不兼容；synthetic 插件、fixture、contribution registry 同步迁移；`BuiltinAgent::InstallSpec`/`permission_args` 旁路删除，迁入 v3 `root_launch`/installer contribution。

### 9.3 Installer 三类型

- `package-manager`（npm/pnpm/pipx/uv/brew/winget…）：Core 以结构化 argv 直接启动，不经 shell；预览后冻结 exact package/version/registry/argv/recipe digest，执行时不重解析 `latest`、不接受插件替换包名。
- `verified-download`：Core 下载固定 HTTPS 资源，验证 sha256 或插件信任锚签发的 release metadata/签名，检查 archive 路径，原子发布到用户级受管目录；动态版本在预览解析为 exact version/URL/digest。
- `custom-command`：插件声明 executable + argv/env 模板；结构化命令需 `spawn/package_install` 授权且 Root Mode 开启；仅 shell 字符串额外要求 `shell`，不得把字符串伪装成 argv。
- `requires_elevation=true`（系统包管理器/machine scope/受保护目录）：普通模式不可自动回退提权；Root Mode 关闭时返回可恢复 `elevation_required`。
- 下载与执行硬边界（附录 A5）：HTTPS、`install_redirect_max`(fixed) 且仅限预览冻结域名、`install_download_max_bytes`、`install_archive_max_bytes`、`install_download_stall_timeout_ms`；拒绝绝对路径/`..`/symlink/junction/设备文件；package manager postinstall 提示「非软件沙箱」。Secret：安装 recipe 默认不得引用 Provider/API Secret；确需认证用独立 installer credential Schema，Core 内存短暂解析，永不进 argv/日志/收据。

### 9.4 发现与 adoption

- discovery 只搜宿主允许的 PATH 与已登记受管目录（每 candidate 超时 `discovery_probe_timeout_ms`），canonicalize 并记录入口 link/shim 与最终 target identity；相同 canonical executable 去重为一个 Installation；未检测到 CLI 时 Catalog 显示插件安装选项而非置灰。
- 外部安装默认 `external`、只 launch；仅当可执行 hash/签名与插件固定可信 artifact 完全匹配才可 adopt（创建 adoption receipt），否则只能重装到 managed directory；合法 symlink 可发现，启动前 target 被替换则拒绝；archive 解压一律拒绝 symlink/junction；受管目录只接受收据拥有且目标仍在 owned root 内的 link。

### 9.5 安装任务状态机与执行流程

```text
absent/external/detected → queued → resolving → downloading|executing → verifying → installed
                                                        ↘ failed | cancelled | repair-needed
installed → update-available → updating → verifying → installed
installed → repairing → verifying → installed
installed → uninstalling → absent
```

每次状态变化发单调序号事件（Snapshot+resume 恢复）；`installed` 只能由 post-install discovery + version probe 成功产生（退出码 0 不算）；cancel 是命令非 UI 本地状态；package manager 部分外部状态 → `repair-needed` 展示诊断，不谎报回滚；同 command id 幂等、异目标版本/recipe digest 冲突拒绝。执行：Controller 发起 → Core 返回预览（来源、argv 摘要、目标目录、权限、覆盖、校验、回滚能力、是否需 Root、是否影响 pinned Revision）→ 冻结 plan（`install_plan_handle + recipe_digest + catalog_revision`，`install_plan_ttl_secs`）→ 提交命令 → 校验授权/平台/Root epoch → Core 或 Broker 在新的 staging/job/process group 执行 → 输出先脱敏再写 journal/推送（`install_output_max_bytes`/`install_output_max_lines`）→ 重新 probe → receipt + 原子切换（L-SWITCH）→ 失败清理 staging、可回滚则回滚、否则 `repair-needed`。job 超时 `install_job_timeout_ms`；取消协作式，超时后按 `install_cancel_kill_grace_ms` 强杀。提权身份与目标身份分离：receipt 固定 `requesting_principal/target_owner/scope`；UAC 输入另一管理员账号不得把 user-scope 包写进该管理员 profile；无法保证 target principal 的 recipe 必须拒绝或改用明确 machine scope。

### 9.6 Installation Receipt（不可变）

receipt id、plugin full id/version/content hash、agent_type_id、installer id、recipe digest、请求/实际版本、平台/架构/scope、package id 或下载 URL、hash/signature 结果、canonical executable 与可执行身份、前后时间、Root epoch（不存 capability）、rollback/uninstall 方法、保留 artifact 与脱敏日志引用、post-install probe 结果。与 Plugin lock 分库分表；不存 API Key/完整环境/未脱敏 argv/终端输入。

### 9.7 版本冻结、pin、卸载

- Pipeline Revision 固定 plugin version/content hash、Agent Instance snapshot、installation id、canonical executable、实际 CLI version 与可执行 hash；新 Agent Run 启动前重核 executable identity，不符 → 拒绝启动 + Needs You（不偷偷用 PATH 新版本）。
- 活动 Agent Run / 用户显式 replay lease pin 期间阻止破坏性 update/uninstall；历史 Revision 默认不永久 pin；side-by-side installer 装新版本只切换未来草稿；全局 manager 无法 side-by-side 时默认阻止，「强制」必须先展示受影响 Revision，旧 Revision 后续 Workflow Run 进入 Needs You。
- uninstall 只删 receipt 拥有的二进制/包，不删用户配置/Provider Profile/Secret/Agent Instance/外部安装；插件禁用/升级不终止已运行 Agent Session，只阻止依赖不可用 adapter/recipe 的新启动；旧插件包按 Revision pin 保留。

### 9.8 Provider 模型探测与缓存

模型下拉 → Provider Type remote catalog probe，Core 发起并按 Profile 解析 Secret；浏览器只收模型 id/显示名/能力/缓存状态；离线/未配置/失败显示缓存 + 明确错误 + 允许手填合法模型 id；`provider_model_cache_ttl_secs`、`provider_probe_timeout_ms`/`provider_probe_retries` 与退避序列见附录 A5。

### 9.9 安全 / 可观测性 / 验收 / 落点

- 安全：shell 注入失败关闭；hash/signature、archive traversal、redirect、oversize、symlink/junction 攻击拒绝；普通模式不能运行 requires-elevation/custom shell recipe；Secret/broker capability 不进日志/事件/收据/浏览器 payload。
- 可观测：job 状态与阶段耗时、probe 命中率、受管/外部安装计数、pin 冲突事件。
- 验收：同 Type 双安装（external+managed）去重正确；安装成功前必经 probe；plan digest/catalog revision TOCTOU 测试；frozen Revision 在 CLI 被替换后拒绝静默运行；update/repair/uninstall 只碰收据拥有内容；Windows/macOS/Linux fake installer 契约测试 + 真实包管理器受控 smoke；模型下拉缓存/刷新/失败回退；`installation_failed`/`elevation_required` 错误路径。
- 落点：`crates/mf-plugins/src/manifest.rs`（v3 parser/validator）、`crates/mf-installer/src/{plan.rs, discovery.rs, receipt.rs, adoption.rs, provider_probe.rs, executor/{mod,package_manager,verified_download,custom_command}.rs, limits.rs}`；adapter 编译 typed `LaunchPlan`（root_launch 映射）在 `crates/mf-plugins/src/adapter_launch.rs`（自 `crates/mf/src/adapter_launch.rs` 迁入）；测试 `crates/mf-installer/tests/contract/{fake_installers,download_hardening,plan_toctou,receipt_pin,model_probe,limits_defaults}.rs`。

---

## 10. Root Broker/Runtime/Install Host 协议与 platform traits

### 10.1 Root Mode 状态机（Core 内）

```text
off → enabling(一次 OS 原生授权/UAC/AuthorizationServices/polkit)
    → on(root_epoch N) → disabling → off
Core 重启 → off（不持久化、不自启动）
```

- 仅当前 Controller 可开关；开启期间新的 Installation Job 与新的 Preview Session / Workflow Run / 手工 Agent Session 默认获得 OS Administrator/root + full-access（需要 plugin `agent_full_access`/`privileged_install` 授权与 `root_launch` 映射，缺失 fail-closed）。
- 已运行普通进程不能原地提权（需重启 Agent Session）；关闭后不杀已启动高权限 Agent，但拒绝新的 Root Agent Session / Installation Job；Root Workflow Run 由 Controller 明确启动后 Core 可自动调度，但每个未启动下游节点 launch 时复验 active root epoch，Root 已关 → Needs You；Controller 暂时断连不终止仍有效 epoch；插件不能自行创建高权限 Agent Session / Installation Job；同一 Root Mode 生命周期不逐动作弹系统确认，但每个 Installation Job 仍由 Controller 显式发起。
- UI/tray 持续红色提示；Node/Agent Session/Workflow Run 显示管理员徽标。

### 10.2 Elevated Broker

- Core 恒为 asInvoker；Broker 是最小化独立进程（Windows `requireAdministrator` manifest 经 UAC；macOS Service Management/launchd privileged helper；Linux polkit helper）。不监听 TCP、不提供 Web API；IPC 用随机实例名 + 显式 OS ACL（Windows named pipe 自定义 DACL：仅当前 logon SID、Core identity、broker——不得用默认 DACL；macOS/Linux 用 UDS + 文件 ACL）。
- 消息字段：protocol version、Core PID/start identity、broker epoch、一次性 128-bit nonce（`broker_request_ttl_ms`）、request id、MAC/capability。Broker 验证调用者 OS 身份、Core 实例与当前 Root epoch；旧 Core/旧 epoch/重放 request 拒绝。浏览器永不持有 Broker capability/pipe 名/nonce/MAC/OS token。
- Broker 只授权与启动，不长期拥有 Root PTY/安装进程；Root Mode 关闭或 Core 退出或 channel 断开（`broker_heartbeat_interval_ms`、`broker_heartbeat_miss_limit`(fixed)）→ Broker 停止接受新任务并退出。

### 10.3 Root 逻辑/物理 owner 分离

- Core SessionRegistry = 逻辑 owner（Agent Session handle、Workflow Run 关联、writer lease、事件、transcript）；Root PTY/进程组物理 owner = session-scoped `mf-root-host`；每个已启动安装物理 owner = job-scoped `mf-install-host`。
- host 只服务一个 Agent Session / Installation Job：不能创建新 Agent Session、Installation Job 或通用命令；IPC 只绑定对应 Agent Session / Installation Job 且受 ACL 保护。
- 输入路径：Core 在 L-INPUT 复验 Controller/writer lease 后，向 host 发带单调 owner epoch + session capability 的消息；host 复验（旧 Core/旧 lease 不能写）。
- Core channel 断开：host 立即拒绝新 input/resize/control，继续把已脱敏输出写入 ACL 保护的 session spool（`root_spool_max_bytes`，`~/.monkeyfence/root-spool/<session>/`），进入 bounded orphan grace（`root_host_orphan_grace_ms`，附录 A6）；新 Core 只能用持久 host receipt + OS identity 做 read-only reattach，Agent Session 进 Needs You；恢复 writer/control 必须重新开启 Root Mode 并由 Broker 为该 Agent Session 续发 capability；grace 到期仍未安全重附 → host 终止 Root process group 并留下可导入的 exit/spool 记录（杜绝无人控制的高权限进程永久存活）。

### 10.4 崩溃恢复

- Broker/Core 任一方崩溃：新 Root 请求全部失败关闭；已启动 Root Agent 按独立进程/Job/process-group 记录继续或被清理策略结束；Core 恢复后不得假定旧 Root capability 有效；失去 live PTY 的 Workflow Run 进 Needs You。
- Root Mode 关闭后：Broker 拒绝新请求并退出；既有 host 可完成/可取消，但不能派生新的 Root Agent Session / Installation Job（验收有显式测试）。

### 10.5 安全 / 审计 / 可观测性 / 验收 / 落点

- 审计记录：Root 开关、OS 授权结果、插件/Agent identity/版本/目标/cwd/脱敏命令摘要/起止/退出状态/安装 provenance 与 rollback receipt；不记录 API Key、MF_RUN_TOKEN、完整 Secret env、终端输入、未脱敏 argv。
- 验收：pipe ACL、错误 SID、错误 PID、旧 nonce/epoch、重放 request 全拒；Core 保持普通权限；Root Mode 关闭/重启后不能启动新的 Root Agent Session / Installation Job；已有 Root Agent 在 UI/tray/Workflow Run/Agent Session snapshot 持续可见；Windows/macOS/Linux fake broker/host 契约测试 + 协议 golden tests（GPUI 退役前置）；UAC alternate credential 不改变 target principal。
- 落点：`crates/mf-elevated/src/lib.rs`（协议/能力/epoch）、`src/platform/{mod,windows,unix}.rs`、`src/spool.rs`、`src/limits.rs`、bins `src/bin/{broker,root_host,install_host}.rs`；测试 `crates/mf-elevated/tests/contract/{fake_broker_matrix,heartbeat,orphan_grace,protocol_golden,limits_defaults}.rs`。

---

## 11. launcher/tray/picker、singleton/discovery 与安全退出

### 11.1 Singleton 与 discovery

- 每 OS 用户一个 Core：OS 级 per-user 命名 mutex（Windows `Local\MonkeyFence.Core`）+ `~/.monkeyfence/core.lock`（pid、owner epoch、build）；启动竞争 → 败者向胜者转发 open 意图后退出。
- discovery 文件：Windows `%LOCALAPPDATA%\MonkeyFence\discovery.json`；Linux `~/.local/state/monkeyfence/discovery.json`；macOS `~/Library/Application Support/MonkeyFence/discovery.json`。内容 instance id、port、pid、build、heartbeat 时间戳；Core 按 `discovery_heartbeat_ms` 更新；stale = 3×heartbeat（派生，默认 15 s）且进程不存活。权限仅当前用户。
- stale discovery fencing：新 Core 启动验证旧 pid 不存在或 heartbeat 过期 + mutex 可取，才接管；不误杀活 Core。

### 11.2 launcher

短生命周期命令 `start | open [project?] | status | stop`：start 幂等拉起 Core（不抢已有 owner）；open 生成 bootstrap nonce 并以默认浏览器打开 `http://127.0.0.1:<port>/#nonce=<128-bit>`（只 IP literal）；status 输出 instance/build/active 摘要；stop 走安全退出（11.4）。

### 11.3 tray 与 picker

- tray 极薄 companion：打开 Web、跨项目 Workflow Run/Needs You 摘要（经本地 IPC 读 Snapshot 摘要）、安全退出、目录选择入口；不拥有 Core、崩溃不影响 Core、不承载第二 UI、不配置自启动；红色 Root Mode 提示。
- picker：按需短生命周期原生目录选择 helper（tray 不在时也可用）；Project 注册必须经系统选择器或本地 CLI；Core canonicalize 后存 Project root；浏览器只拿 opaque project handle。

### 11.4 安全退出（safe shutdown）

`shutdown(ShutdownIntent) -> ShutdownAssessment`：返回并展示全部活动 Workflow Run / Agent Session / Installation Job / 不可中断 Operation；存在活动工作时要求用户明确确认（tray close、browser close、静默更新都不得触发）；确认后执行 §2.3 freeze→drain→stores_closed：冻结宽限 `shutdown_freeze_grace_ms`、drain 上限 `shutdown_drain_timeout_ms`（超时且仍有不可中断工作 → 终止退出并报告 blocker）；强制模式对子进程组 SIGTERM/kill，`forced_kill_grace_ms` 后强杀，然后 flush 收据/transcript、关闭 Store、退出。首版无空闲自动退出。更新/owner handoff 使用同一 zero-active gate（§13）。

### 11.5 安全 / 可观测性 / 验收 / 落点

- 验收：singleton 竞争、stale discovery、crash restart、安全退出均有确定结果；关闭 browser/tray/GPUI 后 Core/Workflow Run/Agent Session 按契约继续；`monkeyfence status/open/stop` CLI 契约测试。
- 落点：`crates/mf-kernel/src/singleton.rs`（owner lock/discovery/handoff manifest）、`crates/mf-companions/src/{lib.rs, bin/launcher.rs, bin/tray.rs, bin/picker.rs}`；常量 `mf-kernel/src/limits.rs` + `singleton.rs`；测试 `crates/mf-companions/tests/contract/{discovery_lifecycle,safe_shutdown,limits_defaults}.rs`。

---

## 12. React Workbench：UI 状态与用户旅程

### 12.1 信息架构与组件基座

- 顶层只有「工作流 / 运行」；Needs You 是运行过滤器；设置是 overlay。三栏 Workbench：Agent 库 / DAG / Inspector；窄屏 Inspector 下移。
- 基座：React 19 + TypeScript；`@xyflow/react`（React Flow 12，锁 `12.11.x` 精确版本）；`@dagrejs/dagre`（藏于 `LayoutEngine` 接口，ELK 仅作接口后升级路径）；`@xterm/xterm@6.0.0` + `@xterm/addon-fit@0.11.0`（WebGL 可选 + DOM fallback，锁精确版本、production build 回归）；状态管理用轻量 store（zustand 风格）持有「snapshot + 事件流应用结果」，React Flow 内部状态仅做交互缓存。
- 用户移动坐标属 presentation metadata；只有显式「自动排列」重算布局；Rust 只验 DAG 合法性，坐标计算留在 Web。

### 12.2 UI 状态边界（权威表）

| 数据 | 权威来源 | Web 可否乐观更新 |
| --- | --- | --- |
| Step/依赖/Agent 指派/策略 | Rust Store | 可，失败必须回滚 |
| DAG 合法性/cycle | Rust | Web 预检，Rust 复检 |
| 坐标/viewport/折叠 | presentation（Rust 持久化） | 可 |
| Pipeline Revision/Agent Run/Settlement | Rust | 不可伪造 |
| PTY / Agent Session 生命周期 | Rust SessionRegistry | 不可 |
| xterm buffer | 浏览器可丢弃缓存 | 从 Rust journal replay |

浏览器刷新只 detach 不结束 Agent Session；乐观 UI 失败回滚 + `revision_conflict` 提示刷新。

### 12.3 核心旅程（验收用）

1. 注册 Project（tray/picker → opaque handle → 出现在顶层）；2. 创建/重命名/删除工作流（CAS collection）；3. 拖入节点、连线、重连、删除（Web 预检 + Rust 复检一致）；4. Inspector 配置 Step 与 Agent Instance；5. 自动排列/手动布局；6. 一键运行 → 运行页实时 DAG；7. 双击运行节点 → Node Session Panel（真实 CLI）；8. `/xxx`/审批/TUI 原生处理；9. 编辑节点双击 → Preview Session；10. Needs You 过滤 → respond/settle；11. 第二标签 Observer/接管；12. 断线重连（events resume / terminal replay）；13. Agent 设置：Catalog → Install/Update/Repair/Uninstall（预览→冻结→执行→receipt）；14. Provider Profile → 模型下拉（live/cache/手填）；15. Root Mode 开关与徽标；16. 安全退出确认。

### 12.4 无障碍与性能

- React Flow 键盘遍历/选择/移动、ARIA 文案中文化；xterm `screenReaderMode`；`onlyRenderVisibleElements`，视口内 DOM 节点按附录 A9 预算；节点组件/callback memoize。
- 性能与大小预算见附录 A9（Dagre 布局、拖动 p95、首屏可交互、typed delta 大小、初始 JS 体积）。

### 12.5 安全 / 验收 / 落点

- 安全：CSP 之下无外部脚本；不在页面存 Secret/nonce（nonce 用后即清）；XSS 防线等同终端劫持防线。
- 验收：全部旅程在真实浏览器（Playwright/人工）通过；production build（Vite）路径覆盖 xterm #5800；IME（Windows Microsoft Pinyin/搜狗）人工验收；UI 可用性验收一律真实浏览器。
- 落点：`web/`（`web/src/{app,workbench,dag,inspector,terminal,settings,api,state}/`、`web/vite.config.ts`）；产物 `web/dist` 由 `crates/mf-web` build script 内嵌（include hash assets manifest）；e2e `web/e2e/*.spec.ts`。

---

## 13. Bridge、owner handoff、side-by-side bundle 与 rollback

### 13.1 迁移总原则

- 不允许 GPUI/Web 双 owner、双写或 UI mutation bypass；Terminal v1 必须在 Core 拆进程前完成；不能发布「自动抢 Controller 但无写能力」的 read-only Web——用户可见 Web 必须已具备核心领域写入（workflow create/edit/run + 会话写入）。
- Core owner 切换必须 freeze/drain/epoch/Store close/OS lock/handoff/acquire 线性化；whole versioned bundle side-by-side 更新/回滚，不能只换 UI。

### 13.2 Bridge A 与滚动 anchor

- Bridge A = 仍由 GPUI 进程托管 CoreKernel、用户行为不变的版本：已理解 Project v7、Catalog v2、service-v1、Terminal v1、Manifest v3、Provider Type、CLI Installation/Receipt、Root/host record 与 owner fencing，并具备这些持久状态的 runtime reader/安全阻断路径；加入 freeze/drain/handoff 协议与 side-by-side bundle 管理，但不执行进程所有权迁移。
- durable feature reader-before-writer：每个新 durable feature 在版本 N 先发布 reader/runtime/安全阻断且不写新格式（登记 `durable_feature_versions`），版本 N+1 才启用 writer，N 成为新的滚动 rollback anchor；超出 Bridge A v1 feature set 的新功能必须先发 Bridge B，不能继续把 Bridge A 当回退。
- pre-Bridge 当前二进制不是 rollback target：升级到 Bridge A 必须先安全退出旧进程；Bridge A 之后的升级才允许协议化在线 handoff。

### 13.3 Core owner 原子移交（8 步，线性化于 L-OWNER）

```text
1) updater/launcher 取用户级 update lock，验证当前 bundle/owner identity
2) old Core 进入 freezing：拒绝新 command / Agent Session / Installation Job / Root Mode enable；
   旋转 Controller/Root/writer lease epoch
3) publication barrier 上等待已线性化 HTTP command、PTY input queue、outbox publication、可中断 Operation drain
4) 复查活动 Workflow Run / Agent Session / Installation Job / 不可中断 Operation == 0
   （freeze 后不得再进新工作）
5) flush Transcript/outbox/command receipt；关闭全部 Project/Catalog/service Store 句柄
6) old Core 写 handoff manifest（build、schema、owner epoch、DB identity），释放 CoreOwnerLock，
   永久进入 handed-off（不得自行 reopen）
7) new Core 原子 acquire CoreOwnerLock，校验 handoff/DB owner epoch/schema/bundle，更新 discovery 后才接受 Client
8) new Core 启动失败：仅 Bridge A 或 schema-compatible previous bundle 可在无新写入前提下以更高 owner epoch
   reacquire（handoff_reacquire_window_ms 窗口）；否则保持停止并给恢复诊断
```

SQLite WAL 不是 ownership lock；升级/切换只在一个 singleton Core owner 下执行；活动 Workflow Run / Agent Session / Installation Job / 不可中断 Operation 非零时延期，不强杀 Agent。

### 13.4 Side-by-side bundle 与 rollback

- 一个原子 versioned bundle = Core、同版本 embedded Web assets、launcher、tray/picker、mfctl、Broker/Runtime/Install hosts；安装到 side-by-side 版本目录（Windows `%LOCALAPPDATA%\Programs\MonkeyFence\versions\<semver>\`），稳定 launcher 经 `current.json` 指针选择。
- rollback 切换整个 schema-compatible bundle（不存在「只回滚 UI/launcher」；旧 Core 只提供自己 bundle 的 assets）。eligibility 同时检查 `min/max readable schema`、`durable_feature_versions`、plugin manifest/receipt reader、Operation/host protocol 与 binary compatibility（只看 DB schema 不够）。
- schema 升级前 Backup API 备份；升级后已有新写入时不自动恢复旧备份（显式灾难恢复除外）；保留 previous bundle 直到新 Core 健康检查 + 首轮 contract smoke 通过；pointer 切换与 owner handoff 用同一 zero-active gate；被活动 Workflow Run/replay lease pin 的 plugin/installation/receipt 不清理。

### 13.5 安全 / 验收 / 落点

- 验收：8 步协议的故障注入（每步以 fault harness 终止进程）有确定状态；v6/v7、v1/v2、session.json 迁移幂等；Bridge rollback 演练通过；活动工作存在时升级确实延期；Core crash 后不可重附 PTY 进入 lost/Needs You 并从 durable transcript 恢复。
- 落点：`crates/mf-kernel/src/singleton.rs`（handoff manifest/owner epoch/update lock）、`crates/mf-companions`（bundle manager/pointer）；打包 `packaging/windows/`（WiX）；测试 `crates/mf-kernel/tests/contract/{owner_handoff,bundle_rollback,limits_defaults}.rs`。

---

## 14. Observability、测试、容量预算与 rollout gates

### 14.1 Observability

- 结构化日志（脱敏、含 trace_id）+ metrics（§2.7、各模块）+ append-only audit（§4/§10）。日志按 `log_rotate_max_bytes`/`log_rotate_keep` 轮转（附录 A10），不记 Secret/MF_RUN_TOKEN/未脱敏 argv/终端输入。
- 问题定位：每个 problem/WS close 都可由 `trace_id` 关联日志；Operation / Installation Job 有完整阶段时间线。

### 14.2 测试决策（Testing Decisions）

- 好测试只测外部行为：契约测试面向 wire 协议（HTTP/WS envelope、close code、错误码）与 CoreKernel 五接口，不测内部函数/表结构私有细节；golden fixtures 固定 schema 形状。
- 缝隙（seams）：理想缝隙只有一个——**CoreKernel**。外部行为测试优先打最高缝隙：loopback HTTP/WS 对真实 Core 进程（fixture stores）做 wire contract tests；UI 旅程在真实浏览器（Playwright）；Terminal 矩阵必须跑真实 Codex/Claude/GLM CLI（不用 mock 代替）；crash 注入用平台无关 fault/terminate harness（Windows `TerminateProcess` / Unix `SIGKILL` 由 harness 抽象）在每个线性化点终止 Core；性能用浏览器 Performance trace / rAF drag latency / heap（不用 React Profiler `actualDuration`）。既有先例：`crates/mf/src/*_e2e_tests.rs`、`crates/mf-agent` schema 测试——沿其 fixture 风格迁到 `tests/contract/`。
- 每个实现 ticket 用 TDD/contract fixtures；完成后由 Codex 做 Standards + Spec review；UI 可用性验收一律真实浏览器。

### 14.3 容量与性能预算

预算项以附录 A9 为准（budget，不可配置），此处只列验证口径：

- 吞吐：journal append p99、fan-out 附加延迟、命令处理 p95 在事件风暴与终端洪泛并发下同时测量，互不饿死。
- 规模：100/500/1000 节点 Snapshot 大小/构建延迟、Dagre 布局（原型实测 100/500/1000 = 11–16/49/105–125 ms，预算上限见 A9）、节点拖动 p95、视口 DOM 数。
- 终端洪泛（`yes`/大日志）：ack outstanding 预算、Ctrl+C 送达时延、seq==ack 收敛（原型实测 33,447 seq 全确认）。
- 冷启动：10 个已登记 Project 下 Core 到可接受命令的 p95。

### 14.4 Rollout gates

- **Gate T5（Bridge A）**：reader-before-writer 全部 durable feature 有 reader + 安全阻断测试；freeze/drain 协议测试通过。
- **Gate T6（拆进程）**：owner handoff 8 步测试 + legacy GPUI 经 local transport 读写正常；Terminal v1 contract 全绿（硬前置）。
- **Gate T8（Web 写入发布）**：Web 已具备 Project Workflow / Workflow Run / Agent Session 写入并通过并行 Controller/Observer/takeover 测试；在此之前不发布用户可见 Web bootstrap。
- **Gate T11（Web 默认）**：真实 CLI 矩阵 + IME + production build + 安全矩阵 + 迁移幂等 + rollback 演练全通过。
- **Gate T12（删除 GPUI）**：§13.5 全部 + Windows 真实包验收 + 三平台 fake 契约测试 + 「无仅存在于 GPUI 的产品能力」清单核验。

---

## 15. 精确 crate/file 变更与删除计划

### 15.1 新增

| 路径 | 内容 |
| --- | --- |
| `crates/mf-kernel` | lib + bin `monkeyfence-core`：kernel/handles/command/operation/projection/journal/lease/singleton/shutdown/run_control/legacy_transport/project_registry/config/limits + `tests/contract` |
| `crates/mf-terminal` | pty(platform traits+resize)/redactor/journal(ring+seq)/transcript/session(runtime+registry+handle)/channel/term_screen/limits + contract/matrix 测试 |
| `crates/mf-installer` | plan/discovery/receipt/adoption/provider_probe/executor{package_manager,verified_download,custom_command}/limits + fake installer 测试 |
| `crates/mf-elevated` | 协议 lib + platform traits + spool + bins broker/root_host/install_host + fake 契约测试 |
| `crates/mf-web` | gateway/auth/headers/assets/api/* /ws/* /problem/limits + golden fixtures |
| `crates/mf-companions` | bins launcher/tray/picker + 共享 discovery/safe-exit client |
| `web/` | React Workbench（Vite、React Flow、xterm、api/state/e2e） |
| `packaging/windows/` | WiX per-user MSI + bootstrapper + side-by-side 布局 |

### 15.2 迁移（当前 → 目标）

| 当前 | 目标 | 阶段 |
| --- | --- | --- |
| `crates/mf/src/app_ctx.rs` | 深化为 UI-neutral CoreKernel（字段私有），最终删除 | T2 |
| `crates/mf/src/project_overview.rs` | 删除；由 `mf-kernel/src/projection.rs`（Snapshot+event journal）取代 | T2 |
| `crates/mf/src/runtime_host.rs`、`pty_spawn.rs`、`term.rs` | `crates/mf-terminal`（session/pty/term_screen） | T2–T3 |
| `crates/mf/src/adapter_launch.rs` | `crates/mf-plugins/src/adapter_launch.rs`（typed LaunchPlan） | T2 |
| `crates/mf/src/pipe_server.rs` | `crates/mf-kernel/src/run_control.rs`（Named Pipe/UDS adapter；MF_RUN_TOKEN 语义不变、不复用到 Web） | T2 |
| `crates/mf/src/workflow_editor.rs` | 领域校验 → `crates/mf-agent/src/workflow_validation.rs`；GPUI render 部分待删 | T2 |
| `crates/mf/src/run_monitor.rs` | Needs You / Workflow Run projection → `mf-kernel/src/projection.rs`；GPUI render 待删 | T2 |
| `crates/mf-plugins/src/manifest.rs` | v2→v3 parser/validator + capabilities + provider_types + installers | T4 |
| `crates/mf-plugins/src/builtin.rs` 等 | synthetic manifest/fixture 全部 v3；删 `InstallSpec`/`permission_args` 旁路 | T4 |
| `crates/mf-agent/src/schema.rs`/`store.rs`/`catalog_store.rs` | v7（含 identity/receipt/outbox/transcript）+ future guard；catalog-v2 新库与迁移 | T1 |
| `session.json` | service-v1 `project_registry` 幂等导入（marker；原文件保留） | T1 |

### 15.3 删除与保留（全部在 T12，除非标注）

- 删除 `crates/mf`（GPUI legacy client 整体：workspace、navigation、theme、editor、file_tree/file_index、search、quick_open、diff_view、vcs_panel、console、task_*、agent_workspace、settings GPUI 页、workflow_canvas 等 render 层）。
- 删除 `crates/mf-core`（editor buffer/highlight）、根 `Cargo.toml` 的 `gpui`/`gpui_platform` 本地 path 依赖与 `vendor/gpui_platform`、Windows GPUI 资源。
- **保留 `crates/mf-vcs`（headless）**：它仍被 `mf-plugins` 的 `GitWorktreeProvider`（`git_worktree_provider.rs`/`vcs_provider.rs`）与 Execution Directory Provider / Execution Lease 使用；T12 只删除 VCS/diff/delivery 的 UI（`crates/mf/src/{vcs_panel,diff_view}.rs` 等 GPUI 侧），不动 headless runtime，不破坏 Execution Lease/plugin pin 语义。若未来要删 `mf-vcs`，必须先把 Git worktree 所需能力与契约测试迁入 `mf-plugins` 且保持上述语义不变——不属于本迁移范围。
- 删除后 workspace 成员：`mf-kernel`（含 core bin）、`mf-terminal`、`mf-installer`、`mf-elevated`、`mf-web`、`mf-companions`、`mf-agent`、`mf-plugins`、`mf-skills`、`mf-vcs`、`mfctl`；clean checkout 不再需要 Zed/GPUI 本地依赖。
- 不删除：项目文件、Git/P4 数据、`.mf-agent` Store、用户 CLI 配置、Secret Store。

---

## 16. 实施 DAG（T0–T12，/to-tickets 主骨架）

```text
T0  Baseline fixtures + existing behavior characterization
      └─ Project v6/Catalog v1/session.json/插件 pin/运行恢复/Settlement/Handoff golden fixtures；
         工作流创建/编辑/运行、Needs You、Preview/Node 终端、重启恢复端到端基线；
         Windows 安装/升级/卸载/备份现状记录
T1  Storage guards, public IDs, revisions, service intent/outbox, owner lock
      └─ future-version guard+测试；public_handle 列与 workflow_node/edge_identity；
         semantic/presentation 双 revision 与 project_meta.collection revision；
         command_receipt/projection_outbox（仅 store-local outbox_id）/service-v1/project_registry；
         CoreOwnerLock/owner epoch/stale discovery fencing；catalog-v2 迁移（v1 只读备份）；多库崩溃恢复测试
T2  CoreKernel extraction; remove GPUI mutation bypass
      └─ app_ctx 深模块化、字段私有；GPUI 全部读写走 dispatch/snapshot/subscribe_events/attach_terminal；
         仓库检查无 mutation bypass；§15.2 迁移行完成
T3  Terminal pipeline/protocol + Transcript + Root host seam
      └─ redactor 统一管线（删旁路、覆盖后注入 token）；epoch/seq/ring/transcript/resize/ACK/
         writer lease/history gap/crash-incomplete；GPUI 改用 TerminalChannel；
         Root host protocol/fixture（owner epoch/spool/read-only reattach/orphan grace）——B3 gate
T4  Manifest v3 / Provider / CLI Installation / Root runtime readers
      └─ manifest v3 parser/synthetic 迁移；discovery/adoption/plan 冻结/executor 三类型/receipt/pin；
         provider probe+cache；LaunchPlan fail-closed；fake installer 三平台契约；
         数据结构 lane 依赖 T1+T2，Root 执行集成子项另依赖 T3（见 §16.1）
T5  In-process Bridge A + reader-before-writer + whole-bundle manager
      └─ durable_feature registry；freeze/drain/handoff 协议；side-by-side bundle + current.json；
        Gate T5
T6  Atomic standalone Core split + legacy GPUI client
      └─ monkeyfence-core bin/singleton/discovery/launcher/tray/picker/loopback skeleton；
         GPUI 经 mf.legacy-transport.v1 成为 client；Gate T6
T7  WebGateway/auth/API/event implementation + embedded assets
      └─ bootstrap/CSP/Host/Origin、Snapshot/commands/events WS、hash assets 内嵌、golden wire tests
T8  Web Workbench + Project Workflow / Workflow Run 写入 + Needs You
      └─ React Flow 编辑/CAS/自动排列；运行/Needs You/settle；Gate T8（用户可见 Web 必须已可写）
T9  Agent settings/Provider/models/CLI install/Root UI
      └─ Agent Catalog 卡片与动作（已检测/可安装/外部/受管/需更新/需修复）、Provider Profile CRUD
         （write-only Secret）、模型下拉（live/cache/手填）、安装预览→冻结→执行→receipt/进度 UI、
         Root Mode 开关/徽标/影响提示；交付与验收见 §16.1
T10 Node/Preview xterm Terminal v1 + real CLI matrix
      └─ 真实 Codex/Claude/GLM 矩阵 + IME + production build + 洪泛
T11 Web default + tray/launcher/picker + rollout/rollback
      └─ Gate T11；GPUI 隐藏 --legacy-ui；完整 Bridge 周期 + 回退演练
T12 GPUI/Zed/editor/VCS/general-terminal deletion
      └─ §15.3 全部（VCS 仅删 UI，headless mf-vcs 保留）；Gate T12
```

主干图表达的是 **gate/合流顺序**，不是禁止并行开发；「可并行起点」见下表，lane 到达 gate 时必须满足全部前置。

并行规则：T2 contract 冻结后，Web 组件、Gateway、安全 header、Windows packaging 可并行；T1+T2 后 Manifest/Installation 数据结构可并行（Root host integration 仍依赖 T3）；各 lane 在 T5/T6/T8 gate 必须重新汇合，不得跳过 hard blocker。每个 ticket 是 tracer bullet：声明 blockers、可独立验收、迁移/回滚与测试；禁止「重写整个前端」「迁移 Core」巨型 ticket；不在实现 ticket 顺手迁移编辑器/VCS/通用终端。

### 16.1 Ticket 边缘表（blockers / 可并行起点 / 交付物 / 验收）

| Ticket | 硬 blockers（合流门槛） | 可并行起点 | 主要交付物 | 验收 / gate |
| --- | --- | --- | --- | --- |
| T0 | 无（起点） | 立即 | v6/v1/session.json/插件 pin/Settlement/Handoff golden fixtures；工作流端到端行为基线；Windows 安装现状记录 | fixtures 全绿并入基线 |
| T1 | T0 | T0 期间即可准备 fixture | future guards；`public_handle` 列 + node/edge identity；双 revision + `project_meta`；receipt/outbox；service-v1/Project Registry；owner lock/fencing；catalog-v2 迁移 | schema/迁移幂等 + identity 回填/GC + 多库崩溃恢复测试；kernel 依赖的 durable DTO 冻结 |
| T2 | T1 | 无 | CoreKernel 抽取；GPUI 全走 dispatch/snapshot/events/terminal；删 mutation bypass；§15.2 迁移行 | 仓库无 bypass 检查 + kernel contract 全绿；**此后 Web/Gateway/打包 lane 可并行** |
| T3 | T2 | 无 | Terminal 管线/协议/Transcript；Root host seam（协议+fixture） | B3 gate：TerminalChannel contract（含 Root host owner epoch/spool/reattach/grace）全绿 |
| T4 | T1、T2（数据结构 lane）；Root 执行集成子项另需 T3 | T2 contract 冻结后即可并行数据结构 | Manifest v3/Provider/Installation 结构与 readers；executor 三类型；fake installer 三平台契约 | v3 解析/授权指纹/plan TOCTOU/下载硬边界/三平台 fake 契约；Root 集成子项按 T3 gate 验收 |
| T5 | T3、T4 | T2 后可预研 bundle manager | Bridge A：reader-before-writer + freeze/drain/handoff + side-by-side bundle | Gate T5 |
| T6 | T5 | T2 后可并行开发 WebGateway/组件/打包（见 T7） | standalone Core 拆分 + legacy GPUI client + singleton/discovery/launcher/tray/picker skeleton | Gate T6 |
| T7 | T5、T6（合流）；开发可在 T2 后并行 | T2 | WebGateway/auth/API/events + 内嵌 assets + golden wire tests | wire contract + 安全矩阵全绿（隐藏运行时，不给用户 bootstrap） |
| T8 | T7 | T7 期间可并行 Workbench 组件 | Workbench + Project Workflow / Workflow Run 写入 + Needs You | Gate T8：用户可见 Web 必须已具备核心写入 |
| T9 | T7（命令/Operation 面）；与 T8/T10 并行 | T7 | Agent Catalog 卡片/动作；Provider Profile（write-only Secret）；模型下拉（live/cache/手填）；安装预览→冻结→执行→receipt/进度；Root Mode 开关/徽标/影响提示 | 旅程 13–15 真实浏览器 + 命令 contract + Observer 只读 + `elevation_required`/`installation_failed`/`root_authorization_denied` 错误路径 + fake installer 联动 |
| T10 | T3、T7 | T7 后（与 T8/T9 并行） | Node/Preview xterm Terminal v1 | 真实 Codex/Claude/GLM 矩阵 + IME + production build + 洪泛（附录 A9 budget） |
| T11 | T8、T9、T10 | 无 | Web 默认 + tray/launcher/picker + rollout/rollback | Gate T11 |
| T12 | T11 + Gate T12 全条件 | 无 | GPUI/Zed/编辑器/通用终端/VCS UI 删除；headless `mf-vcs` 保留（§15.3） | Gate T12 |

---

## 附录 A：参数总表

### 附录 A 说明

- 每个可配置项由「默认 / 允许范围 / hard cap」三元组定义：可用 `~/.monkeyfence/config.toml` 的 `[limits]` 段在允许范围内覆盖默认值，任何取值不得超过 hard cap。
- 标 `fixed` 的行是安全/协议上限，不可配置（默认即上限）；标 `budget` 的行是验收阈值而非运行参数，不可配置；标「派生」的行由其他参数计算，不可独立配置。
- 本表是唯一数值来源，正文只引用参数名；每组参数的 `limits_defaults` 契约测试断言默认值、范围边界与 hard cap 与本表一致。

### A1 Workflow 事件与 API（`crates/mf-kernel/src/limits.rs`；测试 `tests/contract/{journal_limits,journal_overflow,retention_gc,limits_defaults}.rs`）

| 参数 | 默认 | 允许范围 | hard cap | 说明 |
| --- | --- | --- | --- | --- |
| `journal_max_events` | 20,000 | 1,000–100,000 | 100,000 | 进程内事件 journal 条数；触及上限 fail-closed（§5.3） |
| `journal_max_bytes` | 64 MiB | 4–256 MiB | 256 MiB | 同上（字节轴） |
| `journal_min_age_secs` | 1,800 | 0–86,400 | 86,400 | 容量允许时的目标不驱逐窗口 |
| `journal_event_max_bytes` | 1 MiB | 64 KiB–2 MiB | 2 MiB | 超限改 resync 指引 |
| `client_event_queue_max_events` | 2,000 | 100–20,000 | 20,000 | 超限 resync+4409 |
| `client_event_queue_max_bytes` | 8 MiB | 1–64 MiB | 64 MiB | 同上（字节轴） |
| `events_ws_ping_interval_ms` | 20,000 | 5,000–60,000 | 60,000 | server 发起 |
| `events_ws_idle_timeout_ms` | 90,000 | 30,000–300,000 | 300,000 | ping 不消耗 seq |
| `command_rate_per_client` | 40/s | 5–200/s | 200/s | 429 `rate_limited`；burst = 3×速率（派生，不可独立配） |

### A2 Terminal（`crates/mf-terminal/src/limits.rs`；测试 `tests/contract/{terminal_limits,limits_defaults}.rs`）

| 参数 | 默认 | 允许范围 | hard cap | 说明 |
| --- | --- | --- | --- | --- |
| `frame_max_bytes`（双向） | 256 KiB | fixed | fixed（=默认） | 4413 |
| `input_rate_bytes_per_sec` | 64 KiB/s | 8–512 KiB/s | 512 KiB/s | 4429；burst = 4×速率（派生） |
| `outstanding_output_max_bytes` | 8 MiB | 1–32 MiB | 32 MiB | pause=75%、resume=25% 水位（派生） |
| `slow_client_grace_ms` | 30,000 | 5,000–120,000 | 120,000 | 超时 4409 |
| `replay_ring_max_bytes` | 16 MiB | 1–64 MiB | 64 MiB | 内存 ring/会话 |
| `transcript_flush_interval_ms` | 1,000 | 250–5,000 | 5,000 | durable flush 周期 |
| `transcript_flush_batch_bytes` | 256 KiB | 64 KiB–1 MiB | 1 MiB | flush 批大小 |
| `transcript_session_max_bytes` | 64 MiB | 8–256 MiB | 256 MiB | 单会话终态转录 |
| `transcript_retention_days` | 14 | 1–90 | 90 | 终态后保留 |
| `transcript_project_cap_bytes` | 1 GiB | 128 MiB–4 GiB | 4 GiB | LRU 清已终结会话 |
| `writer_lease_ttl_ms` | 10,000 | 4,000–60,000 | 60,000 | 续租复验 epoch |
| `writer_lease_renew_after_ms` | 6,000 | 派生（60%×ttl） | 随 ttl | 不可独立配置 |
| resize cols/rows 边界 | cols 2–500、rows 2–300 | fixed | fixed（=默认） | 协议/安全上限 |
| `resize_max_rate` | 10/s | 1–20/s | 20/s | 服务端合并窗口 = 1000/rate（派生） |
| `attach_timeout_ms` | 5,000 | 1,000–30,000 | 30,000 | 升级后首帧 |
| `terminal_ws_ping_interval_ms` / `terminal_ws_idle_timeout_ms` | 20,000 / 90,000 | 5,000–60,000 / 30,000–300,000 | 60,000 / 300,000 | server 发起 |
| `pty_drain_max_block_ms` | 10 | budget | budget（不可配） | reader 反压工程上限 |

### A3 Web/auth（`crates/mf-web/src/limits.rs`；测试 `tests/contract/{auth_limits,limits_defaults}.rs`）

| 参数 | 默认 | 允许范围 | hard cap | 说明 |
| --- | --- | --- | --- | --- |
| `bootstrap_nonce_ttl_secs` | 120 | 30–600 | 600 | 128-bit、单次、URL fragment |
| `web_session_ttl_secs` | 43,200 滑动 / 86,400 绝对 | 滑动 600–86,400 | 绝对上限 fixed 86,400 | 内存态，Core 重启失效 |
| `csrf_entropy_bits` | 256 | fixed | fixed（=默认） | 内存态 |
| `auth_exchange_rate_per_minute` | 10 | 5–60 | 60 | 每 source，防爆破 |

### A4 命令/审计（`crates/mf-kernel/src/limits.rs`；测试 `tests/contract/{retention_gc,limits_defaults}.rs`）

| 参数 | 默认 | 允许范围 | hard cap | 说明 |
| --- | --- | --- | --- | --- |
| `receipt_retention_days` | 30 | 7–365 | 365 | 未终结/被审计引用不清理 |
| `receipt_max_rows_per_store` | 200,000 | 10,000–1,000,000 | 1,000,000 | 超限删最旧终态 |
| `operation_retention_days` | 90 | 7–365 | 365 | 终态后 |
| `audit_retention_days` | 365 | 30–3,650 | 3,650 | append-only |
| `gc_interval_ms` | 3,600,000 | 300,000–86,400,000 | 86,400,000 | 周期 GC + 启动时 |
| `operation_progress_interval_ms` | 1,000 | 250–10,000 | 10,000 | 进度事件 |

### A5 安装与 Provider（`crates/mf-installer/src/limits.rs`；测试 `tests/contract/{installer_limits,limits_defaults}.rs`）

| 参数 | 默认 | 允许范围 | hard cap | 说明 |
| --- | --- | --- | --- | --- |
| `install_plan_ttl_secs` | 600 | 60–3,600 | 3,600 | plan_handle+digest+catalog_revision |
| `install_job_timeout_ms` | 1,800,000 | 300,000–7,200,000 | 7,200,000 | 每 job |
| `install_cancel_kill_grace_ms` | 60,000 | 10,000–300,000 | 300,000 | 协作取消后强杀 |
| `install_download_max_bytes` | 2 GiB | 16 MiB–8 GiB | 8 GiB | 每 artifact |
| `install_archive_max_bytes` | 4 GiB | 64 MiB–16 GiB | 16 GiB | 解压后 |
| `install_redirect_max` | 5 | fixed | fixed（=默认） | 仅预览冻结域名 |
| `install_download_stall_timeout_ms` | 60,000 | 10,000–300,000 | 300,000 | 无字节即失败 |
| `install_output_max_bytes` / `install_output_max_lines` | 2 MiB / 10,000 | 256 KiB–16 MiB / 1,000–100,000 | 16 MiB / 100,000 | 脱敏后 job journal |
| `discovery_probe_timeout_ms` | 5,000 | 1,000–30,000 | 30,000 | 每 candidate 命令 |
| `provider_model_cache_ttl_secs` | 300 | 60–86,400 | 86,400 | Provider Type 级声明夹于范围内 |
| `provider_probe_timeout_ms` | 10,000 | 2,000–30,000 | 30,000 | 远端模型目录探测 |
| `provider_probe_retries` | 2 | 0–3 | 3 | 退避 500 ms、2,000 ms ±20% 抖动（fixed 序列） |

### A6 Root/Elevated（`crates/mf-elevated/src/limits.rs`；测试 `tests/contract/{heartbeat,orphan_grace,limits_defaults}.rs`）

| 参数 | 默认 | 允许范围 | hard cap | 说明 |
| --- | --- | --- | --- | --- |
| `broker_heartbeat_interval_ms` | 2,000 | 1,000–10,000 | 10,000 | Core↔Broker 心跳 |
| `broker_heartbeat_miss_limit` | 3 | fixed | fixed（=默认） | 连失判定断开 |
| `root_host_orphan_grace_ms` | 300,000 | 60,000–1,800,000 | 1,800,000 | 到期终止 Root process group |
| `broker_request_ttl_ms` | 30,000 | 10,000–120,000 | 120,000 | 一次性 nonce 有效期 |
| `root_spool_max_bytes` | 32 MiB | 4–256 MiB | 256 MiB | 每会话 ACL 保护 spool |

### A7 生命周期（`crates/mf-kernel/src/limits.rs`、`singleton.rs`；测试 `crates/mf-companions/tests/contract/{discovery_lifecycle,safe_shutdown,limits_defaults}.rs`）

| 参数 | 默认 | 允许范围 | hard cap | 说明 |
| --- | --- | --- | --- | --- |
| `discovery_heartbeat_ms` | 5,000 | 1,000–30,000 | 30,000 | stale = 3×heartbeat（派生） |
| `shutdown_freeze_grace_ms` | 5,000 | 1,000–30,000 | 30,000 | 冻结宽限 |
| `shutdown_drain_timeout_ms` | 120,000 | 30,000–600,000 | 600,000 | 超时报告 blocker 并终止退出 |
| `forced_kill_grace_ms` | 10,000 | 2,000–60,000 | 60,000 | 强制模式子进程组 |
| `handoff_reacquire_window_ms` | 60,000 | 15,000–300,000 | 300,000 | 新 Core 失败回退窗口 |

### A8 Windows 首发（`packaging/windows/` + `crates/mf-companions`；测试：MSI 安装/升级/卸载矩阵；全部 fixed）

| 项 | 值 |
| --- | --- |
| `windows_min_build` | 19042（Win10 20H2）+ Win11（fixed） |
| `windows_arch` | x64；arm64 后置（fixed） |
| `windows_package_format` | per-user WiX MSI + bootstrapper exe；side-by-side `versions\<semver>\` + `current.json`；无 Service/自启动（fixed） |
| `supported_browsers` | Edge/Chrome/Firefox 当前-2（fixed） |
| `windows_privilege_model` | Core/launcher/tray/picker asInvoker；仅 Broker requireAdministrator（fixed） |

### A9 性能预算（budget，不可配置；测试 `web/e2e/perf.spec.ts` + `crates/mf-kernel` perf + `crates/mf-terminal` flood）

| 预算 | 值 |
| --- | --- |
| `journal_append_p99_ms` | ≤5 |
| `journal_fanout_additive_ms`（8 客户端） | ≤10 |
| `command_p95_ms`（非长任务） | ≤50 |
| `workspace_snapshot_p95_ms` | ≤500 |
| `workflow_snapshot_1000_nodes_max_bytes` | ≤5 MiB |
| `dagre_layout_1000_nodes_ms` | ≤250（原型实测 100/500/1000 = 11–16/49/105–125 ms） |
| `node_drag_p95_ms`（1000 节点） | ≤32 |
| `viewport_dom_nodes` | ≤150 |
| `flood_ctrlc_delivery_ms` | ≤200 |
| `typed_delta_node_position_set_bytes` | ≤512 |
| `core_cold_start_p95_ms`（≤10 Project） | ≤5,000 |
| `web_first_interactive_s` | ≤2 |
| `web_initial_js_gzip` | ≤1 MiB（asset 总量 ≤16 MiB） |

### A10 Observability（`crates/mf-kernel/src/limits.rs`；测试 `tests/contract/limits_defaults.rs`）

| 参数 | 默认 | 允许范围 | hard cap | 说明 |
| --- | --- | --- | --- | --- |
| `log_rotate_max_bytes` | 10 MiB | 1–100 MiB | 100 MiB | `~/.monkeyfence/logs/core.log` 单文件 |
| `log_rotate_keep` | 5 | 1–20 | 20 | 保留份数 |

---

## 附录 B：Core 重启语义总表

| 对象 | 重启后 |
| --- | --- |
| server_instance_id / stream_epoch | 新值；旧 epoch 客户端 resync |
| Web session / nonce / CSRF / client id / Controller lease | 全部失效（重新 bootstrap） |
| Terminal writer lease | 全部撤销 |
| live PTY | lost/Needs You；只读 transcript 至 durable_through_seq（complete=false）；普通 PTY 无重附着，Root host 走 §10.3 read-only reattach 契约 |
| Root Mode | 强制 off；旧 root epoch 失效；host 走 orphan grace/read-only reattach 契约 |
| 未终结 Operation / intent | `reconciling`；以 target receipt 为权威，不重放业务写 |
| 已终结 command receipt / audit / receipt | 持久保留（A4 规则） |
| workflow journal | 新 epoch 重建；旧 outbox 标 reconciled |
| CoreOwnerLock | 经 handoff/启动校验重新 acquire |

## 附录 C：决策追溯

本文每章对应权威输入：§2–§5 ← `2026-09-01-gpui-web-migration-retirement.md`（CoreKernel/批次/owner handoff/多库 command）+ `2026-09-01-web-api-terminal-protocol-v1.md`（#4/#9 状态所有权与契约）；§6 ← `2026-09-01-web-gateway-root-security.md`（#8）；§7–§8 ← `#9` + `2026-08-31-web-dag-terminal-stack.md`（checkpoint 方案被 #9 明确覆盖：v1 无 checkpoint）；§9 ← `2026-09-01-agent-plugin-cli-install-contract.md`（#10）；§10 ← #8+#10；§12 ← `2026-09-01-node-session-preview-prototype-results.md`（#7 原型验证）+ #5；§13 ← 迁移文档；ADR 0005 supersede 细则（VCS UI 删除、Manifest v3 取代旧 agent contributions、Web 控件渲染声明式 Schema）。旧 GPUI 设计文档（`docs/superpowers/specs/2026-08-*`）仅为历史。
