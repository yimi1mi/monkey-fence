# MonkeyFence 🐒

**多项目 Agent 工作台 · 插件化智能体 · 可编辑 DAG 流水线 · 原生编辑 · 人工审阅**

MonkeyFence 是一个面向 Windows 研发团队的原生 AI Agent 工作台:同时打开多个项目,每个项目内创建与版本控制完全解耦的任务(ADR 0001),通过插件贡献的本地 CLI Agent、API Agent 与 mock Agent 混合执行可编辑的 DAG 流水线(ADR 0002)。

```
┌──────────────────────────────────────────────────────────────────┐
│ 活动栏 │ 任务(按项目分组) │ 编辑器 / Agents 看板 / Pipeline │ 版控 │
└──────────────────────────────────────────────────────────────────┘
```

## 快速开始

```bash
cargo run [项目路径]        # 默认二进制是 monkeyfence(default-members)
cargo run --release -- [项目路径]
```

- `Ctrl+Shift+O` 打开文件夹(**可重复打开多个项目**,互不干扰)
- `Ctrl+Shift+W` 任务侧边栏(按项目分组:新建 / 选择 / 归档)
- `Ctrl+Shift+/` Agent 工作区(`Agents` 看板 / `Pipeline` 视图)
- `Ctrl+,` 设置(智能体 / 插件 / Provider / 引擎 / 编辑器)
- 活动栏顶部 `⋮` 打开“所有操作”:添加项目、快速打开、任务、版控、
  Agent、Pipeline、搜索、终端、设置及常用编辑操作均可鼠标触达;快捷键只做加速
- 任务侧栏同时提供 `+ 添加项目` 与 `+ 新建任务`,无需记忆快捷键

### 无 GUI 自测(v2 冒烟:验证流水线状态机端到端)

```bash
cargo run -- --agent-smoke .
```

冒烟覆盖:CLI Agent PATH 检测表 → 手工 DAG → 自动派发 → mock 结构化结算 → 下游解锁 → 失败进入「需要你」→ 能力令牌断言(错误拒绝 / 幂等 / 冲突拒绝)→ 人工跳过 → 收敛,并在 `.mf-agent/` 留下产物与历史。

## 领域模型(见 `CONTEXT.md` 与 `docs/adr/`)

| 概念 | 说明 |
|---|---|
| **Project** | 同时打开的一个目录;独立的任务数据库 / 调度器 / 会话注册表 |
| **Task** | 项目内一级目标(不绑定 Git/P4/worktree/分支/变更集) |
| **Pipeline Revision** | Task 的不可变 DAG 版本;编辑产生新 Revision |
| **Step** | DAG 节点:工作说明 + 依赖 + Agent 指派 + 会话策略 |
| **Agent Profile** | 插件贡献的可配置执行器(pty / http / plugin-worker) |
| **Agent Session** | 后台 Session Registry 拥有的 CLI/API 会话,可复用 |
| **Agent Run** | Step 的一次执行尝试,持有一次性能力令牌 |
| **Settlement** | 显式结算(`mfctl step complete/fail` 或结构化 Runtime),唯一成功依据 |

状态机:
- Task:`draft → ready → running → needs-you ⇄ running → succeeded/failed/cancelled`(另有 `archived`)
- Step:`pending → ready → running →(awaiting-outcome|needs-input)→ succeeded/failed/blocked/skipped/cancelled`
- 调度规则:依赖全部成功或显式跳过后 Step 就绪并**自动派发**;失败只阻塞后代,独立分支继续;`done` / `tui-idle` **不能**自动结算;默认全局并发 4、每项目 2(设置可改);运行中修改 DAG 必须先暂停,且只允许修改尚未启动的 Step。

## Agent 工作区

### Agents 视图(Orca 风格四列看板)

- 四列:**需要你**(失败/阻塞/等待输入/未结算)· **工作中** · **已完成**(成功未确认)· **空闲**(可复用会话)
- 默认汇总**所有打开项目**,支持按项目循环切换与文本过滤(Agent/任务/指令/回复)
- 卡片:Agent 图标名称 · 会话名 · 最近用户指令 · 最后回复(PTY 为终端尾部)· 项目与任务 · 状态 · 未读标记
- 点击卡片打开近全屏终端(键盘直通,重新挂载恢复当前屏幕)或 API transcript
- 「需要你」卡片 / 详情:判定成功 · 判定失败 · 继续发送提示;已完成可确认;空闲可隐藏(历史保留)或终止

### Pipeline 视图(左到右拓扑列)

- 每列一个依赖层级;节点显示标题 · Agent · 状态 · 尝试次数 · 依赖
- 选中节点编辑器:标题 · 工作说明 · Agent Profile · session policy(fresh / reuse:key)· 前置步骤
- 工具栏:从模板创建 · AI 生成(Planner 草案,**不得绕过用户确认**)· 添加 Step · 校验 · 保存修改 · 确认并运行 · 暂停 / 继续 · 取消
- 失败节点:重试 · 跳过(必须人工确认)· 替换 Agent(产生新 Revision)

## 插件系统(`mf-plugins`)

统一扩展缝隙:Agent、流水线模板、技能、工具都由插件贡献;内置内容以**合成插件**暴露,与第三方走同一权限模型。

插件根清单 `monkeyfence-plugin.toml` 示例:

```toml
[manifest]
version = 1
publisher = "zhipu"
id = "demo-agent"
name = "Demo Agent"
version_str = "0.1.0"
min_app_version = "0.1.0"
description = "演示插件"
homepage = "https://example.com"

[capabilities]
fs_read = false
fs_write = false
net = true
spawn = false
hooks = false

# 可选后台 worker(独立进程 + NDJSON 协议)
# [worker]
# command = "worker.exe"

[[agents]]
id = "demo"
name = "Demo"
runtime = "pty"                 # pty | http | plugin-worker
command = "demo-cli"
args = []
permission_args = ["--yes"]

[[pipelines]]
id = "default"
name = "默认流水线"
file = "pipelines/default.json"  # PipelineDraft JSON

[[skills]]
path = "skills/demo"
```

- 安装来源:`bundled` / 本地目录 / Git URL / marketplace(首版前三种)
- 安装流程:复制或 clone 到 staging → 校验清单与路径(拒绝 `..`、绝对路径、符号链接逃逸)→ 内容 SHA-256 → 原子发布到 `~/.monkeyfence/plugins/` → 锁文件 `plugins.lock.json` 记录来源/版本/commit/哈希/授权指纹
- **新插件默认禁用**;用户审查权限后启用;worker、钩子、能力或说明内容变化改变指纹,需要**重新授权**;插件代码授权前不运行;禁用/未授权插件的 worker 不得启动
- ⚠ 权限只约束 MonkeyFence 宿主接口;worker 进程与 CLI 始终以当前 Windows 用户权限运行

### 内置智能体(设置 → 智能体)

- **CLI**(只检测 PATH,不复制凭据/配置目录):Codex · Claude · OpenCode · Cursor · Kimi · Gemini CLI · GitHub Copilot · Qwen Code · iFlow CLI · Aider · Amp;「可安装」区常驻,用户自行选择安装——codex/claude/opencode/gemini/copilot/qwen/iflow(官方 npm 包,已逐一核实仓库归属)与 aider(官方 PyPI)支持一键安装,cursor/kimi/amp 为官方独立安装器(跳官方页,npm 同名包非官方不自动执行)
- **API**:OpenAI 兼容 · Anthropic · mock(来自 `~/.monkeyfence/config.toml` 的 providers)
- **空白终端**;默认智能体(Auto / 指定);权限模式 Yolo / Manual;状态钩子总开关(命名空间内写入 + 备份 + 可逆移除);自动生成标签标题;Agent 工作时保持唤醒
- Agent 详情可配置:Command · Arguments · Environment · Permission arguments · Hook 安装状态 · 插件来源/版本

## mfctl 显式结算

Agent Run 启动时获得一次性能力令牌(环境变量 `MF_RUN_TOKEN` / `MF_PIPE` 自动注入 Agent shell),MonkeyFence 通过本地命名管道 `\\.\pipe\monkeyfence-mfctl-<pid>` 接收:

```bash
mfctl step complete --summary "一句话总结"   # 相同结算重复提交幂等
mfctl step fail --reason "失败原因"
mfctl agent-state <working|waiting|blocked|done>
mfctl pipeline propose --file draft.json     # Planner 提案,须用户确认
```

冲突结算被拒绝;`done` 无显式结算 → Step 进入 `awaiting-outcome`、看板进入「需要你」;用户可手工判定成功/失败或继续发送提示。

## 多项目与数据

- 每项目 `<project>/.mf-agent/orchestration.db`(带 `schema_migrations` 正式迁移)
- 迁移:旧 `runs→Task`、`tasks→Step`、`dispatches→Agent Run`;旧表与消息/问题历史保留为只读;`work-items.json` 兼容导入一次(忽略 `vcs_ref`,原文件保留)
- 崩溃恢复:重开时未结算 Agent Run → `interrupted`,对应 Task → `needs-you`
- 统一项目上下文:当前项目/任务由 `project_context.rs` 的原子 activation seam 唯一决定(`ProjectId` 为规范化绝对路径);项目、任务、编辑器标签、文件树、VCS、搜索与终端 cwd 属于同一个原子激活的项目上下文;点击项目标题、任务、Agent 卡片或跨项目文件都会先原子切换到所属项目
- 编辑器标签与 ConsoleDock 按项目分桶:A→B→A 后标签顺序、活动标签与终端内容原样恢复;每项目终端首次创建时 cwd 即该项目根
- 前台只显示一个项目的文件树/编辑上下文;其他项目的任务与 Agent 后台继续运行;关闭含活动 Agent Run 的项目必须先确认停止;关闭当前项目回退到最近激活的剩余项目
- 新建任务使用显式 Composer(Project 必选、Title/Goal 必填),不存在"第一个项目"隐式归属
- TaskSidebar 与 Agents 看板消费同一份带 revision 的统一项目总览快照(`project_overview.rs`):每个 Orchestrator 的 UI 事件由 Event Hub 持续消费(drain 线程),UI 变慢不会反压调度
- 会话持久化到 `~/.monkeyfence/session.json`(原子写):打开项目、前台项目、每项目最近选中 Task 与干净编辑器文件;重启自动恢复(Diff/未保存 Buffer/终端 PTY 不持久化),旧格式仍可读取

## P4 / Git 面板(独立)

自动检测:Perforce client root 下 → P4 模式;否则 Git。变更集、Diff 审阅(Alt+Y/Z hunk 级)、提交/搁置/同步/历史照旧,与 Task 生命周期完全解耦。

## 架构

```
crates/
  mf-core     Buffer(ropey + 事务式 undo)、tree-sitter 高亮
  mf          GPUI 应用:多项目 Workspace、任务侧边栏、Agents 看板、
              Pipeline 视图、Agent 终端/transcript、设置(智能体/插件)、
              SessionRegistry(PTY/HTTP/PluginWorker)、mfctl 管道服务
  mf-agent    v2 编排:Store 迁移 + Task/Revision/Step/Session/Run、
              DAG 校验、Orchestrator 调度器、显式结算、崩溃恢复;
              提供方层(OpenAI 兼容 / Anthropic / mock)
  mf-plugins  插件系统:清单、安装/锁文件/权限指纹、内置 Agent、
              状态钩子写入器、NDJSON worker
  mf-vcs      p4 CLI 封装、git2、统一 diff 解析
  mf-skills   技能加载/匹配/注入
  mfctl       显式结算 / 状态上报 / 流水线提案(命名管道客户端)
```

## 测试

```bash
cargo test --workspace   # 100+ 项:迁移/回滚/数据保留、两项目隔离、DAG 校验、
                         # 并发/串行化/失败阻塞、暂停编辑规则、结算令牌、
                         # interrupted 恢复、插件逃逸/禁用/重授权、钩子不破坏用户配置、
                         # PATH 检测、mfctl 管道往返、work-items 导入、
                         # 两项目 E2E、终端模拟器、配置往返
```

## 已知限制(首版)

- 仅 Windows 本地运行;WSL/SSH/远程主机未支持(设置页明确标注)
- 第三方插件不能注入任意 GPUI 界面;plugin-worker Runtime 已定义协议但未接入调度
- AI 生成草案当前由 mock Planner 演示;真实 provider 的结构化规划待接入
- Agent 命令/参数覆盖为会话级(未持久化);agent 修改后的打开文件在重新聚焦标签时重载
- 编辑器多光标/分栏/软换行、终端鼠标选择未实现(终端 dock 支持任意嵌套分屏)

## 许可

Apache-2.0
