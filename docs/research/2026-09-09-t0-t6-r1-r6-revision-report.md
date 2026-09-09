# T0–T6 修订执行报告(R1–R6 修复 + 缺口补齐)

日期:2026-09-09。对应审查交接:`2026-09-09-t0-t6-review-handoff.md`。
基线:`main@86c19890` + 工作区未提交实现(保留,未 reset/覆盖整批/commit/push)。
改动:67 个文件(+4842/−491,含全部 T0–T6 与本轮修复;未跟踪源码 18 个)。

## 1. R1–R6 逐项修复与正式回归

### R1 · 运行图完整节点定义与真实实例身份 — 已修复

- 投影(`crates/mf-kernel/src/run_projection.rs`、`projection.rs`):步骤快照携带活动
  Revision 冻结节点的全部字段——真实 `agent_instance_id`(快照 instance.id,非
  agent_type)、`acceptance_criteria`/`output_schema`/`input_bindings`/
  `context_policy`/`require_input_review`。
- 面板(`web/src/workbench/shell.tsx` RunGraphEditor):未启动节点的编辑表单含
  标题/指令/Agent 实例/验收说明/输出约束(JSON,提交前本地校验)/人工检查开关;
  提交 payload 携带全部扩展字段,未修改字段原样回传;新增节点默认 explicit_only。
- 守卫(`crates/mf-web/src/execution_ports.rs`):已启动节点的实例 id 与全部五个
  扩展字段逐字比较,恢复严格冻结校验。
- 正式回归:`crates/mf-web/tests/contract/input_review_regress.rs`
  - `graph_panel_roundtrip_must_resolve_saved_agent_instances`(真实 Catalog 解析)
  - `graph_patch_preserves_extended_node_fields_for_unstarted_nodes`(五字段完整保留)

### R2 · 禁止删除已启动节点 — 已修复(prepare + 事务内双层)

- prepare 前置(`execution_ports.rs`):所有 attempts>0 旧节点必须出现在补丁。
- 事务内复验(`store.rs create_patched_revision_tx`):提交时刻从当前活动图重读
  attempts>0 节点集合再验证——覆盖 prepare 与提交之间的竞争;状态读取失败不等于
  “无已启动节点”(查询错误直接中止事务)。
- 正式回归:`input_review_regress.rs`
  - `graph_patch_must_reject_removing_a_started_node`(prepare 拒绝)
  - `graph_patch_tx_must_reject_removing_started_node_after_prepare`(prepare 合法
    但提交内容删除已启动节点 → 事务拒绝,活动 Revision/运行/Handoff/租约无部分变化)

### R3 · 绑定覆盖统一渲染 — 已修复

- `node_input.rs` 重构:编译期保留 `nodes_resolved_template`(`${nodes.*}` 已替换、
  `${inputs.*}` 未替换的中间形态)与渲染上下文(节点/任务标题、goal、验收说明);
  新增 `render_business_prompt(RenderParts)` 作为唯一渲染路径。
- `apply_overrides`:绑定值变化后从中间模板重放进同一渲染——resolved_instructions
  与 business_prompt(含“输入映射”区块)重建;补值同样生效,“(输入映射 x 缺失)”
  占位不再残留;显式 business_prompt 覆盖具有最高优先级(整体替换,优先级在文档
  注明)。
- 正式回归:`input_review_regress.rs`
  - `binding_only_override_must_reach_the_sent_prompt`(只改绑定值,旧值不残留)
  - `missing_required_backfill_must_render_actual_value`(必填补值渲染实际值)

### R4 · 输入确认绑定用户看过的版本 — 已修复(独立 input_revision 轴)

- schema v12 追加 `input_revision INTEGER NOT NULL DEFAULT 1`(v12 本轮未发布,
  未新增迁移)。
- Store/RunMutation/kernel 命令/桥接/Web 全链路携带 `expected_input_revision`:
  保存 CAS 该版本并在成功时 +1(旧确认随之失效);确认同时 CAS input_revision
  与活动 Pipeline Revision(改图换版后旧确认拒绝)。同版本重复确认幂等。
- 幂等语义摘要:`workflow_run_payload` 对覆盖内容与图补丁节点集计算 sha256
  `semantic_digest`(Debug/日志仍脱敏;同 command_id 不同内容必冲突)。
- Web:`act()` 返回布尔;`confirmInput(true)` 保存失败即中止;NodeInputCard 以
  ref 跟踪保存后的新版本,同一交互内“保存并确认”用新版本提交。
- 正式回归(真实 Core 命令路径,`crates/mf-kernel/tests/contract/input_gate_commands.rs`):
  - `stale_input_confirmation_is_rejected_through_real_command_path`
  - `same_command_id_with_different_overrides_conflicts`
  - `save_failure_does_not_confirm_stale_value`
  - 行为级:`input_review_regress.rs::saving_input_must_advance_a_confirmation_revision`
- 探针按交接说明迁移:改为验证独立输入版本轴(推进 + 陈旧确认冲突),强于原
  “任一 CAS 轴推进”断言。

### R5 · 重试保留有效上游 — 已修复

- `orchestrator.rs review_gate_open`:重试分支与首次一致,按当前有效 Revision 经
  `upstream_handoffs_with_sources` 重新编译(删除原 `HashMap::new()` 空上游路径);
  新 attempt 有独立输入记录与确认身份,来源谱系指向真实上游 Agent Run。
- 正式回归:`input_review_regress.rs::review_retry_must_keep_upstream_handoff`
  (失败→FreshSession 重试→新待确认记录解析出 UPSTREAM_VALID.md 且 source 非空)。

### R6 · 改图后输入历史可见 — 已修复(按 node_key 权威归属)

- Store 新增 `latest_node_input_of_key(task_id, node_key)`;运行投影按 node_key
  命中当前图节点(改图继承后输入卡仍展示)。**不改写历史数据**:输入记录保持
  原 step_id/revision 归属,历史 Revision 事实不变(区别于 Handoff 的
  step_id 重映射,后者服务于结算路由,两者互不混淆)。
- 正式回归:`input_review_regress.rs`
  - `patch_must_retain_input_history_for_inherited_step`(连续改图后仍可见 +
    原 step 归属断言)
  - `run_projection_shows_inherited_input_after_patch`(投影级)
- 探针迁移:按交接说明改为通过 `latest_node_input_of_key` 权威接口断言,并加
  “新 Step 行无自身记录、历史归属旧行”的反向断言。

## 2. 原计划缺口补齐情况

| 缺口 | 状态 | 证据 |
| --- | --- | --- |
| 人工检查纳入运行级「需要你」(计数/焦点一致) | ✅ | run_projection + workspace_projection:awaiting_review 生成 `input-review` reason(priority 0,focus_step 指向该节点);needs_you 标志并入;`node_input_summaries` 携带 review_state |
| 运行 DAG 显示状态与待检查输入 | ✅ | RunDetail 新增 RunDagCanvas(React Flow):节点带状态着色与“待确认输入/已发送”徽标 |
| 点击连线查看传递字段、区分纯控制依赖 | ✅ | 边点击显示该依赖上传递的 input_bindings(必填/可选);无映射且无显式引用时标注“仅等待完成(纯控制依赖)” |
| 间接祖先引用高亮路径 | ✅ | 节点点击高亮全部传递上游(节点虚线 + 边加粗) |
| 模板/待发送/已发送三态及版本作用域 | ✅(部分) | 输入卡摘要行区分 等待确认(未派发)/已确认(待派发)/已发送(只读),含覆盖差异提示与 input_revision 展示;「模板试算(示例值)」未做——示例值不进入正式派发的预演 UI 缺失,列为未完成项 |
| 真正 Core 重启恢复 | ✅ | `crates/mf-agent/tests/core_restart_recovery.rs`(销毁并重建 Orchestrator/Store,等价进程内 Core 重启):待确认门控跨重启仍拦截且确认后恰好派发一次;已确认未派发跨重启保持、恢复后单次派发;改图提交后重启、恢复只跑 C/B、A 不重跑 |
| 依赖环可操作反馈 | ✅ | `sync_workflow_identity_tx_with_stats` 每次语义保存执行完整 DAG 校验(环在写入点拒绝)+ E2E `无效连线(依赖环)被 Core 拒绝并给出可操作反馈` |
| 上游跳过/缺失字段/失败反馈 | ✅(存量子集) | 缺失字段:R3 回归 + 失败结算留可操作 result;上游跳过→步骤 Blocked 徽标(既有);「保存/确认失败」反馈:R4 内核回归 + act 布尔传播 |
| 陈旧页面/重复请求 | ✅ | R4 陈旧确认/同 id 不同内容冲突(内核路径);重复确认幂等不重复派发 |

未完成项(明列):

1. 模板试算预览(用户填示例值做只算不发的预演)未实现;输入卡的“模板/解析/
   发送”三段为真实数据展示。
2. 运行图编辑面板与项目工作流画布仍是两套交互(面板为表单式);共用图组件的
   完全统一未做。
3. mfctl 管道侧的输入保存/确认命令未加(仅 Web 命令面)。

## 3. 实际执行的验证

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo check --workspace --tests` | 0 error |
| `RUST_TEST_THREADS=1 cargo test --workspace` | **83 个测试目标全部 0 failed**(一次并发重启用户 Core 时的锁竞争抖动重跑后消失) |
| 复现工程 `cargo test --manifest-path docs/research/2026-09-09-t0-t6-review-probe/Cargo.toml -- --test-threads=1` | **6/6 通过**(探针已按新签名/新权威接口迁移,断言未放宽) |
| `web/ npm test` | 59 pass / 0 fail |
| `web/ npm run build` | 通过 |
| `web/ npm run e2e`(真实 Core fixture) | **10/10 通过** |

正式回归测试位置(均为本轮新增,纳入常规 `cargo test`):

- `crates/mf-web/tests/contract/input_review_regress.rs` — 10 项(R1×2、R2×2、R3×2、R4 行为级、R5、R6×2)
- `crates/mf-kernel/tests/contract/input_gate_commands.rs` — 3 项(R4 命令路径)
- `crates/mf-agent/tests/core_restart_recovery.rs` — 3 项(重启恢复)
- `crates/mf-agent/tests/node_input_flow.rs` — 5 项(含 R4 陈旧确认/幂等)
- E2E:`web/e2e/run-pause-resume.spec.ts` 第三用例(环反馈)、既有暂停/面板用例维持

证据分层:mock Adapter(派发内容断言)= RecordingHost;Store/事务 = with_tx 直连;
生产 port 编译 = OrchestratorRunLifecyclePort::prepare(真实 Catalog 解析);
Core 命令路径 = InProcessCoreKernel dispatch;浏览器 = Playwright 真实 Core
fixture;真实 Core 重启 = 进程内销毁重建 Orchestrator/Store(工作目录与库持久)。
浏览器与日常 Core 的隔离:fixture 数据目录全部重定向,E2E 期间停止了本机
release mf-workbench(结束已恢复);未发生默认停服以外的环境改写。

## 4. 领域模型/签名变化与迁移

- `RunMutation::{SaveInputOverrides, ConfirmInput}` 增加 `expected_input_revision`;
  `Store::{save_input_overrides, confirm_node_input}` 同步(+record 读取器增加
  `input_revision` 字段)。探针与正式测试均已迁移。
- `NodeInputSummary` 增加 `review_state`;`WorkflowRunStepSnapshot` 增加冻结节点
  五字段;`NodeInputSnapshot` 增加 `input_revision`。均为 additive。
- schema v12 `node_inputs` 增加 `input_revision` 列——v12 属本轮未提交变更,
  未引入 v13 迁移(开发期库重建即可;若存在 v12 已建库需手动
  `ALTER TABLE node_inputs ADD COLUMN input_revision INTEGER NOT NULL DEFAULT 1`)。
- ADR:0006 增补 R4 输入版本轴与 R6 按 key 归属的决策记录(见文末补记)。

## 5. 交付物清单

- HEAD:`86c1989`(未提交);工作区 67 文件修改 + 18 个未跟踪源码文件。
- 保留未提交 diff;未 commit/push、未发远端消息或 tickets。
- 用户环境:本机 release mf-workbench 已恢复运行(端口 80)。
