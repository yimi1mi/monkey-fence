# MonkeyFence 需求分档（基于 Zed / Orca 实测体验）

> 本文档独立撰写，未参考 docs/product 与 docs/prototypes。
> 体验来源：Zed debug 构建（D:/workspace/zed，Windows DirectX 后端）实际操作；
> Orca v1.4.190 桌面 App + `orca` CLI 实际走通完整工作流（新建 worktree → spawn codex →
> 状态流转 → 依赖 DAG 解锁 → 归档清理）。

---

## 0. 产品定位一句话

**MonkeyFence = Zed 的编辑器手感 + Orca 的 agent 工作流，两者在同一个原生 GPUI 窗口里互相长在对方身上**：
编辑器的每一次选中、每一个 diff 都能直接喂给 agent；agent 的每一次产出、每一个状态都直接落在编辑器和卡片上，不需要"切到另一个工具"。

三个参照系各自的本质：

| 参照 | 本质 | MonkeyFence 应取什么 |
|---|---|---|
| Zed | 键盘优先的原生速度（GPUI 渲染、一切皆 action + keybinding、面板皆 dock） | 编辑器操作体验的**全集**：多光标、分屏、面板键位体系、命令面板 |
| Orca | 以 worktree 为隔离单元的 agent 车间（卡片墙 + 终端矩阵 + Run/Task/Dispatch 编排） | agent 工作流的**骨架**：卡片状态流转、依赖 DAG、ask/reply、tui-idle 感知 |
| MonkeyFence 自己 | 原生融合 + P4 版控（前两者都没有的） | 编辑器 ↔ agent 的**双向直连**，以及游戏行业必需的 P4 |

---

## 1. 分档原则

- **P0 — 不可用基座**：缺了它，"编辑器"或"agent 工作流"任一半身不遂。先做。
- **P1 — 核心体验**：做到它，才配说"像 Zed / 像 Orca"。是日常效率的主干。
- **P2 — 融合差异化**：Zed 和 Orca 各自都没有、只有融合才可能有的能力。这是 MonkeyFence 的护城河。
- **P3 — 远期**：方向正确但当前投入产出低。

每条需求标注：`[来源]`（实测依据）、`[现状]`（已有/部分/缺失，对应现有代码）、`[落点]`（建议实现 crate）、`[验收]`（可执行的验收标准）。

---

## 2. P0 — 不可用基座

### E1. 编辑器多光标与选区操作 `[来源:Zed 实测 Ctrl+D 连选]`
- Ctrl+D 选中下一个匹配词（实测：连按 3 次得到 3 个同步选区，选区为蓝紫高亮）；
  Ctrl+Shift+D 跳过；Ctrl+L 选中行；Alt+Click / Ctrl+Alt+↑↓ 追加光标。
- 状态栏显示光标数（"3 cursors"）与选区范围。
- `[现状]` editor.rs 单光标，无多选区概念。`[落点]` mf-core/buffer.rs 选区模型改为 Vec<Selection>，editor.rs 渲染与按键处理跟上。
- `[验收]` 在 workspace.rs 上 Ctrl+F 找 "cx" 后连按 Ctrl+D 3 次，出现 3 个同步选区，输入字符三处同时修改，Ctrl+Z 一次全撤销。

### E2. 面板键位体系（一键直达一切面板）`[来源:Zed keymap 实测: ctrl-shift-e/b/g/d/f、ctrl-shift-/ agent]`
- Zed 的面板键位是一套体系而非散件：`Ctrl+Shift+E` 项目、`Ctrl+Shift+B` 大纲、`Ctrl+Shift+G` Git、`Ctrl+Shift+F` 搜索、`Ctrl+Shift+/`(问号位) Agent。
- `[现状]` 只有 Ctrl+B 单一开合左面板。`[落点]` main.rs 的 bind_keys 统一注册；workspace.rs 的 LeftPanel 扩展为 PanelId + dock 方向（左/底/右）。
- `[验收]` 任一界面状态下，按对应键位焦点直达该面板（已有面板则聚焦而非重复创建，再按一次收起）。

### E3. 项目全局搜索 `[来源:Zed 实测: 底部 dock、按文件分组、每行匹配片段、大小写/全字/正则开关]`
- Ctrl+Shift+F 打开底部 dock：搜索框 + Aa/字/.* 三个开关 + 结果按文件分组，行内匹配片段高亮，显示每文件命中数与总数。
- 点击结果打开文件并跳转高亮；Shift+Click / Ctrl+Click 多个结果以**多缓冲区**打开（Zed 招牌，可并入 P1）。
- `[现状]` 无（quick_open 只做文件名）。`[落点]` 新 search.rs（后台 ripgrep 风格扫描可先用 ignore+ropey，nucleo-matcher 做容错）。
- `[验收]` 搜 "LeftPanel" 命中 workspace.rs 等文件并分组显示，正则开关生效，点击跳转行号正确。

### E4. Agent 会话面板（右 dock 化）`[来源:Zed 实测: agent panel 右侧 dock + 底部输入框 + 模式切换]`
- 现有 agent_panel 从左侧 tab 迁到**右侧 dock**（与 Zed 一致）：会话流（消息气泡）+ 底部输入框（Enter 发送 / Shift+Enter 换行）+ 模式切换（Write/Ask）+ 附加上下文按钮。
- `[现状]` agent_panel.rs 有输入框与事件处理，但在左面板 tab 里，无消息流渲染。`[落点]` mf/src/agent_panel.rs 重构为右 dock。
- `[验收]` Ctrl+Shift+/ 开合；输入一条指令，消息流显示用户消息、agent 回复、工具调用折叠条目。

### E5. Worktree 卡片墙（agent 车间的主视图）`[来源:Orca 实测 worktree ps: displayName/status/comment/unread/agents[]/lastActivityAt]`
- 左侧新面板（或活动栏专用 tab）：按仓库分组的**工作树卡片列表**。每卡片字段（实测 Orca 模型）：
  - 名称 + 分支名；`workspaceStatus` 徽章（todo → in-progress → in-review → completed 四色）；
  - 一行 `comment` 检查点文字（"42/42 tests; review clean" 这类）；
  - 活跃 agent 数（"codex ●运行中 / claude ✓完成"）；unread 圆点；最近活动时间。
- 卡片操作：点击进入该 worktree 的终端矩阵；右键/按钮改状态、写 comment、归档。
- `[现状]` mf-agent 有任务 DAG 但 UI 只有左 tab 简单输入。`[落点]` 新 mf/src/board.rs + mf-vcs/git.rs 增加 worktree 管理（create/list/clean）。
- `[验收]` 从卡片"新建工作树"生成独立 checkout + 新分支，卡片出现且状态 todo；agent 启动后自动变 in-progress 并带 unread。

### E6. Agent 终端矩阵（PTY + tui-idle 感知）`[来源:Orca 实测: terminal create --command codex → wait tui-idle → send → 卡片状态自动变化]`
- 每个工作树内可开多个终端 tab，可水平/垂直分割（已有 ConPTY 分屏基础）；终端可运行任意 CLI agent（codex/claude CLI）。
- **PTY 内容感知**：解析 VT 流提取 agent 状态（空闲提示 "Ask ... to do anything" / 运行中 "Working" / 完成 "Worked for X"），驱动卡片状态——这是 Orca 不依赖 agent API 就能感知任意 agent 的关键。
- `[现状]` term.rs 有完整 VT 模拟 + console.rs 有 ConPTY 分屏，但无多 tab、无状态感知。`[落点]` mf/src/term.rs 加状态嗅探器（正则匹配空闲/工作横幅）。
- `[验收]` 在测试 worktree 里 spawn codex，终端 idle 时卡片显示"空闲"，发送 prompt 后变"运行中"，agent 回复后变"完成"。

### E7. 任务 DAG 引擎的 UI 化 `[来源:Orca 实测: task-create with deps; A completed → B pending→ready 自动解锁]`
- mf-agent 引擎已有 DAG/邮箱/派发/熔断（--agent-smoke 可验），但用户看不见。P0 要求最小可视化：
  Run（目标）→ Task 卡片列表：标题、状态（pending/ready/dispatched/completed/failed/blocked）、依赖（"依赖 #A"）、指派的终端。
- 手动流转：右键任务 → 派发到选中终端 / 标记完成 / 标记失败；依赖满足自动 ready。
- `[现状]` 引擎有，UI 无。`[落点]` mf/src/board.rs 任务区或 agent_panel 扩展。
- `[验收]` 建两个任务 A、B（B 依赖 A），A 完成后 B 自动从 pending 变 ready；派发 B 到终端后显示 dispatched。

---

## 3. P1 — 核心体验（"像 Zed / 像 Orca"的部分）

### E8. 编辑器分屏与多 pane `[来源:Zed 实测: Ctrl+\ 左右分屏，圆角胶囊标签，非激活标签半透明]`
- Ctrl+\ 垂直 / Ctrl+K Ctrl+V（或自定义）水平分屏；每个 pane 独立标签组；胶囊形标签；Ctrl+W 关闭；Ctrl+1..9 切 pane。
- `[现状]` 无。`[落点]` workspace.rs 的 Tab 模型升级为 pane 树（可先线性支持 1×N）。

### E9. 命令面板全量化 `[来源:Zed 实测: 顶部居中弹窗、模糊匹配、每项右侧显示快捷键]`
- 现有 Ctrl+Shift+P 升级：所有 action 可检索；条目右侧渲染 keybinding 提示；最近使用置顶；支持 `>` 前缀切命令 / 无前缀查文件（合并 Ctrl+P 入口）。
- `[验收]` 输入 "terminal" 能看到"打开终端（Ctrl+`）"并回车执行。

### E10. 文件树 git/P4 状态标记 `[来源:Zed 文件树实测 + Orca 卡片/徽章体系]`
- 文件树节点显示修改（M 橙）/新增（A 绿）/删除（D 红）标记，P4 检出（红色）标记；活动栏 VCS 图标角标显示待提交数。
- `[现状]` vcs_panel 有变更列表，但文件树无标记联动。`[落点]` mf/file_tree.rs 订阅 mf-vcs 的变更集。

### E11. 大纲面板（符号导航）`[来源:Zed ctrl-shift-b]`
- 当前文件的 struct/enum/fn/impl 树，点击跳转。Rust 可用 tree-sitter（Zed 同源依赖现成）；MVP 可先用正则提取 `^\s*(pub )?(fn|struct|enum|impl)`。
- `[落点]` mf-core/highlight.rs 旁新增 symbols.rs。

### E12. 状态栏信息密度 `[来源:Zed 实测: 底部状态栏含分支/位置/语言/缩进/编码]`
- 左：当前 worktree + 分支 + P4 CL/变更数；中：任务/agent 活动指示（"2 agents working"）；右：行:列、选区数、语言、Tab/空格、编码。
- `[现状]` 无状态栏或极简。`[落点]` workspace.rs render_status_bar。

### E13. 编辑器内 diff 审阅流（keep/reject）`[来源:Zed keymap 实测: agent diff 上下文 alt-y Keep / alt-z Reject / shift-alt-y KeepAll / shift-alt-z RejectAll]`
- agent 改动后进入 diff 审阅态：逐块（hunk）Keep / Reject 快捷键，全保留/全撤销；编辑器行内 gutter 标记 added/changed。
- `[现状]` diff_view.rs 有对比视图，无 hunk 级操作。`[落点]` mf-vcs/diff.rs hunk 模型 + diff_view.rs 操作按钮与键位。
- `[验收]` agent 修改文件后按 alt-y/alt-z 逐块采纳/拒绝，文件按块合并。

### E14. 未读与活动流 `[来源:Orca 实测: unread=true, lastActivityAt, "Worked for 23m 22s"]`
- 卡片 unread 圆点 + 活动栏角标；点击后清除；agent 完成时系统通知（OS 级）。
- 完成的 agent 显示耗时与最后一条消息摘要（Orca 卡片实测字段 lastAssistantMessage）。
- `[落点]` mf-agent/engine.rs 事件总线 → UI 订阅。

### E15. 内嵌终端作为一等 dock `[来源:Zed ctrl+` 实测: PowerShell、面板 tab]`
- 终端从"控制台分屏"升级为底部 dock 的 tab 之一（与搜索/诊断并列），支持多 tab、重命名、快速 kill；Ctrl+` 全局开合。
- `[现状]` console.rs 独立面板。`[落点]` 并入 workspace dock 系统。

### E16. 设置与主题闭环 `[来源:Zed settings/主题选择实测; MonkeyFence settings.rs 已有雏形]`
- settings.rs 已有：补齐键位自定义（改 keybinding 即改 UI 提示）、字体/字号/主题（亮/暗/高对比）持久化；agent provider 配置（API key、模型选择）。
- `[验收]` 改键位后命令面板提示同步更新。

---

## 4. P2 — 融合差异化（MonkeyFence 的护城河）

### F1. 选中即上下文（编辑器 → agent 直连）`[来源:Zed ctrl-shift-. AddSelectionToThread 的深化]`
- 编辑器任意选中 / 光标符号 / 当前文件 / 项目搜索结果 → 一键附加为 agent 上下文（消息框显示 chip）。
- 反向：agent 回复中的 `文件:行号` 自动变为可点击链接，点击打开并高亮（实测 Orca lastAssistantMessage 里全是这种链接，但目前只在终端里不可点——MonkeyFence 原生渲染就能点）。
- `[落点]` agent_panel.rs 上下文管理 + mf-core/buffer 定位 API。

### F2. P4 原生融入工作流（差异点：Orca 只有 git）
- worktree 卡片增加 P4 模式：卡片=变更列表（CL），状态流转对应 P4 submit/review；agent 产出自动"加入 CL"；审阅态 = p4 diff 的 hunk keep/reject。
- changelist 与 agent 任务的关联（哪个 agent 改的哪些文件，card 上可追溯）。`[落点]` mf-vcs/p4.rs + board.rs。

### F3. 工作流流转的显性看板（todo → in-progress → in-review → completed 泳道）`[来源:Orca workspace-status 实测 + 其卡片缺泳道视图]`
- 卡片墙可切换两种视图：列表（Orca 式）与**四列泳道**（看板式），拖拽改状态。
- 状态变更自动产生事件：in-review 时自动请求 reviewer agent（见 F4）。
- `[落点]` board.rs 视图切换 + mf-agent/engine.rs 状态事件。

### F4. Reviewer agent 闭环 `[来源:Orca 实测 lastAssistantMessage 中大量 review 报告的启发；实测中 reviewer 与 coder 是两个独立 worktree 会话]`
- 卡片进入 in-review → 一键 spawn 只读 reviewer agent（不改文件，只产出报告），报告显示在卡片；人决定 keep/reject（联动 E13）。
- 熔断可视化：3 次失败的 task 显示红色"熔断"徽章 + 一键重试/人工接管。`[落点]` mf-agent 引擎已有熔断，补 reviewer 预设角色。

### F5. ask/reply 人在环（阻塞问答）`[来源:Orca orchestration ask/reply 实测模型: question 消息阻塞等待，超时不丢，resume 续问]`
- agent 需要决策时（不是聊天），弹**阻塞式问句卡**：卡片/编辑器顶部横条 + 选项按钮（Orca ask --options 模式），选择后 agent 才继续。
- 全部问句进入 inbox 视图，可离线补答；超时问句不丢失（按消息 ID resume，而不是重复提问）。
- `[落点]` mf-agent 消息类型增加 question/decision_gate（引擎类型已有雏形则 UI 化）。

### F6. 多 agent 并行车间（一屏监控）`[来源:Orca 实测: 单 worktree 6 个终端、10 个 worktree]`
- "车间视图"：网格化缩略终端（每个 agent 终端缩略流），点击放大；全局心跳/超时警示；一键向多终端广播同一条指令。
- `[落点]` console.rs 的 ConPTY 矩阵 + board.rs。

### F7. 终端内容语义化（PTY → 结构化事件）`[来源:Orca 实测: preview 字段即 VT 流解析产物，能读出 "Worked for 23m"、"Ask Codex to do anything"]`
- E6 的深化：VT 嗅探不止判定状态，还提取：最后回复摘要（喂卡片）、错误行（喂诊断面板）、文件路径（变为可点链接）。
- 这是"任意 CLI agent 无 API 集成"的通用方案，MonkeyFence 已有完整 VT 模拟器（term.rs）即天然地基。

---

## 5. P3 — 远期

- **H1 多 worktree 同屏编辑**：一个窗口同时编辑主仓 + 多个 worktree 文件（tab 带工作树徽章），diff 跨工作树。
- **H2 编排编排器**：可视化 DAG 画布（连线、并行组、decision gate 分支），保存为可复用工作流模板（对应 Orca automations 的 cron/RRULE 定时）。
- **H3 团队协同**：卡片 comment 变成团队留言板；artifact 分享（Orca artifacts 的公开链接）。
- **H4 内嵌浏览器/预览**（Orca browser/emulator 的对应物）：agent 跑 web 项目时内嵌预览页与点击回放。
- **H5 WSL/远程执行主机**（Orca executionHost 模型）。

---

## 6. 分档依据速查（为什么这样切）

1. **P0 的判据**：砍掉任何一条，"编辑器"或"agent 车间"直接不可用——多光标/面板键位/搜索是编辑器底座；卡片墙/终端矩阵/DAG 可视化是车间底座。E5/E6/E7 三条互为犄角：卡片是视图、终端是执行、DAG 是状态。
2. **P1 的判据**：单独可用但彼此独立，属于"体验补全"。做完 P0+P1，MonkeyFence ≈ Zed 编辑体验 + Orca 工作流的并集。
3. **P2 的判据**：每一条都需要编辑器和 agent 双方同时存在才有意义（选中即上下文、P4×agent、泳道×reviewer、语义化终端）。这是并集之外的交集价值。
4. 现状盘点：P0 中 E1/E2/E3 缺失，E4/E5/E6/E7 半成品（引擎有、UI 弱）；P1 大多缺失；P2 依赖 P0/P1。**建议实现顺序：E2(键位) → E1(多光标) → E4(agent 右 dock) → E6(终端矩阵+tui感知) → E5(卡片墙) → E7(DAG UI) → E3(搜索) → 其余 P1 → P2 按 F1/F5/F3 优先**（F1 最便宜、感知最强）。

## 7. 本次实测证据清单（关键截取）

- Zed：信任对话框(Unrecognized Project/Restricted Mode)；Ctrl+P 打开 workspace.rs；命令面板显示命令+键位；Ctrl+` 内嵌 PowerShell 跑通 git status；Ctrl+Shift+F 搜 "LeftPanel" 按文件分组；Ctrl+D×3 得 3 选区；Ctrl+\ 分屏胶囊标签；ctrl-shift-/ 右侧 Agent 面板；keymap 中 agent diff 键位（alt-y/alt-z 系）。
- Orca：`worktree ps` 卡片字段全集（workspaceStatus/comment/unread/agents[].state/lastAssistantMessage/toolName）；`terminal create --command codex` + `wait tui-idle` + `send` 全链路；卡片随 agent 活动自动 in-progress+unread，agent 完成 state=done；`orchestration run-create/task-create --deps` 建 A/B 依赖，A completed 后 B 自动 pending→ready（工作流流转实测）；`worktree set --comment/--workspace-status` 检查点写入。
