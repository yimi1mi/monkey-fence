# MonkeyFence 多项目 Agent 工作台

MonkeyFence 围绕项目内的任务组织 AI 辅助开发。任务从意图、流水线执行、人在环决策一直到结果审阅保持同一上下文,但与版本控制完全解耦。

## 统一语言

**项目（Project）**:
MonkeyFence 同时打开的一个目录。每个项目拥有独立的任务数据库、调度器和 Agent 会话注册表。
_避免_: 仓库、工作区(Workspace 一词专指工作台界面)

**任务（Task）**:
项目内的一级目标,由用户创建、选择和审阅;不绑定 Git、P4、worktree、分支或变更集。
状态:`draft / ready / running / needs-you / succeeded / failed / cancelled / archived`。
_避免_: 工作项、Run

**工作流模板（Workflow Template）**:
可复用的 DAG 定义,包含节点、依赖、输入输出与默认运行策略;分配给 Task 后在启动时冻结为 Pipeline Revision。
_避免_: Pipeline Revision、运行记录

**项目工作流（Project Workflow）**:
项目内可编辑、可重复运行的 DAG 定义,是默认的编排单位;用户从项目工作流直接发起运行,无需先创建 Task。
_避免_: Task、任务本地草稿

**全局工作流模板（Global Workflow Template）**:
跨项目复用的工作流蓝图;创建项目工作流时复制其当前版本,此后两者互不联动,复用是显式动作。
_避免_: 项目工作流

**流水线版本（Pipeline Revision）**:
Task 的一个不可变 DAG 快照,固定工作流、Agent Instance 与插件版本。编辑已运行的流水线会产生新 Revision;历史 Revision 只读。
_避免_: 计划(Plan)、执行(Execution)

**工作流运行（Workflow Run）**:
一次项目工作流的冻结执行视图,内部由 Task 与 Pipeline Revision 承载;实时 DAG、节点详情与人工介入动作都挂在运行上。
_避免_: Agent Run(专指 Step 的一次尝试)、会话

**步骤（Step）**:
DAG 节点,包含工作说明、依赖声明和 Agent 指派。
状态:`pending / ready / running / awaiting-outcome / needs-input / succeeded / failed / blocked / skipped / cancelled`。
_避免_: Task(旧模型中 Task 指 DAG 节点,现已拆分)

**智能体类型（Agent Type）**:
插件贡献的 CLI 执行类型,声明配置 Schema、运行能力与适配器契约。
_避免_: Agent Instance、Provider(Provider 专指 API 提供方)、模型

**智能体实例（Agent Instance）**:
用户保存的一套独立 Agent Type 配置,包含命令、参数、加密 Secret 与执行契约;编辑实例不改变真实 CLI 的全局配置或已运行会话。
_避免_: Agent Type、Agent Session、Agent Profile

**智能体会话（Agent Session）**:
可选复用的 CLI/API 会话,由后台 Session Registry 拥有子进程与终端状态;UI 只持有句柄。
_避免_: 终端、窗格(Pane)

**离散 CLI 会话（Ad-hoc CLI Session）**:
挂在 Task 下但不属于 Pipeline Revision 的交互式 Agent Session;不参与 Task 成功判定,可显式提交 Handoff 或转为工作流节点。
_避免_: Step、Agent Run

**智能体执行（Agent Run）**:
Step 的一次执行尝试,持有一次性能力令牌,以显式结算(complete/fail)为唯一成功依据。
_避免_: Dispatch、执行(Execution)

**结算（Settlement）**:
Agent Run 的显式终结动作:通过 `mfctl step complete|fail`、结构化 Runtime API 或用户手工判定提交;相同结算幂等,冲突结算拒绝。
_避免_: 完成(Complete 一词指结算的一种结果)、收敛

**需要你（Needs You）**:
存在至少一个可由用户采取动作解除的运行级提醒;`done`、进程退出或终端空闲不能自动等同于成功结算,这些情况进入需要你。
_避免_: Task 状态、节点计数

**结构化交接（Handoff）**:
Agent Run 向下游提交的结果对象,包含摘要、文件、产物、验证、阻塞项、建议与自定义输出;原始终端输出只作为日志引用。
_避免_: 完整会话转录、日志

**执行位置租约（Execution Lease）**:
Execution Directory Provider 为一次 Agent Run 提供的路径与释放契约;来源可以是项目目录或临时 worktree,Task 与 Step 不感知 VCS。
_避免_: Workspace、Task 工作树

**变更集（Change Set）**:
文件与 hunks 的快照,与版本控制面板独立存在,不参与 Task 成功条件。
_避免_: 工作区、Changelist

**交付（Delivery）**:
版本控制侧的最终动作:Git commit、Perforce shelve 或 submit。交付与 Task 生命周期解耦。
_避免_: 任务完成

## 退役词汇

以下词汇来自旧模型(绑定版控的工作项),新代码不得使用:

- 工作项（Work Item)→ 用 Task
- 工作区（Workspace,指 worktree 隔离)→ Task 不再绑定隔离工作区
- 执行（Run,指工作项的一次执行)→ 用 Agent Run(Step 的一次尝试)
- 智能体档案（Agent Profile)→ 插件贡献用 Agent Type,用户配置用 Agent Instance
