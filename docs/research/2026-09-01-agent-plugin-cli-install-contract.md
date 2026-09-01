# Agent Plugin 驱动的 CLI 安装契约

- 日期：2026-09-01
- Wayfinder ticket：[确定 Agent Plugin/CLI 安装、发现、更新与 Root Mode 契约](https://github.com/yimi1mi/monkey-fence/issues/10)
- 依赖：[Web Gateway、Root Mode 与 Elevated Broker 安全决策](./2026-09-01-web-gateway-root-security.md)
- 状态：可进入规格化与实现

## 决策摘要

MonkeyFence 参考 Orca 的 Agent 目录、能力探测和结构化安装体验，但不假定 Orca 已经提供通用 Agent CLI 安装器。MonkeyFence 自己扩展插件契约，让 Agent Plugin 声明 CLI 的发现、版本探测、模型探测以及跨平台安装 recipe；安装由 Rust Core Service 执行并记录收据，插件和浏览器均不直接获得安装能力。

三个对象和生命周期必须分离：

1. **Plugin Package**：提供 Agent Type、适配器、Schema、安装 recipe 和 UI 元数据；沿用内容寻址、默认禁用、授权后启用与 Revision 固定。
2. **CLI Installation**：本机已有或由 MonkeyFence 管理的外部可执行程序；可被发现、安装、更新、修复和卸载。
3. **Provider Profile / Agent Instance**：API Key、Endpoint、模型与启动覆盖；只引用 CLI/Agent Type，不属于安装动作。

因此，安装 Claude Code/Codex/GLM 等 CLI 不会读取或改写 API Key；配置 Provider Profile 也不会修改用户真实 CLI 的全局配置。用户可以直接使用已检测到的本机 CLI，也可以从插件提供的 recipe 新建一份受管安装。

## 领域关系

```text
Plugin Package@hash
  └─ Agent Type
       ├─ discovery/version/model probes
       ├─ 0..n installer recipes
       └─ adapter contract

  └─ Provider Type
       ├─ Provider Profile schema
       ├─ remote model catalog probe
       └─ model id → Agent launch mapping

CLI Installation ── Installation Receipt
       │
       ├─ Default CLI entry（沿用外部配置，只读意图）
       └─ Agent Instance ── Provider Profile / isolated config
                              │
Pipeline Revision ────────────┘
  固定 plugin hash + Agent Instance snapshot + CLI executable identity
```

同一 Agent Type 可以发现多份安装；同一安装可以被多个 Agent Instance 引用。外部安装和受管安装都可以启动，但只有拥有可信收据的受管安装能无歧义地执行 update/repair/uninstall。

## Manifest v3

现有 v2 只包含 `command` 与 `detect_commands`，不足以表达安全安装、实际版本和来源。实现时将 manifest 提升到 v3，不为安全敏感字段做静默 v2 兼容推断；内置合成插件与第三方示例同步迁移。

建议的声明形态：

```toml
[manifest]
version = 3

[capabilities]
spawn = true
net = true
package_install = true

[[agent_types]]
id = "codex"
name = "Codex"
adapter = "codex"
command = "codex"
modes = ["interactive", "oneshot"]
supports_isolated_config = true

[agent_types.discovery]
commands = ["codex"]
version_argv = ["--version"]
version_parser = "semver-first"

[agent_types.models]
local_probe = "adapter"

[agent_types.root_launch]
permission_mode = "full-access"
argv = ["--dangerously-bypass-approvals-and-sandbox"]

[[provider_types]]
id = "openai-compatible"
protocol = "openai"
config_schema = "schemas/openai-provider.json"
model_probe = "remote-catalog"
cache_ttl_seconds = 300

[[agent_types.installers]]
id = "npm-global"
platforms = ["windows-x64", "linux-x64", "macos-arm64"]
kind = "package-manager"
manager = "npm"
package = "@openai/codex"
argv = ["install", "--global", "@openai/codex@{version}"]
scope = "user"
post_install_probe = true
```

具体序列化可以在 `/to-spec` 中微调，但以下语义不可改变。

### Agent Type 必备声明

- 稳定 `id`、`adapter`、默认命令与运行模式；
- discovery 候选命令，不能由浏览器提交任意路径；
- 结构化 version probe 与解析器；
- 可选 CLI 本地 model probe、会话恢复和 attach 能力；
- `root_launch`：最高权限模式的结构化 argv/env 映射；没有映射时 Root Agent 启动失败关闭，不能只获得 OS elevation 后仍宣称“最高权限”；无内部权限层的 Generic Command 必须显式声明 `passthrough-full-access`；
- 支持的平台、架构、最低/最高 CLI 版本；
- 0..n 个稳定 installer id；
- Provider Schema/默认模型映射与 CLI 安装分开声明。

### Provider Type 必备声明

Manifest v3 新增 `provider_types` contribution。Provider Type 负责 Endpoint/API 协议、Provider Profile Schema、远端模型目录和模型 id 归一化；Agent Type 只负责 CLI 本地能力探测，以及把已选择模型编译到结构化 argv/env/config。CC Switch 式 `/models` 获取不能挂在 CLI discovery 上。

远端模型探测由 Core 内建协议适配器执行。若第三方 plugin worker 必须参与，只发一次性、Provider-scoped probe capability，不把可复用 API Key、secret ref 或其他 Profile Secret交给 worker；探测输出在进入缓存、事件和日志前脱敏。

### Installer 类型

第一版只接受三个经过宿主校验的类型：

- `package-manager`：npm/pnpm/pipx/uv/brew/winget 等受支持适配器；Core 以结构化 argv 直接启动，不经过 shell。
- `verified-download`：Core 下载固定 HTTPS 资源，验证 sha256 或签名，检查 archive 路径，再原子发布到用户级受管目录。
- `custom-command`：插件声明 executable + argv/env 模板；结构化命令需要 `spawn/package_install` 授权且 Root Mode 已开启。只有使用 shell 字符串时才额外要求 `shell`，不能把字符串伪装成 argv。

系统包管理器、machine scope 或写入受保护目录的 recipe 标记 `requires_elevation = true`。普通模式不可自动回退到高权限；Root Mode 关闭时返回可恢复的 `elevation_required`。

### 权限与指纹

Manifest v3 新增至少以下能力：

- `package_install`：创建受管 CLI 安装任务；
- `privileged_install`：允许 recipe 请求 Elevated Broker；
- `agent_full_access`：允许该 Agent Type 在 Root Mode 下获得 full-access 启动配置。

下载仍需 `net`，启动仍需 `spawn`，shell recipe 仍需 `shell`。能力、installer recipe、worker、内容哈希任一变化都改变授权指纹并要求重新授权。插件启用不等于允许安装：用户仍需显式点击 Install/Update/Repair/Uninstall。

当前内置 Agent 的 `permission_args`/`InstallSpec` 是进程内硬编码，只能作为迁移输入：Manifest v3 必须把它们转成经过校验和授权指纹覆盖的 `root_launch` 与 installer contribution，第三方 Agent 不允许绕过 manifest 获得同等能力。

Package manager 即使通过 direct argv 调用也可能执行软件包自身的 postinstall；“防 shell 注入”不等于软件沙箱。Core 在预览后冻结 exact package/version/registry/argv/recipe digest，用户授权的是该不可变计划，不能在执行时重新解析 `latest` 或接受插件替换包名。

## 发现与选择

Core 在受控环境中执行 discovery/version probe，输出归一为：

```text
installation_id, agent_type_id, executable_path, canonical_path,
actual_version, source(external|managed), scope, platform, architecture,
receipt_id?, detected_at, health
```

- discovery 只搜索宿主允许的 PATH 和已登记的受管目录；结果 canonicalize，并记录入口 link/shim 与最终 target identity。Unix npm 等合法 symlink 可被发现，但启动前 target 被替换时必须拒绝；archive 解压仍一律拒绝 symlink/junction，受管目录只接受收据明确拥有且目标仍在 owned root 内的 link。
- 相同 canonical executable 只生成一个 Installation。
- 可检测用户已有的本机 CLI，来源标为 `external`，不接管其生命周期。
- 外部安装不能仅凭同名、路径或版本“采用”为受管安装。只有其可执行 hash/签名与插件固定的可信 artifact 完全匹配时才可创建 adoption receipt；否则只能重新安装到 managed directory，原安装继续标记 `external`。
- 未检测到 CLI 时 Agent Catalog 显示插件提供的安装选项，而不是简单置灰。

模型下拉箭头调用 Provider Type 的远端 model catalog probe；可选的 Agent Type local probe 只校验 CLI 支持能力。请求由 Core 发起，按 Provider Profile 在 Core 内解析 Secret，浏览器只收到模型 id、显示名、能力和缓存状态；API Key 不进入 URL、插件 worker、事件或终端 journal。离线、未配置或探测失败时显示缓存模型和明确错误，允许手填合法模型 id。

## 安装任务状态机

```text
absent/external/detected
  → queued → resolving → downloading/executing → verifying
  → installed
  ↘ failed / cancelled / repair-needed

installed → update-available → updating → verifying → installed
installed → repairing → verifying → installed
installed → uninstalling → absent
```

- 每次状态变化生成单调序号事件；API 重连用 snapshot + resume sequence 恢复。
- `installed` 只能由 post-install discovery + version probe 成功产生，不能以进程退出码 0 代替。
- cancel 是命令，不是 UI 本地状态；Core 确认后才显示已取消。
- package manager 可能留下部分外部状态，失败后进入 `repair-needed` 并展示诊断，不能谎报已回滚。
- 相同 command id 幂等；不同目标版本或 recipe digest 的重复命令冲突拒绝。

## 执行流程

1. Controller 选择 Agent Type、installer、目标 scope、版本和 host。
2. Core 返回安装预览：来源、解析后的 argv 摘要、目标目录、权限、预计覆盖、下载校验和回滚能力。
3. Controller 提交带 command id、lease epoch、expected revision 的命令。
4. Core 校验插件启用/授权、recipe digest、平台、目标和 Root Mode epoch。
5. Core 或 Elevated Broker 在新的 staging/job/process group 中执行；浏览器和 plugin worker 均不能直接连接 Broker。
6. Core 对输出先脱敏再写 journal/推送进度，限制大小、速率和持续时间。
7. 完成后重新 discovery/version probe，生成 Installation Receipt 并原子切换选择。
8. 未通过验证时清理 staging；可安全回滚时执行回滚，否则进入 `repair-needed`。

网络下载由 Core 完成，要求 HTTPS、限制重定向域名/次数、响应大小和解压大小，拒绝绝对路径、`..`、symlink/junction 与设备文件。`verified-download` 必须验证 manifest 固定的 hash，或验证由插件内容固定信任锚签发的 release metadata/签名；动态版本在预览时解析成 exact version、URL 与 digest，不能用远端“latest”响应绕过 recipe digest。

Secret 只能以受控 secret reference 注入目标 Agent 会话。安装 recipe 默认不能引用 Provider/API Secret；确有安装认证需求时使用独立 installer credential Schema，并在 Core 内存中短暂解析，永不进入 argv、日志或收据。

## Installation Receipt

受管安装成功后保存不可变收据：

- receipt id、plugin full id/version/content hash；
- Agent Type id、installer id、recipe digest；
- 请求版本、实际版本、平台、架构、scope；
- package id 或下载 URL、hash/signature 结果；
- canonical executable path、可执行身份/hash（可获得时）；
- install/update 前后时间、Root Mode/broker epoch（不保存 capability）；
- rollback/uninstall 方法、保留 artifact 与脱敏日志引用；
- post-install probe 结果。

收据不是插件 lock file，也不保存 API Key、完整环境、未脱敏 argv 或终端输入。

## 版本冻结、更新和卸载

- Pipeline Revision 固定 plugin version/content hash、Agent Instance snapshot、installation id、canonical executable、实际 CLI version 和可获得的 executable hash。
- 新 Agent Run 启动前重新核对 executable identity。受管安装与冻结身份不符时拒绝启动并进入 Needs You；不偷偷使用 PATH 上的新版本。
- 受管安装只在活动 Run 或用户显式创建的 replay lease 期间保持 pin；历史 Revision 默认保留身份用于审计，不永久阻止更新。支持 side-by-side 的 installer 安装新版本后只切换未来草稿；旧版本保留到活动 pin/replay lease 释放。
- 全局 package manager 无法 side-by-side 时，默认阻止破坏活动 pin 的 update/uninstall。Root Mode 也不自动突破可复现性；用户选择“强制”时必须先看到受影响 Revision，旧 Revision 后续运行进入 Needs You。
- 外部安装默认只有 launch；update/repair/uninstall 交给其原工具。显式 adopt 后才进入受管生命周期。
- uninstall 默认保留用户配置、Provider Profile、Secret 和 Agent Instance；只删除收据明确拥有的二进制/包，不按猜测路径递归删除。
- 插件禁用/升级不终止已运行会话；它会阻止依赖不可用 adapter/recipe 的新启动。旧插件包按 Revision pin 保留。

## Root Mode 集成

沿用 #8 决策：Core 始终普通权限，Root Mode 的安装与 Agent 启动交给最小 Elevated Broker。

- Root Mode 开启后，新的 CLI install/update/repair/uninstall 和新的 Agent Session 默认请求 OS Administrator/root + MonkeyFence full-access。
- “full-access”同时包含 OS elevation 和 Agent 自身权限模式。Adapter 必须从受授权的 `root_launch` 生成参数；不能仅把普通 Agent 进程以管理员身份启动后就宣称最高权限。
- 同一 Root Mode 生命周期不逐动作弹系统确认，但每个安装任务仍必须由 Controller 明确发起；插件不能在启用、更新、开机或后台探测时自动安装。
- Root Mode 关闭或 Core 重启后，旧 epoch 的 queued job 不得开始；运行中的 Root Agent 继续并持续显示管理员标记。
- Elevated Broker 只授权并启动。每个 Root Agent 的 PTY/进程组由独立 Elevated Runtime Host 持有，每个安装由 job-scoped Elevated Install Host 持有；Root Mode 关闭时 Broker 退出，而既有 host 可以完成或被取消，但不能创建新 Session/Job 或执行通用命令。
- Root Run 可在 Controller 明确启动后由 Core 自动调度，但每个尚未启动的下游节点都要校验同一 active Root epoch；Root Mode 已关闭时该节点进入 Needs You。Controller 暂时断连不单独终止仍有效的 epoch，插件不能自行创建 Root job。
- Controller 手工发起的 `custom-command`/任意 Agent command 只有在 Root Mode、插件授权与当前 Controller 三者同时成立时可进入 Broker；工作流下游启动则使用已授权 Run + active Root epoch，不要求浏览器保持连接。
- Browser 只持有领域 command id，不持有 broker pipe 名、nonce、MAC 或 OS token。

提权身份与安装目标身份分离。Receipt 固定 `requesting_principal`、`target_owner` 与 `scope(user|machine|managed)`；Broker 不得因 Windows UAC 输入另一管理员账号而把 user-scope 包写进该管理员的 profile。受管 user scope 始终写入 Core 原用户的 managed directory 并恢复/验证 owner ACL；无法保证 target principal 的 package manager recipe 必须拒绝或改用明确的 machine scope，不能静默换 scope。

## UI 交互

Agent 配置页使用 CC Switch 类似的信息密度，但保持上述边界：

- Agent Catalog 卡片显示 `已检测/可安装/外部/受管/需更新/需修复`、版本、来源和 scope。
- 卡片动作：Install、Use existing、Update、Repair、Uninstall、Reveal receipt；不把 API Key 表单混在安装弹窗。
- 安装完成后引导创建 Provider Profile/Agent Instance；Provider Profile 包含 Endpoint、API Key、模型获取下拉与高级参数。
- 模型下拉每次可显式刷新，并显示来自 live probe 还是 cache。
- 安装预览明确显示是否需要 Root Mode、是否可回滚、是否影响 pinned Revision。
- 进度可关闭后继续；托盘和跨项目全局页面显示同一 Core job。MonkeyFence 每个 OS 用户只有一个 Core Service，服务可登记多个 Project。
- Observer 能看进度、日志与收据，不能发起、取消、更新或卸载。

## Attach 与恢复边界

- 插件可声明 `resume`：用 CLI 官方 session id 启动新进程恢复上下文。
- `attach` 只允许 Session Registry 已拥有的 live PTY handle，或有明确版本化协议的 adapter。
- 不提供“按 PID 附着任意终端”；外部正在运行但不受 MonkeyFence 管理的 CLI 不能因为同名命令被接管。
- 双击节点进入 Node Session Panel，仍由真实 CLI 处理 `/model`、`/skills` 等命令；Web 不解析 Agent 命令。

## 失败恢复与审计

- 所有 job 在隔离 process group/job object 中运行，有超时、输出上限、取消和 Core 崩溃恢复记录。
- Core 重启后将未知中的 job 标为 `reconciling`，通过 receipt、process identity 与 version probe 重新判断；不能把未知直接判成功。
- 审计记录插件、recipe、目标、版本、权限、Root epoch、脱敏命令摘要、结果和收据；不记录 Secret。
- update 先保留上一份受管 artifact/receipt；验证新版本后原子切换。无法回滚的系统 package manager 必须在预览中标明。
- repair 必须重放同一 recipe digest 或由插件明确提供兼容迁移；不能在后台换来源。

## 首版非目标

- 不构建通用软件商店或自动后台更新器；
- 不自动迁移 CC Switch 数据或修改 Claude/Codex 全局配置；
- 不承诺接管任意外部 package manager 安装；
- 不允许插件注入任意 Web UI 或把安装脚本下发给浏览器；
- 不把 CLI 正常安装等同于 Provider 可用或账号已登录。

## 验收门槛

- 同一 Agent Type 能同时显示外部安装与一份受管安装，canonical path 去重正确；
- 缺失 CLI 可从插件 recipe 安装，成功前必须经过 post-install version probe；
- package-manager 和 verified-download 使用结构化执行，shell 注入测试失败关闭；
- hash/signature、archive traversal、redirect、oversize、symlink/junction 攻击均拒绝；
- 普通模式不能运行 requires-elevation/custom shell recipe；Root Mode 使用 Broker 且 Core 权限不提升；
- Observer、旧 Controller lease、旧 Root epoch、重复/冲突 command id 均被拒绝；
- 安装日志、事件、收据和浏览器 payload 不含 API Key、installer credential 或 MF_RUN_TOKEN；
- frozen Revision 在 CLI 被替换后拒绝静默运行；side-by-side/pin 与 global manager 冲突路径有测试；
- update/repair/uninstall 只处理收据拥有的内容，不删除用户配置或外部安装；
- Provider 模型下拉通过 Core probe，支持缓存/刷新/失败回退且不暴露 Secret；
- Windows、macOS、Linux 各有 fake installer + fake broker 契约测试；真实包管理器只做受控 smoke test；
- 双击节点进入真实 PTY，slash/skill 命令仍由原生 Agent CLI 完整处理。

## 现有代码迁移清单

- `manifest.rs` 的 parser/validator 从 v2 提升到 v3，新增 Agent discovery/root launch/installer 与 Provider Type；不把旧 `detect_commands: Vec<String>` 当成可执行 probe。
- 所有 synthetic manifest、fixture、测试和 contribution registry 同步迁移；旧 v2 安装贡献必须明确报版本不兼容。
- `BuiltinAgent::InstallSpec` 与 `permission_args` 删除旁路，迁入 v3 installer/root launch contribution。
- Plugin lock 与 Installation Receipt 使用不同存储类型和表；插件内容 pin 不能冒充 CLI provenance。
- Runtime Host 接收 adapter 编译后的 typed full-access LaunchPlan，并以缺失映射 fail-closed。
- 测试覆盖 Root 关闭后旧 PTY/Install Host 仍可控但不能派生新 Root 任务、UAC alternate credential 不改变 target principal、合法 symlink CLI 可发现但替换后拒绝、历史审计 Revision 不永久 pin。
