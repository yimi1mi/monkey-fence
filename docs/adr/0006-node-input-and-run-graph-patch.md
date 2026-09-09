# ADR 0006:节点输入冻结与运行中图补丁

日期:2026-09-09。状态:已接受。

## 背景

工作流节点间的输入原先在派发时临时拼接(`build_workflow_prompt`),
用户无法查看、修改或追溯实际发送给 Agent 的内容;运行中修改 DAG 只有
底层残缺能力(旧 `save_edited_revision` 无快照、非事务),未接命令面。
需求:节点间传递的 prompt 可视化可编辑;运行中可安全改图。

## 决策

1. **节点输入由唯一编译器产生并冻结**(`mf-agent/src/node_input.rs`):
   模板解析( `${inputs.*}` / `${nodes.*}` )、必填校验、来源追踪、
   业务 prompt 与只读结算协议段在同一实现中拼装;模板预览与派发共用。
   派发消费按 attempt 持久化的 `node_inputs` 记录(schema v13),
   Adapter 不得在发送时重新组装。上下文策略二值:
   `explicit_only`(只传显式选择)与 `legacy_ancestors`(旧行为);
   旧数据缺省按 legacy 解释,不静默改变语义。
2. **输出约束在成功结算的事务前置校验**:Revision 冻结的
   `output_schema`(JSON Schema 子集:object/properties/required/items/type)
   不合格时拒绝结算(`SettleError::OutputSchemaViolation`),保持待结算,
   Agent 可修正后重新提交;下游不因失败解锁。
3. **人工检查是持久门控,不是前端弹窗**:`require_input_review` 的节点
   就绪后创建 `awaiting_review` 输入记录并跳过派发(归还并发槽);
   `workflow.run.save_input_overrides` / `confirm_input` 命令
   (Controller Lease + expected revision + 幂等)驱动 awaiting → confirmed;
   确认后下一调度 tick 派发,重复确认幂等、确认后覆盖被拒。
4. **暂停是事务性派发前置**:`dispatch_run_consuming` 的 CAS UPDATE
   含 `NOT EXISTS(paused)`;暂停/恢复不推进 task revision
   (与 settle 同口径,避免 run 聚合 replace 事件破坏 journal head 连续;
   paused 由快照直读)。
5. **图补丁是一次权威事务**(`workflow.run.apply_graph_patch`):
   基线 = 活动 Revision 公开句柄;port prepare 复用 start 编译缝隙
   (实例解析/插件 pin/目录 pin),已启动(attempts>0)节点按冻结定义
   还原且守卫拒绝修改;kernel effect 内 `create_patched_revision_tx`
   一个事务完成:新 Revision(status 链 superseded→active)、继承
   status/attempts/result、handoffs.step_id 重映射到新行(同一 Handoff
   不复制)、未启动节点按新依赖重算状态、待确认输入复位、
   `agent_tasks.active_revision` 切换。应用后保持暂停,显式恢复。
   禁止将旧 `save_edited_revision` 接到 Web。

## 补记(R1–R6 审查修复,2026-09-09)

- **R4 输入版本轴**:`node_inputs.input_revision` 是输入自身的 CAS 轴——
  保存覆盖 CAS 并推进它,确认同时绑定 (input_revision, 活动 Pipeline
  Revision);同版本重复确认幂等。命令幂等摘要对覆盖内容/图补丁节点集取
  sha256(日志脱敏不等于语义忽略)。
- **R6 输入历史归属**:输入记录的 step_id 永不改写;当前运行视图按
  `node_key` 经权威查询命中(与 handoffs.step_id 重映射的结算路由目的
  不同,两者并存)。
- **R3 统一渲染**:编译保留 `nodes_resolved_template` 中间形态;绑定覆盖
  经 `render_business_prompt` 单一路径重建 prompt;显式 business_prompt
  覆盖整体替换(优先级最高)。
- **R2 双层守卫**:已启动节点不可删除在 port prepare 与提交事务内各验证
  一次(事务内基于提交时刻活动图)。

## 后果

- 结算按 step_key 定位活动 Revision 的步骤行,运行中节点的补丁后
  结算自然落到新图(令牌/会话谱系不变)。
- 暂停/恢复/补丁不推进 run revision,也不发 run 聚合投影事件;
  UI 依赖命令后强制重拉详情(commandSeq)。
- Agent 主动改图(提案-确认)应复用本补丁路径,后续单独决策。

## 收口补记（2026-09-09）

输入准备必须先于 attempt：先保存编译结果，人工检查或必填缺失时保持门控；确认验证完整输入与具体版本。创建 Agent Run 的事务同时消费该输入记录，并复验活动 Pipeline Revision、暂停状态和下一次会话选择。存储失败不得降级为没有输入记录的派发；启动结果不确定时保留已关联的 attempt，重试创建自己的输入记录。

暂停是独立的运行控制状态，重试、跳过和图补丁不隐式恢复；独立分支仍可在其他分支需要介入时调度。工作流编辑和运行图编辑共用画布、节点表单及 Core 试算语义，两个适配器分别提交项目工作流命令和原子运行图补丁。运行详情定期读取权威快照，输入门控变化无需等待 Task revision 或浏览器重载。

默认生产 Core 继续遵循 ADR 0005 的每用户单例。验收 fixture 使用显式实例命名空间，并同时隔离 owner/discovery、管道、service/catalog 数据和临时项目；不以停止用户日常 Core 作为测试前提。

启动 Operation 的 step ID 使用带版本的独立派生：新算法保留 UUIDv7 时间部分，对完整主命令 ID 和阶段做域分离哈希，并保证不等于主命令 ID。旧算法覆盖一个字节，会在该字节原本等于阶段时撞上 acceptance receipt，造成偶发补偿。派生版本随 durable payload 保存；缺少版本的旧记录按原算法恢复，升级不改变其已冻结身份。
