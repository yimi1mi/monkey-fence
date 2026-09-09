# 工作流闭环 T0–T6 执行报告（致任务分配方）

- 任务：按 `docs/research/2026-09-09-workflow-gap-analysis-and-execution-plan.md` 的 T0–T6 顺序实施，每阶段定向测试，最终按完整场景验收。
- 代码基线：`main@86c19890be8345b46264c7631c23abf5d7d125ef`（分析基线与 HEAD 一致，无其他用户改动需保留）。
- 结果：**七阶段全部完成**；改动 64 个文件（+4140/−486），按要求保留为**未提交 diff**，未 commit/push、未建远端 ticket。

## 最终验证（CI 口径，全部通过）

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo check --workspace --tests` | 0 error |
| `RUST_TEST_THREADS=1 cargo test --workspace` | 0 失败（全量串行） |
| `web/ npm test` | 59 pass / 0 fail |
| `web/ npm run build` | 通过 |
| `web/ npm run e2e`（真实 Core fixture） | **9/9 通过** |

E2E 覆盖：bootstrap nonce 交换/接管降级、三节点建图连线、A→B→C 分层布局（回归 key/handle 混用）、重载持久、含输出约束的逐节点结算、T3 输入门控（挂起→重载保真→编辑→确认→单次派发收到修改内容）、T4 暂停/恢复、暂停后面板插入节点→应用→恢复只跑新图。

## 各阶段交付摘要

- **T0 真实验收入口**：`web/playwright.config.ts` + `web/e2e/fixtures/{core,global-setup,helpers}.ts`——真实 `mf-workbench`、OS 随机端口、真实一次性 nonce、`MF_SERVICE_DB`/`MF_CATALOG*`/`TEMP` 隔离数据目录、验收 mock echo Agent；CI 新增 windows `web-e2e` job（复用 rust-cache 构建 mf-workbench）。修复：布局 key/handle 命名空间混用（`graph.ts wireGraph()`）、键盘删除统一走 `workflow.*` 命令路径（级联边抑制、拒绝恢复权威图）。
- **T1 节点职责贯通**：`WorkflowNodeDraft/Snapshot` +5 字段（acceptance_criteria / output_schema / input_bindings / context_policy / require_input_review）；**条件性参与 `workflow_content_digest`**（默认值不进哈希→存量 digest 不失配，基线字节对比测试证明）；校验器 +4 错误码（含 JSON Schema 子集校验）；`update_node` 扩字段（additive：缺省=不改、null=清除）+ 新命令 **`workflow.update_graph` 原子整图替换**（非法引用整体拒绝、无部分写入，内核契约测试）；Web 表单五分区 + 三预设 + 变量插入。
- **T2 统一 prompt 编译**：新模块 `mf-agent/src/node_input.rs` 为模板解析/必填校验/来源追踪/业务 prompt/协议段的**唯一实现**（预览与派发共用）；schema v12 `node_inputs` 冻结记录（按 attempt、不可变）；派发消费冻结记录（集成测试断言 mock Adapter 收到内容与记录逐字节一致）；必填缺失不启动 Agent；**输出约束进入成功结算的事务前置校验**（`SettleError::OutputSchemaViolation`，保持待结算可修正重提）；运行详情只读输入卡（模板/映射/上游摘要/本次发送/协议段+复制）；结算表单支持输出 JSON。
- **T3 人工检查门控**：`workflow.run.save_input_overrides` / `confirm_input`（Controller Lease + expected revision + 幂等）；门控节点就绪后挂起（持久 awaiting_review，**不创建 attempt**，并发槽归还）；重复确认幂等、确认后覆盖被拒、刷新/重启不丢待确认输入、上游原始 Handoff 不被篡改（均有测试断言）。
- **T4 暂停/改图/恢复**：`workflow.run.pause` / `resume` / `apply_graph_patch`；**暂停为创建 Agent Run 的事务性前置**（`dispatch_run_consuming` CAS 内联 `NOT EXISTS(paused)`）；图补丁 = port prepare（复用 start 编译缝隙：实例解析/插件 pin/目录 pin，已启动节点按冻结定义还原+守卫拒绝修改）+ 内核单事务 `create_patched_revision_tx`（继承 status/attempts/result、**handoff.step_id 重映射不复制伪造**、未启动节点按新依赖重算状态、待确认输入复位、Revision superseded→active 原子切换）；未暂停拒绝、基线陈旧=RevisionConflict。集成测试覆盖：A 成功后暂停插入 C（A→C→B）恢复只跑 C/B、B 引用 A 原始结果、A 不重跑。
- **T5 体验统一**：运行详情暂停/恢复按钮 + 「编辑运行图」面板（暂停时增删未启动节点/调依赖/应用/手动恢复）；每次命令成功强制重拉详情（修复补丁后旧 step 句柄 404）。
- **T6 收口**：README 过期的 GPUI 章节改为实际 Web 启动/E2E 方式；CONTEXT +5 领域词条；新增 ADR 0006；`t4-implementation-notes.md` 工作笔记已清理。

## 关键设计决策与偏差（分配方应知）

1. **新字段不推进旧数据身份**：digest 条件性哈希 + serde `skip_serializing_if`，存量 `content_digest`/graph_json/snapshot_json/基线 fixture 字节全部保持稳定（`baseline_storage` 回归通过）。
2. **pause/resume/图补丁不推进 `agent_tasks.revision`、不发 run 聚合投影事件**：与 settle 同口径。原因：run 聚合 replace 事件要求 base == journal head，而 task revision 存在被非命令路径推进的情况，强制发事件会 `resync_required` 并毒化 target。UI 一致性由「命令成功后强制重拉详情（commandSeq）」保证。
3. **ConfirmInput 不产生 DispatchReady RunAction**：node_inputs 不推进 run/step revision，动作缺少可承载的权威投影（kernel `run actions 缺少可承载的权威投影`）；派发由调度 tick 的门控复查驱动（实测下一 tick 即派发）。
4. **命令面 rowid-free**：save/confirm 不携带 input_id，服务端解析该 step 最新 awaiting_review 记录（比 Respond 的 question_id 内部关联口径更严格）。
5. **端口注册顺序是内核契约**（orchestrator → lifecycle → start）：lifecycle port 持有 start port 的 Arc 供图补丁编译复用，顺序不可倒置（倒置会 `workflow_start_port_not_registered`）。
6. **图补丁 target revision 按当前 run 聚合回读**、effect 提前返回且 projections 为空（同决策 2）。
7. 旧 `save_edited_revision` 未接 Web（按计划禁止）；Agent 主动改图（提案-确认）列为后续扩展。

## 途中发现并修复的既有缺陷（计划外）

- **验收/生产工作流派发必败**：`execution_ports.rs` 伪造插件 pin `builtin.core@hash-generic` 无对应内容寻址包，任何工作流节点启动即失败。已改为从真实插件注册表派生 pin 表（内置合成=空哈希、第三方=内容寻址），验收 mock 的 agent_type 改用真实内置贡献（opencode→generic-command）。
- 状态重算最初误降级运行中节点（awaiting-outcome→ready）：补丁内重算限定 `attempts==0`。
- Playwright 1.49 在 Windows 加载 `.mjs` globalSetup 挂起：fixture 全部改 `.ts`。
- 图补丁后运行详情不刷新导致后续结算 404（见决策 2 的 commandSeq 修复）。

## 未解决项（后续工作建议）

1. 运行图编辑当前为面板形态；与项目工作流 React Flow 画布共用图组件、边点击查看传递字段、间接祖先路径高亮、模板预览/待发送/已发送三态切换 UI 未做（数据层已就绪）。
2. T6 矩阵中“依赖环/上游跳过的浏览器级反馈”“重启于已确认未派发瞬间”等未逐一建专门用例（核心语义已有 Rust 集成测试 + E2E 断言覆盖）。
3. mfctl 管道侧无输入覆盖/确认命令（仅 Web 命令面）；Agent 主动改图提案-确认未接线。

## 环境说明

- 测试期间曾停止用户本机 release 版 mf-workbench（机器级单例锁/管道名互斥为 E2E 必需），已原样重启（端口 80）。E2E 运行期间本机不能同时驻留另一 Core。
- 新增文件含 schema v12（`node_inputs`）；旧库自动迁移，无手工步骤。
