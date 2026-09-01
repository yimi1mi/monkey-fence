# MonkeyFence 多项目 Agent 工作台

MonkeyFence 围绕项目内的任务组织 AI 辅助开发。任务从意图、流水线执行、人在环决策一直到结果审阅保持同一上下文,但与版本控制完全解耦。

## 统一语言

**项目（Project）**:
MonkeyFence 登记并可同时打开的一个目录。每个项目拥有独立任务数据库与调度状态；Agent Session 由每用户跨项目 Core Session Registry 统一持有，并按 Project scope 隔离。
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

**插件包（Plugin Package）**:
MonkeyFence 安装、校验、授权和内容寻址的一份版本化扩展包,通过 Manifest 贡献 Agent Type、Provider Type、installer recipe、节点或其他声明式能力。
_避免_: CLI Installation、Agent Instance、插件进程

**智能体插件（Agent Plugin）**:
贡献至少一个 Agent Type 的 Plugin Package 角色;它可以同时贡献 Provider Type 与 CLI installer,但没有独立于 Plugin Package 的安装生命周期。
_避免_: Agent Type、CLI 安装器、Agent CLI

**提供方类型（Provider Type）**:
插件贡献的模型提供方协议与配置契约,声明 Provider Profile Schema、远端模型目录探测和模型标识映射。
_避免_: Provider Profile、Agent Type、模型实例

**CLI 安装（CLI Installation）**:
核心服务检测或管理的一份外部 Agent CLI 可执行安装,包含来源、版本、路径与安装收据;它由 Agent Plugin 声明如何发现和安装,但不等同于插件包或 Agent Instance。
_避免_: Plugin Package、Agent Instance、Provider Profile

**安装收据（Installation Receipt）**:
核心服务对一次受管 CLI 安装保存的不可变来源记录,包含插件/recipe 摘要、目标、实际版本、可执行身份、校验与回滚信息;用于更新、修复、卸载和 Revision 冻结检查。
_避免_: 插件锁文件、运行日志、Provider 配置

**智能体实例（Agent Instance）**:
用户保存的一套独立 Agent Type 配置,包含命令、参数、加密 Secret 与执行契约;编辑实例不改变真实 CLI 的全局配置或已运行会话。
_避免_: Agent Type、Agent Session、Agent Profile

**提供方配置（Provider Profile）**:
一套可复用的模型提供方连接配置,包含协议、Endpoint、模型映射和加密凭据引用;智能体实例选择它作为启动配置的一部分。
_避免_: Agent Instance、API Key 明文、全局 CLI 切换

**智能体会话（Agent Session）**:
可选复用的 CLI/API 会话,由后台 Session Registry 拥有子进程与终端状态;UI 只持有句柄。
_避免_: 终端、窗格(Pane)

**预览会话（Preview Session）**:
从项目工作流编辑节点启动的独立智能体会话,用于试验配置和对话引导;不属于工作流运行,不产生 Settlement、Handoff 或下游解锁。
_避免_: Agent Run、Workflow Run、正式节点执行

**Root 模式（Root Mode）**:
用户在当前核心服务生命周期内主动开启的最高权限能力;开启后新的 CLI 安装任务和智能体会话可以获得操作系统管理员权限与 full-access,但核心服务本身仍保持普通用户权限。
_避免_: 普通 full-access、永久自启动、核心服务整体提权

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

**控制权租约（Controller Lease）**:
核心服务授予唯一可写 Web Client 的单调 epoch;所有领域写命令和终端写入租约都绑定它,新 Controller 会使旧 epoch 立即失效。
_避免_: 登录会话、API Token、Execution Lease

**终端写入租约（Terminal Writer Lease）**:
绑定 Controller client、controller epoch、WebSocket 连接与单个 Agent Session 的临时写权限;Observer 始终只能查看终端输出。
_避免_: Controller Lease、Agent Session、PTY handle

**Web 交互客户端（Web Interaction Client）**:
用户创建、编辑、运行和介入项目工作流的唯一交互表面;它投影核心服务的权威状态并提交用户意图,不拥有独立的调度状态机。
_避免_: GPUI 页面、桌面前端、第二状态机

**核心服务（Core Service）**:
MonkeyFence 中工作流、运行和智能体会话的权威业务能力拥有者;交互客户端可以断开或重连,但不接管其生命周期与状态判定。
_避免_: UI 后端、页面服务、Rust 前端

**节点会话面板（Node Session Panel）**:
附着在工作流节点对应 Agent Session 上的交互表面,保留真实 Agent CLI 的终端、命令与对话语义。
_避免_: 网页聊天框、通用终端、模拟 Agent

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
