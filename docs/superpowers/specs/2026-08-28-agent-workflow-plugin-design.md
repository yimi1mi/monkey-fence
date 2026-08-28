# Agent 工作流与实例插件化设计

日期: 2026-08-28
状态: 已批准

## 1. 概要

MonkeyFence 将现有 Agent 编排升级为用户可配置的 Agent 工作流。用户可以为同一种 CLI 创建多个独立 Agent Instance,把实例拖入可复用 Workflow Template,再将模板分配给 Task 或为 Task 创建专用工作流。运行时冻结不可变 Pipeline Revision,按 DAG 执行串行、并行和汇合节点,并通过结构化 Handoff 传递结果。

系统采用微内核架构。内核只拥有领域状态机、插件生命周期、运行事件和安全边界;Agent、节点、执行目录、Secret 存储、模板及 UI 扩展全部通过插件贡献。任何实例编辑和运行都不得修改 Claude Code、Codex 或其他真实 CLI 的全局配置。

## 2. 目标

- 同一 Agent Type 可创建任意数量的独立 Agent Instance。
- 每个实例拥有独立命令、参数、环境、加密 Secret、运行模式与执行契约。
- 支持一次性和交互式 CLI。
- 支持串行、并行和汇合 DAG。
- Task 可分配全局模板,也可创建专用工作流并另存为模板。
- 支持有限自动重试、人工重试、跳过、取消和人工结算。
- 支持结构化 Handoff,不默认传递完整会话转录。
- 支持可插拔执行目录;Git worktree 只是一个插件实现。
- Task 下可以创建不属于 DAG 的离散 CLI 会话。
- 所有外部能力由插件提供,第三方 UI 使用声明式 Schema。
- 所有敏感配置加密保存并在日志、导出和错误中脱敏。

## 3. 非目标

- 第一版不支持条件分支、循环或运行时动态生成节点。
- 不修改真实 CLI 的用户级全局配置。
- 不为不支持进程级隔离的 CLI 提供全局配置覆盖回退。
- 不把 Git、P4、worktree、分支、提交或交付状态作为 Task 成功条件。
- 不允许第三方插件注入任意 GPUI、DLL 或宿主进程内代码。
- 不迁移或兼容任何现有 MonkeyFence 持久化数据。

## 4. 领域模型

### 4.1 Agent Type

插件贡献的 CLI 类型。它声明配置 Schema、能力、检测方式、支持的运行模式和 Agent Adapter 契约。Claude Code、Codex 与 Generic Command 作为首批内置合成插件提供。

### 4.2 Agent Instance

用户保存的 Agent Type 配置。字段包括:

- 稳定 ID、名称、标签与作用域。
- Agent Type ID、插件版本与内容哈希。
- 可执行文件、参数数组、环境变量。
- Agent 专属配置与加密 Secret。
- 默认运行模式。
- 输入注入、完成检测和结果提取配置。
- 用户全局配置与可选项目覆盖。

编辑实例只影响下一次启动。已运行会话和已冻结 Revision 不随实例修改而变化。

### 4.3 Workflow Template 与 Pipeline Revision

Workflow Template 是可编辑、可复用的 DAG 数据。Task 启动时,Workflow Compiler 解析模板、任务专用修改、实例配置和插件版本,生成不可变 Pipeline Revision。Revision 固定:

- 节点和依赖。
- 实例配置快照。
- 插件版本及内容哈希。
- 提示词与变量映射。
- 重试和并行策略。
- Secret 引用,但不包含明文 Secret。

### 4.4 Step、Agent Run 与 Agent Session

Step 是 Revision 节点。Agent Run 是 Step 的一次执行尝试;重试创建新的 Agent Run。Agent Session 是 Runtime Host 管理的 CLI 进程或可恢复会话。

### 4.5 Handoff

Handoff 固定字段为:

- `status`
- `summary`
- `changed_files`
- `artifacts`
- `verification`
- `blockers`
- `recommendations`
- `output`
- `raw_log_ref`

`output` 是插件或节点声明的自定义 JSON。原始终端输出只通过 `raw_log_ref` 引用。

### 4.6 Execution Lease

Execution Directory Provider 为 Agent Run 提供路径和释放契约。默认实现返回项目目录;Git 插件可以返回临时 worktree。内核不识别 VCS 概念。

### 4.7 Ad-hoc CLI Session

离散 CLI 会话挂在 Task 下,但不属于 Revision,不影响 Task 成功判定。用户可以显式把会话输出提交为 Handoff,或用该配置创建工作流节点。

## 5. 微内核模块

### 5.1 Plugin Host

Plugin Host 负责插件发现、安装、授权、版本兼容、能力注册和 worker 生命周期。它扩展现有 `mf-plugins::PluginRegistry`,保留合成插件、内容哈希与授权指纹机制。

外部界面保持小而稳定:

- 列出可用贡献。
- 解析指定版本贡献。
- 创建受限 worker 会话。
- 校验权限和内容哈希。

Plugin Host 使用内容寻址目录保存已安装包。被活动 Revision 固定的包版本必须保留到运行结束,确保排队中的节点不会因插件更新而切换实现;历史 Revision 只保留版本与哈希用于审计,不承诺在旧包已清理后重新执行。

### 5.2 Workflow Compiler

Workflow Compiler 将 Template 和 Task 输入编译为 Revision。它负责:

- DAG 无环校验。
- 节点、依赖和变量引用校验。
- Agent Instance 与插件版本解析。
- 并行安全校验。
- Secret 引用和项目覆盖解析。
- 生成不可变快照。

### 5.3 Run Coordinator

Run Coordinator 只处理领域状态:

- Step 就绪与依赖解锁。
- 并发上限。
- Agent Run 创建。
- 失败、阻塞、重试、跳过和取消。
- Settlement 幂等与冲突拒绝。
- Handoff 发布。

它不包含 Claude、Codex、Git 或操作系统分支。

### 5.4 Runtime Host

Runtime Host 接收插件生成的 LaunchPlan,启动和监控进程,保存日志,维护 Agent Session 句柄,并把观察结果送回 Coordinator。它不解释 Agent 专属配置。

### 5.5 State Store

State Store 使用新 SQLite Schema 保存 Task、Template、Revision、Step、Run、Session、Handoff、Execution Lease、离散 CLI 会话、插件固定记录和追加式事件日志。Secret 密文与普通配置分离。

## 6. 插件贡献与接口

### 6.1 Agent Adapter

每个 Agent Type 由 Agent Adapter 实现以下行为:

- 校验 Agent Instance 配置。
- 检测 CLI 是否可用。
- 将实例快照编译为 LaunchPlan。
- 选择参数、stdin 或临时文件输入。
- 观察一次性或交互式完成状态。
- 提取 Handoff。
- 可选恢复会话。

LaunchPlan 使用 `executable + argv + env + cwd + temp_files`,默认不经过 Shell。高级 Shell 模式必须单独声明和授权。

### 6.2 Node Type

Node Type 声明节点 Schema、输入输出和运行语义。第一版包含 Agent Node 与 Join Node。后续条件、审批或外部系统节点可以通过相同界面扩展,但不在第一版启用。

### 6.3 Execution Directory Provider

接口只提供:

- `acquire(run_context) -> ExecutionLease`
- `merge(lease_set) -> MergeOutcome`
- `release(lease)`

默认实现使用项目目录。Git 插件可以在内部创建 worktree、执行无冲突合并并清理租约。冲突返回 `needs-you`,不返回 Task 失败。

### 6.4 Secret Store

Secret Store 提供 `seal`、`unseal_for_run`、`delete` 和脱敏描述。默认实现使用系统凭据保护的主密钥加密本地 Secret。插件只能用当前 Agent Run 的能力令牌读取明确授权的 Secret。

### 6.5 Declarative UI Contribution

插件通过 Schema 声明设置表单、节点属性、状态徽标和操作按钮。MonkeyFence 统一渲染。第三方插件逻辑只运行在独立 worker 中,不能加载宿主进程代码。

### 6.6 Manifest 扩展

插件清单支持:

- `agent_types`
- `node_types`
- `execution_directory_providers`
- `secret_stores`
- `workflow_templates`
- `tools`
- `skills`
- `ui_schemas`

权限至少覆盖进程启动、Shell、执行目录读写、网络、Secret、Git/worktree、后台 worker 和 UI Schema。

## 7. 插件生命周期与权限

安装流程保持:

`临时目录安装 → 清单与路径校验 → 内容哈希 → 原子发布 → 默认禁用 → 用户授权 → 启用`

权限或内容变化会改变授权指纹并要求重新授权。插件更新不影响已开始的 Revision;运行固定插件版本和内容哈希。禁用插件阻止新节点启动,已运行节点默认允许完成;强制停止必须由用户显式触发。

worker 使用版本化 NDJSON 协议,支持心跳、超时和重启检测。插件崩溃只影响使用该插件的节点。

插件和 Agent CLI 仍以当前操作系统用户权限运行。MonkeyFence 限制的是内部接口、Secret 分发和贡献授权,不是完整 OS 沙箱。

## 8. Agent Instance 安全与隔离

- 实例配置只保存在 MonkeyFence 内部。
- 不改写 `~/.claude`、`~/.codex` 或其他真实 CLI 全局配置。
- 敏感字段加密保存,UI 默认遮罩。
- 解密只发生在启动前内存中。
- 临时配置写入当前 Run 专属目录。
- 插件缺失、禁用、版本不兼容或不支持隔离时阻止启动。
- 日志、Handoff、错误和默认导出不得包含 Secret。
- 完整 Shell 命令需要额外权限;默认使用直接进程启动。

## 9. 工作流编译与执行

### 9.1 模板使用

Task 可以分配已有全局模板,也可以创建专用工作流。专用工作流默认不进入全局列表,可显式另存为模板。点击运行时冻结新 Revision。

### 9.2 节点输入

节点输入由任务目标、节点提示词、上游 Handoff 和显式自定义变量组成。变量使用稳定节点键引用,例如 `${nodes.test.output.report_path}`。未定义变量会使编译失败。

### 9.3 拓扑

第一版支持串行、并行分叉和多依赖汇合。禁止循环、条件分支和动态节点。

### 9.4 并行目录

支持隔离时,并行节点获得独立 Execution Lease。不支持隔离时默认禁止并行。用户可以开启“共享目录并行”风险开关并自行承担冲突风险。

Git 插件汇合时按固定顺序尝试无冲突合并。冲突进入 `needs-you`;用户可以启动集成 Agent 或人工处理。

### 9.5 完成与失败

一次性模式以进程退出和插件结果解析为主。交互式模式使用插件信号或人工确认。只有明确启动失败、异常退出、显式失败、验收失败或重试耗尽才标记失败。

等待输入进入 `needs-input`;主机或进程状态未知进入 `interrupted`/无法确认;合并冲突进入 `needs-you`;用户停止进入 `cancelled`。

### 9.6 重试

- 默认手动重试。
- 节点可配置有限自动重试次数。
- 自动重试保留文件修改并创建新 Agent Session。
- 手动重试可继续仍存活的会话或创建新会话。
- 失败节点阻塞下游,无依赖分支继续。
- 重试成功后自动解除下游阻塞。
- 用户可以跳过并继续或终止整个运行。

## 10. 离散 CLI 会话

每个 Task 标题旁提供 `+` 菜单,可以:

- 新建普通终端。
- 直接启动检测到的 Claude Code、Codex 等默认 CLI,沿用 CLI 已有外部配置且不执行任何写入。
- 选择用户 Agent Instance。
- 创建仅用于本次 Task 的临时 Agent Instance 并启动。

离散会话继承 Task、Project 路径和目标,可用于咨询、分析、引导或临时操作。它不属于 DAG,不改变 Task 状态。用户可显式提交 Handoff 或将其配置转换为工作流节点。

## 11. 产品界面

### 11.1 Agent Instance 页面

页面展示所有插件贡献的 Agent Type 和用户创建的 Agent Instance。检测到的 CLI 可直接创建或启动实例;缺失 CLI 置灰并解释原因。表单由插件 Schema 渲染,支持 Secret 遮罩、配置验证和高级命令设置,但没有修改真实 Agent 的“应用”动作。

Agent Type 的默认 CLI 入口不是持久化 Agent Instance:它只按插件默认命令启动现有 CLI,允许用户像 Orca 一样快速打开全部已检测 Agent。需要独立配置时,用户再基于该类型创建 Agent Instance。

### 11.2 Workflow Editor

默认布局 B:

- 左侧 Agent Instance 库。
- 中间 DAG 画布。
- 右侧可折叠节点属性面板。

工具栏提供 B 侧栏与 A 上下布局开关,默认 B,并保存用户偏好。画布支持拖入实例、连线、自动排列和运行前校验。

### 11.3 Task 创建与分配

用户输入目标后选择已有模板或新建 Task 专用工作流。运行前显示将冻结的工作流、实例和插件版本。

### 11.4 Run Monitor

Run Monitor 在同一 DAG 上展示节点状态。节点详情包含日志、Session、Handoff、文件修改、验证结果和 Execution Lease。可执行继续会话、新会话重试、跳过、人工结算和终止。冲突、等待输入和未知状态集中显示在“需要你”。

### 11.5 Plugin Manager

现有插件页增加贡献类型、权限、固定版本和兼容状态。第三方 UI 只显示其声明式 Schema。

## 12. 持久化与重新开始策略

本功能使用全新 Schema,不读取、不迁移也不兼容任何旧 MonkeyFence 数据。实施阶段允许清理 MonkeyFence 拥有的全部用户级和项目级持久化数据,包括旧任务、流水线、会话、插件、设置和项目记录。

清理范围不得包含:

- Project 源码或普通项目文件。
- `.git`、P4 或其他 VCS 数据。
- Claude Code、Codex 等真实 CLI 配置。
- CC Switch 或其他应用数据。

执行清理前必须解析并校验绝对路径属于 MonkeyFence 数据目录,禁止对 workspace 根目录、用户主目录或未解析路径执行递归删除。

## 13. 重启恢复

- 可重新连接的进程恢复原状态。
- 无法确认的进程标记为未知,不推断失败。
- 已退出但缺少结果的一次性进程进入等待确认。
- 用户可以继续观察、人工结算或重试。
- 追加式运行事件用于恢复派生状态和审计。

## 14. 测试策略

### 14.1 单元测试

- DAG 和变量校验。
- Revision 快照冻结。
- Step/Run 状态机和 Settlement 幂等。
- 重试和下游解锁。
- 权限指纹和插件版本固定。
- Secret 加密与全链路脱敏。

### 14.2 插件契约测试

- Agent Adapter。
- Node Type。
- Secret Store。
- Execution Directory Provider。
- 声明式 UI Schema。
- NDJSON worker 心跳、超时和协议版本。

### 14.3 集成测试

- 串行、并行和汇合。
- worktree 隔离和共享目录风险开关。
- 失败、自动重试、人工重试、跳过和取消。
- 插件升级、禁用和崩溃隔离。
- 应用重启与未知状态恢复。
- 离散 CLI 不改变 Task 状态。

### 14.4 UI 测试

- 创建和编辑 Agent Instance。
- 分配模板或创建 Task 专用工作流。
- `+` 菜单启动离散 CLI。
- Workflow Editor 两种布局。
- Run Monitor 和人工处理动作。

### 14.5 跨平台测试

覆盖 Windows、Linux 和 macOS,以及 Git 项目和普通文件夹 Project。测试使用伪 Agent Adapter,不依赖真实 Claude/Codex 账号。

## 15. 验收标准

- 同一 Claude Code Agent Type 可以创建多个独立实例并并行运行,互不覆盖。
- 编辑实例不影响真实 CLI 全局配置或运行中会话。
- Task 可以选择模板、创建专用工作流或添加离散 CLI。
- 工作流支持串行、并行、汇合和有限重试。
- Handoff 按固定字段和自定义 JSON 传递,不复制完整终端日志。
- 不支持隔离的 CLI 或目录策略不会静默改写全局配置。
- 重启后未知状态不会被误判为失败。
- Secret 不出现在日志、Handoff、默认导出和错误信息中。
- 外部能力全部通过插件贡献,内核没有 Claude、Codex 或 Git 专属分支。

## 16. 代码落点

- `mf-agent`:领域对象、Workflow Compiler、Run Coordinator、State Store、Session/Run/Handoff。
- `mf-plugins`:Manifest v2、Plugin Host、贡献注册、worker 协议、权限与版本固定。
- `mf`:Agent Instance、Workflow Editor、Task 分配、Run Monitor、离散 CLI 和插件管理 UI。
- `mfctl`:Settlement、Handoff、插件诊断和运行观察命令。

现有 `PipelineDraft`、Task/Revision/Step/Run 状态机、Session Registry 与 PluginRegistry 作为实现基础,但旧持久化数据不保留。

## 17. 实施约束

设计与任务拆解由当前 Codex 任务负责。实际代码实施任务通过 Codex Computer Use 交给 Zcode 执行;当前任务负责检查 diff、运行验证、反馈修复轮次和最终验收,不能仅依赖 Zcode 的完成声明。

开发过程中,Zcode 可以按功能里程碑自由创建本地 Git 提交,用于隔离变更、审阅和回滚。该授权不包含 push、合并远程分支、创建 PR 或发布;这些外部动作需要用户另行明确要求。

## 18. 实施里程碑

该设计保持一份统一规格,实施拆成按依赖排序的里程碑:

1. 全新 Schema、领域对象、事件日志与最小 Plugin Host。
2. Manifest 扩展、内容寻址版本固定、权限和声明式 UI Schema。
3. Agent Type、Agent Instance、Secret Store 与 Generic Command Adapter。
4. Claude Code/Codex Adapter、Runtime Host 和离散 CLI 会话。
5. Workflow Compiler、Handoff、Run Coordinator 与重试语义。
6. Execution Directory Provider、Git worktree 插件与汇合处理。
7. Agent Instance 页面、Workflow Editor、Task 分配和 Run Monitor。
8. 重启恢复、跨平台契约测试、安全测试和完整验收。

每个里程碑独立验证并允许创建本地 Git 提交;后续里程碑不能绕过前置接口直接依赖实现细节。
