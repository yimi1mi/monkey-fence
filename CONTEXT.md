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

**流水线版本（Pipeline Revision）**:
Task 的一个不可变 DAG 版本。编辑已运行的流水线会产生新 Revision;历史 Revision 只读。
_避免_: 计划(Plan)、执行(Execution)

**步骤（Step）**:
DAG 节点,包含工作说明、依赖声明和 Agent 指派。
状态:`pending / ready / running / awaiting-outcome / needs-input / succeeded / failed / blocked / skipped / cancelled`。
_避免_: Task(旧模型中 Task 指 DAG 节点,现已拆分)

**智能体档案（Agent Profile）**:
插件贡献的可配置执行器,声明 runtime 类型(pty / http / plugin-worker)、命令、参数、环境与检测方式。
_避免_: Provider(Provider 专指 API 提供方)、模型

**智能体会话（Agent Session）**:
可选复用的 CLI/API 会话,由后台 Session Registry 拥有子进程与终端状态;UI 只持有句柄。
_避免_: 终端、窗格(Pane)

**智能体执行（Agent Run）**:
Step 的一次执行尝试,持有一次性能力令牌,以显式结算(complete/fail)为唯一成功依据。
_避免_: Dispatch、执行(Execution)

**结算（Settlement）**:
Agent Run 的显式终结动作:通过 `mfctl step complete|fail`、结构化 Runtime API 或用户手工判定提交;相同结算幂等,冲突结算拒绝。
_避免_: 完成(Complete 一词指结算的一种结果)、收敛

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
