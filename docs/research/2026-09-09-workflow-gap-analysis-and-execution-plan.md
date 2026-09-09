# 工作流能力差距分析与 zcode 执行计划

分析日期：2026-09-09。代码基线：`main@86c19890be8345b46264c7631c23abf5d7d125ef`。

本文是本次代码分析及执行交接材料；需求、PRD 和正式 tickets 的权威入口仍为 GitHub Issues。没有创建或修改远端 Issue。本次连接器读取该仓库 Issue 返回 404，因此本文不对远端需求的最新状态作判断。

## 1. 目标与结论

用户的三个目标：

1. 编排工作流，每个节点有清晰职责。
2. 支持动态改变节点图。
3. 节点之间传递的 prompt 可视化，用户可以编辑。

第 3 点按“可视化”理解。第 2 点按“运行前自由编辑；运行中暂停派发后修改尚未启动的节点，再继续”落地。Agent 主动提议改图列为后续扩展，不把无限循环、自主改写正在执行的节点作为首版前提。

**结论：已有可复用的编排基础，最缺的是可查看、可修改、可追溯的节点输入，以及经过 Core 权威命令支持的运行中改图。应继续完善现有 Rust Core + React Web，不重写调度器或恢复旧 GPUI 页面。**

## 2. 现状与代码证据

以下路径均相对项目根目录，行号基于上述基线。

| 能力 | 当前状态 | 证据及缺口 |
| --- | --- | --- |
| 每个节点独立职责和 Agent | 基础已实现 | `crates/mf-agent/src/workflow.rs:60` 的 `WorkflowNodeDraft` 有 `title/instructions/agent_instance_id/deps`；`web/src/workbench/workflow_editor.tsx` 已有编辑表单。职责目前主要靠自由文本表达，尚无明确的输入要求、输出要求和验收字段。 |
| 串行、并行、汇合、重试、显式完成 | 已有实现和定向测试 | `workflow_compiler.rs`、`orchestrator.rs`、`run_mutation.rs`；不能把进程退出或终端空闲当成节点成功。 |
| 运行前编辑 DAG | 已实现基础操作 | `workflow_editor.tsx` 已接入 React Flow 和增删节点、连线、断线、移动命令；编辑对象是 Project Workflow，不是正在执行的 Pipeline Revision。 |
| 运行中修改 DAG | 旧底层有局部能力，当前产品链路未接通 | `orchestrator.rs:1028/1128` 有暂停和保存流水线方法；`mf-kernel/src/command.rs:31` 与 `kernel.rs:110` 的命令枚举没有 pause/resume/apply graph patch。不能仅加 Web 按钮就认定完成。 |
| 节点间传递结果 | 已实现 | `handoff.rs` 有结构化 Handoff；`orchestrator.rs:3582` 加载传递祖先交接；`${nodes.a.summary}`、`${nodes.a.output.report_path}` 等引用会在派发时替换。 |
| 查看上游交接 | 部分实现 | `web/src/workbench/shell.tsx:1068` 显示只读交接卡与 output JSON；这能看到上游结果，但不能看到下游实际收到的完整 prompt。 |
| 完整 prompt 可视化 | 缺失 | `orchestrator.rs:4079` 临时拼接目标、上游摘要、指令和结算协议，直接用于启动；现有模型和运行投影没有对应的持久 prompt 输入记录。 |
| 编辑节点间实际传递的内容 | 缺失关键闭环 | 可编辑工作流指令模板、可在终端输入；没有“下游启动前检查输入 → 修改本次输入 → 确认发送”的调度门控和编辑记录。 |
| 数据流图 | 缺失 | 当前边只有上下游依赖；没有字段映射、输入来源、必填项状态、实际发送内容的展示。运行详情仍是步骤时间轴，未复用 DAG 展示状态与数据流。 |
| 浏览器端到端保障 | 尚未落地 | `web/e2e/bootstrap.spec.ts` 明确为骨架，多数用 `http://127.0.0.1:0/#nonce=fixture`；没有发现真实 fixture 配置，`.github/workflows/ci.yml` 的 Web job 只跑 build/test。 |

### 现有代码中会影响本次实现的具体问题

1. **旧改图方法不能直接作为新能力入口。** `Store::save_edited_revision`（`store.rs:1876`）只插入新的 PipelineDraft/Step 行，未保存工作流 `snapshot_json`；而 `dispatch`（`orchestrator.rs:3260`）找不到冻结节点就回退旧 Profile 路径。该方法还用 `with_conn` 执行多条写语句，而不是一次事务。需要新的事务级工作流 Revision 变更实现，复用既有校验和状态迁移规则。
2. **改版本必须继承交接来源。** 旧方法重新创建 Step 行；`upstream_handoffs` 按当前 Revision 的 Step ID 查交接。只复制成功状态而不建立旧 Agent Run/Handoff 到新 Revision 节点的继承关系，会丢失上游输入来源。这是静态代码显示的接入风险，不能用旧 PipelineDraft 测试通过来证明工作流热修改已经安全。
3. **缺字段现在仍会派发。** `substitute_node_references` / `resolve_handoff_path`（`orchestrator.rs:4130` 之后）把缺少交接或缺字段转成提示字符串；编译器主要检查引用节点是否为祖先，不验证实际字段存在、类型或必填性。下游可能拿到不完整输入继续工作。
4. **图上连线不等于数据传递选择。** 当前构造 prompt 会自动附带所有祖先的摘要，再替换显式引用。新输入映射应明确哪些内容进入 prompt，避免用户以为只传选中字段，实际又自动注入全部祖先摘要。
5. **布局有身份混用。** `workflow_editor.tsx` 给 `autoLayout` 的节点 ID 是 handle，deps 却是语义 key；`graph.ts:57` 按 ID 查依赖，深链层级可能计算错误。当前单测用同一命名空间的 ID，没覆盖真实投影。修复 key→handle 映射并使用真实 wire 形态测试。
6. **编辑器部分删除动作只有本地效果的风险。** React Flow 的 `onNodesChange/onEdgesChange` 直接应用本地变化，而领域删除目前由显式按钮/双击边发命令。统一键盘删除与按钮删除的持久命令路径，失败必须恢复权威图。

## 3. 首版行为约定

### 3.1 职责、数据、prompt 的关系

保留 Step / Handoff / Agent Run 等现有术语。一个节点的职责通过现有 `title + instructions` 承载，增加可选的 `acceptance_criteria`（验收说明）、`output_schema`（自定义 output 的要求）。首版不建立独立的“角色系统”；可提供需求分析、实现、审查三个预设，选择后复制为普通节点配置，用户可自由修改。

输入映射保存在**下游节点**的 `input_bindings` 中，每项包括：本地名称、来源节点 key、Handoff 字段路径、是否必填、可选默认值。图上的边仍由 `deps` 决定，不能再维护一套互相冲突的数据依赖图。

- 绑定只能指向合法祖先；保留现有传递祖先引用。跨多跳引用在界面标为“间接上游”，点击高亮整条依赖路径。
- 保留 `${nodes.<key>.<path>}` 兼容语法；可增加 `${inputs.<name>}` 作为可视化绑定生成的引用。两者必须走同一个解析器。
- 删除节点/断开依赖后，如果留下无效输入引用，提交失败并定位引用；用户通过一次原子批量编辑同时修改图和引用。
- 新节点默认只传显式选择的内容；旧工作流通过显式的 legacy context policy 保留既有“祖先摘要 + 模板引用”行为。迁移后在界面提示该策略，不静默改变旧工作流含义。
- 首版 `output_schema` 仅校验 `Handoff.output`，不改动固定 Handoff 字段。明确支持 JSON Schema 的 object/properties/required/type/items；遇到不支持的关键字报错，不能声称支持完整标准。
- 验收说明会进入业务 prompt，但文字要求不等于程序已验证；机器只验证可执行的输出约束，成功仍来自显式 Settlement。

### 3.2 三种编辑作用域

| 编辑动作 | 生效范围 |
| --- | --- |
| 编辑 Project Workflow 的节点说明/输入映射 | 以后新启动的运行；不改变已冻结的运行 |
| 在运行中修改尚未启动的节点图/节点配置 | 当前 Workflow Run 的新 Pipeline Revision；默认不回写 Project Workflow |
| 修改本次待发送输入 | 当前节点的下一次 Agent Run 输入记录；不篡改上游原始 Handoff，也不覆写已发送输入 |

用户应始终看到“保存到工作流”“仅本次运行”“本次发送”的区别。

### 3.3 prompt 预览和发送

预览分为“模板/试算预览”和“待发送输入”。尚未产生的上游结果显示未就绪；用户可以提供示例值做试算，示例值不得进入正式派发。

正式派发前，由 Core 生成并持久化一次节点输入记录：关联 Project、Workflow Run、Pipeline Revision、Step、随后创建的 Agent Run，保存模板、解析值、来源 Agent Run/Handoff、用户覆盖、最终业务 prompt、协议段版本、摘要和时间。

- 展示业务 prompt 与只读结算协议段；用户可以编辑业务内容。能力令牌继续由环境注入，不能加入可编辑正文或输入审计记录。
- Agent Adapter 必须消费这份已冻结输入，不能在发送时重新从“最新 Handoff”临时组装另一份。
- 对复用会话明确显示“本次发送内容”；已有会话历史、外部 CLI 的系统配置不在这份记录中，不能声称预览了模型的全部上下文。
- 自动模式：校验 → 冻结输入 → 派发。人工检查模式：校验 → 等待用户 → 编辑/确认 → 冻结输入 → 派发。
- 人工检查模式通过持久派发门控阻止调度；不能只靠打开一个弹窗拖延。复用运行级 Needs You 提醒，新增可定位原因，不另建前端状态机。
- 在用户确认前若图版本或来源发生变化，旧预览失效，需要重新准备和确认。重复点击确认最多创建一次 attempt。
- 新输入的必填来源缺失、类型错误时不启动 Agent；保留原错误和输入，提供补值/修改映射入口。optional 字段只使用用户明确设置的默认值。
- 已开始/已发送的输入只读；若需改动，明确新建重试 attempt 或新增节点，不能覆盖历史。

### 3.4 动态改图

- 暂停表示**停止派发新的 Agent Run**；正在运行的 Agent 可以继续工作和结算，界面应说明这一点。
- 暂停应与派发竞争有清晰顺序：暂停提交成功后不得创建新 attempt；此前已开始的 attempt 仍算已启动节点。
- 只允许修改 `attempts == 0` 的节点。`attempts > 0` 的节点，即使失败、等待输入或已完成，也保留其定义和历史输入；需要改变职责时新增节点。
- 可新增节点、删除未启动节点、修改未启动节点指令/Agent/映射、调整合法依赖。已启动节点的输入依赖保持不变；可给它增加新的下游节点。
- 应用图修改时，整体执行 DAG/变量/实例/并行隔离校验；一次事务创建并激活完整新 Pipeline Revision，维护当前版本、receipt/outbox 和继承关系。
- 历史 Revision 不变；未改变节点沿用原冻结 Agent Instance/plugin/目录 provider 约束；新增或重新绑定节点才冻结其选定配置。首版不支持运行中更换整个目录提供器。
- 继承的成功节点不重新执行；其结果通过来源引用读取，不能复制成新的伪造 Handoff。仍在运行的旧 attempt 后续结算要正确关联当前逻辑节点，并保持一次性令牌和旧版本历史。
- 原子应用成功后仍保持暂停，用户检查新图后显式恢复。回答问题、重试和恢复会话不能意外清掉暂停标志。
- 若用户的改图基于旧版本或节点在提交前已启动，拒绝并刷新。整个运行已终结时不复活原运行，提示基于工作流发起新运行。

这些约定延续不可变 Pipeline Revision 和“先暂停，只改未启动 Step”的现有方向。若后续需要循环、改变已执行节点或 Agent 自动批准自己的图修改，需要明确补充领域决策，不能暗改当前约定。

## 4. 分步执行计划

按 T0 → T1 → T2 → T3 → T4 → T5 → T6 顺序执行。每步包含最小可验收切片，相关测试通过后继续，不需要每一步重新询问许可；发现与最新需求冲突时报告具体冲突。

### T0：建立真实验收入口，补齐与本需求相关的基础缺陷

**落点：** `web/e2e/`、新增 Playwright 配置和测试 fixture、`web/src/dag/graph.ts`、`web/src/workbench/workflow_editor.tsx`、`.github/workflows/ci.yml`。

1. 测试 fixture 启动真实 Core/Web，使用操作系统分配的端口和真实 bootstrap nonce，注册测试专用 mock Agent；使用隔离数据目录，不调用真实收费模型。
2. 先跑通“创建三节点工作流 → 连线 → 保存 → 重载 → 启动 → mock 结算”的浏览器场景。接入 Windows CI，复用 Rust 构建产物。
3. 修复布局 key/handle 混用，覆盖 A→B→C 三层和并行汇合。统一键盘/按钮删除的持久化行为，拒绝时恢复快照。

**验收：** 一条真实浏览器流程通过；重载后图与数据一致；测试不再靠端口 0、固定 nonce 或只检查文件存在。

### T1：节点职责和输入/输出约束贯通存储、快照、命令与表单

**落点：** `mf-agent/src/workflow.rs`、`workflow_validation.rs`、`workflow_compiler.rs`、`schema.rs`、`store.rs`、`catalog_store.rs`；`mf-kernel/src/kernel.rs` 与工作流命令/投影；`mf-web/src/api/kernel_bridge.rs`；Web 节点表单。

1. 扩展节点 draft/snapshot，加入验收说明、输出约束、input_bindings、context policy、启动前是否检查输入。
2. 完成旧数据默认值/迁移，更新内容摘要规范。新增语义字段必须参与 `workflow_content_digest`、semantic CAS、模板保存、复制与运行冻结；移动节点仍只影响 presentation revision。
3. 把职责、验收说明、期望输出和 Agent 绑定分区展示，添加三个可编辑预设。变量选择器使用合法祖先字段，支持插入到指令中。
4. 在一次命令内支持连线/删除与引用调整；前端预检只做反馈，Core 校验是权威。

**验收：** 三节点可配置不同职责；映射/说明经过保存、重启和冻结仍一致；改说明影响新运行，旧 Revision 保持原值；无效图或引用不发生部分写入。

### T2：统一 prompt 编译、保存和只读预览

**落点：** 建议新增 `mf-agent/src/node_input.rs`，收拢 `orchestrator.rs` 的 prompt 构造与引用解析；扩展 Store、Kernel 输入查询、Web 节点 Inspector。

1. 建立一个节点输入模块，由同一套实现完成解析、必填/类型校验、来源追踪、业务 prompt 和结算协议段拼装；模板预览与运行派发共用它。
2. 给输入记录和来源交接建立稳定身份，按 attempt 保存不可变发送历史；读取详情按需请求，运行总快照只带摘要/状态/句柄，避免把所有长 prompt 放进每次轮询。
3. 修改正式派发路径：使用已持久输入记录创建启动参数。自动模式也必须保存输入，便于复盘。
4. 显示“指令模板 / 上游原始输出 / 解析后的输入 / 本次发送内容”，支持复制；示例预览和已发送内容有明显标识。
5. 输出约束在成功 Settlement 的事务前置校验中检查；不合格时保留未成功状态并返回可操作错误，允许 Agent 修正或用户补齐后重新结算，不自动解锁下游。

**验收：** A 输出 `output.report_path`，B 引用后预览能看到实际路径；mock Adapter 收到的内容与冻结记录一致；错误字段不启动 B；修改 A 以外的工作流草稿不会改变 B 的历史输入。

### T3：用户检查、编辑并确认本次输入

**落点：** 节点输入模块及持久门控、`mf-agent/src/run_mutation.rs`、`mf-kernel/src/kernel.rs/command.rs/run_lifecycle.rs/run_projection.rs`、Web 协议/Inspector。

1. 增加“保存本次输入覆盖”“确认本次输入”语义命令（具体 wire 名称沿用现有命名规范），都使用 Controller Lease、expected revision、命令幂等和事务 outbox。
2. 下游 ready 时若启用人工检查，Core 准备输入并挂起派发，Needs You 点击直接进入该节点。
3. 用户可以改绑定值和业务 prompt，界面显示自动生成值与覆盖值的差异；保留上游原始 Handoff，记录覆盖来源。
4. 确认绑定准确的 input revision、Pipeline Revision 和来源；冲突时不发送。覆盖表单不能通过普通运行事件或 Debug 日志泄露完整正文。
5. 完成崩溃恢复：等待确认状态、草稿覆盖和已确认输入都可恢复；已发送但无法确认启动结果时进入现有 interrupted/Needs You 路径，不盲目重复发给 CLI。

**验收：** A 完成后 B 不启动；用户改一段内容后点击确认，B 只启动一次且确实收到修改内容；A 的历史不变；刷新/重启不会丢失待确认输入；旧标签页确认被拒绝。

### T4：经 Core 命令支持暂停、改图和恢复

**落点：** `mf-agent/src/run_mutation.rs/store.rs/orchestrator.rs`，建议新增 `run_graph.rs` 收拢改图规则；Kernel 命令、事务、投影、Web wire。

1. 加入 pause/resume/apply graph patch 命令和图变更预览。patch 指明基线 Pipeline Revision、目标节点身份和原子操作列表。
2. 把暂停检查纳入创建 Agent Run 的事务性派发前置条件，不能仅依赖 tick 开始时读取的旧 `task.paused`。
3. 对新的工作流 Revision 完整保存冻结 snapshot、内容摘要、实例/插件身份和目录约束；禁止用旧 `save_edited_revision` 直接接 Web。
4. 显式保存节点跨 Revision 的继承/来源映射，覆盖已成功节点的 Handoff、正在执行的旧 attempt 的后续结算、重试计数、会话与执行租约归属。不能只复制 status/result。
5. 校验/写入/激活/receipt/outbox 在一次权威事务内完成；外部 pin/运行时动作复用现有 durable action/补偿机制。失败保留原活动图。
6. 图或依赖源改变时，使受影响的未发送输入失效；已发出的输入历史保持只读。

**验收：** 原图 A→B，A 已成功且 B 未启动，暂停后插入 C 变成 A→C→B，恢复只运行 C/B；二者仍能引用 A 的原始结果。再次测试 A 尚在运行时暂停和插入 C，A 随后结算能正确推动新图。非法 patch、陈旧提交、半途崩溃均不出现半张图或重复 attempt。

### T5：统一工作流图、运行图和输入检查体验

**落点：** `web/src/workbench/workflow_editor.tsx`、`run_detail.ts`、`shell.tsx`、`web/src/inspector/`、`web/src/dag/`、对应样式。

1. 提取共享的图展示和节点/边选中交互，分别接受 Project Workflow 编辑命令和 Workflow Run 控制命令；前端不自行推断调度状态。
2. 运行视图保留依赖数据。节点卡片显示职责摘要、Agent、状态、输入是否就绪/等待检查；点击节点打开职责、输入、输出、会话和历史记录。
3. 点击边显示该关系上传递的字段和对应下游输入；纯控制依赖明确显示“仅等待完成”；间接祖先引用高亮路径。
4. 在运行图上提供“暂停派发 → 编辑未启动部分 → 查看变更 → 应用新版本 → 恢复”，冻结节点显示不可编辑原因。
5. 增加“模板预览 / 本次待发送 / 已发送”切换，编辑状态、冲突反馈和版本作用域清楚可见。

**验收：** 用户无需写命令或查看数据库，就能完成 T3/T4 场景；取消/失败保存不残留假连线；重连后看到 Core 的正确版本；观察者可以查看但不能修改。

### T6：完成端到端验收与文档收口

**落点：** Web E2E、`mf-agent` / `mf-kernel` / `mf-web` 定向契约测试、README、CONTEXT、相关 ADR。

必须覆盖：

1. 分析→实现→审查，各自职责和输出要求不同，输出自动进入下游输入。
2. 并行 A/B→C 汇合，C 的输入标明两份来源；某一必填输入未就绪不得启动。
3. 上游已完成，用户修改下游待发送内容并确认，真实 Adapter 收到修改值。
4. 暂停后增删未启动节点、调整连线并恢复；已执行节点不重复运行，旧输出仍可用。
5. 运行中的节点在暂停/改图期间完成；新图正确接收其结算，旧 token 不越权。
6. 缺字段、上游跳过、输出类型错误、坏引用、依赖环都有明确反馈和恢复入口。
7. 改图或编辑输入的旧页面提交遇到 revision conflict；重复确认不重复派发。
8. 待确认、已确认未派发、改图提交前后重启；重连后状态和历史一致。
9. 历史 Revision 和已发送输入只读，工作流模板编辑不污染它们。
10. Legacy 工作流、实例/plugin pin、显式 Settlement、会话重试与目录隔离原有契约保持通过。

README 删除或标明旧 GPUI 快捷键/路径，改为实际 Web 启动和操作方式。CONTEXT 只补充已经落地的领域语义；ADR 如需调整应明确记录，不能把历史废弃计划重新当成当前要求。

## 5. 验证命令与本次验证结果

本次已执行，全部通过：

```powershell
cargo test -p mf-agent --test workflow_compiler --test workflow_run --test retry_and_handoff --test review_regress
# 54 passed，包含真实临时 worktree 的相关测试；不等于浏览器真机验收。

# 工作目录 web/
npm test
# 54 passed
npm run build
# 通过；存在包体积提示。
```

Rust 有既有编译 warning，本次未做无关清理。没有执行尚未配置真实 fixture 的 Web E2E，也没有执行完整 workspace 测试。

zcode 实施时先运行所改模块的定向测试；收口时按 CI 执行：

```powershell
cargo fmt --all -- --check
cargo check --workspace
$env:RUST_TEST_THREADS = '1'
cargo test --workspace

# 工作目录 web/
npm test
npm run build
npm run e2e
```

最后一项必须在 T0 建立的真实 fixture 下通过。不要把“测试文件写好了”当作端到端验收完成。

## 6. 实施边界与交接指令

本轮优先满足上述三项用户目标。条件路由、循环、任意脚本节点、子工作流、完整角色市场、多人实时协作、重新开发桌面编辑器不在必做范围。现有插件、安装器、Root 模式与版本控制能力仅在兼容本轮改动所需时触及。

后续如需要 Agent 主动改图，应复用 T4 的 patch 校验和应用路径：Agent 只能提交带 base revision 的提案，用户查看具体图差异并确认；修正当前“确认提案”只确认最新 draft 的宽泛行为，使确认绑定用户正在查看的那一个提案。该扩展不是运行前/运行中人工动态改图的交付前提。

可直接交给 zcode：

> 阅读 AGENTS.md、CONTEXT.md、当前 ADR 和本文。以工作区实际 HEAD 为准，先核对它相对分析基线的变动，保留已有用户修改。按 T0–T6 实现“节点职责明确、运行中可安全改图、节点间输入可视化并可编辑”的闭环。继续使用 Rust Core 权威状态、不可变 Pipeline Revision、Controller Lease/CAS、显式 Settlement 和既有插件快照机制。不要直接把旧 PipelineDraft 改图函数接到 Web，不要用前端状态代替调度门控。每个阶段执行定向测试并报告实现结果，完成后给出总体验收结果和未解决项。只修改本需求相关代码；除非另有明确授权，保留未提交 diff，不自行 commit/push 或创建远端 tickets。
