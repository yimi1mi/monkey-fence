# MonkeyFence 设计文档

日期:2026-08-26
状态:已定稿(v1,初版范围)

## 1. 目标与非目标

MonkeyFence 是一个用 Rust 开发的 AI 代码编辑器:

- **像 orca 一样**:内置 agent 任务派发与流转(协调者/工作者、任务 DAG、持久化邮箱、人机决策门)。
- **像 zed 一样**:原生 GPU 渲染(GPUI + DirectX),打开项目、浏览、编辑代码必须快。
- **像 SourceTree 一样**:内置 Perforce(P4)版控操作界面(变更列表、diff、提交、搁置、历史),同时提供 Git 基础操作。
- 版控:MonkeyFence 项目自身用 git 管理。

**非目标(初版)**:实时协作、扩展插件市场、远程开发、终端内嵌(后续版本)。skills 体系为自有设计,不照抄 orca 的内容与协议。

## 2. 技术栈决策

| 决策 | 选择 | 理由 |
|---|---|---|
| UI 框架 | GPUI(path 依赖本地 zed checkout `D:/workspace/zed`)+ gpui_platform | Windows DirectX 后端一等公民;zed 同源,天然满足"zed 级速度";避免 fork zed 的 workspace |
| 文本缓冲 | ropey(而非 zed 的 rope/text) | 独立、简单、Helix 验证过;zed 的 text 锚点体系为协作而设,初版不需要 |
| 语法高亮 | tree-sitter + tree-sitter-highlight(常用语言子集) | 增量解析、行业标准 |
| 模糊匹配 | nucleo-matcher | zed 同款引擎(独立 crate),快速打开的核心 |
| 项目扫描 | ignore crate(gitignore 感知)+ notify 文件监听 | zed worktree 同思路:后台并行扫描、流式可用 |
| 编排持久化 | rusqlite(内置 SQLite) | orca 验证的关键设计:任务/派发/邮箱全部落库,崩溃可恢复 |
| LLM 客户端 | ureq(SSE 流式) | 同步 HTTP + 线程池,避免引入 tokio(GPUI 有自己的执行器) |
| P4 集成 | 调用本机 `p4` CLI,`-ztag` 机器可读输出解析 | 无需链接 P4API C++ 库;p4 2024.1 已验证本机可用 |
| Git 集成 | git2 crate | 状态/暂存/提交/日志/diff,无子进程解析 |

## 3. 架构

```
┌─────────────────────────── crates/mf (GUI 应用) ───────────────────────────┐
│  GPUI 外壳:标题栏/活动栏/标签页/面板布局/命令面板/快速打开/状态栏              │
│  ┌ EditorView ┐ ┌ FileTree ┐ ┌ AgentPanel ┐ ┌ P4Panel/GitPanel ┐ ┌ Palette ┐│
└──────┬─────────────┬─────────────┬──────────────┬───────────────────────────┘
       │mf-core       │(mf 内)      │mf-agent      │mf-vcs
┌──────▼─────┐  ┌────▼─────┐  ┌────▼───────────┐  ┌▼──────────────────┐
│ Buffer(rope│  │ 项目扫描/ │  │ 编排引擎        │  │ P4: p4 -ztag 解析 │
│ undo/编码) │  │ 文件监听/ │  │ 任务DAG/邮箱/   │  │ 变更列表/提交/搁置 │
│ Highlighter│  │ 快速打开  │  │ 派发/断路器     │  │ Git: git2 状态/日志│
└────────────┘  └──────────┘  │ LLM 提供方      │  └───────────────────┘
                              │ (OpenAI 兼容/   │
┌─────────────┐               │  Anthropic)    │  ┌───────────────────┐
│ mf-skills   │◄──────────────┤ 工具循环        │  │ SQLite (orchest.) │
└─────────────┘  注入系统提示  └────────────────┘  └───────────────────┘
```

依赖方向:`mf` → `mf-core`/`mf-agent`/`mf-vcs`/`mf-skills`;后四者互不依赖(mf-agent 仅依赖 mf-skills 的类型)。

## 4. 模块设计

### 4.1 mf-core:Buffer 与编辑器核心

- `Buffer`:ropey Rope + 文件路径 + 修改标记 + undo/redo(编辑事务栈,事务 = 一组连续插入/删除)+ UTF-8。API:`text()`、`len_bytes()`、`line(row)`、`edit(range, text)`、`save()`。
- `Highlighter`:tree-sitter 解析 + 高亮查询 → 每行 `Vec<(range, HighlightTag)>`;编辑后防抖 120ms 重跑(小文件全量,>1MB 仅视口)。初版支持:Rust/TOML/JSON/Markdown/Python/JS/TS/C/C++/YAML。
- `EditorView`(在 mf 内,GPUI Entity):光标(偏移量)、选区(锚点+头)、滚动位置;单宽字体逐行渲染,tab 展开 4 空格,行号槽;按键:可打印字符插入、方向键/Home/End/PgUp/PgDn、Enter/Tab/Backspace/Delete、Ctrl+Z/Y/S/C/V/X/A/F、Alt+↑↓ 移动行。

### 4.2 工作区(在 mf 内)

- `Workspace`:打开的文件夹(项目根)、标签页列表(multimap:活跃标签 + 历史)、面板开合状态。
- 项目扫描:`ignore` 并行走目录(gitignore/隐藏目录裁剪),结果流式进入文件索引;`notify` 监听变更增删。打开项目后立即可用(不等扫描完)。
- 快速打开 Ctrl+P:nucleo 对全路径模糊匹配,后台线程,按键即查。Ctrl+Shift+P 命令面板复用同一组件。
- 文件树:懒展开,目录/文件图标(文本图标初版),右键/菜单:新建/重命名/删除。
- 最近项目:JSON 存于 `~/.monkeyfence/`。

### 4.3 mf-agent:任务编排(参考 orca 机制,自行实现)

**持久化模型(SQLite,单写者线程)**:

- `runs`:一次目标(objective)+ 归属邮箱命名空间。
- `tasks`:`id, run_id, parent_id, spec, status, deps[]`;状态机 `pending → ready → dispatched → completed | failed | blocked`。创建时计算初始状态(依赖未全完成 = pending);完成任务时在同一事务内提升依赖者(ready)。
- `dispatches`:每次派发一条(认领者、状态 pending/dispatched/completed/failed、failure_count、时间戳);`INSERT…SELECT WHERE status='ready' AND NOT EXISTS(活跃派发)` 原子认领;失败计数跨重试累计,超阈值熔断。
- `messages`:持久邮箱(from/to/subject/body/type/线程);类型:status/dispatch/worker_done/escalation/question/decision_gate。
- `questions`:人机决策问题(阻塞等待用户答复,UI 呈现)。

**执行模型(与 orca 的差异:agent 是内置 LLM 而非外部 CLI TUI)**:

- `planner`:接收 objective,产出任务 DAG(通过 create_task 工具)。
- `worker`:认领任务,带工具循环执行。工具:`fs_read/fs_write/fs_patch/fs_list/run_cmd/spawn_subtask(生成子任务并等待)/send_message/ask_human/complete_task/report_failure`。
- `reviewer`(可选关卡):对完成任务做审查,通过/打回。
- 调度循环(每 2s):处理邮箱 → 检查过期派发 → 派发 ready 任务(并发上限)→ 收敛检查。agent 之间只通过邮箱与任务库通信,不共享内存。

**LLM 提供方**:`~/.monkeyfence/config.toml` 配置 `[providers.x]`(base_url/api_key/model/协议 openai|anthropic),角色映射 planner/worker/reviewer → 提供方。SSE 流式,工具调用走原生 function-calling。无 key 时提供方为 `mock`(演示任务流转,不调网络)。

### 4.4 mf-skills:自有技能体系

- 格式:目录 = 一个技能,`skill.toml`(id/title/triggers[]/tags[]/tools_allow) + `INSTRUCTIONS.md`(注入正文)。
- 位置:`~/.monkeyfence/skills/`(全局)与 `<project>/.monkeyfence/skills/`(项目级,优先)。
- 机制:任务 spec 与 trigger 匹配 → INSTRUCTIONS.md 注入 worker 系统提示;tools_allow 限制该技能下可用的工具。命令面板可手动调用。**与 orca 不同:声明式提示增强 + 工具白名单,无 CLI 回调协议,内容全部自写。**
- 内置技能(自写):`rust-tdd`(红绿重构)、`p4-safe-submit`(提交前检查)、`read-before-edit`(先读后写纪律)。

### 4.5 mf-vcs:P4 面板(SourceTree 风格)+ Git

**P4(基于 `p4 -ztag` 输出解析)**:

- 左侧栏:待提交变更列表(default + 编号 CL,展开显示文件与动作 add/edit/delete)+ 提交历史(最近 N 条,`p4 changes` / `p4 describe -s`)。
- 文件操作:双击 → diff 视图(`p4 diff -du` vs depot / `p4 print` 基准内容,统一 diff 渲染,+/- 着色);勾选文件参与操作。
- 工具栏:刷新 / 提交(弹窗:描述 + 文件勾选 → `p4 submit -c`)/ 还原(`p4 revert`)/ 搁置与恢复(`p4 shelve` `unshelve`)/ 同步(`p4 sync`)/ 历史(`p4 filelog`)。
- 状态栏:当前 client/stream(`p4 info`)。
- 自动刷新:`p4 opened` 轮询(5s 防抖,面板可见时)。

**Git(git2)**:状态列表(M/A/D/?),暂存/取消暂存/提交/日志/diff,与 P4 面板同布局风格,按项目根是否有 .git 自动选择显示。

### 4.6 UI 布局(GPUI)

```
┌──────────────────────────────────────────────────────┐
│ 标题栏(项目名 + 菜单按钮)                              │
├──┬───────────────────────────────────────────┬───────┤
│活│ 标签栏 [file.rs ×] [lib.rs ×]              │ Agent │
│动│───────────────────────────────────────────│ 面板  │
│栏│                                           │(任务  │
│  │           编辑器(多标签,分栏 v2)          │ 看板/ │
│文│                                           │ 对话/ │
│件│  ── 或 ── diff 视图 / Agent 任务详情       │ 提问) │
│树│                                           │       │
├──┴───────────────────────────────────────────┴───────┤
│ P4/Git 面板(可折叠:变更列表 | diff 预览)              │
├──────────────────────────────────────────────────────┤
│ 状态栏:分支/client │ 光标位置 │ Agent 活动 │ 编码      │
└──────────────────────────────────────────────────────┘
```

活动栏切换左面板:资源管理器 / 版控(P4/Git)/ Agent / 设置。命令面板与快速打开为居中悬浮层。

## 5. 关键数据流

1. **打开文件**:文件树/快速打开点击 → 读文件(后台线程)→ Buffer → 标签页 → EditorView 渲染;语法高亮后台解析,完成后增量着色。
2. **agent 任务流转**:用户在 Agent 面板输入目标 → planner 产出任务 → 调度器派发给 worker(线程池,每 worker 一个 LLM 会话)→ 工具执行改文件(fs_write 后编辑器自动重载,弹 diff 通知)→ worker_done → 依赖提升 → 全部完成汇总。UI 通过 crossbeam 通道收事件,GPUI 主线程消费刷新。
3. **P4 提交**:面板勾选文件 + 填描述 → `p4 change -o` 模板 → `p4 -i submit` → 刷新状态。
4. **崩溃恢复**:编排状态全在 SQLite,重启后 runs/tasks/dispatches 原样恢复,中断的 dispatched 任务标记失败可重派。

## 6. 错误处理与测试

- 所有 IO/LLM/p4 失败 → Result<anyhow> 上浮;UI 用 toast/面板横幅呈现,不 panic。
- 熔断:任务连续失败 ≥3 次标记 blocked,需人工在面板重置。
- 单元测试:mf-core 的 Buffer 编辑/undo 往返;mf-agent 的任务状态机提升/认领竞态(SQLite 事务);mf-vcs 的 ztag 解析(样本固件);skills 加载。
- 集成验证:本仓库自身作为测试项目(打开 MonkeyFence 开发 MonkeyFence + mock provider 跑完整任务流转)。

## 7. 里程碑

1. M1:GPUI 外壳 + Buffer/编辑器 + 打开项目/文件树/快速打开 ✅冒烟已启动
2. M2:mf-agent 完整(mock provider 驱动)→ Agent 面板 UI
3. M3:P4 面板完整 + Git 基础
4. M4:skills + 真实 LLM 提供方接入 + 文档

## 8. 开放问题(已决策)

- IME 中文输入:GPUI Windows 后端走 Win32 消息,初版验证可用性,不行则列为已知问题。
- 多光标/分栏:v2,不阻塞初版。
