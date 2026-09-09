# T0–T6 第二轮复审交接：S1–S5 与剩余验收

日期：2026-09-09。审查对象：[R1–R6 修订报告](2026-09-09-t0-t6-r1-r6-revision-report.md)。

**结论：有实质修复，但仍暂不通过整体验收。上一轮 6 项基础探针已通过；本轮额外复现了 4 项 Rust 边界失败和 1 项真实 React 组件交互失败。**

## 1. 基线和已经认可的进展

基线仍为 `main@86c19890be8345b46264c7631c23abf5d7d125ef` 加当前全部未提交实现。继续保留现有修改，不重置工作区、不重新实现已经正确的部分。需求仍以[原 T0–T6 计划](2026-09-09-workflow-gap-analysis-and-execution-plan.md)和[第一轮交接](2026-09-09-t0-t6-review-handoff.md)为准；本文不新增远端 tickets。

已经确认：真实实例 ID 和扩展字段进入运行投影；普通删除已启动节点被拒绝；绑定覆盖重新渲染；保存推进独立 input revision；重试重新读取上游；继承节点可以按 key 找到历史输入。运行画布和 input-review 提醒也已有实现。

本轮实际执行：

| 检查 | 结果与范围 |
| --- | --- |
| `cargo test -p mf-kernel --lib input_ -- --test-threads=1` 对应的命令回归 | 3 项通过 |
| `cargo test -p mf-web --test contract input_review_regress -- --test-threads=1` | 10 项通过 |
| `web/ npm test` | 59 项通过 |
| 第二轮独立 Rust 工程：原 6 项 + 新增 4 项 | 6 通过、4 失败 |
| 抽取当前 NodeInputCard 的隔离浏览器交互 | 复现 S2，退出码 1 |

最初的 Rust 筛选命令还包含 `-p mf-web --lib`，该目标没有命中 input_ 测试；随后已显式执行上表的 mf-web contract 目标。没有把 0 项测试计作验证通过。

本轮没有重跑完整 workspace、Web build 或完整 Core 浏览器 E2E，也没有停止用户日常 Core。组件交互检查使用 headless Chromium 渲染当前源码中的真实 NodeInputCard，外层用测试数据更新 props，验证的是组件行为，不是生产全链路。

## 2. S1 · P1：上一轮 v12 数据库不能自动升级

位置：`crates/mf-agent/src/schema.rs:12/1117`，以及 `migration.rs` 中 `current == target` 直接返回的分支。

事实：本轮在仍为 v12 的建表 DDL 中添加 `input_revision`，没有迁移已有 v12 数据。上一轮代码已经可以创建这种库；“尚未提交/发布”不能说明用户和测试环境没有使用过它。

复现：构造上一轮的 v12 表结构（node_inputs 无 input_revision），关闭后用当前 Store::open 打开，再查询输入，返回 `no such column: input_revision`。该探针只使用临时库，没有读取或修改用户数据库。

修复要求：增加自动、事务性的升级路径及适当的版本/兼容检测，沿用备份屏障；不能要求用户删库或手工 ALTER。一起验证旧 input_json 的兼容性，保留待确认输入、覆盖值和历史归属。

验收：真实旧 v12 fixture 自动升级，原数据保留，节点输入可读可编辑可确认，重复打开幂等；新建库同样通过。

探针：`existing_v12_database_must_upgrade_input_revision_automatically`。

## 3. S2 · P1：旧编辑内容自动采用新版本号，绕过了冲突保护

位置：`web/src/workbench/shell.tsx:1814`，NodeInputCard 的版本同步 effect 与 promptText/bindingValues 状态。

复现：用户在 v1 上编辑 `DRAFT_BASED_ON_V1`；组件接收到服务器 v2 和另一份内容；effect 把 currentRevisionRef 更新为 2，但编辑草稿保持原样。点击保存，发出的 payload 是 `input_revision: "2"` 加旧草稿。后端 CAS 会把它当作基于 v2 的合法修改。

这不是服务端没有 CAS，而是前端把陈旧草稿伪装成最新版本上的修改。相同问题也需要检查重试时复用输入卡、浏览器复制标签页等场景。

修复要求：正在编辑的草稿固定创建时的输入身份/版本；新快照到达后提示冲突/重新加载/合并，不能只推进版本号。未编辑状态可同时更新内容和版本。保存成功后使用服务端返回的真实版本和生效内容；失败不确认。

验收：编辑 v1 期间收到 v2，保存仍带 v1 并被拒绝，或用户明确解决冲突后才以 v2 保存；不得默默覆盖 v2。换 attempt 后编辑内容与输入身份同步。

探针：[ui-stale-draft.cjs](2026-09-09-t0-t6-second-review-probe/ui-stale-draft.cjs)。它抽取现有组件并记录真实按钮触发的 payload，不复写产品组件逻辑。

## 4. S3 · P1：图补丁提交时只复验存在性，未复验新启动节点的定义

位置：`crates/mf-agent/src/store.rs:3766` 的 create_patched_revision_tx。

复现：B 尚未启动时 prepare 一份修改 B 指令的合法补丁；随后恢复派发，让 B 按旧指令启动，再次暂停；提交之前准备的补丁。事务检查 B 仍存在就接受了新指令，当前 Revision 中 B 的定义与实际执行内容不一致。

这是事务层的真实复现，尚不是通过完整 Kernel 并发请求驱动的 E2E。生产入口的暂停/恢复不推进 run revision，不能假定其他版本检查必然阻止这类穿插；原计划要求在提交点验证冻结约束。

修复要求：事务内重新读取当前 attempts 和冻结节点定义；所有已启动节点既不能删除，也不能修改职责、指令、实例、依赖、输入映射、输出要求、策略等冻结字段。prepare 结果还应绑定适当的派发/图版本事实，出现竞争整体拒绝。

验收：prepare 后节点启动，提交改变该节点定义的补丁必须拒绝，原图/运行/输入/租约无部分改变。补充真正 Kernel 命令路径的并发场景，不能只把新图参数手改后调用 Store 并声称验证了 prepare 竞争。

探针：`graph_commit_must_recheck_newly_started_node_definition`。

## 5. S4 · P2：未启动节点改了定义，界面仍展示旧输入为当前输入

位置：`crates/mf-kernel/src/run_projection.rs:55` 和 `crates/mf-agent/src/store.rs:6433`。

复现：B 已生成待确认输入，随后暂停并把 B 的指令改为 `NEW B INSTRUCTIONS`。新图尚未恢复派发时，从真正 Kernel snapshot 读取：B.instructions 是新内容，但 B.input.template 仍是 `do B`。

原因：按 `(task_id, node_key)` 无条件取最新历史记录，混淆了“已执行节点继承原发送历史”和“未启动节点定义变更后应重新准备的输入”。

目前已证明的是当前输入展示错误，不据此声称 Agent 实际发送了旧内容；正式派发仍有按 Step 读取的另一条路径。两者不一致本身已经违反预览要求。

修复要求：区分已发送历史和当前待发送输入。已启动节点可通过明确来源关系展示原记录；未启动节点图语义变化后，旧输入标记失效/历史，新输入未准备时显示未就绪。不要把同名 key 当成所有版本都相同的输入身份。

验收：改指令、改映射、删除后重建同 key、连续改图都不会把过期输入显示为当前输入；已执行节点历史仍可查。测试必须调用真正运行投影。

探针：`patched_unstarted_node_must_not_show_obsolete_input_as_current`。本轮通过 InProcessKernelRuntime + LegacyKernelClient 的 workflow_run_snapshot 检查结果，未只查询 Store getter。

## 6. S5 · P2：取消运行后，人工检查提醒仍然存在

位置：`crates/mf-kernel/src/run_projection.rs:353/400`、`workspace_projection.rs` 的 input-review 汇总，以及取消时的门控处理。

复现：B 等待人工检查，取消整个 Workflow Run；真正运行快照同时返回 `status: "cancelled"` 和 `needs_you: true`，原因仍是 input-review。用户无法通过这个提醒继续已取消任务。

修复要求：Needs You 只汇总当前仍可操作的有效门控。终态运行、已取消 Step、过期版本和不再有效的输入不应继续贡献提醒；处理门控生命周期并保持历史数据可追溯。

验收：取消后工作台摘要与运行详情都无 input-review 提醒；正常待确认场景仍正确计数/聚焦；重启后结果一致。

探针：`cancelled_gate_must_not_remain_in_needs_you`，同样使用真正 Kernel snapshot。

## 7. 报告需要准确描述的验证范围

- `core_restart_recovery.rs` 实际重建 Orchestrator/Store/RecordingHost，并复用部分 catalog/pins/directory 对象。它是有价值的恢复测试，但没有退出再启动 mf-workbench/Core 进程，没有覆盖服务库、Kernel journal、认证/管道与跨进程恢复。不能把这层证据标为“真正 Core 进程重启已验收”。
- `input_review_regress.rs::run_projection_shows_inherited_input_after_patch` 目前调用的是 `latest_node_input_of_key`，没有调用 Kernel snapshot；测试名与“投影级”报告超出了实际覆盖。S4/S5 的探针示范了真实投影测试入口。
- 模板试算预览、统一图编辑交互仍明列未完成，继续按原计划补齐。mfctl 输入命令和 Agent 主动改图仍属于后续扩展，不因本轮审查扩大范围。
- “同 command_id 不同内容”的回归应固定其余 expected 条件，只改变内容，避免版本变化也能制造冲突而掩盖摘要回归。

真实进程重启 E2E 应在隔离测试环境完成，不把停止用户正在使用的 Core 作为默认前置步骤。

## 8. 复现材料和后续顺序

完整材料：[独立 Rust 工程](2026-09-09-t0-t6-second-review-probe/Cargo.toml)、[Rust 用例](2026-09-09-t0-t6-second-review-probe/src/lib.rs)、[Rust 失败日志](2026-09-09-t0-t6-second-review-probe/baseline-rust-results.log)、[组件用例](2026-09-09-t0-t6-second-review-probe/ui-stale-draft.cjs)、[组件失败日志](2026-09-09-t0-t6-second-review-probe/baseline-ui-results.log)。

从项目根目录运行：

```powershell
cargo test --offline --manifest-path docs/research/2026-09-09-t0-t6-second-review-probe/Cargo.toml --target-dir target -- --test-threads=1
node docs/research/2026-09-09-t0-t6-second-review-probe/ui-stale-draft.cjs
```

当前基线预期：Rust 6 passed / 4 failed，Node 退出码 1。Node 用例依赖项目 web/node_modules 和已安装的 Playwright Chromium；测试过程不启动 Core。独立工程没有加入产品 workspace，应将修复后的行为测试迁入正式测试目录。

建议顺序：S1 数据迁移 → S2 输入草稿版本 → S3 事务冻结约束 → S4/S5 当前输入与提醒归属 → 剩余 T5 和真实进程恢复验收。修复后重新运行正式测试和完整交付检查。

给 zcode 的指令：

> 继续当前未提交实现，阅读第二轮交接日志，逐项修复 S1–S5，保留已经通过的 R1–R6 基础回归。将新增行为测试纳入正式测试，补齐原计划剩余项，并按真实覆盖层级报告验证结果。保留工作区已有修改，不 commit/push，不删用户数据库，不默认停止日常 Core。
