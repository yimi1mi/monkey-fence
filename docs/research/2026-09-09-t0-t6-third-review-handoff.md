# T0–T6 第三轮复审交接：两个正确性问题与验收收口

日期：2026-09-09。审查对象：[S1–S5 第二轮修复报告](2026-09-09-t0-t6-second-round-report.md)。

**结论：上一轮十个探针均已通过，保留这些修复。当前仍需处理 U1/U2 两个已复现的正确性问题，并按实际覆盖补完验证和交互；暂不通过整体验收。**

## 1. 基线、已认可结果与范围

基线为 `main@86c19890be8345b46264c7631c23abf5d7d125ef` 加当前全部未提交实现；本轮检查时 tracked diff 为 69 文件。没有 reset、修改业务代码、commit/push 或操作远端。

本轮实际执行：

- 第二轮独立 Rust 工程的 10 个用例：**10 passed**。覆盖原六个基础场景及 S1 迁移、S3 指令竞争、S4 过期输入、S5 取消提醒。
- 本轮新增两个定向探针：**2 failed**，对应下方 U1/U2。
- 当前 S2 组件探针存在缺失 `useMemo` 导入，渲染报 `useMemo is not defined`。在临时副本中只补该导入后，真实组件的冲突禁用、冲突提示、显式重新加载和以新版本保存检查通过。认可 S2 修复，不把测试装配错误当作产品组件缺陷。
- 新管道字符串的双反斜杠已用独立随机名称调用 Windows CreateNamedPipe 验证可以创建，不列为缺陷。

本轮没有重跑全 workspace、Web build 或完整 Core E2E。新增锁测试保留生产配置的关系，但把 owner/discovery 文件落点转移到临时目录，只使用随机测试数据库派生的互斥名；没有读取、覆盖用户 owner 文件或停止日常 Core。

## 2. U1 · P1：S3 冻结检查仍漏掉实例配置和版本

位置：`crates/mf-agent/src/store.rs`，`create_patched_revision_tx` 内 `patched.instance.id == frozen.instance.id` 的比较。

完整复现：

1. 运行 A→B，启动时两节点冻结 Agent Instance v1。
2. A 成功后暂停，B 尚未启动。
3. 在 Catalog 中更新同一个实例 ID，使其成为 v2，修改 executable/argv。
4. prepare 一份不修改 B 标题/指令/依赖的补丁，B 在准备的快照中使用 v2。
5. 恢复派发，B 按当前活动 Revision 的 v1 启动；再次暂停。
6. 提交之前准备的补丁。事务接受，虽然 B 实际收到的是 v1，补丁却把冻结配置改成 v2。

实际断言结果：`sent version 1, proposed version 2`，提交没有被拒绝。

这是 S3 原有冻结约束的剩余漏洞，不是新增需求。现在仅比较实例 ID，遗漏实例版本、执行配置和插件身份；相同 ID 并不表示同一冻结配置。

修复：对已启动节点比较完整冻结语义，至少包括完整 AgentInstanceSnapshot、PluginSourcePin 和既有职责/依赖/映射/输出/策略。优先使用明确的完整快照等值规则，避免手工字段清单再次漏项。读取当前冻结快照失败时应报错，不能 `.ok()` 后跳过冻结校验。

验收：上述同 ID 不同版本/argv/executable 的竞争必须被拒绝；当前 Revision、输入历史和执行归属不变。已启动节点原样继承时仍可新增下游。继续保留已经通过的“删除拒绝”和“修改指令拒绝”测试。

探针：`third_started_node_must_keep_full_frozen_agent_instance`。使用生产 prepare 和真实 Store/Orchestrator，验证到事务层；后续还应纳入正式命令契约测试。

## 3. U2 · P1：Core 测试隔离不完整，且改变了生产单例契约

位置：`crates/mf-kernel/src/singleton.rs` 的 `core_mutex_name_for`、`OwnerLockSetup::platform`、`OwnerLockPaths::platform_default`；以及新增管道命名装配。

当前实现只按 service DB 派生 Windows mutex 名，但 `OwnerLockSetup::platform` 仍为所有数据库选择相同的用户级 `core.lock` / `discovery.json`。

复现：准备不同 service DB 的两个 production OwnerLockSetup，确认互斥名不同而 owner/discovery 路径相同；将这一共享路径关系复制到临时目录以避免触碰用户文件。第一实例 acquire 成功，第二实例返回 `OwnerActive`。因此“不同 mutex + 不同数据目录”仍不构成完整 Core 隔离。

此外，代码对默认数据库也无条件增加 mutex 后缀；注释声称默认名与旧 CORE_MUTEX_NAME 完全相同并不成立。路径指纹仅对路径字符串转小写，没有统一路径表示。不要在未梳理 owner/discovery、数据库身份和兼容行为时把它当成生产多实例支持。

领域约束：[ADR 0005](../adr/0005-web-client-headless-core.md) 仍规定每个操作系统用户一个跨项目 Core。为了 E2E 并存而普遍改变生产单例语义，与现有决策不一致。

建议修复方向：保留默认生产单例契约，给验收环境增加明确的完整实例命名空间/测试装配入口；同时隔离 mutex、owner 文件、discovery、service/catalog 数据和管道。若确实要改生产多实例模型，应显式补充决策与相同项目数据库不可被两个权威调度器同时拥有的约束，不能仅改互斥名。

验收：正常 Core 和隔离 Core 可以并存，互不更改 discovery/owner epoch/项目状态；同一实例的第二个启动仍被仲裁；默认生产实例与原约定兼容。测试不默认停止用户服务。

探针：`third_isolated_owner_setups_must_not_share_discovery_records`。它是当前错误装配关系的诊断复现；引入完整隔离入口后，应改为从新入口装配并验证真正并存与无串写，不必保留探针中为复现旧问题而构造的共享路径前提。

## 4. 验收和报告尚需收口的内容

### 4.1 真正进程重启已有实现，但矩阵还没有全部覆盖

认可 `web/e2e/core-restart.spec.ts` 确实 spawn、硬杀并重启 mf-workbench，已经比之前仅重建 Orchestrator/Store 前进了一步。

按当前测试步骤，它覆盖的是：

- 待确认输入 → 硬杀 → 重启后仍待确认。
- 确认后已经派发并进入待结算 → 硬杀 → 重启后可结算。

它没有覆盖标题声称的“已确认未派发”，也没有在图补丁提交后重启。注释说“单次 attempt”，但测试没有直接断言 attempt/Agent Run 数量。

补充原计划要求的两个场景：暂停派发后确认输入，证明尚无新 attempt，再重启并恢复；应用 A→C→B 图补丁后重启，证明只执行 C/B、A 不重跑且 A 输出仍可引用。直接检查运行/attempt 数量与发送记录，不只检查状态文字。

### 4.2 模板试算尚未实现原计划的完整语义

当前试算只出现在 `NodeInputCard` 的 `awaiting && input.bindings.length > 0` 分支。工作流编辑阶段没有对应入口；未启用人工检查或没有显式 binding 的节点也不能试算。

实现是前端对 `${inputs.*}` 做字符串替换，保留 `${nodes.*}` 未解析，且没有使用 Core 的统一输入编译/校验逻辑；它只展示指令片段，不是完整业务 prompt 的试算。因此可以称为“运行中绑定示例替换”，不能把原计划的模板预演标为全部完成。

按原 T2/T5 补齐工作流编辑阶段的只读试算入口，与正式派发共用 Core 编译语义，支持合法的 nodes/inputs 引用、缺失诊断和示例值隔离。示例数据不得进入正式覆盖或发送记录。

运行图和工作流图统一编辑交互仍按报告明列为未完成项；不用借此重做已有正确的底层实现。

### 4.3 把 S2 测试接入常规验证

当前组件探针提取的 NodeInputCard 新增了 useMemo，探针 prelude 没有对应导入，原样执行会失败。先修复探针装配，再把 S2 行为加入实际执行的测试入口，避免报告写“已通过”而当前文件不可运行。临时补导入后的 S2 断言已通过。

## 5. 复现材料与下一步

- [第三轮 Rust 工程](2026-09-09-t0-t6-third-review-probe/Cargo.toml)
- [第三轮用例](2026-09-09-t0-t6-third-review-probe/src/lib.rs)
- [U1/U2 失败日志](2026-09-09-t0-t6-third-review-probe/baseline-new-cases.log)
- [上一轮十项通过日志](2026-09-09-t0-t6-third-review-probe/baseline-previous-cases.log)
- [原组件探针的诊断副本](2026-09-09-t0-t6-third-review-probe/ui-original.cjs)
- [仅补充 import 的组件探针](2026-09-09-t0-t6-third-review-probe/ui-import-fixed.cjs)

从项目根目录运行：

```powershell
cargo test --offline --manifest-path docs/research/2026-09-09-t0-t6-third-review-probe/Cargo.toml --target-dir target third_ -- --test-threads=1
node docs/research/2026-09-09-t0-t6-third-review-probe/ui-import-fixed.cjs
```

当前基线预期：两个 third_ Rust 用例失败；补 import 后组件探针通过。组件用例依赖 web/node_modules 和 Playwright Chromium；不启动 Core。

给 zcode：

> 保留已通过的十个基础回归，先修 U1 完整冻结快照检查与 U2 Core 隔离装配，再完成第 4 节中原计划尚缺的验收和试算入口。将新增行为纳入正式测试，更新报告时准确区分已确认未派发和已派发待结算。保留现有未提交实现，不 commit/push、不删库、不默认停止日常 Core；无需重做已经验证正确的部分。
