# MonkeyFence 界面设计（基于 Zed / Orca 实测）

> 配套文档：[requirements.md](requirements.md)（需求分档）。本文只谈"长什么样、怎么操作"。
> 设计原则从实测提炼：**Zed 的骨架（dock + 键位 + 命令面板）× Orca 的血肉（卡片 + 终端 + 流转）**。

---

## 1. 总布局（三 dock 骨架）

Zed 实测确认的骨架：窄活动栏 + 左右底三个 dock + 中央编辑器。MonkeyFence 完全沿用，但左 dock 是 Orca 式卡片墙：

```
┌──┬─────────────────┬──────────────────────────────────────┬───────────────┐
│  │ 项目/版控 ▽      │  ┌─[workspace.rs ×]─┐ ┌─[engine.rs ×]─┐             │
│文 │                 │  │                  │ │               │   Agent 会话  │
│件 │ ▾ MonkeyFence   │  │  编辑器 pane 1    │ │  编辑器 pane 2 │             │
│   │   ▾ crates/     │  │                  │ │               │  ┌─────────┐ │
│版 │ ▾ v2d0-project  │  │                  │ │               │  │ 消息流   │ │
│控 │   ▾ mf-ux-test  │  │                  │ │               │  │ (气泡)   │ │
│   │     ●in-prog    │  └──────────────────┘ └───────────────┘  │         │ │
│车 │     ●review ◀︎  │  ┌────────────────────────────────────┐  └─────────┘ │
│间 │     ○todo       │  │ TERMINAL │ SEARCH │ TASKS │ (tabs) │  [chip][chip]│
│   │                 │  │ $ codex ▌                          │  ┌─────────┐ │
│a │                 │  │ › Ask Codex to do anything         │  │ 输入框    │ │
│g │                 │  └────────────────────────────────────┘  │ [Write▾]➤ │ │
│e │                 │                                          └─────────┘ │
│nt│                 │                                          ○ codex 运行中│
├──┴─────────────────┴──────────────────────────────────────┴───────────────┤
│ ⎇ mf-ux-test │ ◆ 2 agents working │        │ 128:4 │ 2 cursors │ Rust │ UTF-8│
└──────────────────────────────────────────────────────────────────────────┘
```

| 区域 | 来源依据 | 说明 |
|---|---|---|
| 活动栏（最左窄条） | Zed | 图标项：文件树 / 版控(P4·Git) / **车间**(卡片墙，MonkeyFence 新增) / 搜索 / agent。VCS/车间图标带角标数 |
| 左 dock | Zed 面板体系 + Orca 卡片 | 三个 tab，Ctrl+Shift+E / G / 新增 Ctrl+Shift+W 切换 |
| 中央编辑器 | Zed | 胶囊标签、多 pane 分屏、diff 审阅态 |
| 底 dock | Zed | TERMINAL / SEARCH / TASKS 三个面板 tab |
| 右 dock | Zed Agent 面板实测 | Agent 会话（Ctrl+Shift+/ 开合），底部常驻当前 worktree 的 agent 状态条 |
| 状态栏 | Zed | 三段式，中间段是 MonkeyFence 独有的 agent 活动指示 |

---

## 2. 车间面板（卡片墙）—— 核心新组件

实测 Orca 卡片字段直接映射为视觉层级（重要度从上到下）：

```
┌────────────────────────────────────┐
│ ● mf-ux-test            [in-review]│  ← unread 圆点 + 名称 + 状态徽章
│ ⎇ mf-ux-test · 2h ago              │  ← 分支 + 最近活动
│ UX测试通过:42/42 tests,review clean │  ← comment 检查点（斜体灰）
│ ✦codex ●运行中  ✦claude ✓23m      │  ← agent 芯片:类型+状态/耗时
│ ▸ 3 terminals   ⚑ 无阻塞           │  ← 终端数 + 熔断/问句警示
└────────────────────────────────────┘
```

- **状态徽章四色**（Orca 实测 workspace-status）：`todo`（灰）→ `in-progress`（蓝）→ `in-review`（黄）→ `completed`（绿）；失败/熔断红。
- **agent 芯片**：图标（codex/claude/自定义）+ 状态点（● 运行中 / ✓ 完成 / ? 等待问句 / ✕ 失败）+ 耗时。
- **分组**：按仓库分组（Orca 实测 repo 分组），组头 = 仓库名 + 折叠；父子工作树缩进显示。
- **卡片交互**：
  - 单击 = 进入该工作树的终端矩阵（底 dock 切到该 worktree 上下文）；
  - 状态徽章点击 = 弹四态菜单（或拖到泳道视图，见 §6）；
  - 尾部 `⋯` 菜单：写 comment（检查点）/ 归档 / 删除 / 在资源管理器显示 / 复制路径。
- **卡片来源**：`worktree ps` 同构数据，UI 与 CLI 同源（Orca 模式：CLI 能做的 UI 都能做）。
- 新建入口：面板头 `+ 新建工作树` → 弹窗（名称 / 基于 main / 要启动的 agent：无·codex·claude·自定义命令）。

## 3. 底 dock 三面板

### 3.1 TERMINAL（终端矩阵）
- 顶部终端 tab 栏：`[codex ▊] [claude] [+]`，tab 名取 agent 类型（Orca 实测 title="✳ Claude Code" 模式）；活动 tab 有呼吸点。
- `+` 下拉：新终端 / 分割（右/下）/ 运行 agent…（预设 codex/claude/自定义命令）。
- 终端右上角动作条：重命名 / 分割 / kill / 在卡片上标记状态。
- VT 状态嗅探条（P0-E6）：终端顶部细条显示解析出的 agent 状态（空闲 / Working… / Worked for 23m），与卡片状态联动。
- 快捷键：Ctrl+` 开合面板；Ctrl+Shift+` 新终端；Alt+←→ 切 tab。

### 3.2 SEARCH（项目搜索）
Zed 实测结构照搬：`[搜索框____] [Aa][字][.*] [替换▽]`，下方按文件分组结果（文件名+命中数，行片段高亮）。Ctrl+Shift+F 直达。结果支持"以多缓冲区打开"（P1）。

### 3.3 TASKS（任务 DAG）
最小可视化（P0-E7）：
```
RUN: 优化工厂NPC性能                    [▶新建任务]
┌───────────────────────┬───────────────────────┐
│ ✓ #A 移除IndexOf扫描   │ ● #B 缓存重建点位      │
│   (codex · 12m)       │   依赖 #A ✓ → ready    │
│                       │   [派发到终端▾]         │
└───────────────────────┴───────────────────────┘
  ✕ #C 3次失败 熔断 [重试][人工接管]
```
- 任务卡：状态色同卡片徽章体系；依赖链显示 `依赖 #A ✓/○`；dispatched 的任务显示目标终端名。
- 熔断任务（实测 Orca：3 次连续失败自动 failed）红色 + 两个出口按钮。
- 拖拽任务到终端 = 派发（对应 `dispatch --inject` 语义：注入 spec + 生命周期前导）。

## 4. 右 dock：Agent 会话面板

Zed 实测结构 + MonkeyFence 融合件：

```
┌─ Agent ──────────────────[⇆ worktree ▾]─┐
│  ┌ 消息流 ────────────────────────────┐  │
│  │ ▌user:  优化这个循环 (chip: Factory…│  │
│  │ ▌agent: 我来读取文件…               │  │
│  │   ▸ 工具 Read FactoryNpc… (折叠)    │  │
│  │   ✓ 修改 2 文件 [查看diff]           │  │
│  │ ▌agent: 完成,42/42 通过             │  │
│  └────────────────────────────────────┘  │
│  [+] 附加上下文: [📄file] [≣selection]   │
│  ┌ 输入框____________________ ┐ [Write▾][➤]│
│  └────────────────────────────┘          │
│  ● codex 运行中 12m · 此 worktree        │
└──────────────────────────────────────────┘
```

- 消息气泡：user 右对齐 / agent 左；工具调用折叠条目（图标+名称+参数摘要，点击展开）；文件改动条目带 [查看diff] 直达 diff 审阅态。
- **上下文 chip**（F1）：编辑器选区/当前文件/搜索结果一键挂载，显示为可删除 chip。
- 模式切换：Write（可改文件）/ Ask（只读问答，对应 reviewer 预设）。
- 底部状态条：当前会话绑定的 worktree + agent 运行态。
- **阻塞问句横条**（F5，人在环）：agent ask 时输入框被替换为问句卡：
  ```
  ❓ #B 任务是否需要兼容旧 schema?  [兼容] [不兼容] [自定义…]
  ```
  选择即 reply；未答问句沉淀到 inbox（车间的 ⚑ 入口）。

## 5. 编辑器细节（Zed 实测复刻清单）

| 元素 | 设计 | 实测依据 |
|---|---|---|
| 标签 | 圆角胶囊，激活实底、非激活半透明；等宽图标 + 关闭 ×；中键关闭 | Ctrl+\ 分屏实测 |
| 多光标 | 主光标实心、副光标空心；选区蓝紫高亮；状态栏显示 "N cursors" | Ctrl+D×3 实测 |
| 当前行 | 细背景高亮 + 左侧行号区亮色 | 实测 |
| 缩进线 | 折层级 1px 垂线，激活层级提亮 | Zed 默认 |
| diff 审阅态 | agent 改动的文件进入特殊 tab（标签加 ◆）：hunk 块左侧行号条显示 ▽/×；底部悬浮条 `[alt-y 保留] [alt-z 拒绝] [全部保留] [全部拒绝] [完成]` | Zed agent diff 键位体系实测 |
| 面包屑 | 编辑器顶部细条：crate › module › symbol（P1 大纲联动） | Zed 风格 |
| 信任对话框 | 首次打开文件夹弹 "不受信任的项目"（Restricted Mode 说明 + [保持受限]/[信任并继续]），受限模式禁用 agent 与任务执行 | **Zed 打开 MonkeyFence 时实测遇到** |

## 6. 泳道视图（P2-F3，卡片墙第二形态）

车间面板顶部视图切换 `[列表] [泳道]`：

```
│ 待办 todo │ 进行中 in-progress │ 审阅 in-review │ 完成 completed │
│ ┌──────┐  │ ┌──────┐           │ ┌──────┐       │ ┌──────┐      │
│ │card  │  │ │card ●│ 运行中2   │ │card ✦│reviewer│ │card ✓│      │
│ └──────┘  │ └──────┘           │ └──────┘       │ └──────┘      │
```

- 拖拽跨列 = 改 workspaceStatus（写回引擎，CLI 同步）。
- 拖入 in-review 列自动触发 F4：弹"选择 reviewer agent（只读）"。
- 列头显示计数与总耗时。

## 7. 键位总表（P0 建立、P1 补全）

| 键 | 动作 | 来源 |
|---|---|---|
| Ctrl+P | 快速打开文件（模糊） | 已有 |
| Ctrl+Shift+P | 命令面板 | 已有 |
| Ctrl+B | 左 dock 开合 | 已有，改"开合当前 tab" |
| **Ctrl+Shift+E / G / W** | 项目 / 版控 / **车间** | Zed 体系 + 扩展 |
| **Ctrl+Shift+F** | 项目搜索（底 dock） | Zed |
| **Ctrl+Shift+/** | Agent 会话（右 dock） | Zed |
| **Ctrl+` / Ctrl+Shift+`** | 终端面板 / 新终端 | Zed + WindowsTerminal 习惯 |
| **Ctrl+\\ / Ctrl+K Ctrl+\\** | 垂直 / 水平分屏 | Zed |
| **Ctrl+D / Ctrl+L** | 加选下一个匹配 / 选中行 | Zed |
| **Ctrl+1..9** | 切 pane | Zed |
| **Alt+Y / Alt+Z** | diff 审阅：保留 / 拒绝当前 hunk（Shift+ 全部） | Zed agent diff |
| **Ctrl+Shift+.** | 把编辑器选区附加为 agent 上下文 | Zed AddSelectionToThread |
| **Ctrl+Shift+M** | 任务面板（底 dock TASKS） | 新增 |
| F2 | 卡片重命名 / 任务重命名 | 通用 |

原则：所有 UI 动作必须注册为 GPUI action + keybinding（Zed 实测"命令面板显示键位"依赖此结构），命令面板因此自动收录全部动作。

## 8. 状态机（两套核心流转）

### 8.1 工作树卡片（人驱动 + 事件自动）
```
todo ──agent启动──▶ in-progress ──agent全部done──▶(建议)in-review ──人确认──▶ completed
  ▲                    │3次失败                        │reviewer报告          │
  └──重开───────────────▶failed(红)                     └─拒绝─▶ in-progress(返工)
```
- 自动迁移用虚事件（E6 tui 嗅探 / E7 worker_done），人永远可手动改（Orca 实测：`--workspace-status` 手动、agent 活动自动改 in-progress+unread）。

### 8.2 任务（引擎已有，实测 Orca 依赖解锁语义）
```
pending ──依赖满足──▶ ready ──派发──▶ dispatched ──worker_done──▶ completed
   │                    │              │3次失败✕3                  ▲
   └────────────────────┴──────────────▶ failed(熔断) ──重试───────┘
                     ask/question 挂起 ↕ blocked(等待人答复)
```

## 9. 视觉规范（沿用现有 theme.rs，补三件事）

1. **状态色环**：todo=灰#737373 / in-progress=蓝 / in-review=黄 / completed=绿 / failed=红（Orca 实测 badgeColor 灰为默认态）。同一套色用于：卡片徽章、任务卡、泳道列头、状态栏 agent 指示——一处定义处处用。
2. **密度**：卡片/任务卡行高 5 行封顶，comment 超长省略（Orca 卡片同款紧凑度，一屏 8-10 卡）。
3. **unread/unread-free**：圆点用主题强调色；进入卡片（单击）即清除。

## 10. 开发落地提示（与代码的对应）

- dock 系统：workspace.rs 的 `LeftPanel` 枚举升级为 `{PanelId, Dock: Left|Bottom|Right}` 三维；活动栏按 dock 方向分组图标。
- 卡片墙：board.rs 消费 mf-agent 的 DB（rusqlite 已有）+ mf-vcs 的 worktree 枚举；数据结构直接抄本文字段（= Orca `worktree ps` JSON 同构，未来可做 Orca 兼容层）。
- tui 嗅探：term.rs 的 VT 流上挂正则窗口（最近 N 行）匹配空闲/工作横幅；结果作为引擎事件广播。
- diff 审阅：mf-vcs/diff.rs 增加 hunk 级 apply/revert API（similar 库已具备分块能力）。
- 键位：main.rs 的 bind_keys 宏已有，按 §7 表补全；命令面板条目自动带出键位（Zed 同构）。
