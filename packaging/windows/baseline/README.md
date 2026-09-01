# Windows 旧交付生命周期基线（Issue #15 / T0d）

本目录是 MonkeyFence 迁移到 Web 交互客户端 + 无界面 Core Service（canonical spec：
`docs/superpowers/specs/2026-09-01-web-interaction-core-service.md`）之前的 **Windows
交付现状基线**：以只读方式记录「当前如何安装、升级、卸载、备份数据」，作为后续
T1+（备份 API、迁移）与附录 A8（MSI 交付）实施前后的对照起点。

- Spec Issue：[#15](https://github.com/yimi1mi/monkey-fence/issues/15)（Part of #11）
- 基线 commit：`b967d06`
- 状态：**只读现状记录**。不实现 MSI、升级器、卸载器或 side-by-side bundle，
  不修改任何生产 Rust 代码。

## 文件

| 文件 | 作用 |
| --- | --- |
| `capture.ps1` | 只读采集脚本：采集当前交付形态并输出确定性 JSON（schema `monkeyfence.windows-baseline.v1`） |
| `expected.json` | 机器可读契约期望（golden）：clean/existing 两种 fixture 输入的完整期望输出 + 18 项能力状态期望 |
| `run-tests.ps1` | 契约测试：双 shell 执行、字节稳定、golden 比较、只读性验证、expected.json 机器独立性断言 |

## 当前事实（截至 2026-09-01 / main `b967d06`）

### 启动与交付形态

- **cargo-run GPUI 单进程桌面工作台**：根 `Cargo.toml` 的 `default-members` 使裸
  `cargo run` / `cargo build` 默认构建主程序。
- 主程序 bin 为 **`monkeyfence`**（`crates/mf`，GPUI 桌面 UI）；辅助 CLI 为
  **`mfctl`**（`crates/mfctl`，经按进程命名的 Named Pipe 连接运行中的主程序，
  能力令牌 `MF_RUN_TOKEN`）。
- `gpui` 是本地 path 依赖（Zed 同源 checkout），平台引导 vendored 于
  `vendor/gpui_platform`；clean checkout 需要 Zed 本地依赖。
- 交付方式 = **源码 checkout + cargo 构建**；构建产物位于 `target\`（以及开发机
  常见的 `CARGO_TARGET_DIR=target-dev`），无任何安装器。

### 数据位置（capture 只探测名称与存在性，绝不读取文件内容）

| 位置 | 内容 | 代码事实 |
| --- | --- | --- |
| `~/.monkeyfence/` | `config.toml`、`catalog-v1.db`（目录库，`user_version=1`，含 Agent Instance/模板/Secret 引用/插件包）、`session.json`、`ui-prefs.json`、`skills/` | `crates/mf-agent/src/schema.rs`、`crates/mf-agent/src/config.rs`、`crates/mf/src/app_ctx.rs`、`crates/mf/src/workflow_editor.rs`、`crates/mf-skills/src/lib.rs` |
| `<project>/.mf-agent/` | `workflow-v1.db`（项目库，`user_version=6`）、`work-items.json`、`step-run-*.md`（Handoff 产物）、旧库 `orchestration.db` / `workspaces.json`（不读不删，属残留） | `crates/mf-agent/src/schema.rs`、`crates/mf/src/work_items.rs`、`crates/mf/src/runtime_host.rs` |
| `<project>/.monkeyfence/skills/` | 项目级技能目录 | `crates/mf-skills/src/lib.rs` |

测试/嵌入式重定向环境变量：`MF_CATALOG_DB`（目录库路径）、
`MONKEYFENCE_SESSION_PATH`（会话状态路径）。

`data_locations.user/project/project_monkeyfence` 均带 `probe_ok`；目录/固定文件
存在性或枚举遇到权限/IO 错误时置为 `false`，不会把“无法判断”编码为空目录。
`residue.agent_cli_config_backups` 同样携带 `probe_ok`。

### 备份现状（目前**没有**一致的在线备份）

- **无在线数据库备份**：根 `Cargo.toml` 的 `rusqlite` 仅启用 `bundled` feature
  （未启用 `backup`，SQLite Backup API 未编译）；`crates/mf-agent/src/schema.rs`
  的链式迁移（`user_version` 逐版升级）前没有备份步骤。
- **现存的唯一备份行为**：`crates/mf-plugins/src/hooks.rs` 在写入 Agent CLI 配置
  文件前生成 `<file>.monkeyfence-backup-<ts>` 整文件备份——只覆盖 CLI 配置文件，
  不覆盖项目库/目录库/Secret。
- **实践中的用户备份方式**：关闭应用后手工拷贝 `.mf-agent` / `~/.monkeyfence`
  （例如仓库内可见的 `.mf-agent/mf-agent-bak/` 目录即整目录手工拷贝残留）。

### 升级 / 卸载现状

- 升级 = `git pull` + 重新 `cargo build`；无更新器、无版本目录、无回滚机制，
  pre-Bridge 当前二进制不是 rollback target（spec §13.2）。
- 卸载 = 删除源码 checkout 与数据目录；无卸载器、无注册表 ARP 条目、
  无 Windows Service、无自启动项。旧库文件（`orchestration.db` 等）既不读取
  也不删除，卸载后留在数据目录中。

## 能力矩阵：当前状态 vs 未来目标

> 「未来目标」列为 spec 附录 A8 / 正文的设计目标，**当前均不存在**；
> capture.ps1 对每一项都显式输出 `status`，缺失时为 `absent`，不得省略，
> 也不得把下表的未来设计误报为现状。`probe_failed` 表示探测本身失败
> （如权限受限），不等于 absent。

| 能力（输出字段） | 当前状态 | 当前判定依据（探测/代码事实） | 未来目标（spec 章节） |
| --- | --- | --- | --- |
| `msi_installer` | absent | HKCU/HKLM(含 WOW6432Node) 无 `WindowsInstaller=1` 的 MonkeyFence ARP 条目；普通残留键不会冒充 MSI | 附录 A8：per-user WiX MSI |
| `bootstrapper_exe` | absent | repo target / PATH 无 `monkeyfence-bootstrapper`；bundle 空目录只记 residue | 附录 A8：bootstrapper exe |
| `uninstaller` | absent | 无带 UninstallString 的 ARP 条目 | 附录 A8 / §9.5 |
| `updater` | absent | repo target / PATH 无 `monkeyfence-updater`；versions/current.json 残留不冒充更新器 | §13.4 side-by-side 更新 |
| `side_by_side_versions` | absent | `%LOCALAPPDATA%\Programs\MonkeyFence\versions` 与 `current.json` 未形成完整组合 | §13.4：`versions\<semver>\` |
| `current_json_pointer` | absent | `current.json` 不存在 | §13.4 |
| `windows_service` | absent | `Get-Service` 无 monkeyfence 匹配 | §1.2：首版无 Service |
| `autostart` | absent | Run/RunOnce 键、启动文件夹、计划任务、Automatic MonkeyFence 服务均无匹配 | §1.2：首版无自启动 |
| `launcher` / `tray` / `picker` | absent | 二进制（repo target、PATH）与进程均未发现 | §11 / T6 |
| `core_service_bin` | absent | 当前为单进程 GPUI，无独立 `monkeyfence-core` | §2 / T6 |
| `elevated_broker` | absent | 无 `mf-broker`；无 Root Mode | §10 / T9 |
| `discovery_file` | absent | `%LOCALAPPDATA%\MonkeyFence\discovery.json` 不存在 | §11.1 / T6 |
| `user_data_migration` | absent | `~/.monkeyfence/service-v1.db` 不存在；迁移代码未实现 | §3.4 / T1（session.json→project_registry） |
| `online_sqlite_backup` | absent | rusqlite 未启用 backup feature；链式迁移无前置备份 | §3.1 / T1：schema 升级前 SQLite Backup API 一致备份 + manifest |
| `user_db_backup_routine` | absent | 无任何定时/在线 DB 备份例程；实践为手工离线拷贝 | （T1+ 随备份能力引入） |
| `agent_cli_config_backup` | **present** | `crates/mf-plugins/src/hooks.rs` 写 CLI 配置前的整文件备份 | 现有能力（scope 仅 CLI 配置文件） |

## capture.ps1

### 用法

```powershell
# 真实机器采集,输出 stdout(人读/CI 日志)
powershell -NoProfile -ExecutionPolicy Bypass -File capture.ps1
pwsh -NoProfile -File capture.ps1

# CI / 测试:注入 fixture 路径,输出到文件(UTF-8 无 BOM,字节稳定的推荐通道)
pwsh -NoProfile -File capture.ps1 `
    -UserHome <dir> -ProjectRoot <dir> -RepoRoot <dir> `
    -LocalAppData <dir> -AppDataRoaming <dir> -OutputPath <file.json>
```

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `-UserHome` | `%USERPROFILE%` | 派生 `~/.monkeyfence` 用户数据探测 |
| `-ProjectRoot` | `-RepoRoot` | 派生 `<project>/.mf-agent` 与 `<project>/.monkeyfence` 探测 |
| `-RepoRoot` | 脚本向上三级 | 仓库 target 二进制与 `Cargo.toml` 元数据探测 |
| `-LocalAppData` | `%LOCALAPPDATA%` | 未来 per-user 安装位置残留探测 |
| `-AppDataRoaming` | `%APPDATA%` | 启动文件夹自启动残留探测 |
| `-OutputPath` | 无（stdout） | **唯一允许的写**：一个尚不存在的普通本地路径输出文件；拒绝 UNC/device/extended/ADS 与 8.3 short-name 别名；父目录必须已存在，且不得位于用户/项目数据、源码/应用或未来 bundle/Core 目录，也不得经过 reparse point |

Windows PowerShell 5.1 与 PowerShell 7+ 均可执行（文件为带 BOM 的 UTF-8）。

### 确定性与规范化

- 同一输入（含注入路径）在同一 PowerShell 版本下**连跑两次输出字节一致**；
  所有枚举按 `OrdinalIgnoreCase` 排序（与系统区域设置无关）。
- 不输出时间戳、PID、进程计数、文件大小、随机值。
- 除 `capture.inputs`（五个注入根）、`runtime_facts.binaries.path_lookup`
  （真实 PATH 查找结果）与 `residue.processes.running`（采集瞬间的本机进程名）
  外，全部字段为相对名称 / 布尔 / 枚举 / 代码事实，机器无关。
- 输出 schema 字段固定（顶层：`schema` / `capture` / `runtime_facts` /
  `data_locations` / `residue` / `delivery_capabilities` / `backup_capabilities`）；
  变更必须升级 schema id。

### 只读与敏感内容边界

允许：读文件/目录元数据（名称、类型、存在性）、读注册表（.NET 只读打开）、
查询进程/服务/计划任务/PATH 命令、读仓库 `Cargo.toml` 元数据。

禁止（脚本内无任何此类调用）：

- 写注册表、写自启动项、写用户数据库、写应用目录；除 `-OutputPath` 指定的
  单个新输出文件外不写任何文件，也不创建目录。为防止参数误用，脚本拒绝覆盖
  任何现有文件，拒绝写入 `~/.monkeyfence`、项目 `.mf-agent/.monkeyfence`、
  源码目录、未来 bundle/Core 数据目录，并拒绝包含 symlink/junction 的父目录链；
  UNC/device/extended/ADS 与 8.3 short-name 别名同样 fail-closed 拒绝。最终写入使用原子 `FileMode.CreateNew`，
  不存在检查与创建之间也不会覆盖竞态目标。
- 读取任何数据库文件内容（`catalog-v1.db` / `workflow-v1.db` 等只探测存在性），
  读取或输出 Secret / Provider 配置 / API Key / `MF_RUN_TOKEN`；不读
  `config.toml` / `session.json` 内容，只记录存在性。
- 输出 PID、完整命令行等进程细节（仅名称级布尔结果）。

## expected.json（契约期望）

- 由 `run-tests.ps1 -UpdateGolden` 生成/更新：对 clean / existing 两种 fixture
  输入各固化一份完整期望输出，注入路径以占位符表示
  （`<USER_HOME>` / `<PROJECT_ROOT>` / `<REPO_ROOT>` / `<LOCAL_APP_DATA>` /
  `<APP_DATA_ROAMING>`），**不绑定开发机用户名或绝对路径**（测试断言）。
- `capability_expectations` 固化 18 项能力的目标状态（17 项 absent +
  `agent_cli_config_backup` present），测试对两种 fixture 输出逐一断言，
  保证缺失能力显式可见、不得省略。
- `strict_compare_ignore_paths` 声明跨机器不稳定字段
  （`path_lookup`、`processes.running`），golden 中这些字段固化为空数组。

## run-tests.ps1（契约测试）

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File run-tests.ps1              # 运行(自动探测 powershell.exe + pwsh.exe)
powershell -NoProfile -ExecutionPolicy Bypass -File run-tests.ps1        # 在 PS 5.1 下同样可运行
pwsh -NoProfile -File run-tests.ps1 -UpdateGolden                        # 重新生成 expected.json 的 golden
```

断言内容（当前 39 项，双 shell 各跑一遍均需全绿）：

1. 两种 fixture（干净用户环境 / 已有 `~/.monkeyfence` + `Project/.mf-agent` 数据
   环境）× 每个可用 shell 连跑两次，输出**字节一致**；
2. 默认 stdout 输出为合法 JSON 且与 `-OutputPath` 内容一致；跨 shell 输出
   **解析等价**；
3. 输出与 `expected.json` golden 严格结构等价（含 JSON primitive 类型，
   占位符替换 + 忽略声明字段）；
4. 18 项能力字段齐全且 status 符合期望；schema 关键字段齐全；输出不含时间戳；
5. `-OutputPath` 在 PS 5.1/7 下均拒绝覆盖现有文件、用户数据库及 extended/UNC/
   8.3 short-name 别名，且拒绝在源码/应用目录创建新文件；被拒绝目标的哈希/存在性保持不变；
6. **只读性**：fixture 输入树（含每个文件 SHA-256）采集前后不变；被读取的
   注册表键（`reg export` 哈希）、服务与计划任务清单采集前后不变；
7. `expected.json` 不含绝对路径与当前用户名。

测试产物全部位于 `%TEMP%\mf-baseline-tests-<guid>\`，结束自动清理。

## 迁移 / 回滚

- 本目录为纯只读采集与测试，不含生产代码路径；对运行中的应用与用户数据零影响。
- 回滚 = 删除 `packaging/windows/baseline/` 目录。

## 平台限制与已知边界

- 仅支持 Windows（注册表 / `%LOCALAPPDATA%` / Named Pipe 等均为 Windows 概念）；
  其他平台不可运行本脚本。
- PS 5.1 与 PS 7 的 `ConvertTo-Json` 格式不同（缩进 4/2 空格；PS 5.1 把非 ASCII
  转义为 `\uXXXX`）：**同一 shell 内字节稳定**，跨 shell 只保证解析等价。
  CI 做字节级比较请固定 shell 并使用 `-OutputPath`（UTF-8 无 BOM）。
- Agent CLI 配置备份残留探测仅覆盖 `~/.claude`、`~/.codex`、`~/.config`、
  `~/.gemini` 四个常见目录（hooks 的备份文件总是写在对应 CLI 配置文件旁边）。
- 二进制文件/PATH、数据目录、CLI 备份残留、启动文件夹、服务、计划任务及安装位置探测在权限受限环境
  （如精简 CI 容器）可能 `probe_failed`——
  这是诚实上报，不冒充 absent；相应 capability 同样输出 `probe_failed`。
- golden 的注册表/服务/计划任务 matches 在已安装过 MonkeyFence（或残留未清理）
  的机器上会非空并导致测试失败——这是**预期信号**：该机器状态偏离
  「未安装 MonkeyFence」基线。
- 仓库当前没有 CI workflow 配置，本 ticket 未新增（范围限定）；采集脚本本身
  支持路径注入，Windows CI 可直接以 fixture 参数执行。
