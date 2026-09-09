# T0–T6 第三轮修复执行报告(U1/U2 + 验收收口)

日期:2026-09-09。对应交接:[第三轮复审交接](2026-09-09-t0-t6-third-review-handoff.md)。
基线:`main@86c19890` + 全部未提交实现(R1–R6/S1–S5 回归原样保留,断言未放宽)。
改动:71 个文件(+5425/−498);未跟踪源码 27 个。未 commit/push、未动远端。

## 1. U1 · 冻结检查比较完整实例配置 — 已修复

- `create_patched_revision_tx`(`crates/mf-agent/src/store.rs`):对已启动
  (attempts>0)节点的冻结比较改为**派生 PartialEq 整体相等**(`patched == frozen`,
  覆盖完整 `AgentInstanceSnapshot`(版本/executable/argv/env/config/secret 引用)、
  `PluginSourcePin` 与全部职责字段)——不再手工列字段清单,杜绝再次漏项。
- 当前活动 Revision 的快照读取/解析失败时**报错拒绝提交**(移除 `.ok()` 跳过);
  已启动节点在快照中缺失同样拒绝。
- 正式回归:
  - `crates/mf-web/tests/contract/input_review_regress.rs
    ::third_started_node_must_keep_full_frozen_agent_instance`
    (同实例 ID 升级 v2 改 executable/argv → 生产 port prepare(v2) → 恢复让 B 按
    v1 启动 → 再暂停 → 提交补丁被事务拒绝,活动 Revision 不变;断言含
    `sent version 1, proposed version 2` 的复现前提);
  - 既有"删除拒绝/指令修改拒绝"测试保持通过。
- 第三轮探针 `third_started_node_must_keep_full_frozen_agent_instance` 通过。

## 2. U2 · 恢复生产单例契约 + 完整隔离入口 — 已修复

- **默认契约恢复**(ADR 0005):未设置 `MF_CORE_INSTANCE_DIR` 时,互斥名恒为
  `CORE_MUTEX_NAME`(不再无条件加后缀),owner lock/discovery 仍落用户级
  `~/.monkeyfence`——与既有决策完全一致;mfctl 管道名同样保持稳定名。
- **完整隔离入口**:`MF_CORE_INSTANCE_DIR=<根目录>` 显式启用隔离实例——
  互斥名、owner lock(`core.lock`)、discovery(`discovery.json`)、mfctl 管道名
  全部按该根目录派生/落盘,与默认单例及其他隔离实例互不共享。文档注释明确:
  仅重定向 `MF_SERVICE_DB` **不**构成隔离(owner/discovery 仍共享,第二实例被仲裁)。
- E2E fixture(`web/e2e/fixtures/core.ts`、`core-restart.spec.ts`)补设
  `MF_CORE_INSTANCE_DIR`,隔离 Core 与用户日常 Core 真正并存(本轮全部 E2E
  在用户 release mf-workbench 运行中执行,未停止)。
- 正式回归:`crates/mf-kernel/tests/contract/instance_isolation.rs`(3 项)
  - `default_setup_keeps_legacy_singleton_contract`(默认互斥名恒为旧名、任意
    service DB 路径不改名、owner/discovery 留用户级);
  - `isolated_setups_get_full_namespace_and_coexist`(不同根互斥名/owner/discovery
    全隔离且可真实同时持有;同一根第二实例仍被仲裁);
  - `service_db_redirect_alone_keeps_shared_owner_paths`(仅重定向 service DB
    时 owner/discovery 路径共享=不构成隔离)。
  注:默认契约测试因用户日常 Core 正持有默认互斥,采用名称/路径装配断言;
  真实 acquire 仲裁行为由隔离测试的同根仲裁用例覆盖(同一命名空间、同一机制)。
- 第三轮探针按交接迁移:不再构造共享路径复现前提,改为从新入口装配并验证
  真实并存/无串写/同根仲裁(`third_isolated_owner_setups_must_not_share_discovery_records`)。

## 3. 4.1 进程重启矩阵补齐 — 已完成

`web/e2e/core-restart.spec.ts` 新增第二用例(三节点 甲→乙(人工检查)→丙):

- **已确认未派发**:暂停 → 确认乙 → 断言乙状态徽章仍为"就绪"(尚无新
  attempt)→ 硬杀重启 → 乙仍就绪 → 恢复 → 乙进入待结算(单次派发,输入卡
  "已发送"= 恰好一条发送记录)→ 结算 → 丙就绪 → 结算 → 运行成功。
- **三节点完整链**与第一用例(待确认/待结算两个时点硬杀)合成矩阵;
  每个步骤以输入卡"已发送"断言恰好一次派发记录(发送记录级,非仅状态文字)。
- 注:原计划的"图补丁提交后重启(A→C→B)"在 E2E 中以暂停面板流程串联被
  上轮用例覆盖核心语义;Rust 层 `core_restart_recovery.rs
  ::graph_patch_state_survives_restart_and_resumes_correctly` 保留补丁提交后
  重建装配、恢复只跑 C/B、A 不重跑、A 输入历史可见的断言。

## 4. 4.2 编辑阶段模板试算(Core 统一编译语义) — 已完成

- 新增 Core 端点 `POST /api/v1/workflow-template/preview`
  (`crates/mf-web/src/workbench_serve.rs`):与正式派发**共用
  `node_input::compile_node_input`**(同一 `${inputs.*}`/`${nodes.*}` 解析、必填
  校验、`render_business_prompt` 渲染、上下文策略、缺失诊断);输入为节点草稿 +
  可选示例上游字段;响应标注 `preview_only`,**不落库、不进入覆盖或发送记录**
  (示例数据隔离)。
- 编辑器节点表单新增「模板试算预览」面板(`workflow_editor.tsx
  TemplatePreviewPanel`):任意节点(含未启用人工检查/无显式 binding)可试算;
  示例值输入(任务目标、上游摘要、output 字段)仅传给预览端点;展示解析后
  说明、完整业务 prompt 与缺失必填诊断。运行中输入卡的本地示例替换面板保留。

## 5. 4.3 组件探针修复并接入常规验证 — 已完成

- 探针 prelude 补 `useMemo` 导入(缺失导致 `useMemo is not defined`)。
- 新增 `web` 脚本 `test:ui` 执行组件探针;`npm run e2e` 链路末尾自动运行
  (`build → 主套件 → 重启套件 → 组件探针`),S2 行为进入常规验证入口。

## 6. 实际执行的验证

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` / `cargo check --workspace --tests` | 通过 / 0 error |
| `RUST_TEST_THREADS=1 cargo test --workspace` | **83 个测试目标 0 failed** |
| 第一轮探针(6 项) | 6/6 通过 |
| 第二轮探针(10 项) | 10/10 通过 |
| 第三轮探针(12 项,含 U1/U2) | **12/12 通过** |
| `web/ npm test` | 59 pass / 0 fail |
| `web/ npm run build` | 通过 |
| `web/ npm run e2e` 主套件(真实 Core fixture) | **10/10 通过** |
| 重启套件(独立配置,真进程硬杀) | **2/2 通过**(含新增已确认未派发场景) |
| `npm run test:ui`(S2 组件探针) | 通过 |

新增正式测试位置(纳入常规 cargo test / npm run e2e):

- U1:`input_review_regress.rs::third_started_node_must_keep_full_frozen_agent_instance`
- U2:`crates/mf-kernel/tests/contract/instance_isolation.rs`(3 项)
- 4.1:`web/e2e/core-restart.spec.ts` 第二用例
- 4.2:模板试算端点 + 编辑器面板(浏览器主套件覆盖表单可达)
- 4.3:`web/package.json` `test:ui` 挂入 `e2e`

## 7. 领域/装配变化说明

- 生产单例契约**完全恢复**为 ADR 0005 原样(默认互斥名/owner/discovery/管道名
  均不变);新增的 `MF_CORE_INSTANCE_DIR` 是显式的隔离实例入口,仅在设置时生效,
  不改变默认行为——建议复审确认该入口语义。
- `core_mutex_name_for` 默认分支不再派生后缀(修正上轮"默认名不变"注释与
  实现不符的问题)。

## 8. 未解决项(明列)

1. 运行图编辑面板与项目工作流画布仍非同一编辑组件(与上轮一致,后续 UI 重构)。
2. 图补丁提交后重启的 E2E 级(A→C→B 浏览器场景)未单独建用例;Rust 装配重建
   级覆盖保留(见 §3 注)。
3. mfctl 管道输入命令、Agent 主动改图提案仍属后续扩展。

## 9. 交付物清单

- HEAD:`86c1989`(未提交);71 文件修改 + 27 未跟踪源码文件。
- 保留未提交 diff;未 commit/push、未删库;用户日常 release mf-workbench
  (端口 80)全程运行,未被停止(隔离实例入口使其与测试 Core 并存)。
