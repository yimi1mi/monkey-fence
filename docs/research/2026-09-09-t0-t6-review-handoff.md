# T0–T6 审查交接日志：退回修复与补验收

日期：2026-09-09。交接对象：zcode。

**结论：当前版本暂不通过验收。先修复 R1–R6，再补齐原计划的图形交互和恢复验收，最后重新提交执行报告。**

## 1. 上下文与材料

用户的三个核心要求仍然是：每个节点有明确职责；支持动态改变节点图；节点之间的 prompt 可视化并可由用户编辑。

- 原计划：[工作流差距分析与 T0–T6 执行计划](2026-09-09-workflow-gap-analysis-and-execution-plan.md)。
- 被审查的报告：[T0–T6 执行报告](2026-09-09-t0-t6-execution-report.md)。
- 审查基线：`main@86c19890be8345b46264c7631c23abf5d7d125ef`，**加上工作区当前 T0–T6 未提交实现及未跟踪源码**，不是只审查 HEAD。
- 已保存的独立复现工程：[Cargo.toml](2026-09-09-t0-t6-review-probe/Cargo.toml)。
- 复现用例：[src/lib.rs](2026-09-09-t0-t6-review-probe/src/lib.rs)。
- 原始失败日志：[baseline-result.log](2026-09-09-t0-t6-review-probe/baseline-result.log)。

本文是审查与执行交接记录，不新增远端需求或 ticket。沿用 AGENTS.md、CONTEXT.md 和原计划；保留已有用户修改，不 reset、不覆盖整批实现、不自行 commit/push。

## 2. 已完成的验证及其边界

审查时实际重跑：

| 检查 | 结果 |
| --- | --- |
| `cargo test -p mf-agent --test node_input_flow --test run_graph_patch` | 8 项通过 |
| `web/ npm test` | 59 项通过 |
| 额外构造的 6 项定向回归检查 | 6 项失败，详见 R1–R6 |

6 项失败是针对预期产品行为的断言失败，不是编译错误。工程复用仓库现有测试 fixture、真实 Store/Orchestrator 和生产图补丁 prepare 实现，不启动外部 Agent CLI。

审查没有重新运行完整 workspace、Web build 或浏览器 E2E；那些“全通过”结果来自 zcode 原报告。浏览器 fixture 与用户正在运行的 Core 有单例冲突，审查没有停止本机服务。

复现工程只证明下面注明的层级，不能把它当作完整 Core 命令/E2E 验收。例如 R4 检查的是确认所依赖的版本没有变化；修复后仍必须增加真实命令路径的陈旧确认测试。

从项目根目录复现（当前未修复基线预期退出码 101、6 failed）：

```powershell
cargo test --offline --manifest-path docs/research/2026-09-09-t0-t6-review-probe/Cargo.toml --target-dir target -- --test-threads=1
```

`--offline` 使用本机已有依赖；新机器若缺依赖，可去掉该参数。工程使用相对路径，并有独立 `[workspace]`，没有加入产品 workspace，普通 `cargo test --workspace` 不会自动运行它。修复时应将对应的产品行为测试迁入正式测试目录。

## 3. 必须修复的 R1–R6

### R1 · P1：运行图未完整保留节点配置，真实实例解析失败

代码入口：`web/src/workbench/shell.tsx:1180`、`:1300`；`crates/mf-kernel/src/run_projection.rs:110`；`crates/mf-web/src/execution_ports.rs:228` 附近的实例解析。

复现：使用真实 Catalog 中保存的 Agent Instance 创建 A→B；A 启动后暂停；按运行图面板的字段构造原样补丁。结果报 `Agent Instance generic-command 不存在`。

原因：运行投影的 `agent_instance_ref` 来自 Step 的 Agent Type，面板却把它作为 `agent_instance_id` 回传；生产 Catalog 按实例 ID 查找。验收用的 `AcceptanceMockCatalog` 接受任意引用，掩盖了错误。

静态代码另确认：面板构造/提交的节点只有 key/title/instructions/agent_instance_id/deps，丢失 `acceptance_criteria`、`output_schema`、`input_bindings`、`context_policy`、`require_input_review`。未启动节点可能失去人工确认/输出约束；已有 `${inputs.*}` 引用的图可能在编译时被拒绝。新增节点目前也没有编辑职责指令和 Agent 的完整表单。

修复：为运行图提供完整的冻结节点编辑视图或提交明确的字段 patch；实例引用必须保持真实身份，未修改配置必须原样保留。避免从展示用 Step 投影反推完整领域定义。

验收：真实保存实例的 A→B→C 可暂停改图；只修改 B 的依赖，其余五个扩展字段与实例身份保持；有人工门控的 B 不会因此自动启动；新增节点可以设置实际职责和 Agent。

复现测试：`graph_panel_roundtrip_must_resolve_saved_agent_instances`（已证明实例身份错误；字段完整性需另补正式回归）。

### R2 · P1：图补丁允许删除已经启动的节点

代码入口：`crates/mf-web/src/execution_ports.rs:191`；`crates/mf-kernel/src/kernel.rs:4495` 附近的 patch 提交事务。

复现：A 正在运行，暂停派发；提交只剩 B 的合法无环图。生产 port prepare 接受该补丁。

原因：只遍历新图检查仍存在的节点，没有反向检查所有 `attempts > 0` 的旧节点是否都保留。后续 Store 创建新图也没有补上这个约束。

修复：全部已启动节点必须出现在新图，且定义满足冻结规则；检查应在提交事务内基于当前活动图/attempts 再验证，不能只依赖 UI 禁止删除或事务外 prepare。状态读取失败不能当成“没有已启动节点”。

验收：通过真正 Core 命令尝试删除 running/awaiting-outcome/needs-input/succeeded/failed 且 attempts>0 的节点都被拒绝；活动 Revision、运行、Handoff 和租约无部分变化。补充 prepare 与提交之间状态变化的竞争测试。

复现测试：`graph_patch_must_reject_removing_a_started_node`（已证明生产 prepare 接受删除）。

### R3 · P1：用户补值没有进入实际发送的 prompt

代码入口：`crates/mf-agent/src/node_input.rs:423` 的 `apply_overrides`。

复现：B 使用 `${inputs.report}`；仅修改 binding value 为 `USER_FIXED.md`，不整段覆盖业务 prompt。解析值已更新，但 `full_prompt` 仍包含 `UPSTREAM_VALID.md`。缺失补值场景同样保留旧占位文字，却清除了必填错误。

修复：绑定覆盖必须通过统一输入渲染实现更新映射段、resolved instructions 和 business prompt。若用户同时提交完整 prompt 覆盖，明确优先级，预览必须展示实际生效结果。不要直接对已渲染的字符串做不受约束的替换。

验收：分别覆盖“只补必填缺失”“只改现有绑定值”“只改业务 prompt”“同时修改”四种情况；Adapter 收到的内容与确认记录一致，原始 Handoff 不变。

复现测试：`binding_only_override_must_reach_the_sent_prompt`。

### R4 · P1：输入确认没有绑定用户看过的版本

代码入口：`crates/mf-agent/src/store.rs:994/1036`；`crates/mf-kernel/src/kernel.rs:3751` 附近的命令语义摘要；`web/src/workbench/shell.tsx:1605` 附近的保存后确认。

复现事实：保存不同输入覆盖后，run revision 和 step revision 都不变。确认命令没有输入句柄/版本，而是选该 Step 最新的记录。因此现有 expected 无法区分用户看到的内容与后来保存的内容。

修复：引入稳定输入身份与可比较的输入版本（或另一种完整受保护的等价方案），保存和确认都校验它；确认同时绑定 Pipeline Revision 和来源。保存变化应使旧确认失效。修复事件/投影的版本连续性，不能通过不推进版本来避开 CAS。

同时检查两处相关问题：

- `workflow_run_payload` 将覆盖内容摘要成固定 `{redacted:true}`、图补丁摘要成节点数量；不同内容可能得到相同语义摘要。应对完整语义作安全摘要/HMAC，不能把日志脱敏等同于忽略内容。
- Web 的 `act()` 捕获错误后正常返回，`confirmInput(true)` 可能在保存失败后继续确认旧值。错误必须传回调用方，或将保存并确认设计为一次原子命令。

验收：用户看到版本 v1，输入已变成 v2，提交 v1 确认必须冲突且不派发；相同 command_id 携带不同内容必须冲突；保存失败不得继续确认；确认同一版本重试只能派发一次。

复现测试：`saving_input_must_advance_a_confirmation_revision`。**该探针针对当前两个 CAS 轴；若修复采用独立 input revision，应迁移为验证新输入版本/陈旧确认的行为测试，而非被迫推进旧轴。**

### R5 · P1：人工检查节点重试时丢失有效上游数据

代码入口：`crates/mf-agent/src/orchestrator.rs:3675` 的 `review_gate_open`。

复现：A 成功输出路径；B 确认后启动，失败，再新会话重试。新待确认输入把 A 的路径判为缺失。

原因：上一记录为 dispatched 时，用 `HashMap::new()` 编译新输入，未读取仍存在的上游 Handoff。

修复：按当前有效 Revision 和明确的结果来源重新准备下一 attempt 的输入，保留来源谱系；每次重试有独立记录与确认身份，覆盖继承策略应明确。

验收：显式映射和 legacy 祖先引用在重试后都保持有效；未确认不启动；确认后只发送一次；新 attempt 的来源指向正确上游 Agent Run。

复现测试：`review_retry_must_keep_upstream_handoff`。

### R6 · P2：改图后继承节点的输入历史从运行详情消失

代码入口：`crates/mf-agent/src/store.rs:3686` 的新版本继承；`crates/mf-kernel/src/run_projection.rs:117` 的输入投影。

复现：A 已执行且有输入记录，暂停并应用补丁后生成新的 A Step 行；按新 Step 查询不到原输入记录，运行详情不再展示 A 输入卡。原数据还在旧 Step 下，不是物理删除。

修复：建立跨 Revision 的节点/执行/输入来源关联，当前运行可查看继承节点的原输入，历史 Revision 也能保持原归属。单纯改 Handoff.step_id 并不能解决 Agent Run、Node Input、Session、Lease 的全部谱系。

验收：A 完成后连续改图两次，仍能从当前图查看 A 原始发送内容和来源；历史版本的事实不变；A 不重跑。再验证 A 尚在运行时改图后的结算、会话和输入可见性。

复现测试：`patch_must_retain_input_history_for_inherited_step`。如果新设计通过独立来源查询返回历史，应将测试迁移到对应权威投影接口，不能为了旧 getter 的断言改写历史数据。

## 4. 原计划尚未完成的部分

这些是原 T3/T5/T6 已要求的内容，不是本次新增范围：

- 人工检查门控纳入运行级“需要你”，计数和焦点定位一致。当前 `awaiting_review` 没有进入 reasons，运行仍可能显示普通 running。
- 工作流图与运行图共用图展示/选择交互，运行 DAG 显示状态和待检查输入。
- 点击连线查看传递字段，区分纯控制依赖，间接祖先引用可以高亮路径。
- 模板试算、待发送、已发送三种视图及版本作用域；完整的节点职责/输入/输出检查。
- 真正重启 Core 验证待确认、已确认未派发、改图提交前后的恢复；`page.reload()` 只证明浏览器重载。
- 依赖环、上游跳过、缺失/错误字段、保存/确认失败、陈旧页面及重复请求的可操作反馈。

原报告中“七阶段全部完成”应修正；保留原报告作为历史材料，完成修复后新增一份如实对应验收证据的报告。

## 5. 建议修复顺序

1. **先处理 R1/R2/R6：**正确的运行节点编辑数据、事务级冻结约束、跨版本来源关联。
2. **再处理 R3/R4/R5：**统一渲染、输入身份与版本、确认原子性、重试输入。
3. **补齐需要你与 T5 图形交互。**
4. **把复现行为纳入正式测试，补真实实例解析和 Core 重启 E2E。**
5. **运行定向测试与最终检查，提交修订后的执行报告供复审。**

阶段验证通过后继续推进。领域模型或函数签名变化时同步迁移探针，保留同等或更强的产品行为断言；不要删断言、扩大 mock 容忍度或只改期望值让测试通过。

最终检查按原计划及 CI：Rust fmt/check/workspace tests、Web test/build、真实 Core E2E。浏览器验收与日常 Core 的隔离需要正确解决，不能默认停止用户正在运行的服务。

## 6. 下次交付必须带什么

- R1–R6 分别对应的修复文件与正式回归测试名。
- 第 4 节各原计划缺口的完成情况，未完成项必须明列。
- 实际执行的命令、通过/失败数量、测试范围；把 mock/Store/生产 port/Core 命令/浏览器/真实 Core 重启证据分清。
- 当前 HEAD、未提交改动清单，以及是否出现额外迁移或 ADR 调整。
- 保留未提交 diff；在获得额外授权前不 commit/push、不发远端消息或创建 tickets。

直接交给 zcode 的指令：

> 读取 `docs/research/2026-09-09-t0-t6-review-handoff.md` 和其中关联的原计划、复现工程。继续修复当前未提交实现，先处理 R1–R6，再补齐原 T3/T5/T6 缺口。每阶段运行正式回归测试，最后提交逐项对应证据的新执行报告。保留现有用户修改，不 commit/push；不要把已知缺陷改写成“设计偏差”或放宽 mock 后宣称完成。
