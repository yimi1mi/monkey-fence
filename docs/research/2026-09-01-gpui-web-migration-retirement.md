# GPUI → Web Interaction Client 迁移与退役路径

- 日期：2026-09-01
- Wayfinder ticket：[确定 GPUI 迁移、并行运行与退役路径](https://github.com/yimi1mi/monkey-fence/issues/6)
- 依赖：Wayfinder #2/#3/#4/#5/#7/#8/#9/#10
- 状态：可进入规格化与 tickets 拆分

## 决策摘要

迁移采用“**先建立唯一 CoreKernel seam，再拆进程，再并行 UI，最后删除 GPUI**”。禁止 Web 与 GPUI 各自直接写 Store/Orchestrator/SessionRegistry；整个迁移期只有一个 Rust Core Service 持有业务状态、调度器、PTY、插件与 Secret。

最终产品交互全部在 Web，Rust 只作为无界面的 Core Service 和少量本地系统 companion：

```text
默认浏览器
    │ HTTP / Workflow WS / Terminal WS
    ▼
monkeyfence-core                 每 OS 用户唯一、普通权限、无产品 UI
    ├─ CoreKernel               唯一命令/状态接口
    ├─ Project Registry
    ├─ per-project Store + Orchestrator
    ├─ Catalog / Plugin / Secret / CLI Installation
    ├─ SessionRegistry（所有 Session 的逻辑 owner）
    ├─ normal SessionRuntime + PTY + TerminalJournal/Transcript
    ├─ ProjectionHub + event journal
    ├─ WebGateway + embedded hashed assets
    └─ mfctl RunControl IPC

monkeyfence-launcher             短生命周期：start/open/status/stop
monkeyfence-tray                 极薄 companion：打开 Web、摘要、安全退出、目录选择
    └─ 不拥有 Core，不承载第二套产品 UI，不配置自启动

monkeyfence-picker               按需短生命周期原生目录选择 helper；tray 不在时也可用

按需独立进程
    ├─ Elevated Broker
    ├─ Elevated Runtime Host（Root PTY/进程组的物理 owner）
    ├─ Elevated Install Host
    ├─ Plugin Worker
    └─ Agent CLI
```

`mfctl` 继续是 Agent Run 内部 Settlement/控制工具，不与面向用户的 launcher 合并。关闭浏览器或 tray 不停止 Core；只有显式安全退出才评估并终止服务。

安全退出必须返回并展示全部 active Run/Session/Installation Job；存在活动工作时要求用户明确确认，不能由 tray close、browser close 或静默更新触发。首版不做空闲自动退出。

## 当前事实与风险

当前 `main.rs` 直接启动 GPUI 并创建 `Workspace`，`Workspace::new` 再创建 `AppCtx`。因此窗口、Store、Orchestrator、Session Registry、PTY、插件和 `mfctl` Named Pipe 仍在一个 UI 进程里。

`AppCtx` 已包含跨项目 Core 雏形，但公开暴露多个可变子系统：

- `SessionRegistry`、`PluginRegistry`、`CatalogStore`、`GlobalLimiter`、config/catalog；
- 每项目 `Store/Orchestrator`；
- 面向 GPUI 的 `ProjectOverviewHub`。

GPUI 页面目前可以绕过统一命令层直接访问 Store/Orchestrator/Registry。若此时直接加入 Web API，会出现两个写入面、两个投影模型和不可审计旁路，破坏 #4/#8/#9 的 Controller Lease、CAS、幂等和 Secret 边界。

其他现状：

- `ProjectOverviewHub` 是带真实 project path/rowid 的整表重建投影，不是 Snapshot + event journal；
- `PtySession` 仍是固定 Screen、256 KiB tail 和 raw writer，key 为 `project path + rowid`；
- 项目库是 `.mf-agent/workflow-v1.db` schema v6，用户目录库是 `catalog-v1.db` schema v1；
- `session.json` 混合 Project 列表与 GPUI open files/layout；
- GPUI DAG 没有持久节点坐标；
- 根 `Cargo.toml` 直接依赖 `D:/workspace/zed/crates/gpui`，`gpui_platform` 是 Windows vendor；Named Pipe 与 KeepAwake 也仍是 Windows-only；
- 当前没有 tray，关闭 GPUI 窗口等于结束宿主进程。

最大风险不是“页面重写”，而是迁移时仍留有绕过 CoreKernel 的写路径，以及试图跨进程搬运 live PTY。

## CoreKernel 深模块

不把 `AppCtx` 原样包装成 HTTP。先形成 UI-neutral 深模块，所有字段私有，外部接口限制为：

```text
dispatch(CommandEnvelope) -> CommandResult | OperationHandle
snapshot(SnapshotQuery) -> SnapshotEnvelope
subscribe_events(EventCursor) -> EventStream
attach_terminal(SessionHandle, TerminalAttach) -> TerminalChannel
shutdown(ShutdownIntent) -> ShutdownAssessment
```

Axum WebGateway、legacy GPUI adapter、launcher/tray local IPC 和测试 harness 都只能调用这组接口。认证方式可因 transport 不同，但进入 CoreKernel 后共享：

- Controller/Observer role 与 lease；
- aggregate revision/CAS；
- command idempotency 与 Operation；
- opaque handle 和 Project scope；
- Secret/capability/redaction；
- audit 与 projection publication barrier。

本地 legacy GPUI 不能被列为“可信内置调用”而免检。进程拆分后，它通过受当前用户 ACL 保护的 versioned local transport 注册成 Client，仍只能成为 Controller 或 Observer。

### Root Session 的逻辑与物理所有权

Core SessionRegistry 是所有 Session handle、Run 关联、writer lease、事件与 transcript 的逻辑 owner。普通 PTY 的物理 owner 在 Core SessionRuntime；Root PTY/进程组按 #8/#10 由 session-scoped Elevated Runtime Host 物理持有。

- Browser/GPUI 只连接 Core，永不连接 elevated host；
- Core 在 Terminal input/resize 的线性化点验证 Controller/writer lease，再把带单调 owner epoch/session capability 的消息发给 host；host 也复验，旧 Core/旧 lease 不能写；
- host 只服务一个 Session，不能创建新 Session、安装 job 或通用命令；
- Core channel 断开后 host 立即拒绝新 input/resize/control，继续把已脱敏输出写入 ACL 保护的 session spool，并进入 bounded orphan grace；
- 新 Core 只能用持久 host receipt + OS identity 做 read-only reattach，Session 进入 Needs You。恢复 writer/control 必须重新开启 Root Mode 并由 Broker 为该 Session 续发 capability；
- grace 到期仍未安全重附时，host 终止 Root process group 并留下可导入的 exit/spool 记录，避免无人控制的高权限进程永久存活。

### 当前代码归位

| 当前模块 | 目标位置 |
| --- | --- |
| `app_ctx.rs` | 深化为 UI-neutral CoreKernel，字段私有 |
| `project_overview.rs` | 替换为一致 Snapshot + event journal 的 ProjectionHub |
| `runtime_host.rs`、`pty_spawn.rs` | Core SessionRuntime |
| `adapter_launch.rs` | Core Plugin/Agent launch 内部实现 |
| `pipe_server.rs` | RunControlIpc；Windows Named Pipe / Unix Domain Socket adapter |
| `workflow_editor.rs` | 只保留领域校验；GPUI layout/render 不进入 Core |
| `run_monitor.rs` | Needs You/Run projection 进入 Core，GPUI render 留待删除 |
| `term.rs` | 暂留纯 VT screen/transcript 能力；不作为 Web 第二终端状态机 |
| `Workspace` 与 GPUI pages | legacy client，最终整体删除 |

## 迁移批次与阻塞关系

### 批次 0：冻结基线

- 为 Project Store v6、Catalog v1、session.json、插件 pin、运行恢复、Settlement/Handoff 建 golden fixtures；
- 建立项目工作流 create/edit/run、Needs You、Preview/Node terminal、重启恢复端到端基线；
- 记录当前 Windows 安装、升级、卸载与数据库备份行为；
- 原型仍是 throwaway evidence，不直接复制 Node server 到生产。

### 批次 1：先修存储与所有权基础

- 为 Project/Catalog schema 增加“future version 必须拒绝”的守卫和测试，禁止旧代码改写更高 `user_version`；
- 引入 persistent public ids、aggregate revisions、command intent/target-local receipt/outbox、Operation 与 Transcript Store；
- 引入 per-user CoreOwnerLock、owner epoch、stale discovery fencing 和每 Store owner metadata；
- 新建 `catalog-v2.db` 并从 v1 幂等迁移，保留 v1 只读备份。当前 pre-Bridge 二进制不是回滚目标，不能再打开 v2；
- 建立 service-v1/Project/Catalog 多库崩溃恢复测试。

CoreKernel 在这些 durable contract 落地前不能声称已经支持 opaque handle、跨重启幂等或 Operation。

### 批次 2：抽取 CoreKernel，GPUI 仍同进程

- 将 AppCtx、RuntimeHost、ProjectOverview、adapter launch、mfctl IPC 移入不依赖 GPUI 的 UI-neutral crate/module；
- GPUI 所有写入改走 CoreKernel dispatch，所有读取改走 Snapshot/Projection；
- AppCtx 字段改私有，删除 UI 直接拿 Store/Orchestrator/SessionRegistry 的路径；
- 用仓库检查和测试保证新增 UI 代码不能引用内部 Store mutation API。

### 批次 3：Terminal v1 与 Root host 两条 lane 并行合流

- B3a Terminal lane：所有普通 PTY 路径统一为 `raw PTY → redaction → seq/journal/transcript → Screen/fan-out`，实现 opaque Session handle、epoch/seq、resize、ACK/replay、writer lease、history gap、crash-incomplete transcript；
- B3a：GPUI 改用 CoreKernel TerminalChannel，不能再拿 raw writer 或直接调用 `SessionRegistry::send_prompt_raw`；
- B3b Root lane：从 B1+B2 同时启动 session-scoped Elevated Runtime Host protocol/fixture/runtime，验证 owner epoch、writer lease、spool、read-only reattach 与 orphan grace；
- B3 gate 只有在 B3a+B3b 的同一 TerminalChannel contract tests 全部通过后完成。

Terminal v1 是拆进程的硬前置；不能先把 GPUI 变成远程 client，再补它依赖的数据面。

### 批次 4：发布 in-process Bridge A

- 仍由 GPUI 进程托管 CoreKernel，用户行为不变；
- Bridge A 已理解 Project v7、Catalog v2、service-v1、Terminal v1、Manifest v3、Provider Type、CLI Installation/Receipt、Root/host record 与 owner fencing，并具备这些持久状态的 runtime reader/安全阻断路径；
- 加入 freeze/drain/handoff 协议和 side-by-side bundle 管理，但此版本不执行进程所有权迁移；
- Bridge A 先读后写：它尚不产生新产品配置，也必须能读取、展示或安全阻止后续 v1 会写入的全部 durable feature。它成为第一个 rollback anchor；当前 pre-Bridge 版本明确不兼容新 durable state。

### 批次 5：原子拆分为 standalone Core

- bundle 新增 `monkeyfence-core`、singleton/discovery、launcher、tray/picker 和 loopback WebGateway skeleton；
- GPUI 成为通过 versioned local transport 连接独立 Core 的 legacy client，不再拥有 Store/PTY；
- 切换使用下述 freeze/drain/fencing 协议，active Run/Session/Installation Job/不可中断 Operation 非零时延期；
- tray 只做 companion，关闭/崩溃不影响 Core；不注册开机自启动。

### 批次 6：Web foundation 隐藏开发

- 实现同版本内嵌 Web bundle、bootstrap、CSP/Host/Origin、Snapshot、workflow events 和 Workbench 读取；
- 发布运行时不依赖 Node.js/CDN；Core 只提供本版本内容哈希 asset；
- 在 CI/dev-only harness 中验证 service lifecycle、重连、1000 节点和跨项目导航；此阶段不给普通用户创建 Web bootstrap，不与 legacy GPUI 竞争 Controller。

不能发布“自动成为 Controller、但没有写能力”的 read-only Web。用户可连接的 Web 必须至少具备核心领域写入。

### 批次 7：发布 Web 完整写入并与 GPUI 并行

- Workflow create/edit/rename/delete、节点/连线/布局、run/cancel/retry/respond/settle 全部切到 #9 command/CAS；
- Provider Profile、Agent Instance、模型探测、CLI install/update/repair/uninstall 与 Root Mode 接入 Operation；
- Node Session Panel/Preview Session 使用 xterm + Terminal v1，slash/skill/TUI 由真实 CLI 处理；
- 系统目录选择由 tray 或短生命周期 picker helper 触发，browser 只拿 opaque Project handle；
- Web bootstrap 成为 Controller 时 GPUI 降为 Observer；Web 已具备写能力，不会出现无人可写。GPUI 可显式 takeover 作 Bridge fallback。

### 批次 8：Web 默认，GPUI 隐藏回退

- launcher 默认打开浏览器；tray 提供打开、跨项目 Run/Needs You 摘要与安全退出；
- GPUI 只通过 `--legacy-ui`/诊断开关打开，普通导航不展示；
- 完整 bundle 保留至少一个稳定 Bridge 周期与回退演练；
- 代码编辑器、文件树、Git/P4、diff、通用 terminal 不迁移到 Web，也不阻塞默认切换。

### 批次 9：删除 GPUI

- 先删 Agent/Provider/Plugin settings、workflow canvas、agent workspace、workflow runs、run monitor render；
- 再删 Workspace/navigation/theme，以及 editor/file tree/search/VCS/diff/general console；
- 删除 `gpui`、`gpui_platform`、本地 Zed path 和 Windows GPUI resource 依赖；
- `crates/mf` 不再包含产品 UI，只保留/拆出 Core、launcher/tray 和 mfctl 所需代码。

## 可并行实施工作

硬依赖 DAG：

```text
B0 → B1 storage/schema guard + owner fencing → B2 CoreKernel seam

B2 → B3a Terminal v1
B2 → B3b Root host protocol/runtime
B2 → M3 Manifest v3 / Installation readers
B2 → B6 hidden Web foundation

B3a + B3b + M3 → B4 in-process Bridge A → B5 standalone Core split
B5 + B6 → B7 released Web parity → B8 Web default → B9 GPUI delete
```

只有 B1 的 durable DTO/contract 与 B2 的 CoreKernel interface 冻结后，以下工作可并行成独立 tickets：

- Web Workbench/React Flow/xterm 组件与 contract fixture；
- WebGateway/auth/header/asset packaging；
- service singleton/discovery/launcher/tray/picker；
- TerminalJournal/Transcript/PTY protocol；
- Plugin Manifest v3/CLI Installation readers 与 Root host lane；
- Windows installer 与升级 gate。

并行 lane 可以提前编码，但达到 B4/B5/B7 gate 时必须满足 DAG 中所有前置，不得因“代码可编译”跳过 durable schema、Terminal 或 owner handoff。它们只能依赖已冻结的 DTO/contract tests，不能各自定义新的领域状态。实现阶段必要时可把独立前端或机械迁移 ticket 交给 Zcode；每个 ticket 完成后仍由 Codex 按 Spec + Standards review。

## Core owner 原子移交

SQLite WAL 不是 Core ownership lock。Bridge A 与之后版本必须实现以下线性化协议：

1. updater/launcher 获得当前用户 update lock，并验证当前 bundle/owner identity；
2. old Core 进入 `freezing`，拒绝新 command/Session/Install/Root job，旋转 Controller/Root/writer lease epoch；
3. 在 publication barrier 上等待已线性化 HTTP command、PTY input queue、outbox publication 与可中断 Operation drain；
4. 再次确认 active Run/Session/Install/不可中断 Operation 为零；检查与 freeze 之间不能再进入新工作；
5. flush Transcript/outbox/command target receipt，关闭所有 Project/Catalog/service Store handle；
6. old Core 写入带 build、schema、owner epoch、DB identity 的 handoff manifest，释放 CoreOwnerLock，并永久进入 `handed-off`，不能自行 reopen；
7. new Core 原子 acquire CoreOwnerLock，校验 handoff/DB owner epoch/schema/bundle，更新 discovery 后才接受 Client；
8. new Core 启动失败时，仅 Bridge A 或 schema-compatible previous bundle 可以在尚无新写入的前提下以更高 owner epoch reacquire；否则保持停止并给出恢复诊断。

pre-Bridge 当前版本没有 owner fencing，不能直接在线移交。升级到 Bridge A 时必须先安全退出旧进程；从 Bridge A 的下一次升级开始才允许协议化 handoff。

## 数据迁移

不重建、不双写的权威数据：

- `.mf-agent/workflow-v1.db`；
- Task/Pipeline Revision/Step/Agent Run/Settlement/Handoff；
- Agent Instance snapshot、Plugin pin、Execution Lease 和加密 Secret。

Catalog 内容从 `catalog-v1.db` 一次性迁入 `catalog-v2.db`，之后只有 v2 是权威；v1 保留为只读迁移备份，不双写。这样 pre-Bridge 旧程序最多破坏/改写已废弃的 v1，不会把 v2 的 `user_version` 降回 1。

Web 不创建第二份 Project 业务数据库。新增用户级 `service-v1.db`，保存跨项目协调状态：

- Project Registry 与稳定随机 Project public id；
- 全局唯一 command intent/index、Operation coordinator、durable audit；
- per-user singleton/lifecycle 元数据、durable feature activation registry 与 migration marker。

当前 `stream_epoch` 与有界 workflow event journal 仍是进程内状态，不从 service DB 恢复；Core 重启生成新 epoch。持久 opaque resource id 放在其所属 Project/Catalog Store，而不是维护易漂移的 rowid 映射表。

### 多库 command 幂等与 outbox

service DB 与 Project/Catalog SQLite 之间不宣称原子事务。每条 command 使用可恢复的两阶段 intent：

1. service DB 先按 command id + semantic digest 原子保留 intent，固定 target store/aggregate、authenticated principal、client id、Controller epoch 与可选 Root epoch；同 id 不同 digest/target 立即拒绝；
2. 目标 Project/Catalog/service Store 在事务线性化点重新验证当前 principal、Controller lease、expected aggregate revision 与 Root epoch；失效且尚未线性化的 intent 持久终结为 `controller_lease_expired/root_epoch_expired/cancelled`，不得应用。验证成功后在**同一个领域事务**内写业务效果、target-local command receipt 和 projection outbox；
3. coordinator 根据 target receipt 将 service intent/Operation 标记完成，ProjectionHub 从 target outbox 发布 #9 event；
4. crash 在步骤 1 后且尚无 target receipt：Core 重启使原 Controller/Root epoch 失效，reconciler 终结 intent，不执行旧 client 命令；crash 在步骤 2 后：target receipt 证明效果已线性化，reconciler 只补 service result/event publication，不重放业务写；
5. 真正跨多个 Store 的动作建模为带幂等 step receipt 的 Operation saga，不承诺同步全局原子提交；失败时进入可观察 compensation/Needs You。

这保证 #9 的跨重启同 command id 语义，避免 Project commit 后、service receipt 前崩溃造成重复执行。

长 Operation 只在步骤 2 的目标事务创建成功后算 accepted；之后可以按自身 durable policy 继续。Root Operation/Run 的尚未启动 step 仍在每次 launch 时复验 active Root epoch，Core 重启或 Root Mode 关闭后进入 Needs You，不能把已接受 Operation 当作永久 Root 授权。

正常运行时 target transaction 与 outbox drain 仍在 #9 projection publication barrier 内，事件 append 成功后才释放 Snapshot cursor/command response。Core crash 会生成新 stream epoch；恢复器以 target receipt 为权威完成 intent，并把旧 epoch 的 outbox 标为 reconciled，不向新 epoch 重放一个 Snapshot 已经包含的陈旧 delta。

### Project Store v7（expand-only）

新增：

- persistent public ids；
- `workflow_collection_revision`、semantic/presentation revision；
- 节点位置、viewport、折叠等 presentation metadata；
- Terminal Transcript/exit/crash-incomplete metadata；
- command 所需的额外 aggregate revision；
- target-local command receipt 与 projection outbox。

旧 GPUI 没有坐标。Web 首次打开使用确定性 Dagre 布局，随后通过 presentation command 写入；不伪造迁移前坐标。

### Catalog Store v2（expand-only）

使用新文件 `catalog-v2.db`。新增 Manifest v3 的 Provider Type、CLI Installation、Installation Receipt、recipe/版本 pin、Provider model cache、target receipt/outbox。Secret ciphertext 继续走现有 Secret Store，不复制到 service DB。

### session.json

- 只导入 Project 列表与可用 foreground Project；
- `open_files`、active file、GPUI panel/layout 不迁移；
- 原文件保留，写 migration marker，导入幂等；
- 缺失 Project 路径保留成失效记录供用户清理，不自动删除真实目录。

## 回滚与升级

- schema 升级前用 SQLite Backup API 为每个数据库生成一致备份与 manifest，不在数据库打开时裸复制文件；
- Core、同版本 embedded Web assets、launcher、tray/picker、mfctl、Broker/Runtime/Install hosts 是一个原子 versioned bundle；安装到 side-by-side 版本目录，由稳定 launcher 的 active pointer 选择；
- 先发布已理解 v7/v2/service-v1、带 future-schema guard 但仍托管 legacy GPUI 的 Bridge A；
- rollback 切回的是整个 schema-compatible bundle，不存在“只回滚 UI/launcher”。旧 Core 仍只提供它自己 bundle 的 assets；
- rollback eligibility 同时检查 `min/max readable schema`、`durable_feature_versions`、plugin manifest/receipt reader、Operation/host protocol 与 binary compatibility；只看 DB schema 不够。pre-Bridge 当前二进制不是 rollback target；
- 每个新的 durable feature 都遵循 expand/contract：版本 N 先发布 reader/runtime/安全阻断且不写新格式，版本 N+1 才启用 writer，N 成为新的滚动 rollback anchor。若后续功能超出 Bridge A 的 v1 feature set，必须先发布新的 Bridge B，不能继续把 Bridge A 当回退；
- active pointer 切换与 Core owner handoff 使用同一 zero-active freeze/drain gate。保留 previous bundle，直到新 Core 健康检查和第一轮 contract smoke 通过；
- 如果升级后已有新写入，不自动恢复旧备份，以免丢失数据；恢复备份是显式灾难恢复；
- binary/Core ownership 切换和 schema migration 只在一个 singleton Core owner 下执行；
- active Run/Session/Installation Job/不可中断 Operation 存在时延期 Core replacement；不能为了升级强杀 Agent；
- plugin package、CLI Installation 与 receipt 被活动 Run/replay lease pin 时不得清理；
- Core crash 后无法重附着的 PTY 进入 lost/Needs You，并从 durable transcript 恢复到 `durable_through_seq`，不伪装 live。

## GPUI 退役门槛

满足全部条件后才能删除：

- Web 完成 Project Workflow 编辑/自动保存/运行/Needs You/节点终端/Preview；
- Web 完成 Provider Profile、Agent Instance、模型下拉、Plugin/CLI 安装与 Root Mode；
- 所有 GPUI 写路径已走 CoreKernel，仓库检查不再发现 UI 直接写 Store/Orchestrator/Registry/raw PTY；
- Web/GPUI 并行 Controller/Observer、takeover、旧 lease 拒绝通过；
- 真实 Codex/Claude/GLM slash/skill/TUI/IME/Unicode/resize/reconnect/洪泛通过；
- v6→v7、Catalog v1→v2、session.json→Project Registry 迁移幂等，Bridge rollback 演练通过；
- 活动 Run/Session/Install 存在时升级确实延期；
- Windows 默认 Web 版本至少经过一个稳定 Bridge 周期；
- Windows/macOS/Linux fake installer + fake broker/host contract tests 通过；
- 没有仍在产品范围内、但只存在于 GPUI 的能力。

删除 GPUI 不删除项目文件、Git/P4 数据、`.mf-agent` Store 或用户 CLI 配置。已明确 out of scope 的代码编辑器、VCS UI 和通用终端直接退役，不做 Web 对等实现。

## 平台策略

当前正式基线是 Windows；首个 Web 默认版与 GPUI 删除以 Windows **真实产品包**验收为门槛，不用尚未存在的 macOS/Linux 产品包阻塞这次迁移。但 #10 已决定的 Windows/macOS/Linux fake installer + fake broker/host 契约测试、平台 trait 编译与协议 golden tests 仍是 GPUI 退役前置。CoreKernel、IPC、PTY、tray、Root helper 和路径抽象不得继续硬编码 Windows，且不能宣称未完成真实打包的平台已受支持。

### Windows 首发

- 包含 core、launcher、tray/picker、mfctl、Web assets、Broker/Runtime/Install hosts；
- Core/launcher/tray 使用 asInvoker，只有 Broker 带 `requireAdministrator`；
- singleton/discovery/Named Pipe DACL 限当前用户 SID；
- 不注册 Windows Service 或开机自启动；
- 关闭 tray 不停止 Core，安全退出需评估 active work；
- 移除 `D:/workspace/zed`、GPUI vendor 和旧 UI resources。

### 后续 macOS/Linux bring-up

- macOS：签名/notarize app/embedded helpers，Service Management privileged helper，Unix Domain Socket，status item；
- Linux：XDG runtime/data/state、Unix Domain Socket、polkit helper；tray 是增强能力，没有 tray 时 launcher/CLI 仍可打开和安全退出；
- 增加 Unix PTY resize/process-group、keep-awake、目录选择、安装/升级矩阵；
- 分别完成真实构建与包测试后再声明支持，不能仅凭条件编译或 Unix PTY 源码存在宣称可用。

## 验收矩阵

- 关闭 browser/legacy GPUI/tray 后 Core、Run、Session 按契约继续；
- singleton 竞争、stale discovery、crash restart、安全退出都有确定结果；
- Web 与 GPUI 同开只有一个 Controller，Observer 服务端真实只读；
- 同一 Store 只有一个 Core owner，整个仓库没有 UI mutation bypass；
- Snapshot/event/command/Terminal v1 contract tests 与 golden fixtures 通过；
- Windows/macOS/Linux CI 的 fake installer/broker/elevated host、IPC/PTY trait contract tests 通过；
- v6 Project、v1 Catalog、旧 session fixture 可幂等迁移；
- Task/Revision/Step/Run/Settlement/Handoff/Plugin pin 迁移前后等价；
- 活动工作延期升级，零活动时所有权切换与 rollback 成功；
- Web bundle/Core protocol 不匹配会停止连接并提示升级；
- Windows 安装、启动、tray、目录选择、IPC、PTY、CLI install、Root、升级和卸载通过；
- GPUI 删除后 clean checkout 不再需要 Zed/GPUI 本地依赖即可构建发布物。
