# 可视化工作流 DAG 编辑器实施计划（GLM-5.3 执行版）

> **状态：已废弃，请勿执行。** 产品方向已调整为 Web Interaction Client + 无界面 Rust Core Service。本计划只保留为历史设计记录；新的 canonical 决策地图是 [Wayfinder：Web 交互客户端与 Rust 核心服务重构](https://github.com/yimi1mi/monkey-fence/issues/1)。地图清晰后将重新生成 `/to-spec` 与实现 tickets。

> 执行对象：GLM-5.3。请严格按 Task 顺序实施，每完成一个 Task 就运行该 Task 的定向测试。不要把所有改动压成一次大改，也不要自行扩大到条件分支、循环或新的运行时语义。

**目标：** 把当前按拓扑分层显示、在右侧检查器里点选依赖的工作流编辑器，升级为真正的可视化 DAG 画布：从 Agent 库拖入节点、拖动节点位置、从端口拉线建立依赖、直接选择并删除连线，并支持平移、缩放、适配视图和自动排列。

**技术栈：** Rust、GPUI、SQLite/rusqlite、`mf-agent`、`mf`。

**规格依据：**

- `docs/superpowers/specs/2026-08-28-agent-workflow-plugin-design.md` §11.2 已要求“拖入实例、连线、自动排列”。
- `docs/adr/0004-workflow-first-interaction.md` 已确定项目工作流是默认编排单位。
- `CONTEXT.md` 已确定 Project Workflow、Pipeline Revision、Step、Agent Instance 等统一语言。

## 当前实现与缺口

当前基线为 `main` 的 `39e5fb057ebc72fb7a7fb8d464326f85eb42a768`（`feat(workflows): add workflow-first agent UX`）。

- `crates/mf/src/workflow_editor.rs` 已有纯状态模型、环检测和拓扑分层，但没有节点坐标、视口变换、命中测试和交互状态机。
- `crates/mf/src/workflow_canvas.rs` 的“画布”实际是按层排列的 `Div` 行；节点只能点击添加，依赖只能在检查器里点击连接/断开。
- `crates/mf-agent/src/workflow.rs` 的 `ProjectWorkflowDraft` 只包含可执行 DAG 语义。
- `crates/mf-agent/src/store.rs` 的 `project_workflows.graph_json` 只存节点和依赖；没有展示布局。
- GPUI 本地依赖已经提供 `canvas`、`PathBuilder`、`Window::paint_path`、鼠标事件、`on_drag`、`on_drag_move` 和 `on_drop`，不需要引入第三方图编辑库。

## 不可反转的设计决策

1. **依赖方向统一为 `上游 dep → 下游 node`。** 用户从上游节点右侧输出端口拉到下游节点左侧输入端口，落库时写入 `downstream.deps.push(upstream)`。
2. **可执行 DAG 与展示布局分离。** 节点坐标是 Project Workflow 的展示元数据，不进入 `WorkflowNodeDraft`、`workflow_content_digest`、Workflow Template、Pipeline Revision 或 Runtime。
3. **拖动节点不会制造新的执行版本。** 仅节点、依赖、指令、Agent 绑定和并行策略影响工作流内容摘要。
4. **展示元数据保存失败不阻止运行。** 语义保存失败继续使用现有 `save_error` 阻止运行；布局保存失败单独显示“布局未保存”，但不把已成功保存的 DAG 判为不可运行。
5. **旧工作流零迁移可见。** 没有坐标的节点用稳定的拓扑自动排列生成内存布局；只有用户移动节点、从库拖入节点或点击“自动排列”时才保存布局。
6. **全局模板首版不携带画布坐标。** 从全局模板创建项目工作流时执行稳定自动排列；“另存为全局模板”只保存 DAG 语义。复制项目工作流则复制其节点坐标。
7. **编辑器做即时校验，Compiler 仍是运行前权威。** 画布立即拒绝自依赖、环和未知节点，但不得绕开现有 Workflow Compiler。
8. **GPUI 只是适配器。** 坐标变换、自动排列、命中测试、选中、拖动和连线状态放在可纯测试的深模块中；`workflow_canvas.rs` 不新增另一套图规则。
9. **数据库不在指针移动时写入。** 节点拖动只在 mouse-up/drop 提交一次；平移和缩放是当前视图状态，不持久化。
10. **保留现有兜底交互。** Agent 库条目仍可点击添加节点，检查器里的依赖连接/断开仍可使用，避免鼠标精细操作成为唯一入口。

## 目标交互

```text
Agent 库条目
  ├─ 点击 → 在当前视口中心附近自动放置节点
  └─ 拖拽 → 在放下位置创建节点

节点卡片
  ├─ 拖拽卡片 → 移动节点，释放时保存坐标
  ├─ 点击 → 选中并打开右侧检查器
  ├─ 右侧输出端口拖拽 → 显示临时贝塞尔连线
  └─ 放到另一节点左侧输入端口 → 校验并创建依赖

连线
  ├─ 箭头方向：上游 → 下游
  ├─ 点击 → 选中
  └─ Delete / Backspace → 断开依赖并自动保存

画布背景
  ├─ 拖拽空白处或 Space + 左键拖拽 → 平移
  ├─ Ctrl + 滚轮 → 以指针为中心缩放
  ├─ 适配视图 → 把全部节点放入可视区域
  └─ 自动排列 → 按拓扑从左到右重新排布并保存坐标
```

## 完成定义

- 可把默认 CLI 或保存的 Agent Instance 从左侧拖到画布任意位置。
- 可拖动已有节点，切换工作流或重启应用后节点位置保持。
- 节点之间显示有方向的曲线和箭头；连线拖拽有实时预览与目标高亮。
- 自依赖、环、未知节点和重复边不会写入状态或数据库，错误可见且不会残留半条边。
- 可点击连线后用 Delete/Backspace 删除；检查器依赖按钮仍可作为兜底。
- 可平移、缩放、适配视图和一键自动排列；缩放范围稳定，不出现 NaN、无穷值或节点丢失。
- 旧的无布局数据工作流、从模板创建的工作流都能稳定显示。
- 节点移动不改变工作流 `content_digest`，也不改变运行时冻结的 DAG。
- 语义保存失败仍阻止运行；仅布局保存失败不阻止运行。
- 新建、重命名、复制、删除、模板复制、另存模板、运行工作流和右侧检查器全部保持可用。
- 定向测试、`cargo check --workspace`、完整测试与视觉验收通过。

## 工作区安全与 Review 约定

开始前运行并保存输出：

```powershell
git status --short
git rev-parse HEAD
git diff --check
```

预期基线 commit 是：

```text
39e5fb057ebc72fb7a7fb8d464326f85eb42a768
```

当前已有未跟踪目录 `.superpowers/`、`.zcode/`，它们属于用户资产：

- 不读取、不修改、不删除、不纳入提交。
- 禁止 `git reset`、`git checkout --`、`git restore` 和任何递归删除。
- 禁止全仓库脚本替换和会改写大量无关文件的格式化。
- 不 stage、不 commit、不 push；完成后保留未提交 diff 给 Codex review。
- 若 HEAD 已不再是上述基线，先报告新 HEAD，不要擅自 reset。
- 最终 review 的固定点是上述 commit；GLM 报告中必须明确写出实际 HEAD 和所有未提交文件。

## 模块与测试缝隙

计划只建立三个主要缝隙：

1. **Store 缝隙：** Project Workflow 的可执行内容和展示元数据分别读写；测试数据库迁移、往返和错误隔离。
2. **纯编辑器缝隙：** `WorkflowEditorState` 接收编辑/指针意图并返回变化类别；测试图规则、坐标、自动排列、命中和交互状态，不依赖 GPUI。
3. **WorkflowCanvas 缝隙：** GPUI 只把鼠标/键盘/拖放事件翻译成纯编辑器意图，再按变化类别触发语义保存或布局保存。

不要为每种鼠标动作创建一个跨模块 public trait，也不要把 Store、GPUI `Context` 或 `Window` 注入纯编辑器状态。

---

## Task 1：先把现有编辑器状态深化为可视化场景模块

**Blocked by：** 无，可立即开始。

**交付：** 在不改变现有 UI 的前提下，建立可纯测试的坐标、视口、布局、命中和交互状态；后续 GPUI 只消费该模块。

**文件：**

- 修改：`crates/mf/src/workflow_editor.rs`
- 修改：`crates/mf/src/workflow_editor_tests.rs`

### 状态模型

保留 `EditorNode` 和现有 DAG 规则，增加等价于以下职责的类型；具体命名可按仓库风格微调，但职责不得散回 GPUI：

```rust
pub struct CanvasPoint { pub x: f32, pub y: f32 }
pub struct CanvasViewport { pub pan: CanvasPoint, pub zoom: f32 }
pub enum EditorSelection { Node(String), Edge { from: String, to: String } }
pub enum CanvasInteraction {
    Idle,
    MovingNode { key: String, grab_offset: CanvasPoint },
    Connecting { from: String, pointer: CanvasPoint },
    Panning { pointer_start: CanvasPoint, pan_start: CanvasPoint },
}
pub enum EditorChange { None, ViewOnly, Presentation, Semantic }
```

要求：

- [ ] 节点坐标使用画布 world coordinates；GPUI 绝对坐标只在适配层出现。
- [ ] 提供唯一的 `world_to_screen` / `screen_to_world` 变换，并保证互为逆变换。
- [ ] `zoom` 夹在 `0.35..=2.0`；以指针为中心缩放时，指针下的 world point 不漂移。
- [ ] 节点卡片使用统一 world size；端口位置、节点矩形和命中半径由纯模块计算。
- [ ] 端口命中半径按屏幕像素保持可点，不随缩放变得不可用。
- [ ] 自动排列按依赖从左到右分层，层内按稳定 key 排序；相同输入产生完全相同坐标。
- [ ] 适配视图根据全部节点包围盒和固定 padding 计算 pan/zoom；空图回到安全默认值。
- [ ] 节点释放时吸附到 20 world-unit 网格；拖动预览可保持连续，不要每帧跳格。
- [ ] 连线的纯语义统一为 `connect(from_upstream, to_downstream)`，内部调用现有 `add_dependency(to, from)`。
- [ ] 删除节点继续清理悬空依赖和坐标；删除边只移除 `to.deps` 中的 `from`。
- [ ] 重复连接返回稳定 no-op，不重复写 deps。
- [ ] `EditorChange` 明确区分：选择/平移/缩放不保存，移动/自动排列保存展示元数据，增删节点/边保存语义。

### 测试先行

- [ ] world/screen 往返误差在可接受 epsilon 内。
- [ ] 缩放锚点保持不动，且极端滚轮输入被 clamp。
- [ ] 自动排列保证每条边的上游 x 小于下游 x，分叉/汇合不重叠。
- [ ] 相同 DAG 两次排列结果完全一致。
- [ ] 节点、端口和贝塞尔边命中测试覆盖缩放后的坐标。
- [ ] self/cycle/unknown/duplicate edge 行为保持。
- [ ] 删除节点同时清理选中态、交互态、依赖和坐标。

运行：

```powershell
cargo test -p mf workflow_editor_tests -- --nocapture
```

完成后再进入 Task 2。

---

## Task 2：独立持久化项目工作流的节点坐标

**Blocked by：** Task 1。

**交付：** 用户移动节点后，切换工作流或重启仍能恢复布局；布局与可执行 DAG 的摘要和运行版本完全解耦。

**文件：**

- 修改：`crates/mf-agent/src/workflow.rs`
- 修改：`crates/mf-agent/src/schema.rs`
- 修改：`crates/mf-agent/src/store.rs`
- 修改：`crates/mf-agent/src/lib.rs`
- 修改：`crates/mf-agent/tests/project_workflows.rs`
- 修改：`crates/mf-agent/tests/fresh_schema.rs`
- 修改：`crates/mf/src/workflow_canvas.rs`
- 修改：`crates/mf/src/workflow_canvas_tests.rs`

### 存储契约

- [ ] 把项目库 schema 从 v6 升到 v7。
- [ ] 给 `project_workflows` 增加 `presentation_json TEXT NOT NULL DEFAULT '{}'`。
- [ ] 定义可 serde、默认安全的展示类型，例如 `ProjectWorkflowPresentation` 与 `WorkflowNodePosition`；只允许有限 `f32`，拒绝 NaN/Infinity。
- [ ] 不修改 `ProjectWorkflowDraft`、`WorkflowNodeDraft` 和 `workflow_content_digest` 的语义。
- [ ] 增加独立 Store 接口：加载/保存某个项目工作流的展示元数据。
- [ ] 保存展示元数据前确认 workflow key 存在，并拒绝未知节点 key；不存在的位置允许缺省。
- [ ] 位置 map 使用稳定序列化顺序，避免同一布局产生随机 JSON diff。
- [ ] 布局保存不得修改 `content_digest`；也不要创建 Pipeline Revision。
- [ ] `load_project_workflow` 继续只因语义图损坏而失败。`presentation_json` 损坏由独立加载接口报错，不能让工作流本体不可读取、不可运行。
- [ ] v6 → v7 迁移后，已有行得到空展示元数据；`task_workflows` 和已有 Project Workflow 内容不变。

### UI 接入

- [ ] `WorkflowCanvas::load_workflow` 先加载语义图，再加载展示元数据。
- [ ] 缺少坐标时用 Task 1 的稳定自动排列补齐内存场景，但不因“查看”而写库。
- [ ] 增加独立的 `presentation_save_error` 或等价状态；它只显示“布局未保存”，不复用会阻止运行的 `save_error`。
- [ ] 节点拖动/自动排列只调用展示保存接口；语义编辑继续走 `save_current`。
- [ ] 删除节点后清理对应位置；新节点必须同时获得位置。
- [ ] 复制项目工作流时显式复制展示元数据。
- [ ] 从全局模板创建时使用空展示元数据并自动排列。
- [ ] 另存为全局模板仍不携带坐标。

### 测试先行

- [ ] v6 数据库升级到 v7 后语义图和内容摘要不变。
- [ ] 展示元数据创建、覆盖、读取、删除随 Project Workflow 生命周期正确工作。
- [ ] 只移动节点前后 `content_digest` 完全相同。
- [ ] 损坏 `presentation_json` 时语义工作流仍能读取和运行，UI 回退自动排列并显示警告。
- [ ] 未知节点位置和非有限坐标被拒绝。
- [ ] 复制项目工作流复制位置；模板复制不携带位置。
- [ ] 布局保存失败不阻止 `request_run`，语义保存失败仍阻止。

运行：

```powershell
cargo test -p mf-agent --test project_workflows -- --nocapture
cargo test -p mf-agent --test fresh_schema -- --nocapture
cargo test -p mf workflow_canvas_tests -- --nocapture
```

---

## Task 3：用 GPUI 原生画布渲染节点、网格和有向边

**Blocked by：** Task 1、Task 2。

**交付：** 当前工作流不再显示“层 1 / 层 2”的行列表，而是在二维画布里显示节点卡片、端口、曲线和箭头。

**文件：**

- 新建：`crates/mf/src/workflow_graph_view.rs`
- 修改：`crates/mf/src/main.rs`
- 修改：`crates/mf/src/workflow_canvas.rs`
- 修改：`crates/mf/src/theme.rs`（仅在确实缺少语义色时）
- 新建：`crates/mf/src/workflow_graph_view_tests.rs`

### 模块职责

- `workflow_canvas.rs` 保留页面壳、工作流 CRUD、保存编排、Agent 库和检查器。
- `workflow_graph_view.rs` 只负责把纯场景投影成 GPUI 元素，并把输入事件翻译回纯状态意图。
- 禁止在 `workflow_graph_view.rs` 重写环检测、自动排列或坐标持久化规则。

### 渲染要求

- [ ] 根画布使用 `relative + overflow_hidden`；记录其真实 bounds 供 world/screen 转换。
- [ ] 背景网格和连线通过 `gpui::canvas` 绘制，节点卡片通过绝对定位 GPUI 元素渲染在连线上方。
- [ ] 边使用 `PathBuilder::stroke` 画三次或二次贝塞尔曲线，并在下游输入端绘制箭头。
- [ ] 曲线控制点对正向和反向摆放都稳定，不能因 `dx < 0` 产生折返 NaN 或极端尖角。
- [ ] 节点卡片至少显示：标题、Agent 可见名、稳定 step key。
- [ ] 左侧输入端口、右侧输出端口有明确视觉状态；hover/选中/可连接/非法目标使用现有主题语义色。
- [ ] 选中节点使用 accent 边框，选中边使用更粗的 accent 曲线。
- [ ] 空画布显示明确提示：“拖动或点击左侧 Agent 添加节点”。
- [ ] 右侧检查器继续由节点选择驱动，不另建第二套详情状态。
- [ ] 移除旧的“层 N”和 `← dep1,dep2` 文本列表，但检查器的依赖列表保留。

### 测试

- [ ] 场景投影生成正确数量的节点、边、输入端和输出端。
- [ ] `deps` 中的每项都投影为 `dep → node`，方向不反。
- [ ] 缺失坐标的节点使用稳定自动排列。
- [ ] 选择节点和选择边不会互相残留。
- [ ] 画布缩放后节点矩形、端口和边使用同一坐标变换。

运行：

```powershell
cargo test -p mf workflow_graph_view_tests -- --nocapture
cargo test -p mf workflow_editor_tests -- --nocapture
```

完成本 Task 后，应用中应已经能看到真正的二维 DAG，但还不要求所有拖拽交互完成。

---

## Task 4：实现从 Agent 库拖入、节点移动、平移和缩放

**Blocked by：** Task 3。

**交付：** 用户能在画布中自由放置和移动节点，并能浏览大于视口的图。

**文件：**

- 修改：`crates/mf/src/workflow_graph_view.rs`
- 修改：`crates/mf/src/workflow_canvas.rs`
- 修改：`crates/mf/src/workflow_editor.rs`
- 修改：`crates/mf/src/workflow_editor_tests.rs`
- 修改：`crates/mf/src/workflow_canvas_tests.rs`
- 修改：`crates/mf/src/workflow_graph_view_tests.rs`

### Agent 库拖入

- [ ] 库条目使用一个明确的拖拽 payload，至少包含 Agent reference 和可见名。
- [ ] 使用 GPUI `on_drag` 提供轻量拖拽预览；画布用 `on_drag_move` 计算 world drop point，用 `on_drop` 提交。
- [ ] 放下时创建唯一 step key、选中新节点、打开检查器，并分别保存 DAG 与位置。
- [ ] 拖拽取消或放在画布外不得创建节点。
- [ ] 保留单击库条目的快捷方式：在当前视口中心附近寻找不与现有节点重叠的位置后添加。

### 移动与浏览

- [ ] 节点卡片 mouse-down 进入 `MovingNode`，move 只更新内存预览，mouse-up/mouse-up-out 才吸附并保存一次。
- [ ] 节点拖动不能触发背景平移，也不能误启动端口连线。
- [ ] 拖动空白背景平移；Space + 左键拖拽也可平移。
- [ ] Ctrl + 滚轮以指针为中心缩放；普通滚轮保留给容器或不处理，避免破坏页面滚动习惯。
- [ ] 工具栏增加“适配视图”和“自动排列”。适配视图不写库；自动排列写展示元数据。
- [ ] 切换工作流后 viewport 重置并执行一次 fit，不把 A 工作流视口带到 B。
- [ ] pointer 释放、Esc、项目切换和工作流切换都会清空临时拖动态。
- [ ] 每帧只 `cx.notify()`，禁止每帧 Store 写入或同步磁盘 I/O。

### 测试

- [ ] 从库拖入在指定 world point 创建节点，点击添加使用无重叠 fallback point。
- [ ] 节点 move 过程中不保存，释放只保存一次最终坐标。
- [ ] 取消拖拽不改变节点、位置或 dirty 状态。
- [ ] 背景平移和节点拖动互斥。
- [ ] 缩放锚点、clamp、fit 和工作流切换重置行为正确。
- [ ] 布局保存失败显示警告，但语义保存成功的工作流仍可运行。

运行：

```powershell
cargo test -p mf workflow_editor_tests -- --nocapture
cargo test -p mf workflow_graph_view_tests -- --nocapture
cargo test -p mf workflow_canvas_tests -- --nocapture
```

---

## Task 5：实现端口拉线、边选择和断开依赖

**Blocked by：** Task 4。

**交付：** 用户能通过直接操作图来创建和删除依赖，不再必须在检查器里逐项点击。

**文件：**

- 修改：`crates/mf/src/workflow_graph_view.rs`
- 修改：`crates/mf/src/workflow_editor.rs`
- 修改：`crates/mf/src/workflow_canvas.rs`
- 修改：`crates/mf/src/workflow_editor_tests.rs`
- 修改：`crates/mf/src/workflow_canvas_tests.rs`
- 修改：`crates/mf/src/workflow_graph_view_tests.rs`

### 连线交互

- [ ] 只允许从输出端开始连线；按下时进入 `Connecting { from, pointer }`。
- [ ] 指针移动时画一条虚线/半透明预览边，起点固定在上游输出端，终点跟随指针。
- [ ] hover 输入端时先调用纯编辑器校验；合法目标高亮 accent，非法目标高亮 danger 并给出原因。
- [ ] 放到合法输入端才提交 `connect(from, to)`；放到空白、输出端或非法输入端只取消预览。
- [ ] 连接成功后选中新边、自动保存语义图并刷新检查器依赖投影。
- [ ] self、cycle、unknown、duplicate 都不能残留临时边或污染 `deps`。
- [ ] Esc、工作流切换、项目切换、删除源节点都取消连接态。

### 边选择与删除

- [ ] 使用 Task 1 的贝塞尔命中测试在点击画布时选择最近且阈值内的边。
- [ ] 节点命中优先于边，端口命中优先于节点卡片。
- [ ] 选中边后 Delete/Backspace 调用 `disconnect(from, to)` 并保存语义图。
- [ ] 删除节点仍一次性清理所有相连边；不对每条边分别写数据库。
- [ ] 检查器现有“依赖/断开”按钮继续调用同一纯编辑器接口，不保留另一套实现。
- [ ] 文本编辑、重命名模式下 Delete/Backspace 只编辑文本，不能误删图对象。

### 测试

- [ ] `A output → B input` 精确产生 `B.deps == [A]`。
- [ ] 预览连接不改变语义；成功 release 才产生一次 Semantic change。
- [ ] 连接到自己、成环目标、空白处均不改变语义。
- [ ] 多条相近边时选择距离最近者，超出阈值不选边。
- [ ] Delete 删除选中边，Delete 删除选中节点，文本编辑 Backspace 三种路径互不串扰。
- [ ] 检查器断边与画布断边得到完全相同状态。

运行：

```powershell
cargo test -p mf workflow_editor_tests -- --nocapture
cargo test -p mf workflow_graph_view_tests -- --nocapture
cargo test -p mf workflow_canvas_tests -- --nocapture
```

---

## Task 6：收敛画布工具栏、错误反馈和可用性

**Blocked by：** Task 5。

**交付：** 视觉编辑器在正常窗口、窄窗口和错误场景下都可理解、可恢复，不把所有动作挤在截图中的单行工具栏。

**文件：**

- 修改：`crates/mf/src/workflow_canvas.rs`
- 修改：`crates/mf/src/workflow_graph_view.rs`
- 修改：`crates/mf/src/theme.rs`（仅必要增量）
- 修改：`crates/mf/src/workflow_canvas_tests.rs`
- 修改：`crates/mf/src/workflow_graph_view_tests.rs`

### 要求

- [ ] 顶部工作流 CRUD 与运行入口保留，但画布专属的“适配视图 / 自动排列 / 缩放百分比”放入画布内工具条。
- [ ] 工作流选择、名称和主要动作在窄宽度下允许换行或分组，不能覆盖画布。
- [ ] 节点卡片、端口、曲线、网格使用现有 Theme，不硬编码只在浅色主题可见的颜色。
- [ ] 连接错误显示在靠近画布的状态区，同时保留现有全局 status；下一次有效操作可清除旧错误。
- [ ] 语义保存状态和布局保存状态分开显示：`已自动保存`、`工作流未保存（运行被阻止）`、`布局未保存（仍可运行）`。
- [ ] Delete/Backspace、Esc、Ctrl+0（适配视图）有稳定键盘行为；不得抢占当前文本输入。
- [ ] 端口可点击区域至少 14×14 屏幕像素；节点选中和边选中都有无需依赖颜色的形状/粗细反馈。
- [ ] 空图、只有一个节点、十个以上节点、分叉和汇合都有可读布局。
- [ ] 不在本 Task 加 minimap、多选框、撤销栈或右键菜单。

### 测试

- [ ] 保存状态三分法正确，Run 只受语义保存错误影响。
- [ ] 文本输入模式下快捷键不会误操作画布。
- [ ] 空图/单节点/多节点的 fit view 都产生有限且 clamp 后的 viewport。
- [ ] 窄布局仍保留 Workflows/Runs 顶层导航和运行按钮。

运行：

```powershell
cargo test -p mf workflow_canvas_tests -- --nocapture
cargo test -p mf workflow_graph_view_tests -- --nocapture
```

---

## Task 7：端到端闭环、回归和视觉验收

**Blocked by：** Task 6。

**交付：** 证明视觉编排只改变编辑体验，没有破坏工作流创建、保存、运行和恢复语义。

**文件：**

- 修改：`crates/mf/src/agent_workflow_e2e_tests.rs`
- 修改：`crates/mf/src/workflow_canvas_tests.rs`
- 仅在行为文档确实需要时修改：`README.md`

### 自动化主场景

- [ ] 打开项目但不创建 Task。
- [ ] 新建项目工作流。
- [ ] 点击添加一个默认 CLI 节点。
- [ ] 从库拖入两个保存实例节点到指定坐标。
- [ ] 移动其中一个节点并保存。
- [ ] 拉线形成 `A → B`、`A → C`、`B → D`、`C → D` 的分叉/汇合 DAG。
- [ ] 尝试 `D → A`，确认环被拒绝且 Store 不变。
- [ ] 选择一条边删除，再重新连接。
- [ ] 切换到另一个工作流再切回，节点坐标与依赖保持。
- [ ] 重建 AppCtx/重启存储后，坐标与依赖仍恢复。
- [ ] 节点移动前后 `content_digest` 相同；依赖变化后摘要改变。
- [ ] 点击运行后仍自动创建 Task、冻结 Revision 并启动；Revision 中没有任何画布坐标字段。
- [ ] 默认 CLI 的外部配置不被修改。

### 回归场景

- [ ] 项目工作流 CRUD、从模板创建、另存模板和复制仍通过。
- [ ] 复制项目工作流复制坐标；模板往返不携带坐标但能自动排列。
- [ ] 语义 save error 继续阻止 Run；presentation error 不阻止。
- [ ] 环、自依赖、未知依赖在编辑器和 Compiler 两层都被拒绝。
- [ ] 原有 Task Composer、Ad-hoc Session、Run Monitor、Needs You、插件 pin 和 Settlement 测试不变。
- [ ] 没有新增后台 UI 轮询，没有在 mouse-move 中写数据库。

### 手工视觉验收

用一个至少五节点的图验证：一条串行链、一个并行分叉、一个汇合节点。

- [ ] 100%、50%、200% 三档缩放下，端口与节点对齐，箭头落在输入端。
- [ ] 快速拖动节点时没有明显残影、跳动或数据库卡顿。
- [ ] 把下游节点拖到上游左侧后，曲线仍连续可读。
- [ ] 连线合法/非法目标反馈明显，取消后不留幽灵线。
- [ ] 选中节点、选中边、打开检查器、删除对象的状态一致。
- [ ] 侧栏布局 B 和上下布局 A 都能使用画布。
- [ ] 窄窗口不会把主要按钮挤出可点击区域。
- [ ] 浅色/深色主题下网格、节点、边和错误反馈都可见。

如运行环境支持截图，请把最终五节点场景截图路径写入交付报告；不要把临时截图加入 git diff。

### 最终验证命令

按顺序执行并记录每条命令的退出码与摘要：

```powershell
cargo fmt --all -- --check
cargo test -p mf-agent --test project_workflows -- --nocapture
cargo test -p mf-agent --test fresh_schema -- --nocapture
cargo test -p mf workflow_editor_tests -- --nocapture
cargo test -p mf workflow_graph_view_tests -- --nocapture
cargo test -p mf workflow_canvas_tests -- --nocapture
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
- `git status --short` 只包含本计划文件、计划明确涉及的实现文件，以及开始前已有的 `.superpowers/`、`.zcode/`。
- 没有数据库、截图、日志、临时工作树或构建产物进入 diff。

## 明确不在本计划范围

- 条件分支、循环、动态节点、decision gate 和新的 Workflow Compiler 语法。
- 多选、框选、批量移动、复制粘贴、撤销/重做、minimap、搜索节点。
- 节点分组、泳道、注释贴纸、可折叠子图。
- 边标签、多端口类型系统、数据类型检查。
- 在 Run Monitor 中复用可编辑坐标；运行 DAG 继续保持只读语义。
- 让全局 Workflow Template 保存画布坐标。
- 引入 React Flow、WebView、JS 图编辑器或新的第三方布局依赖。
- 修改 Task、Step、Agent Run、Settlement、Execution Lease、Secret 或插件权限语义。

## 交付给 Codex Review 的报告格式

GLM-5.3 完成后不要 commit。报告必须包含：

1. 实际基线 HEAD，以及是否仍为 `39e5fb057ebc72fb7a7fb8d464326f85eb42a768`。
2. 已完成的 Task 编号；未完成、拆分或调整项及原因。
3. 新增/修改文件列表。
4. schema v7 的迁移说明，以及如何证明布局不进入 `content_digest`/Revision。
5. 每条定向与最终验证命令的退出码和摘要。
6. 手工视觉验收结果与截图路径（如有）。
7. 已知风险，尤其是 GPUI 鼠标捕获、缩放命中、曲线选择、布局保存错误隔离。
8. 明确写出：`请 Codex review 当前未提交 diff，固定点为 39e5fb057ebc72fb7a7fb8d464326f85eb42a768。`

Codex review 将分两轴进行：

- **Standards：** 仓库约束、模块深度、重复图规则、GPUI 事件与 Store 写入时机、代码异味。
- **Spec：** 本计划完成定义、每个 Task 验收项、现有工作流运行语义是否保持。
