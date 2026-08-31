# Workflow-first Agent UX 实施计划（GLM-5.3 执行版）

> 执行对象：GLM-5.3。请严格按任务顺序实施，每完成一个任务就运行该任务的定向测试；不要把多个任务合成一次大改。

**目标：** 将 MonkeyFence 的智能体体验改成“项目工作流优先、运行居中、需要人工介入时提醒”。用户从项目工作流直接发起运行，系统在后台创建 Task 和 Pipeline Revision；Task 状态机继续作为内部可靠性机制，但不再主导用户导航。

**技术栈：** Rust、GPUI、SQLite/rusqlite、`mf-agent`、`mf-plugins`。

**现状依据：**

- `crates/mf/src/agent_workspace.rs` 暴露“智能体 / 工作流 / 会话 / 运行”四个顶层页签。
- `crates/mf/src/workflow_canvas.rs` 依赖已选 Task，并暴露“保存草稿 / 编译检查 / 分配 Revision / 确认运行”等内部阶段。
- `crates/mf/src/agent_instances_view.rs` 同时承担 Agent Type 列表、实例配置和临时会话启动。
- `crates/mf-agent/src/store.rs` 的 `task_workflows` 只能保存 Task 本地草稿，没有独立的项目工作流。
- `crates/mf/src/project_overview.rs` 已有跨项目快照和 `AttentionBucket`，应在此基础上扩展提醒，禁止另建 UI 轮询。

## 已确认的产品决策

这些决策不是开放问题，实施时不要自行反转：

1. 核心对象是自定义工作流和一次工作流运行，不是 Task 状态。
2. 用户从工作流直接点击“运行”；系统自动创建内部 Task、冻结 Revision 并启动调度。
3. 项目工作流是默认作用域；全局模板用于跨项目复用，通过“从模板创建”和“另存为全局模板”连接。
4. 单 Agent 场景是单节点工作流，不实现第二套执行路径。
5. 左侧 `Agent` 入口是工作流入口和跨项目提醒入口。
6. 顶层只保留“工作流”和“运行”两个页签。
7. Agent Type / Agent Instance / Secret 等配置移入设置页。
8. 会话不是顶层页面；它是运行节点详情中的交互面。
9. `done`、进程退出或终端空闲仍不能自动等同于成功结算；这些情况进入“需要你”。
10. 徽标按“需要处理的运行数”计数，不按被阻塞节点数计数。
11. 点击提醒必须能定位到具体项目、内部 Task 和优先处理节点。
12. 现有 Task、Ad-hoc Session、Orchestrator、Settlement 和插件 pin 语义必须保持兼容。

## Orca 交互参考

只参考交互逻辑，不复制 React 组件、样式或代码：

- `../orca/src/renderer/src/components/NewWorkspaceComposerModal.tsx`：配置弹层不销毁正在填写的 Composer。
- `../orca/src/renderer/src/components/agent/AgentCombobox.tsx`：默认 Agent、检测到的 Agent、搜索和“管理 Agent”在同一选择器中。
- `../orca/src/renderer/src/components/tab-bar/QuickLaunchButton.tsx`：默认 Agent 优先、启动后聚焦真实会话。
- `../orca/src/renderer/src/components/sidebar/WorktreeCardAgents.tsx`：状态提示附着在主入口和上下文对象上，而不是要求用户进入配置页查看。

MonkeyFence 的适配原则：Orca 的 Composer 选择 Agent；MonkeyFence 的运行 Composer 选择工作流，Agent 归属于工作流节点。

## 工作区安全规则（必须遵守）

当前工作区不是干净状态。计划编写时，以下目标文件已经包含用户修改：

- `crates/mf/src/run_monitor.rs`
- `crates/mf/src/task_composer.rs`
- `crates/mf/src/theme.rs`
- `crates/mf/src/workflow_canvas.rs`
- `crates/mf/src/workspace.rs`

执行前必须运行：

```powershell
git status --short
git diff -- crates/mf/src/run_monitor.rs crates/mf/src/task_composer.rs crates/mf/src/theme.rs crates/mf/src/workflow_canvas.rs crates/mf/src/workspace.rs
```

强制规则：

- 现有修改视为用户资产；在其上做最小增量编辑。
- 禁止 `git reset`、`git checkout --`、`git restore`、删除工作树或覆盖整个文件。
- 禁止用脚本对仓库做全局替换。
- 禁止运行会写全仓库的批量格式化；最终只运行 `cargo fmt --all -- --check`。
- 不要创建 commit，不要 push，不要 stage 文件；实现完成后保留未提交 diff 给 Codex review。
- 不修改 `.superpowers/`、`.zcode/` 以及本计划未列出的无关文件。
- 如果已有 diff 与某一步无法安全合并，停止该步骤并报告冲突，不要猜测性覆盖。

## 非目标

- 不重写 Task / Step / Run 状态机。
- 不删除任务侧栏，也不迁移或删除现有 `task_workflows` 数据。
- 不实现条件分支、循环、动态节点或新的工作流语言。
- 不改变 VCS、Execution Lease、合并、Secret 和插件权限边界。
- 不让第三方插件注入任意 GPUI UI。
- 不为了“像 Orca”而复制 Orca 的视觉样式。

## 目标交互

```text
Agent 入口
  → 项目工作流列表
  → 新建 / 从全局模板创建 / 编辑
  → 节点选择默认 CLI 或已保存配置
  → 输入本次目标
  → 开始运行
  → 实时 DAG
  → 需要人工介入时 Agent 入口与“运行”页签出现徽标
  → 点击提醒定位到具体节点
  → 继续输入 / 确认完成 / 判定失败 / 重试 / 跳过
```

## 完成定义

同时满足以下条件才算完成：

- 无需先创建 Task，即可创建、保存和运行项目工作流。
- 项目工作流跨重启保留，并与全局模板明确区分。
- 工作流节点可选择检测到的默认 CLI，也可选择保存的 Agent Instance。
- 默认 CLI 工作流运行沿用外部 CLI 配置且不写入外部配置。
- 点击“运行”后自动创建 Task、冻结 Revision、启动调度并进入运行详情。
- Agent 工作区只显示“工作流 / 运行”两个顶层页签。
- Agent 配置可在设置页完成。
- 需要人工处理时，左侧 Agent 入口和“运行”页签显示同一个运行级徽标。
- 点击徽标能打开“需要你”过滤，并选中优先处理节点。
- 处理完成后徽标通过统一 overview 快照消失；重启后仍能恢复。
- 定向测试、`cargo check --workspace` 和完整测试通过。

---

## Task 1：锁定领域语言和兼容边界

**文件：**

- 修改：`CONTEXT.md`
- 新建：`docs/adr/0004-workflow-first-interaction.md`
- 修改：`README.md`

### 要求

- [ ] 在 `CONTEXT.md` 增加以下无实现细节的术语：
  - **项目工作流（Project Workflow）**：项目内可编辑、可重复运行的 DAG 定义，是默认编排单位。
  - **全局工作流模板（Global Workflow Template）**：跨项目复用的蓝图；创建项目工作流时复制其当前版本。
  - **工作流运行（Workflow Run）**：一次项目工作流的冻结执行视图；内部由 Task + Pipeline Revision 承载。
  - **需要你（Needs You）**：存在至少一个可由用户采取动作解除的运行级提醒，不等同于某个单一 Task 状态。
- [ ] 保留 Task、Pipeline Revision、Step、Agent Run、Settlement 的现有定义。
- [ ] ADR 说明为什么不直接把 Project Workflow 等同于 Task Workflow，也说明没有删除 Task 状态机。
- [ ] README 的用户主路径改成“项目工作流 → 运行 → 需要你”，并修正仍出现的 `Agent Profile` 旧词。

### 验收

- 文档明确 Project Workflow 默认项目级、全局模板是显式复用入口。
- 文档没有声称进程退出等于成功。
- 文档没有包含数据库表、Rust 类型名等实现细节。

---

## Task 2：新增独立的项目工作流存储

**文件：**

- 修改：`crates/mf-agent/src/workflow.rs`
- 修改：`crates/mf-agent/src/schema.rs`
- 修改：`crates/mf-agent/src/store.rs`
- 修改：`crates/mf-agent/src/lib.rs`
- 新建测试：`crates/mf-agent/tests/project_workflows.rs`

### 数据结构

新增独立类型，不要复用 `task_local: bool` 表示项目工作流：

```rust
pub struct ProjectWorkflowDraft {
    pub key: String,
    pub name: String,
    pub nodes: Vec<WorkflowNodeDraft>,
    pub allow_unsafe_parallel: bool,
}

pub struct ProjectWorkflowRecord {
    pub key: String,
    pub name: String,
    pub nodes: Vec<WorkflowNodeDraft>,
    pub allow_unsafe_parallel: bool,
    pub content_digest: String,
    pub created_at: String,
    pub updated_at: String,
}
```

在项目数据库新增 `project_workflows`：

```sql
CREATE TABLE IF NOT EXISTS project_workflows (
    workflow_key TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    graph_json TEXT NOT NULL,
    allow_unsafe_parallel INTEGER NOT NULL DEFAULT 0,
    content_digest TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

### Store API

- [ ] `save_project_workflow(&ProjectWorkflowDraft) -> Result<ProjectWorkflowRecord>`
- [ ] `load_project_workflow(key: &str) -> Result<Option<ProjectWorkflowRecord>>`
- [ ] `list_project_workflows() -> Result<Vec<ProjectWorkflowRecord>>`
- [ ] `delete_project_workflow(key: &str) -> Result<bool>`
- [ ] 同内容保存不刷新 `updated_at`；使用现有 `workflow_content_digest`。
- [ ] 保存前校验 key/name 非空、nodes 非空；DAG 完整校验仍由 Workflow Compiler 负责。
- [ ] 不删除、不改写现有 `task_workflows` 表。

### 测试先行

- [ ] 新建测试覆盖创建、读取、覆盖、列表稳定排序、删除。
- [ ] 覆盖“同内容保存不刷新更新时间”。
- [ ] 覆盖 `allow_unsafe_parallel` 参与 digest。
- [ ] 覆盖损坏 JSON 返回错误而不是静默空工作流。
- [ ] 覆盖旧项目库升级后 `task_workflows` 数据仍存在。

运行：

```powershell
cargo test -p mf-agent --test project_workflows -- --nocapture
```

---

## Task 3：让正式工作流支持“默认 CLI”节点

当前 `WorkflowNodeDraft.agent_instance_id` 只能解析目录库中的 Agent Instance，导致用户必须先配置实例。保留字段以兼容已有数据，但增加一种保留引用格式：

```text
default-cli:<完整 Agent Type contribution id>
```

**文件：**

- 修改：`crates/mf-agent/src/workflow.rs`
- 修改：`crates/mf-agent/src/agent_instance.rs`
- 修改：`crates/mf-agent/src/orchestrator.rs`
- 修改：`crates/mf/src/adapter_launch.rs`
- 修改：`crates/mf/src/app_ctx.rs`
- 修改：`crates/mf/src/runtime_host.rs`
- 修改测试：`crates/mf-agent/tests/workflow_compiler.rs`
- 修改测试：`crates/mf/src/review_e2e_tests.rs`
- 视编译错误补齐现有测试中的 `AgentInstanceSnapshot` 初始化，但不要改测试语义。

### 解析契约

- [ ] 在 `mf-agent` 定义并导出 `WorkflowInstanceResolver` trait：

```rust
pub trait WorkflowInstanceResolver: Send + Sync {
    fn resolve(&self, reference: &str) -> anyhow::Result<AgentInstanceSnapshot>;
}
```

- [ ] `WorkflowKernel` 保留现有 `catalog` 字段，并新增
  `instance_resolver: Option<Arc<dyn WorkflowInstanceResolver>>`。
- [ ] 增加 `WorkflowKernel::resolve_instance(reference)`：有 resolver 时调用 resolver；否则委托现有 `CatalogStore::snapshot_agent_instance`。Compiler 的两个实例解析闭包都只调用此方法。
- [ ] 所有测试/旧装配点的 `WorkflowKernel` 把 `instance_resolver` 设为 `None`；只有生产 `AppCtx` 注入插件感知 resolver，确保现有行为默认不变。
- [ ] `mf` 层实现插件感知 resolver：
  - 普通字符串按已有 Agent Instance ID 解析。
  - `default-cli:` 引用必须解析完整 contribution id。
  - Agent Type 必须已启用且 CLI 已检测到。
  - 使用插件默认 command、全局 permission mode 对应参数和支持的运行模式合成快照。
  - 合成快照不写入 CatalogStore。
- [ ] 给 `AgentInstanceSnapshot` 和版本快照增加 `#[serde(default)] pub external_config: bool`。
  - 保存的 Agent Instance 固定为 `false`。
  - `default-cli:` 合成快照固定为 `true`。
- [ ] `RuntimeHostImpl` 调用 `compile_instance_launch` 时使用冻结快照的 `external_config`，不再对所有工作流节点写死 `false`。
- [ ] Revision 中保存合成后的完整快照，因此后续全局默认设置变化不得改变已冻结运行。
- [ ] 插件 pin 仍按快照中的完整 `agent_type` 冻结。

### 安全约束

- `external_config = true` 时继续沿用现有 Adapter 规则：跳过隔离配置写入并拒绝 `config_files`。
- 不使用 Agent Type 短 id 作为新引用。
- 不创建隐藏的持久化 Agent Instance。
- 不从 CLI 全局配置中读取或复制 Secret。

### 测试先行

- [ ] 普通 Instance ID 的行为不变。
- [ ] 检测到的 `default-cli:` 引用可以编译成冻结快照。
- [ ] 未检测、禁用或未知 contribution id 返回稳定错误。
- [ ] `external_config` 经序列化、Revision、WorkflowLaunchSpec 到 RuntimeHost 不丢失。
- [ ] 扩展现有“外部配置不被修改”测试，使正式工作流节点也覆盖该断言。

运行：

```powershell
cargo test -p mf-agent workflow_compiler -- --nocapture
cargo test -p mf review_e2e_tests::default_cli -- --nocapture
```

---

## Task 4：实现“从项目工作流直接运行”的应用服务

**文件：**

- 修改：`crates/mf/src/app_ctx.rs`
- 修改：`crates/mf-agent/src/orchestrator.rs`
- 新建：`crates/mf/src/workflow_run_composer.rs`
- 新建测试：`crates/mf/src/workflow_run_composer_tests.rs`
- 新建测试：`crates/mf-agent/tests/project_workflow_run.rs`
- 修改：`crates/mf/src/main.rs`（只注册测试模块/新模块）

### API

新增应用级结果类型：

```rust
pub struct WorkflowRunTarget {
    pub project_root: PathBuf,
    pub workflow_key: String,
    pub task_id: i64,
    pub revision_id: i64,
}
```

新增：

```rust
AppCtx::run_project_workflow(
    root: &Path,
    workflow_key: &str,
    goal: &str,
) -> Result<WorkflowRunTarget>
```

### 行为

- [ ] 从当前项目 Store 读取 Project Workflow。
- [ ] 标题取 goal 第一非空行，限制合理显示长度；完整 goal 写入 Task.goal。
- [ ] 创建 Task。
- [ ] 把 Project Workflow 投影为临时 `WorkflowTemplateVersion`，调用现有 Compiler/assign 路径。
- [ ] 冻结 Revision 后调用现有 `confirm_and_run`。
- [ ] 编译或 pin 失败时删除刚创建且尚未开始运行的 Draft Task，不留下孤儿。
- [ ] 调度已经开始后出现运行期错误时保留 Task/Revision，由现有 Needs You 机制处理，禁止回滚真实运行。
- [ ] 成功后请求 overview refresh，并返回精确 task/revision id。
- [ ] 不把 Project Workflow 自动保存成全局模板。

### Composer 状态

`WorkflowRunComposerState` 只负责：

- 当前项目与工作流摘要（只读）。
- 本次目标（唯一主输入，必填）。
- 可折叠高级选项摘要；首版高级项只展示工作流已经保存的并行策略，不再复制编辑入口。
- 提交中、错误、取消。

事件边界必须明确：

- `WorkflowCanvasEvent::RunRequested { project_root, workflow_key }` 只表达意图，不直接在画布 render 回调中启动运行。
- `AgentWorkspace` 接收事件并创建/展示 `WorkflowRunComposer`。
- Composer 成功后，`AgentWorkspace` 发出已有 `Activate(ActivationTarget::Task)`，再切换到 Runs；不要直接分别修改 Workspace 的项目和 Task 字段。

### 测试先行

- [ ] 空 goal 不能提交。
- [ ] 成功路径创建 Task、Revision 并开始调度。
- [ ] 编译失败不留下新 Task。
- [ ] 单节点和多节点工作流走同一 API。
- [ ] 工作流目标进入每个节点的现有 prompt 构造链。

运行：

```powershell
cargo test -p mf workflow_run_composer_tests -- --nocapture
cargo test -p mf-agent workflow_run -- --nocapture
```

---

## Task 5：把工作流画布改成项目工作流编辑器

**文件：**

- 修改：`crates/mf/src/workflow_editor.rs`
- 修改：`crates/mf/src/workflow_canvas.rs`
- 修改：`crates/mf/src/workflow_canvas_tests.rs`
- 修改：`crates/mf/src/workflow_editor_tests.rs`

### 状态模型

- [ ] `WorkflowCanvas` 接收当前 Project，即使没有选中 Task 也必须可用。
- [ ] 维护项目工作流列表、当前 workflow key、名称、编辑状态和保存状态。
- [ ] 新建工作流时生成稳定 key；名称由用户输入，不使用 task id。
- [ ] 支持：新建、选择、重命名、复制、删除、从全局模板创建、另存为全局模板。
- [ ] 全局模板创建项目工作流时复制当前版本；之后两者互不联动。
- [ ] 每次完成原子编辑动作后保存：添加/删除节点、改变依赖、确认标题或指令编辑、改变 Agent 绑定、改变并行策略。
- [ ] 不在每帧 render 中写数据库。
- [ ] 保存失败必须保留 dirty 状态并显示错误；禁止继续运行旧 Store 内容。

### Agent 节点选择器

左侧 Agent 库按以下顺序展示：

1. 检测到且启用的默认 CLI，引用为 `default-cli:<full-id>`。
2. 已启用的保存配置，引用为现有 instance id。
3. “管理智能体配置……”入口。

要求：

- [ ] 显示用户名称，不把 contribution id 当主标题。
- [ ] 默认 CLI 和保存配置视觉分组，但拖入/选择后都产生 `WorkflowNodeDraft`。
- [ ] 未检测到的 CLI 不出现在画布选择器；它们只在设置页出现并解释原因。
- [ ] “管理智能体配置”打开设置后，当前工作流编辑状态不能丢失。
- [ ] 使用 `WorkflowCanvasEvent::OpenAgentSettings` → `AgentWorkspaceEvent::OpenAgentSettings` → `Workspace` 的单向事件链打开设置；画布不得自行持有或创建第二个 SettingsView。

### 工具栏收敛

删除主路径中的：

- “保存草稿（任务本地）”
- “编译检查”
- “分配（冻结 Revision）”
- “确认运行”

保留：

- 保存状态文本。
- 内联诊断。
- “运行工作流”主按钮。
- “另存为全局模板”次级动作。

“运行工作流”必须先确认当前 Project Workflow 已成功保存，再打开 Run Composer。

### 测试先行

- [ ] 无 Task、只有 Project 时可以创建和编辑工作流。
- [ ] A 项目的工作流不会出现在 B 项目的项目工作流列表。
- [ ] 从全局模板创建后编辑项目工作流不会修改模板。
- [ ] 默认 CLI 和保存实例都能生成正确节点引用。
- [ ] 保存失败时 Run 按钮不可继续使用旧数据。
- [ ] 环、自依赖和未知依赖仍被拒绝。

运行：

```powershell
cargo test -p mf workflow_editor_tests -- --nocapture
cargo test -p mf workflow_canvas_tests -- --nocapture
```

---

## Task 6：重构 Agent 工作区和设置入口

**文件：**

- 修改：`crates/mf/src/agent_workspace.rs`
- 修改：`crates/mf/src/workspace.rs`
- 修改：`crates/mf/src/settings.rs`
- 修改：`crates/mf/src/agent_instances_view.rs`
- 修改：`crates/mf/src/navigation.rs`
- 新建或修改测试：`crates/mf/src/workspace_interaction_tests.rs`
- 新建测试：`crates/mf/src/agent_workspace_tests.rs`

### 顶层页签

`WorkspaceView` 和公开的 `AgentTab` 最终只保留：

```rust
Workflows
Runs
```

- [ ] 普通点击 Agent 入口：有 Needs You 时进入 Runs/需要你；没有时进入上次使用页，首次默认 Workflows。
- [ ] 命令面板的“工作流编排”进入 Workflows。
- [ ] 原“Agent 会话”命令改成“Agent 工作区”，遵循同样的提醒优先逻辑。
- [ ] 选择 Task 不得再强制把 Agent 工作区切到 Workflow 页。
- [ ] Workspace 把当前 Project 和当前 Task 分别传给 AgentWorkspace；没有 Task 时 Workflows 仍能工作。

### 设置页

- [ ] `SettingsView::new_with_app` 创建并持有嵌入式 `AgentInstancesPage`。
- [ ] “全局 Agent 策略”页面改名为“智能体”。
- [ ] 同一页先展示默认/权限/唤醒策略，再展示 Agent Type 与保存配置。
- [ ] `AgentInstancesPage` 增加嵌入模式，避免重复页头和 `size_full` 与设置滚动区冲突。
- [ ] Agent 工作区不再渲染实例配置页。
- [ ] 关闭设置返回工作流时，正在编辑的工作流与 Run Composer 不丢失。

### 兼容

- 可以保留旧的 Ad-hoc Session 启动代码和内部终端 overlay，但不再作为顶层“会话”页展示。
- 不删除 `agent_instances_view.rs`；它成为设置页内的配置组件。
- 不改真实 CLI 的全局配置。

### 测试先行

- [ ] 顶层只投影两个页签。
- [ ] 无提醒时 Agent 入口进入 Workflows。
- [ ] 设置页能创建/编辑实例并返回工作流。
- [ ] 选择 Task 不会改写用户当前 Agent 页签。

运行：

```powershell
cargo test -p mf agent_workspace_tests -- --nocapture
cargo test -p mf workspace_interaction_tests -- --nocapture
```

---

## Task 7：建立运行级 Attention 投影和徽标

**文件：**

- 修改：`crates/mf/src/project_overview.rs`
- 修改：`crates/mf/src/run_node_details.rs`
- 修改：`crates/mf/src/run_monitor.rs`
- 新建：`crates/mf/src/workflow_runs_page.rs`
- 修改：`crates/mf/src/agent_workspace.rs`
- 修改：`crates/mf/src/workspace.rs`
- 修改：`crates/mf/src/main.rs`
- 新建测试：`crates/mf/src/workflow_attention_tests.rs`
- 修改测试：`crates/mf/src/run_monitor_tests.rs`

### Snapshot 投影

在 `ProjectOverviewSnapshot` 增加运行级投影：

```rust
pub struct WorkflowRunAttention {
    pub project_root: PathBuf,
    pub task_id: i64,
    pub task_title: String,
    pub reason_count: usize,
    pub focus_step_id: Option<i64>,
}

pub attention_runs: Vec<WorkflowRunAttention>,
pub attention_run_count: usize,
```

新增一个纯函数作为唯一判定口径，例如：

```rust
direct_attention_for_step(step, latest_run, has_merge_conflict) -> Option<DirectAttention>
```

`project_overview.rs`、`RunMonitor` 和 `WorkflowRunsPage` 必须复用它；禁止三处各写一份状态匹配。

### 聚合规则

一个 Task/Workflow Run 最多贡献一个徽标计数。直接可操作原因包括：

- Step `needs-input`
- Step/Run `awaiting-outcome`
- 失败且可以重试、判定或跳过的节点
- interrupted
- 待处理 merge conflict
- 打开的用户问题
- 会话已死亡但 Run 未结算

不单独计数：

- 仅因上游失败而 blocked 的后代节点
- pending / ready
- 正在自动重试的节点
- 已取消或已归档运行

`focus_step_id` 优先级：等待输入 → 待结算 → merge conflict → failed/interrupted。相同优先级按 Step ID 稳定排序。

### WorkflowRunsPage

- [ ] 左侧过滤：需要你、运行中、最近完成。
- [ ] 列表项以一次运行（内部 Task）为单位，不以 Session 为单位；只把存在 Pipeline Revision 的 Task 投影为工作流运行，普通 Draft Task 不进入此列表。
- [ ] 右侧复用 `RunMonitor` 展示 DAG 和节点动作。
- [ ] 选中 attention 项时调用 `RunMonitor::focus_step(step_id)`。
- [ ] 节点详情继续复用现有终端/transcript、Handoff、文件、验证和动作能力。
- [ ] 运行完成或人工动作后不手工减计数；请求 overview refresh，由新 snapshot 统一更新。

### 徽标和导航

- [ ] 扩展 `activity_button` 支持可选数字徽标；其他入口传 `None`。
- [ ] 左侧 Agent 入口显示 `attention_run_count`，0 时不显示。
- [ ] AgentWorkspace 的“运行”页签显示同一计数。
- [ ] 点击 Agent 入口：
  - 0 个提醒：进入 Workflows 或上次页。
  - 1 个提醒：进入 Runs/需要你并直接选择该运行和 focus step。
  - 多个提醒：进入 Runs/需要你列表，默认选中排序第一项。
- [ ] 跨项目提醒先通过现有 `ActivationTarget::Task` 原子激活项目/Task，再定位 RunMonitor。
- [ ] 重启恢复完全依赖 Store + overview rebuild，不新增易丢失的 UI-only 已读状态。

### 测试先行

- [ ] 同一运行三个 blocked 后代只计一次。
- [ ] 两个项目各一个 Needs You 时徽标为 2。
- [ ] 处理完唯一直接原因后徽标变 0。
- [ ] `focus_step_id` 优先级与稳定排序正确。
- [ ] 点击跨项目提醒原子切换项目、Task、Runs 页和节点。
- [ ] 重启恢复后的 interrupted 运行重新出现在徽标中。

运行：

```powershell
cargo test -p mf workflow_attention_tests -- --nocapture
cargo test -p mf run_monitor_tests -- --nocapture
```

---

## Task 8：端到端闭环和回归验证

**文件：**

- 修改：`crates/mf/src/agent_workflow_e2e_tests.rs`
- 修改：`crates/mf/src/review_e2e_tests.rs`
- 仅在行为变化需要时修改：`README.md`

### 必须覆盖的主场景

- [ ] 打开项目但不创建 Task。
- [ ] 新建项目工作流。
- [ ] 添加一个默认 CLI 节点和一个保存实例节点。
- [ ] 建立依赖并保存。
- [ ] 点击运行，输入目标。
- [ ] 自动创建 Task 和 Revision，并启动第一个节点。
- [ ] 第二个节点进入 awaiting-outcome 或 needs-input。
- [ ] 左侧 Agent 徽标显示 1。
- [ ] 点击徽标直达第二个节点。
- [ ] 人工确认或继续后运行收敛，徽标清零。
- [ ] 重启应用/重建 AppCtx 后项目工作流和 Needs You 都能恢复。
- [ ] 默认 CLI 外部配置目录的哨兵哈希保持不变。

### 回归场景

- [ ] 原有 Task Composer 仍可创建 Task。
- [ ] 原有 Task `+` Ad-hoc Session 仍不改变 Task 状态。
- [ ] 保存实例仍使用隔离配置。
- [ ] 工作流插件 pin、目录 provider pin、重试、Handoff 和 merge recovery 测试仍通过。
- [ ] 多项目 overview 不产生 UI 轮询或调度背压。

### 最终验证命令

按顺序运行并保存输出摘要：

```powershell
cargo fmt --all -- --check
cargo check --workspace
cargo test -p mf-agent
cargo test -p mf
cargo test --workspace
git diff --check
git status --short
```

预期：

- 所有命令退出码为 0。
- `git diff --check` 无输出。
- `git status --short` 只包含用户原有修改和本计划明确涉及的文件。
- 不存在自动生成的数据库、临时工作树、日志或构建产物被纳入 diff。

## 交付给 Codex Review 的说明

实现完成后，不要 commit。向用户报告：

1. 已完成的 Task 编号。
2. 未完成或调整过的计划项及原因。
3. 新增/修改文件列表。
4. 每条验证命令及结果。
5. 已知风险，尤其是默认 CLI `external_config`、Attention 去重、跨项目定位和脏工作树合并。
6. 明确说“请 Codex review 当前未提交 diff”。
