# T0–T6 第二轮修复执行报告(S1–S5 + 剩余验收)

日期:2026-09-09。对应交接:[第二轮复审交接](2026-09-09-t0-t6-second-review-handoff.md)。
基线:`main@86c19890` + 全部未提交实现(保留,R1–R6 回归未放宽)。
改动:69 个文件(+5077/−493);未跟踪源码 23 个。未 commit/push、未动远端。

## 1. S1–S5 逐项修复与正式回归

### S1 · v12 库自动升级 — 已修复

- `PROJECT_SCHEMA_VERSION` → **13**;v12 DDL 恢复为无 `input_revision` 的原始形态;
  新增 `V13_ALTER_DDLS`(`ALTER TABLE node_inputs ADD COLUMN input_revision INTEGER NOT
  NULL DEFAULT 1`),经既有 `apply_guarded_alters`(has_column 守卫)幂等执行,
  走既有迁移链(备份屏障、事务、user_version 前推)。旧库打开即自动升级,
  重复打开 no-op;不要求删库或手工 ALTER。
- 正式回归:`crates/mf-agent/tests/node_input_flow.rs
  ::s1_existing_v12_database_upgrades_input_revision_automatically`
  (构造真实 v12 库 → 打开自动升级 → 升级后输入可读/可保存/可确认且
  input_revision 真正参与 CAS → 重复打开幂等)。
- 探针迁移:第二轮探针的 v12 构造补充 user_version 回拨,使其成为真实 v12 库。

### S2 · 编辑草稿锁定输入版本 — 已修复

- `NodeInputCard` 重构状态机(`web/src/workbench/shell.tsx`):
  - `draftBaseRevision` ref 固定草稿创建时的输入版本;编辑中新快照到达不再
    推进版本,而是置 `conflict`。
  - 冲突态:保存/确认按钮禁用 + 显式冲突提示(旧草稿不会被静默套用到新版本);
    提供「放弃草稿并重新加载」显式解决路径(重新基于服务器内容与版本)。
  - 未编辑状态:新快照同时更新内容与基线版本(显示不落后)。
  - 保存成功后基线推进到服务端接受的下一版本;失败不确认(`act` 布尔传播)。
- 正式回归:第二轮组件探针 `ui-stale-draft.cjs` 迁移为更强断言——冲突时
  保存/确认禁用、提示可见;放弃草稿重新加载后编辑器回填服务器内容,基于
  新版本保存。探针抽取真实组件、真实按钮点击,通过(退出码 0)。

### S3 · 事务内复验新启动节点定义 — 已修复

- `create_patched_revision_tx`(`crates/mf-agent/src/store.rs`):事务内基于
  **提交时刻**的活动 Revision 重读 attempts>0 节点集合与冻结快照,对每个已启动
  节点执行完整冻结定义比较(职责/指令/实例/依赖/输入映射/输出要求/策略/
  验收说明/检查开关)——既不能删除,也不能修改定义。
- 正式回归:
  - `crates/mf-web/tests/contract/input_review_regress.rs
    ::graph_commit_must_recheck_newly_started_node_definition`
    (生产 port prepare 合法补丁 → 恢复派发让 B 按旧指令启动 → 再暂停 →
    提交该补丁被事务拒绝,活动 Revision 不变);
  - 既有 `t4_patch_rejects_modifying_started_node` 收紧为断言事务拒绝
    (此前断言 store 接受、守卫只在 port——按 S3 行为升级,断言变强)。

### S4 · 未启动节点改定义后的当前输入 — 已修复

- 运行投影(`crates/mf-kernel/src/run_projection.rs`):按 node_key 的输入
  归属增加**同代判定**——`dispatched` 记录(已发送历史)始终可见(继承节点的
  原发送内容保留);未发送记录仅当其所属 Revision 仍是活动版本时作为
  「当前待发送输入」展示,否则不再冒充当前输入(改图后由调度重新准备)。
- 正式回归:`input_review_regress.rs
  ::patched_unstarted_node_must_not_show_obsolete_input_as_current`
  (真 Kernel 投影:InProcessKernelRuntime + Legacy client 的
  workflow_run_snapshot,非 Store getter)。

### S5 · 取消后提醒残留 — 已修复

- 运行详情投影与工作台摘要投影(`run_projection.rs` / `workspace_projection.rs`)
  同口径收口:终态任务(succeeded/failed/cancelled/archived)不贡献
  input-review;已终态步骤不贡献;awaiting 记录挂着的 step_id 不在当前活动图
  步骤集(改图换版后的过期门控)不贡献。历史数据不改写,仅汇总口径收口。
- 正式回归:`input_review_regress.rs::cancelled_gate_must_not_remain_in_needs_you`
  (真 Kernel 投影:cancelled + needs_you=false)。

## 2. 报告口径修正(第 7 节要求)

- `core_restart_recovery.rs` 在报告中的层级改为「进程内 Core 装配重建
  (Orchestrator/Store/宿主),非 mf-workbench 进程退出重启」;真实进程重启
  由新增 E2E 覆盖(见 §3)。
- `run_projection_shows_inherited_input_after_patch` 改名为
  `inherited_input_remains_reachable_by_key_after_patch`,注释明确其调用
  Store 权威接口而非 Kernel snapshot;投影级断言由 S4/S5 用例承担。
- `same_command_id_with_different_overrides_conflicts` 固定其余 expected
  条件(同一 input_revision),只变内容——冲突只能来自语义摘要差异。

## 3. 真实进程重启 E2E — 已完成

- 新增 `web/e2e/core-restart.spec.ts` + `playwright.core-restart.config.ts`:
  用例**自行 spawn mf-workbench 进程**(隔离数据目录),断言中途 `taskkill /T /F`
  硬杀,再重新 spawn(数据目录复用、端口重新分配,页面导航到新入口),全程
  两次真实进程死亡/重启:
  1. 待确认输入(等待确认)跨重启保持,重启后门控仍拦截(乙不派发);
  2. 确认后派发、待结算时再次硬杀重启——乙仍待结算、单次 attempt,结算后
     运行成功且输入卡显示「已发送」。
- **不停止用户日常 Core**:为使隔离 Core 与日常 Core 并存(此前机器级互斥/
  固定管道名会冲突),互斥名与 mfctl 管道名改为按 service DB 路径派生
  (`singleton::core_mutex_name_for` + mf-workbench 管道名;默认安装路径
  哈希一致,现有单机行为与 mfctl 客户端兼容不变)。E2E 期间用户 release
  mf-workbench(端口 80)全程在运行。
- 主套件配置将该 spec 排除(它不能与全局 fixture Core 并存),
  `npm run e2e` 串行运行两个配置。

## 4. 剩余 T5 补齐情况

| 项 | 状态 |
| --- | --- |
| 模板试算预览 | ✅ 已实现:输入卡新增「模板试算预览」折叠区,示例值仅本地渲染(不进入覆盖/派发/审计),占位显示当前解析值/缺失 |
| 运行图与工作流图共用图组件 | ❌ 未完成(明列):运行 DAG 画布(只读展示/选中/边详情)与项目工作流编辑器(React Flow 编辑)仍是两套交互;统一编辑组件是后续工作 |
| mfctl 输入命令 / Agent 主动改图 | 不在本轮范围(交接明确) |

## 5. 实际执行的验证

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo check --workspace --tests` | 0 error |
| `RUST_TEST_THREADS=1 cargo test --workspace` | **83 个测试目标 0 failed** |
| 第一轮探针工程(6 项) | 6/6 通过 |
| 第二轮探针工程(10 项) | **10/10 通过**(探针迁移为新签名/更强断言,未放宽) |
| `node ui-stale-draft.cjs`(组件探针) | 通过(冲突禁用+显式解决路径断言) |
| `web/ npm test` | 59 pass / 0 fail |
| `web/ npm run build` | 通过 |
| `web/ npm run e2e`(主套件,真实 Core fixture) | **10/10 通过** |
| `playwright test --config playwright.core-restart.config.ts` | **1/1 通过**(两次真实进程硬杀重启) |

新增/迁移的正式回归(纳入常规 cargo test / npm run e2e):

- S1:`node_input_flow.rs::s1_existing_v12_database_upgrades_input_revision_automatically`
- S2:组件探针(独立工程;产品行为由下列 E2E 与单元覆盖输入版本传播)
- S3:`input_review_regress.rs::graph_commit_must_recheck_newly_started_node_definition`
  + `run_graph_patch.rs::t4_patch_rejects_modifying_started_node`(收紧)
- S4:`input_review_regress.rs::patched_unstarted_node_must_not_show_obsolete_input_as_current`(真投影)
- S5:`input_review_regress.rs::cancelled_gate_must_not_remain_in_needs_you`(真投影)
- 进程重启:`web/e2e/core-restart.spec.ts`(独立配置,真进程)

## 6. 领域/签名变化与迁移

- schema 版本 12 → **13**(S1;自动迁移,含备份屏障)。
- `WindowsNamedOwnerMutex` 名与 mf-workbench 管道名按 service DB 路径派生
  (默认安装行为不变;隔离实例可并存)——建议复审确认该单例语义调整。
- 无其他 API/迁移变化;探针与正式测试同步迁移,断言未放宽。

## 7. 未解决项(明列)

1. 运行图编辑(面板)与工作流画布(React Flow)未共用一套编辑组件;
   统一交互属后续 UI 重构。
2. 组件级 S2 行为未纳入 web `npm test`(需要 DOM 渲染环境);当前由独立
   组件探针 + E2E 旅程覆盖,如需可在 web 侧引入组件测试运行器。
3. mfctl 管道侧输入保存/确认命令、Agent 主动改图提案-确认仍属后续扩展。

## 8. 交付物清单

- HEAD:`86c1989`(未提交);工作区 69 文件修改 + 23 个未跟踪源码文件。
- 保留未提交 diff;未 commit/push、未发远端消息或 tickets;未触碰用户数据库。
- 用户环境:日常 release mf-workbench 全程运行未被停止。
