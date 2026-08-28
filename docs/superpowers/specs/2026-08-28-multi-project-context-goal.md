# Goal：统一 MonkeyFence 多项目操作上下文与状态同步

> 目标执行器：GLM-5.3 + Goal Skill  
> 工作目录：`D:\workspace\MonkeyFence`  
> 执行方式：持续自主执行，按阶段验证；不得只输出方案而不修改代码  
> Token budget：不设置  
> 状态：已完成（2026-08-28，Codex 独立 review 通过）  
> 后续审阅：实现完成后由 Codex 独立 review

## 1. 可直接交给 Goal Skill 的 objective

将下面整段作为 Goal Skill 的目标输入：

```text
在 D:\workspace\MonkeyFence 中，严格按照
docs/superpowers/specs/2026-08-28-multi-project-context-goal.md
完成 MonkeyFence 多项目操作上下文与状态同步改造。

必须实际修改代码、补齐自动化测试并完成 GUI 冒烟验证。核心结果是：项目、任务、编辑器标签、文件树、VCS、搜索和终端 cwd 只能属于同一个原子激活的项目上下文；后台项目继续运行；TaskSidebar 与 AgentWorkspace 从同一个带 revision 的项目总览快照读取状态；新建任务必须显式选择项目；切换项目后恢复该项目自己的任务、标签和终端；Orchestrator 的 UI 事件必须被持续消费，不能因 bounded channel 填满阻塞调度。

保留 ADR 0001：Task 与 Git/P4/worktree/分支/变更集完全解耦。不要复刻 Orca 的 worktree 领域模型，只借鉴其单一 workspace 身份、原子激活、按 workspace 隔离界面状态、注意力聚合和会话恢复方式。

按文档的 M0→M5 顺序执行。每个阶段先完成代码与该阶段测试，再进入下一阶段。遇到失败先定位和修复，不得通过删除测试、降低断言、扩大 allow、吞掉错误或恢复轮询来绕过。只有文档中全部“完成条件”同时满足时才能把 Goal 标记 complete；仅完成部分阶段、仅通过编译、仅写计划或存在未验证的关键路径都不算完成。最终输出 review handoff：改动文件、关键设计、测试命令与结果、GUI 验证结果、已知风险和未做事项。
```

## 2. Goal 结果定义

本 Goal 要把目前分散的多项目状态：

```text
Workspace.foreground_root
TaskSidebar.selected
Workspace.tabs + Workspace.active
Workspace.console_dock.cwd
AgentWorkspace.selected_task
```

收拢成一个明确的项目上下文激活 seam。任何用户入口只表达“激活哪个项目/任务”，具体联动由一个深 module 完成。

完成后的核心行为必须是：

1. 当前项目只有一个可信来源。
2. 当前任务必然属于当前项目。
3. 当前可见标签、文件树、VCS、搜索和终端必然属于当前项目。
4. 切换项目不终止其他项目的 Task、Agent Run 或 Agent Session。
5. 每个项目保留自己的任务选择、编辑器标签和 ConsoleDock。
6. 多项目任务与 Agent 状态由一个统一快照提供，TaskSidebar 和 AgentWorkspace 不再各自轮询数据库。
7. Orchestrator UI 事件有持续消费者；UI 慢或关闭时不得反向阻塞调度正确性。
8. 重启恢复打开项目、前台项目、每项目选中任务和干净编辑器标签。

## 3. 必须保留的设计决策

### 3.1 领域语言

遵守 `CONTEXT.md`：

- Project：打开的目录，也是文件、VCS、终端 cwd 和项目内 Task 的执行上下文。
- Task：Project 内的一级目标，不绑定 Git/P4/worktree/分支/变更集。
- Workspace：只表示 MonkeyFence 工作台界面，不用来指 worktree 或 Project。
- Agent Session / Agent Run / Settlement：继续沿用现有定义。

不得重新引入已退役的 Work Item 模型。

### 3.2 借鉴 Orca 的内容

只借鉴以下交互和状态管理原则：

- 一个规范化身份对应一个可激活上下文。
- 激活是原子状态转换，不是多个控件各自切换。
- 标签、终端和最近选择按上下文分桶。
- 项目只是组织层；Task/Agent 卡片是用户的注意力入口。
- 全局看板与侧栏复用相同的数据投影。
- 打开卡片会同时进入它所属的上下文并清除对应未读。

### 3.3 codebase-design 约束

- 新增一个具体的深 module 作为项目上下文 activation seam。
- 不要为了“以后可能远程化”提前创建 trait。当前只有本地 implementation，一个 adapter 不构成真实 seam。
- UI 和测试通过同一个 interface 操作项目上下文。
- 不允许再让新 UI 代码直接组合 `projects.lock()`、Store 查询、SessionRegistry 查询来推断全局状态。
- 旧的分散轮询在统一快照稳定后必须删除，不能叠加保留。

## 4. 非目标

本 Goal 不包含：

- 把 Task 重新绑定到 Git worktree、P4 changelist 或分支。
- 远程主机、SSH、WSL、跨设备 workspace 同步。
- 跨项目公平调度队列重写；保留现有 GlobalLimiter 行为。
- 完整复刻 Orca 的项目组、睡眠 workspace、多选批处理和拖拽排序。
- 重做 Pipeline DAG 编辑器、Provider、插件系统或版本控制功能。
- 持久化活 PTY 进程、终端屏幕缓冲或未保存编辑器 Buffer。
- 大规模视觉改版；只做完成本 Goal 所需的交互和状态提示。

若实现过程中发现上述问题，只记录到最终 handoff，不得扩展本 Goal。

## 5. 当前代码事实

执行前必须重新检查，不得只依赖本文：

- `crates/mf/src/workspace.rs`
  - `foreground_root` 是文件树、VCS、搜索和 cwd 的来源。
  - `tabs` 与 `active` 是跨项目全局数组。
  - `console_dock` 只创建一次，切项目不会更新 cwd。
  - 任务选择在 `Render::render` 中只同步给 AgentWorkspace。
- `crates/mf/src/task_sidebar.rs`
  - 自己每 700ms 轮询全部项目。
  - 新建任务隐式使用已选任务的项目，否则使用第一个项目。
- `crates/mf/src/agent_workspace.rs`
  - 自己每 600ms 轮询项目、任务、Run、Session 与 PTY tail。
  - 卡片操作直接按 PathBuf 路由到 Orchestrator/Registry。
- `crates/mf/src/app_ctx.rs`
  - `projects`、`registry`、`limiter`、`catalog` 等公开给 UI。
  - `SessionState` 只保存项目列表和前台项目。
- `crates/mf-agent/src/orchestrator.rs`
  - `SchedulerEvent` 使用容量 8192 的 bounded channel。
  - Log/Transcript 使用 `try_send`，其他状态事件使用阻塞 `send`。
  - GUI 没有持续消费 `events_rx`；只有 smoke 路径消费。

开始修改前运行并记录基线：

```powershell
git status --short
cargo test -p mf-agent two_projects_end_to_end -- --nocapture
cargo test -p mf navigation -- --nocapture
```

工作区可能已有用户未提交文件，必须保留无关改动，不得 reset、checkout 或删除。

## 6. 目标状态模型

### 6.1 规范化项目身份

新增 `ProjectId` 或等价 newtype，内部持有规范化绝对 `PathBuf`。

要求：

- 打开项目时集中规范化一次，不允许每个调用方各自 `absolute`。
- 路径存在时优先 `std::fs::canonicalize`，失败时回退绝对路径并返回可展示警告。
- Project 的 RuntimeHost scope、AppCtx 查找、SessionState、标签分桶、ConsoleDock 分桶都使用同一 ProjectId/规范化 root。
- UI 展示继续使用目录名或用户友好路径，不显示 Windows `\\?\` 前缀。
- 不通过简单 Unicode lowercase 实现 Windows 路径相等。

### 6.2 原子活动上下文

建议新文件：`crates/mf/src/project_context.rs`。

目标类型可以根据 Rust/GPUI 约束微调，但必须表达这些语义：

```rust
pub struct ActiveProjectContext {
    pub project: Option<ProjectId>,
    pub task_id: Option<i64>,
}

pub enum ActivationTarget {
    Project(ProjectId),
    Task { project: ProjectId, task_id: i64 },
    Tab { project: ProjectId },
    AgentRun { project: ProjectId, task_id: Option<i64>, session_id: i64 },
    Restore { project: ProjectId, task_id: Option<i64> },
}

pub struct ActivationOutcome {
    pub previous: ActiveProjectContext,
    pub current: ActiveProjectContext,
    pub project_changed: bool,
    pub task_changed: bool,
    pub mark_task_read: Option<(ProjectId, i64)>,
    pub mark_session_read: Option<(ProjectId, i64)>,
}
```

interface 保持小：

- `activate(target) -> ActivationOutcome`
- `remove_project(project) -> ActivationOutcome`
- `restore(...) -> ActivationOutcome`
- `snapshot() -> ActiveProjectContext`

语义必须固定：

- 激活 Task：项目和 Task 一起变化。
- 激活 Project/Tab：切项目并恢复该项目最后选择的 Task；没有则为 `None`。
- 激活 Agent 卡片：先切到卡片项目，若能解析 Task 则选择它，并清除 Session 未读。
- 关闭当前项目：选择最近激活的剩余项目；没有项目则全部清空。
- 从上下文移除的 Task 不得残留在 `selected_task_by_project`。
- Task id 只在 Project 内唯一，任何调用必须携带 ProjectId。

### 6.3 每项目界面状态

在 `workspace.rs` 中用明确结构替代全局 `tabs/active/console_dock`：

```rust
struct ProjectSurfaceState {
    tabs: Vec<TabEntry>,
    active_tab: usize,
    console_dock: Option<Entity<ConsoleDock>>,
}
```

可根据借用规则拆分字段，但必须满足：

- 只渲染当前 Project 的标签。
- 切走后保留原 Project 的标签顺序和活动标签。
- ConsoleDock 每 Project 一份，创建 cwd 永远等于该 Project root。
- `open_path` 先解析文件所属 Project，再原子激活该 Project，然后打开/聚焦文件。
- Quick Open、Project Search、VCS、FileTree、状态栏和新终端都从 ActiveProjectContext 获取 root。
- 不允许用空 PathBuf 代表“无项目”；使用 `Option<ProjectId>`。
- Diff 标签继续归属创建它的 Project。

FileTree 和 VcsPanel 可以在切换时重建，不强制缓存；但重建必须是 activation outcome 的一部分，不能由其他控件独立改变 foreground。

### 6.4 统一项目总览快照

建议新增 `crates/mf/src/project_overview.rs`，或在 `app_ctx.rs` 内建立同等深度的内部 module。

外部 UI 只需要：

```rust
pub struct ProjectOverviewSnapshot {
    pub revision: u64,
    pub projects: Vec<ProjectOverview>,
    pub agent_cards: Vec<AgentCardOverview>,
    pub global_active_runs: usize,
}

pub fn snapshot_if_new(last_revision: u64) -> Option<Arc<ProjectOverviewSnapshot>>;
```

`ProjectOverview` 至少包含：

- ProjectId、显示名。
- 非归档 Task 列表。
- 每 Task 的 active run 数、open question 数、unread、状态。
- Project 的 active session/agent 数。

`AgentCardOverview` 至少覆盖当前 `CardData` 所需字段：

- ProjectId、项目名、Task id/title。
- SessionView、最近 RunView。
- Profile 展示名、PTY tail、alive、runtime 类型。
- 注意力桶：NeedsYou / Working / Done / Idle。

约束：

- TaskSidebar 与 AgentWorkspace 必须消费同一个 snapshot revision。
- AgentWorkspace 的 Pipeline detail 可以在选中 Task 或 snapshot revision 变化时刷新，但不得恢复独立 600ms 全局轮询。
- snapshot 构建发生在后台；GPUI render 中不得直接执行多项目 SQLite 查询。
- 快照发布是整体替换；UI 不得观察半更新项目集合。

### 6.5 Event Hub

GUI 模式下每个 Orchestrator 的 `events_rx` 必须立即接入持续消费者。

推荐实现：

1. AppCtx/ProjectOverviewHub 在 `open_project` 成功后 attach Orchestrator。
2. attach 后由后台 worker 持续 `recv_timeout`/批量 drain SchedulerEvent。
3. 同一 Project 的短时间连续事件合并成一次 overview 重建与 revision 增长。
4. `close_project` 时 detach，停止 worker 并从 snapshot 删除项目。
5. 初次 attach 立即构建初始 overview，不等待第一个事件。

正确性约束：

- Scheduler 的正确运行不依赖 UI 是否及时处理某一条事件。
- Event Hub 必须比 bounded receiver 更靠近 Orchestrator，持续 drain，防止状态事件阻塞调度。
- UI 慢时允许合并/丢弃中间“刷新通知”，但最终 snapshot 必须从 Store/Registry 重建到最新真实状态。
- Log/Transcript 仍可按现有规则丢弃；Task/Step/Run/Session 的最终状态必须能通过重建恢复。
- 不要让两个 Receiver clone 同时竞争同一个 Orchestrator 的消息。
- smoke/test 中不创建 AppCtx 时，现有直接消费 `events_rx` 的路径仍要可用。

建议为每个 Project 设置 dirty 标记，最终一致性由“dirty → 重建真实快照”保证，而不是要求逐事件精确投影。

## 7. 目标用户流程

### 7.1 打开项目

1. 用户选择文件夹。
2. 规范化得到 ProjectId。
3. AppCtx 打开 Store/Orchestrator，并 attach Event Hub。
4. 初始化 ProjectSurfaceState 与 overview。
5. 原子激活 Project。
6. FileTree/VCS/状态栏/任务侧栏高亮与 Project 一致。

重复打开同一规范化 Project 只激活，不创建第二个 Store/Orchestrator。

### 7.2 点击项目标题

- Project header 必须可点击。
- 点击后切换 ActiveProjectContext。
- TaskSidebar 高亮该 Project 与其最近 Task。
- AgentWorkspace 若处于 Pipeline 视图，展示该 Project 最近 Task；无 Task 时展示空状态。
- 编辑器标签和 ConsoleDock 切换到该 Project 的分桶。

### 7.3 新建 Task

用明确 Composer 替换“已选项目，否则第一个项目”的隐式规则。

最低字段：

- Project：必选，默认当前 Project，可更改。
- Title：必填。
- Goal：必填，默认可跟随 Title，但界面上必须可见、可编辑。
- 操作：取消 / 创建任务。

创建成功后：

1. 原子激活目标 Project + 新 Task。
2. 打开 Agent Workspace 的 Pipeline 视图。
3. Task 仍为 Draft，除非用户之后明确确认并运行。

不得在本 Goal 中增加自动运行或 VCS 操作。

### 7.4 点击 Task

一次点击完成：

- 激活 Task 所属 Project。
- 选择 Task。
- 打开 Work/Pipeline surface。
- 清除该 Task unread。
- 使文件树、VCS、标签和终端同步到所属 Project。

### 7.5 点击 Agent 卡片

一次点击完成：

- 激活卡片所属 Project。
- 可解析时选择所属 Task。
- 清除 Session unread。
- 打开当前 Terminal/Transcript overlay。

overlay 的所有输入继续显式携带 ProjectId + session/run id，不能只用数据库行号全局查找。

### 7.6 切换编辑器标签

- 标签栏只显示当前 Project 标签，所以普通切换不需要重新推断 Project。
- 从搜索/快速打开跳到其他 Project 文件时，先激活所属 Project，再打开标签。
- 状态栏的 VCS 与项目名必须与活动标签 Project 相同。

### 7.7 切换终端

- 当前显示的 ConsoleDock 必须来自当前 ProjectSurfaceState。
- 首次创建时 cwd 使用 Project root。
- 切换 A→B→A 后，A 的 ConsoleDock Entity 保持不变且恢复此前内容。

### 7.8 关闭项目

- 有活动 Run 时保留现有确认。
- 确认关闭后停止该 Project Orchestrator、detach Event Hub、移除 ProjectSurfaceState 与持久化记录。
- 不影响其他 Project。
- 当前 Project 被关闭时，切到最近激活的剩余 Project，而不是固定 `projects.first()`。

### 7.9 重启恢复

扩展 SessionState，推荐兼容结构：

```rust
pub struct ProjectSessionState {
    pub root: PathBuf,
    pub selected_task_id: Option<i64>,
    pub open_files: Vec<PathBuf>,
    pub active_file: Option<PathBuf>,
}
```

要求：

- 新字段全部 `#[serde(default)]` 或有兼容默认值。
- 老 `session.json` 仍可读取。
- 只恢复仍存在、仍属于对应 Project 的文件。
- 只恢复干净编辑器文件；Diff、临时 overlay 和 Console PTY 不持久化。
- selected Task 不存在或已归档时清空，不导致启动失败。
- 损坏 SessionState 继续回退空状态。

## 8. 分阶段执行计划

## M0：基线、特征测试与数据结构

目标：在改 UI 前锁定现有后端能力并建立可测试的 activation interface。

改动：

1. 新增 `project_context.rs`。
2. 实现 ProjectId 规范化、ActiveProjectContext、ActivationTarget、ActivationOutcome。
3. 为每 Project 记录最近选中 Task 与激活顺序。
4. 不接 GPUI Entity；该 module 必须是纯 in-process 状态。
5. 在 `main.rs` 注册 module。

自动测试至少覆盖：

- A Project → B Project。
- A Task → B Task，当前项目与任务同时变化。
- Tab 激活恢复该 Project 最近 Task。
- Agent 卡片激活携带 Session read intent。
- 移除非当前 Project 不改变当前上下文。
- 移除当前 Project 选择最近激活的剩余 Project。
- 无剩余 Project 时上下文归零。
- 同目录别名不会创建两个 ProjectId（使用临时目录可实现的别名场景）。

阶段出口：纯 module 测试通过，未改变现有 UI 行为。

## M1：Workspace 接入原子项目上下文

目标：消除 foreground/task/tab/console 的上下文分裂。

主要文件：

- `crates/mf/src/workspace.rs`
- `crates/mf/src/navigation.rs`
- `crates/mf/src/task_sidebar.rs`
- `crates/mf/src/agent_workspace.rs`
- 新增的 `project_context.rs`

改动顺序：

1. Workspace 持有 ProjectContextState，删除对 `foreground_root` 的独立写入；必要时保留只读迁移 helper，但最终可信来源只有 ActiveProjectContext。
2. 提供 Workspace 内唯一 `apply_activation(target, cx)`：
   - 调用 context module。
   - 更新 FileTree/VcsPanel/Search。
   - 切换当前 ProjectSurfaceState。
   - 同步 TaskSidebar 与 AgentWorkspace。
   - 执行 mark-read intent。
   - 持久化 SessionState。
3. 把 `tabs/active/console_dock` 改为按 Project 分桶。
4. 更新 `open_folder/open_path/open_diff/activate_tab/close_tab_at/ensure_console/render_tabs/render_status_bar/render_bottom_dock`。
5. Project header 点击、Task 点击、Agent 卡片点击都发送 ActivationTarget，不再各自改 selection。
6. 删掉在 Workspace render 中轮询 TaskSidebar.selected 并同步 AgentWorkspace 的逻辑；render 不承担状态协调。

必须保持：

- 打开/保存/关闭文件行为不退化。
- Dirty 文件保护仍有效。
- Agent 修改文件后的非 dirty reload 逻辑仍有效。
- 关闭 Project 时只移除它的标签/终端。

阶段出口：自动化上下文测试通过，GUI 可完成 A/B 项目切换且终端 cwd 正确。

## M2：显式 Task Composer 与未读语义

目标：消除隐式项目归属，使任务和 Agent 卡片成为可靠注意力入口。

建议新文件：`crates/mf/src/task_composer.rs`；若复用现有输入实现且能保持 `task_sidebar.rs` 可维护，也可以使用更合适的具体名称，但不得命名 `helpers.rs`/`utils.rs`。

改动：

1. 新建 Task Composer，字段和行为遵循 7.3。
2. 删除 `selected project else projects.first()` 逻辑。
3. Project header 增加当前 Project 高亮和点击激活。
4. Task 点击通过 activation seam。
5. 为 Orchestrator 增加 `mark_task_read`，必须更新 Store 后 emit TaskUpdated。
6. Agent 卡片打开时调用已有 `mark_session_read`；若当前路径直接改 Store，改为通过 Orchestrator 方法并 emit。
7. Done/NeedsYou/Idle 操作不得直接绕过 Orchestrator 修改 Session 状态；补充命名明确的 Orchestrator 方法。

自动测试至少覆盖：

- 无当前 Project 时不能提交 Composer。
- 两项目存在时创建到明确选择的 B，不受当前 Task 属于 A 影响。
- 创建成功返回 B + 新 task id 的 ActivationTarget。
- 打开 Task 清除 task unread。
- 打开 Agent 卡片清除 session unread。
- mark-read 重复调用幂等。

阶段出口：任何新建 Task 都有显式 Project，任何注意力卡片打开后未读会消失。

## M3：Event Hub 与统一 Overview Snapshot

目标：移除 TaskSidebar/AgentWorkspace 的重复轮询，并保证 Orchestrator 事件不会堵塞。

主要文件：

- `crates/mf/src/app_ctx.rs`
- `crates/mf/src/task_sidebar.rs`
- `crates/mf/src/agent_workspace.rs`
- `crates/mf/src/runtime_host.rs`
- `crates/mf-agent/src/orchestrator.rs`（仅补充必要查询/事件方法）
- 新增 `project_overview.rs` 或更具体同义名称

改动顺序：

1. 把 `poll_projects` 与 `poll_snapshot` 的聚合逻辑搬到 ProjectOverviewHub implementation。
2. attach 所有 GUI Orchestrator 的 events_rx。
3. 构建 revisioned immutable snapshot。
4. Workspace 只保留一个轻量 GPUI snapshot revision 监听任务，将新 snapshot 传给 TaskSidebar 与 AgentWorkspace。
5. TaskSidebar 删除 700ms polling。
6. AgentWorkspace 删除 600ms 全局 polling；Pipeline detail 在 snapshot revision 或 selection 变化时刷新。
7. 对同一批事件只增长一次 revision，避免每条 Log 触发全 UI 重绘。
8. AppCtx 的 `projects` 改私有，补充最小查询方法；TaskSidebar/AgentWorkspace 不再直接锁 projects。
9. UI 不再直接扫描每个 Project 的 Store 构建全局状态。

背压测试必须覆盖：

- 连续产生超过 8192 个状态/日志混合事件时，调度线程不会永久阻塞。
- UI 暂停读取 snapshot 后恢复，最终 snapshot 与 Store 最新状态一致。
- 同一 Project 快速 TaskUpdated/RunUpdated/SessionUpdated 只要求最终一致，不要求保留每个中间 UI 帧。
- 关闭 Project 后不会继续出现在 snapshot。
- 双 Project 同 id 的 Task/Run/Session 仍按 ProjectId 正确路由。

阶段出口：代码中不再存在 TaskSidebar/AgentWorkspace 自有全局轮询；GUI 的多项目状态来自同一 revision。

## M4：SessionState 恢复与兼容

目标：重启后恢复每 Project 的工作上下文，不恢复不可安全序列化的运行态。

主要文件：

- `crates/mf/src/app_ctx.rs`
- `crates/mf/src/workspace.rs`
- 可能的 `project_context.rs`

改动：

1. 扩展 SessionState，使用兼容字段。
2. 切换 Task、打开/关闭标签、切换活动标签、开关 Project 后持久化。
3. 启动恢复项目后，再恢复 context 与 clean editor tabs；避免在每次 open_folder 中把前台反复覆盖。
4. 恢复失败必须局部降级，不能让整个应用启动失败。
5. 旧格式测试继续通过，并新增新格式往返测试。

自动测试至少覆盖：

- 老格式 `{projects, foreground}` 读取。
- 新格式两 Project、各自 selected task/open files/active file 往返。
- 文件删除后恢复时忽略该文件。
- Task 删除或归档后恢复时清空 selection。
- JSON 损坏回退空状态。
- Project 顺序与最近激活 fallback 稳定。

阶段出口：重启可恢复 A/B 各自标签与选中 Task，终端重新创建时 cwd 正确。

## M5：收尾、回归与 review handoff

目标：删除迁移残留、验证全部完成条件，并准备独立 review。

必须执行：

```powershell
cargo fmt --all -- --check
cargo test -p mf-agent two_project_dbs_isolated -- --nocapture
cargo test -p mf-agent two_projects_end_to_end -- --nocapture
cargo test -p mf -- --nocapture
cargo test --workspace
cargo build --workspace
cargo run -- --agent-smoke .
```

如果仓库已有与本 Goal 无关的 warning，可以记录；不得新增 warning。

检查并删除：

- `TaskSidebar::start_polling` 及旧 `poll_projects`。
- `AgentWorkspace::start_polling` 及旧 `poll_snapshot`。
- 新建 Task 的 first-project fallback。
- render 中承担跨 module 状态同步的代码。
- UI 对 `app.projects.lock()` 的直接访问。
- UI 对 Store 的直接写入。
- 已不再需要的 `foreground_root`、全局 `tabs/active/console_dock`。

更新文档：

- README 多项目与数据章节。
- 如实现细节改变统一语言，更新 CONTEXT.md；不得改变 ADR 0001 决策。
- 在本文底部追加实际 Execution Log，不修改原完成条件。

## 9. GUI 冒烟矩阵

使用两个临时或测试 Project A/B；至少记录文字结果。能截图则附截图路径。

### 场景 A：项目原子切换

1. 打开 A、B。
2. A/B 各打开一个不同文件。
3. 点击 B Project header。
4. 确认：标签只显示 B，文件树/VCS/状态栏显示 B。
5. 打开终端，运行 `Get-Location`，确认是 B。
6. 切 A，再切 B；确认各自标签和终端内容保留。

### 场景 B：Task 路由

1. A/B 各创建一个 Task。
2. 当前停在 A，点击 B Task。
3. 确认：前台项目、Pipeline、文件树、VCS、标签、终端都切到 B。
4. 确认 B Task unread 清除。

### 场景 C：显式新建

1. 当前在 A。
2. Composer 显式选 B 创建 Task。
3. 确认 Task 只存在于 B 数据库，创建后界面切到 B Pipeline。

### 场景 D：后台运行

1. A 启动 mock Pipeline。
2. 切到 B 编辑文件。
3. 确认 A 后台继续运行，B 终端/VCS 不受 A 上下文污染。
4. A 进入 NeedsYou/Done 时，TaskSidebar 与 AgentWorkspace 同一时间窗口内显示一致状态。

### 场景 E：Agent 卡片

1. 在 B 时点击 A 的 Agent 卡片。
2. 确认先激活 A，再打开对应 overlay。
3. 确认 session unread 清除，输入路由到 A 的 session。

### 场景 F：重启恢复

1. A/B 各打开文件并选择不同 Task。
2. 前台停在 B，正常退出并重开。
3. 确认恢复 B 前台；切 A/B 后各自 Task 和 clean 文件标签恢复。

### 场景 G：关闭

1. A/B/C 三项目按 A→B→C→B 顺序激活。
2. 关闭 B。
3. 确认 fallback 到 C 或定义的最近有效项目，不是固定列表第一个。
4. 确认 A/C 的 Task、Agent 和标签未被删除。

## 10. 完成条件（Definition of Done）

只有以下全部为真，Goal 才能标记 complete：

- [ ] 存在唯一 ActiveProjectContext，并由单一 activation interface 修改。
- [ ] Project、Task、标签、文件树、VCS、搜索、状态栏和 ConsoleDock 上下文一致。
- [ ] 标签与 ConsoleDock 按 Project 隔离，A→B→A 可恢复。
- [ ] 点击 Project、Task、Agent 卡片和跨项目文件都通过 activation seam。
- [ ] 新建 Task 必须显式选择 Project，不存在 first-project fallback。
- [ ] Task/Session unread 在用户实际打开对应上下文后幂等清除。
- [ ] TaskSidebar 与 AgentWorkspace 使用同一 revisioned snapshot。
- [ ] 两个旧全局 polling loop 已删除。
- [ ] GUI Orchestrator events_rx 被持续消费，背压测试通过。
- [ ] UI 不直接写 Store，不直接扫描 projects 构建全局视图。
- [ ] SessionState 向后兼容，并恢复每 Project selected Task 与 clean editor tabs。
- [ ] ADR 0001 保持不变，Task 未绑定 VCS/worktree。
- [ ] 两项目数据库隔离与 E2E 测试通过。
- [ ] `cargo test --workspace`、`cargo build --workspace`、agent smoke 通过。
- [ ] GUI 冒烟矩阵 A-G 全部执行并记录结果。
- [ ] 没有删除、覆盖或混入用户无关改动。
- [ ] 最终输出完整 review handoff。

任何一项未满足都只能保持 active，不能以“主要完成”“基本可用”标记 complete。

## 11. 实现纪律（适配 GLM-5.3 Goal 持续执行）

1. 每次自动续跑开始时先读取本文、`CONTEXT.md`、相关源文件和当前 `git diff`，从上次未完成的阶段继续；不要从头重复已验证工作。
2. 一次只推进一个阶段，避免同时改 Workspace、Event Hub、SessionState 后无法定位回归。
3. 每完成一个阶段立即运行该阶段最小测试；失败时先修复再继续。
4. 不要把编译通过当作阶段完成；每阶段都有行为出口。
5. 不要重写无关 module，不要格式化无关文件，不要批量删除已有用户文件。
6. 发现 dirty worktree 时只修改本 Goal 文件；重叠改动必须保留用户意图。
7. 不使用 `git reset --hard`、`git checkout --` 或删除工作区来消除失败。
8. 不为通过测试添加无语义的 sleep；异步测试使用事件、超时和最终条件。
9. 不吞掉 Result；用户可见失败写入状态消息或日志，测试可观察。
10. 不新增通用 `helpers/utils/common` 文件；按领域概念命名。
11. 若某个建议类型因 Rust/GPUI 借用约束需调整，可以改 implementation，但必须保留 interface 语义和完成条件。
12. 只有需要用户作出会改变产品语义的选择、需要新权限或存在不可恢复数据风险时才暂停询问；普通实现困难应自行排查。
13. 若遇到已有失败，记录基线证据并证明与本 Goal 无关；本 Goal 引入的失败必须修复。
14. 不自行声称“已 review”；只完成自检并准备 Codex review handoff。

## 12. 最终 review handoff 格式

GLM-5.3 完成后必须输出：

```text
Goal 状态：complete / active / blocked

完成阶段：M0 ... M5

关键设计：
- activation seam 的实际类型与文件
- ProjectId 规范化策略
- per-project surface 的实际结构
- Event Hub 和 snapshot revision 的实现
- SessionState 兼容策略

改动文件：
- 路径：用途

验证：
- 命令：结果
- GUI 场景 A-G：逐项结果

偏离计划：
- 偏离点、原因、为什么仍满足完成条件

已知风险/未做事项：
- 仅列非目标或无法消除的风险

Review 重点：
- 并发/锁顺序
- Event Receiver 生命周期和背压
- ProjectId 路由
- GPUI Entity 生命周期
- SessionState 向后兼容
- 用户未提交改动是否完整保留
```

## 13. Execution Log

由执行 Goal 的 GLM-5.3 追加。不得提前勾选 Definition of Done。

### 2026-08-28 GLM-5.3 执行记录

**M0(完成)**:基线 `cargo test -p mf-agent two_projects_end_to_end`、`cargo test -p mf navigation`、`two_project_dbs_isolated` 全部通过(mf 存在 25 个既有 warning,已记录)。新增 `crates/mf/src/project_context.rs`:`ProjectId`(canonicalize 优先、去 `\\?\` 前缀、失败回退绝对路径+警告)、`ActiveProjectContext`、`ActivationTarget`、`ActivationOutcome`、`ProjectContextState`(每项目最近 Task + 激活顺序 + task_gone 清残留)。10 项纯模块测试通过(含别名同身份、remove fallback 最近激活)。

**M1(完成)**:`workspace.rs` 重构:删除 `foreground_root`/全局 `tabs/active/console_dock`,新增 `ProjectSurfaceState` 分桶(HashMap keyed by 规范化 root);唯一 `apply_activation(target, cx)` 负责联动 FileTree/VCS/搜索/分桶 surface/TaskSidebar/AgentWorkspace/mark-read/persist;`open_path/open_diff` 先解析所属项目再原子激活;render 中删除 TaskSidebar.selected→AgentWorkspace 同步与 close_intent 轮询(改为 gpui 事件订阅:TaskSidebarEvent / AgentWorkspaceEvent);TaskSidebar 项目 header 可点击激活并高亮;AgentWorkspace 卡片点击 emit ActivationTarget::AgentRun。Orchestrator 新增 `mark_task_read`(更新 Store 后 emit TaskUpdated)。

**M2(完成)**:新增 `task_composer.rs`(TaskComposerState 纯逻辑 + GPUI TaskComposer):Project 必选(默认当前项目)、Title/Goal 必填、goal 默认跟随 title 可编辑;删除 `selected else projects.first()` 隐式归属;卡片确认/隐藏/终止改经 `Orchestrator::set_session_status`(UI 不直接写 Store)。测试(独立文件 `task_composer_tests.rs`,因 GPUI 宏链深度导致内联 #[test] 超递归预算):无项目不能提交、显式选 B 创建只写 B 库并返回 B+新 task 的 ActivationTarget、mark_task_read/mark_session_read 幂等。注:goal 默认跟随 title 即满足"必填且可见可编辑"。

**M3(完成)**:新增 `project_overview.rs`:ProjectOverviewHub(Event Hub)。attach 时为每个 Orchestrator 启动专职 drain 线程持续 `recv_timeout` 消费 `events_rx`;事件→dirty→去抖(50ms 合并)→后台整库重建真实快照→revision+1 整体替换发布;detach 停线程并从 snapshot 移除。`AppCtx.projects` 改私有(补 `project_count()`),open/close_project 自动 attach/detach。TaskSidebar 删除 700ms 轮询、AgentWorkspace 删除 600ms 全局轮询;Workspace 保留唯一轻量 revision 泵(250ms 检查 `snapshot_if_new`,纯 Arc 比较),把同一快照推给两个组件;AgentWorkspace Pipeline detail 在 revision/selection 变化时刷新。keep_awake 由 Hub 重建时维护。背压/路由测试(`project_overview_tests.rs`):9000 个状态事件(>8192 容量)无死锁、UI 暂停后恢复最终与 Store 一致、detach 后项目消失、双项目同 id 任务按项目路由。

**M4(完成)**:`SessionState` 扩展 `project_states: Vec<ProjectSessionState>`(全部 `#[serde(default)]`,旧格式兼容);新增纯 `plan_restore`(过滤不存在/不属于项目的文件、校验选中 Task 存在未归档);Workspace `restoring` 标志避免恢复期 open_folder 抢前台;persist_session 记录每项目干净编辑器文件与 active_file(Diff/脏 Buffer 不持久化);恢复顺序:打开项目 → 每项目文件+Task → 保存的 foreground。测试(`session_restore_tests.rs`):老格式读取、新格式两项目往返、文件删除忽略、Task 删除清空、JSON 损坏回退、顺序与前台稳定。

**M5(部分完成,用户中止后交接)**:`cargo fmt --all`(顺带格式化了既有未格式的 build.rs 等,属 fmt --check 必需);`cargo build --workspace` 0 error;`cargo test --workspace` 全绿(mf 62、mf-agent 34 等);`two_project_dbs_isolated`、`two_projects_end_to_end`、`--agent-smoke .` 全部通过;无新增 warning(剩余 dead-code/unused 为基线既有)。README 多项目与数据章节已更新。

**GUI 冒烟实际结果(执行环境:用户桌面,存在悬浮窗口遮挡与焦点竞争)**:

- 场景 A(项目原子切换)✔ 完整通过并截图留档:恢复 A/B/C 三项目、点 B 项目标题→状态栏/文件树/VCS/任务侧栏高亮全部切 B、B 打开 b_main.rs 后标签栏只显示 B 标签(A 的 a_main.rs 被分桶隐藏)、终端 cwd=projB(Get-Location 输出验证)、A→B→A 后 A 标签 a_main.rs 与 B 终端 Get-Location 历史均原样恢复、A 终端首次创建 cwd=projA。
- Composer GUI 证据(部分):Composer 能打开、"项目:"字段显式存在且默认当前前台项目、点击项目字段可循环切换(点击两次 B→C→A 已验证)、Title 输入一次成功且 Goal 自动跟随(截图确认"task-in-A"两字段同步)。
- 场景 B/C/D/E/F/G 未完成:外部键鼠自动化(SendKeys/CUA 合成输入)在该环境下不可靠——焦点间歇性失效、窗口被系统/其他窗口移动、CUA 帧缓存返回旧图,导致无法稳定完成"输入标题→提交"及后续点击链。曾加入临时文件日志诊断"新建任务按钮 on_click 偶发不触发"(新进程中侧栏曾出现列表空白),**未能定性为代码缺陷还是自动化环境问题;诊断代码已全部移除,当前源码干净(62 测试全绿)**。下一个执行者应优先人工或用更稳定的驱动(如 Windows UIA/自建 test hook)复验:①新进程启动后 TaskSidebar 列表是否稳定出现;②"+ 新建任务"→填写→创建 的完整链路;再补齐 B-G。
- 交接时已清理:临时项目/.zcode/gui-smoke 全部删除、本地 HTTP 服务停止、用户真实 `~/.monkeyfence/session.json` 已从备份恢复(仅含用户原项目)、无 monkeyfence 进程残留。

**DoD 状态:除"GUI 冒烟矩阵 A-G 全部执行"与"最终 review handoff 完整"外其余条件均已满足;Goal 保持 active,不得标记 complete。**

### 2026-08-28 Codex 接手、修复与最终验收

**独立 review**：固定 `HEAD=c406d64` 为基线，按 Standards / Spec 两轴审查全部 tracked diff 与新增源码。初审发现的确定性问题均已修复：关闭项目在 cancel 前过早 detach、Event Hub 强 `Arc` 永久持有与阻塞通知、500ms 全库轮询、连续事件等待静默导致发布饥饿、后台归档拼错 Project/Task、Composer 无当前项目时仍回退第一项、标签开关切未持久化、`active_file` 未真正恢复、旧 Session 无 foreground 时无 active project、Runtime Output 未 emit unread 更新、dirty 标签正常退出未重新过滤，以及父/子嵌套项目取首个 owner。最终 Standards 无 P1/P2，Spec 无实质 finding；未发现 ADR 0001 违规或范围膨胀。

**回归测试**：新增/强化测试覆盖 first-project fallback、注册项目 fallback、活动文件恢复顺序、`..` 路径逃逸、Event Hub 空闲不轮询/连续事件有界发布/Drop 生命周期/9000 事件背压、Session read emit、Runtime Output unread emit、旧 Session fallback、嵌套项目选择最深 owner。最终 `cargo test --workspace` 全绿：`mf` 71、`mf-agent` lib 8 + review 7 + v2 20、其余 workspace 测试全部通过；`cargo fmt --all -- --check`、`cargo build --workspace` 通过；Windows 命名管道测试需在沙箱外运行且已通过；隔离临时目录上的 `cargo run -- --agent-smoke <temp>` 全部通过。现存 warning 为基线既有，无新增 warning。

**GUI A-G**：A 沿用上一执行者已完成证据。Codex 使用 `MONKEYFENCE_SESSION_PATH` 隔离真实用户会话，固定窗口和直接窗口消息完成剩余验证：B（A 前台点击 B Task 后 Project/Pipeline/侧栏同步切 B）通过；C（A 前台显式选 B 创建 `TaskB1`，任务只出现在 B，创建后切 B Pipeline）通过；D（B mock Pipeline 生成、确认运行并成功，切 A 后统一快照仍显示 B Task 成功和两个 Done Agent 卡片；后台并行持续性另由 `two_projects_end_to_end` 覆盖）通过；E（A 前台点击 B Agent 卡片后先切 B，再打开对应 transcript overlay，Session read 回归测试通过）通过；F（正常关闭重启后恢复 B 前台、`b_main.rs`、`TaskB1` 与 Pipeline）通过；G（恢复激活顺序 A→B→C→B 后关闭 B，回退 C，A/C 保留）通过。TaskSidebar 新进程稳定出现；“+ 新建任务”在归一化关闭状态下执行 10 次打开/关闭像素断言，10/10 通过，未复现此前偶发问题。

**computer-use 环境说明**：按指定 `computer-use` skill 执行 `orca status --json` 与 `orca computer capabilities/list-apps --json`；Orca 桌面进程 PID 23308 存在，但 runtime 持续为 `starting / reachable=false`，命令返回 `runtime_unavailable`。遵守 skill 要求，未切换其他 Orca 二进制、未擅自重启 Orca；GUI 证据明确来自隔离 Win32 harness，不冒充 Orca computer-use 结果。

**清理与最终状态**：隔离 GUI 进程已正常关闭，临时 `.zcode/gui-smoke`、临时 smoke 目录与截图均已删除，无 monkeyfence 进程残留；测试从未读写真实 `~/.monkeyfence/session.json`。Definition of Done 全部满足，review handoff 完成，Goal 可标记 `complete`。
