# MonkeyFence 🐒

**AI 编辑器 · agent 任务流转 · zed 级速度 · P4 版控**

MonkeyFence 是一个用 Rust 编写的 AI 代码编辑器:

- **像 orca 一样**——内置 agent 编排引擎:任务 DAG、持久化邮箱、原子派发、失败熔断、人机问答。
- **像 zed 一样**——基于 GPUI(zed 的 UI 框架,Windows DirectX 后端)原生渲染,后台并行扫描项目,即时快速打开。
- **像 SourceTree 一样**——内置 Perforce(P4)面板:变更列表、diff、提交/还原/搁置/同步/历史;同时提供 Git 基础操作。

```
┌────────────────────────────────────────────────────────┐
│ 活动栏 │ 文件树 / 版控(P4·Git) / Agent │  编辑器 + 标签页  │
└────────────────────────────────────────────────────────┘
```

## 快速开始

```bash
cargo run -p mf --release -- [项目路径]
```

无参数启动后 `Ctrl+Shift+O` 打开文件夹。

### 无 GUI 自测(验证 agent 任务流转)

```bash
cargo run -p mf -- --agent-smoke .
```

输出规划 → 派发 → 工具调用 → 收敛的完整事件流,并在 `.mf-agent/` 下生成任务产出。

## 快捷键

| 键 | 功能 |
|---|---|
| `Ctrl+Shift+O` | 打开文件夹 |
| `Ctrl+P` | 快速打开文件(模糊匹配) |
| `Ctrl+Shift+P` | 命令面板 |
| `Ctrl+B` | 切换左侧面板 |
| `Ctrl+`` | 切换控制台分屏(底部终端 dock) |
| `Ctrl+,` | 打开设置 |
| `Ctrl+W` / `Ctrl+Tab` | 关闭 / 切换标签页 |
| `Ctrl+S` / `Ctrl+Z` / `Ctrl+Y` | 保存 / 撤销 / 重做 |
| `Ctrl+A` / `Ctrl+C` / `Ctrl+V` / `Ctrl+X` | 全选 / 复制 / 粘贴 / 剪切 |
| `Ctrl+←/→` `Ctrl+Backspace` | 按词移动 / 删词 |
| `Alt+↑/↓` `Ctrl+D` | 移动行 / 复制行 |
| `Tab` / `Shift+Tab` | 缩进 / 反缩进(支持选区) |

## 控制台分屏(终端 dock)

`Ctrl+`` 或状态栏「⌨ 终端」打开底部终端 dock,每个窗格是一个真实 shell(ConPTY,默认 `cmd.exe`),**内置完整 VT 终端模拟器,可运行彩色输出与全屏 TUI 应用**(PowerShell / claude-code 类 CLI 等):

- 工具栏:**新窗格 / 右分屏 / 下分屏 / 关闭窗格**,窗格可任意嵌套拆分(树形布局,单子分叉自动折叠);底部 orca 式状态条显示 `N panes` 与快捷键
- 每格独立:tab 头(运行状态点 + **OSC 0/2 动态标题**——CLI 设置的终端标题会显示在窗格头 + 网格尺寸)、悬停 ✕ 单独关闭;点击窗格即激活(顶部高亮边)
- **VT 渲染**(`term.rs`,19 项单测):UTF-8(跨包)、SGR 全色彩(16 色 / 256 色 / 真彩 / 加粗 / 下划线 / 反显)、光标移动与定位、清屏清行(ED/EL/ECH)、行内插删(ICH/DCH/IL/DL)、滚动区(DECSTBM/SU/SD)、**交替屏幕(?1049,支持 vim 类全屏 TUI)**、光标显隐(?25)与光标块渲染、DECSC/DECRC
- 键盘直通:方向键 / Home / End / PageUp / Delete / Insert、**F1-F12**、修饰组合(S/C/A+方向,转 xterm `CSI 1;{mod}` 序列)、Ctrl+C/D/L/Z/A/E/K/U/W
- 关闭最后一个窗格自动收起整个 dock

## 设置

`Ctrl+,` 或状态栏「⚙」打开设置弹窗,修改保存到 `~/.monkeyfence/config.toml`:

- **角色 → 提供方**:planner / worker / reviewer 各自指定提供方名称;点选角色后编辑其提供方的类型(mock / openai 兼容 / anthropic)、base_url、api_key、model(新名称会自动建同名提供方并迁移旧配置)
- **引擎**:并行 worker 数、工具循环轮数、失败熔断次数(下次打开项目生效)
- **编辑器**:字体、字号(保存后立即应用到所有打开的编辑器)

## Agent 编排(核心)

输入目标后:

1. **规划者**(planner)把目标分解为任务 DAG(依赖用 `deps` 声明)。
2. **工作者池**(worker pool)原子认领就绪任务(`pending → ready → dispatched`),带 LLM 工具循环执行:
   `fs_read / fs_write / fs_patch / fs_list / run_cmd / spawn_subtask / send_message / ask_human / complete_task / report_failure`
3. 任务完成自动**提升依赖者**;失败计数跨重试累计,**3 次熔断**(面板可手动重置)。
4. 全部任务终结后**收敛**收尾。
5. 关键决策可 `ask_human` 阻塞等待你的回答(Agent 面板出现问答卡片)。

所有状态(运行/任务/派发/邮箱/问题)持久化在 `<项目>/.mf-agent/orchestration.db`,**崩溃可恢复**;agent 修改过的打开中文件会自动重载。

### 接入真实 LLM

编辑 `~/.monkeyfence/config.toml`(首次运行自动生成模板):

```toml
[providers.glm]
kind = "openai"                       # OpenAI 兼容端点
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key = "your-key"
model = "glm-4.6"

[roles]
planner = "glm"
worker = "glm"
```

也支持 `kind = "anthropic"`。默认 `mock` 提供方无需网络即可演示完整任务流转。

## 技能(Skills)

自有技能体系(非 orca 协议):一个技能 = 一个目录,含 `skill.toml`(触发词 + 工具白名单)与 `INSTRUCTIONS.md`(注入正文)。

- 项目级 `<项目>/.monkeyfence/skills/` 覆盖全局 `~/.monkeyfence/skills/`
- 任务说明命中触发词 → 说明注入工作者系统提示,工具白名单取交集
- 内置技能:先读后写纪律、Rust 红绿重构、P4 安全提交(可自由编辑)

## P4 / Git 面板

自动检测:工作区位于 Perforce client root 下 → P4 模式;否则 Git 模式。

- **P4**:待提交变更(default + 编号 CL)、文件勾选、双击看 diff(`p4 diff -du` 着色渲染)、提交(`submit -d`)、还原、搁置(自动建编号 CL → reopen → shelve)、同步、提交历史(`p4 changes`,按 stream 过滤);状态栏显示 client@server。
- **Git**:状态/暂存/取消暂存/提交/历史/单文件 diff(git2)。

## 架构

```
crates/
  mf-core     Buffer(ropey + 事务式 undo)、tree-sitter 高亮(8 语言,自有 scope 表)
  mf          GPUI 应用:编辑器(自定义 Element 逐行 shaping)、文件树、
              快速打开(nucleo 模糊)、命令面板、Agent/P4/Git 面板、diff 视图
  mf-agent    编排引擎:SQLite 任务 DAG + 派发认领 + 邮箱 + 问答;
              提供方层(OpenAI 兼容 / Anthropic / Mock),工具沙箱(路径越界拦截)
  mf-vcs      p4 CLI 封装(-ztag 解析,真实 2024.1 输出校验)、git2、统一 diff 解析
  mf-skills   技能加载/匹配/注入
```

## 测试

```bash
cargo test --workspace   # 31 项:buffer/undo 往返、DAG 状态机、熔断、问答、端到端 mock 运行、
                         # ztag 解析(真实样本)、diff 解析、git 往返、
                         # 终端 ANSI/OSC 剥离与 \r 覆盖写、配置序列化往返
```

> 注:mf-bin 的单元测试集中在 `main.rs`(rustc 对超大 gpui 模块内联 `#[test]` 的宏展开深度有计数怪癖)。

## 已知限制(v1)

- 多光标、编辑器分栏、软换行未实现(终端 dock 支持任意嵌套分屏)
- 终端网格为固定 26×120(窗格尺寸自适应 resize 待做);鼠标选择/复制待做
- 中文输入走系统 IME(基础支持,组合窗口位置未定制)
- 任务 result 摘要在个别路径下显示为“(无总结)”
- Windows 优先(其他平台依赖 GPUI 后端可用性)

## 许可

Apache-2.0
