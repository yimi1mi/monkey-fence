# Web Gateway、Root Mode 与 Elevated Broker 安全决策

- 日期：2026-09-01
- Wayfinder ticket：[确定本地 Web Gateway 的安全与权限模型](https://github.com/yimi1mi/monkey-fence/issues/8)
- 范围：本地浏览器 ↔ Rust Core Service、Controller/Observer、Terminal Session、Secret、Root Mode、Elevated Broker

## 威胁模型

必须防御：

- 恶意公共网页借浏览器访问 loopback；
- DNS rebinding、错误 Host/Origin、CSRF、WebSocket 跨站劫持；
- Observer 或过期 Controller 发送写命令；
- 猜测项目路径、Session ID、PID 或安装目标；
- Secret/API Key/MF_RUN_TOKEN 进入 Snapshot、事件、终端 journal 或日志；
- 未提权 Core 或同机其他用户连接 Elevated Broker；
- 插件在 Root Mode 未开启时执行任意高权限脚本；
- 慢终端客户端、超大 frame、输入洪泛拖垮 Core。

不承诺防御已经在同一操作系统用户下获得任意代码执行和内存/文件读取能力的恶意软件。Root Agent 被用户明确授权后可以修改整个系统；MonkeyFence 只能保证授权可见、凭据不泄露和生命周期可审计，不能把最高权限重新包装成沙箱。

## Loopback 与同源

- 仅绑定随机端口的 `127.0.0.1` 与 `::1`；不监听 `0.0.0.0`、LAN 或远程地址。
- 发行版只打开 IP literal URL，不依赖可被重绑定的自定义 hostname。
- 每个 HTTP 请求严格校验 Host 为当前已绑定 IP literal + port。
- UI/API/WS 静态资源同源；不开放宽泛 CORS，不接受公共网站 preflight。
- UI WebSocket 必须精确匹配 Origin；缺 Origin 的浏览器入口拒绝。非浏览器 CLI 使用独立本地 IPC，不复用 UI WebSocket。
- Local Network Access/PNA 浏览器权限不能作为认证；即使浏览器允许 public → loopback，请求仍必须通过 MonkeyFence 自身认证。

RFC 6455 明确把 Origin 用于防止浏览器脚本未经授权使用 WebSocket，并允许服务端拒绝错误 Origin。Local Network Access 规范把 public/local → loopback 视为本地网络请求，但该浏览器权限只是额外防线，不代替应用认证。

## Bootstrap 与 Web Session

1. 启动器从用户级 discovery 文件读取 instance identity 与 port；文件权限仅当前 OS 用户可读。
2. 每次打开 Web UI 生成一次性 bootstrap nonce，放在 URL fragment；fragment 不进入 HTTP request、server log 或 Referer。
3. Web 首屏从 fragment 读取 nonce，POST 到 same-origin `/auth/exchange`，随后立刻用 history API 清除 fragment。
4. Core 消耗 nonce 一次，设置随机 HttpOnly、SameSite=Strict、Path=/ 的会话 cookie，并返回只存在于页面内存的 CSRF token 与 client bootstrap。
5. 所有写 HTTP 命令要求 cookie、精确 Origin/Host、CSRF header、client id 与 Controller Lease Epoch。
6. WebSocket upgrade 要求同一 cookie、Origin、client id 与版本化 subprotocol；token 不放 query string。
7. Core 重启时 session、nonce、CSRF、client id、lease epoch 全部失效。

同一用户可有多个已认证 Web Client，但只有一个 Controller。新客户端成为 Controller，旧 Controller 降为 Observer；Observer 可订阅状态与终端输出，不能写 DAG、运行、终端输入、resize、Settlement、安装或 Root Mode。自动重连恢复原角色，不能抢 Controller。

## 浏览器响应头与依赖

发行版不加载 CDN 或远程运行代码；所有资源与 Core 同版本打包、内容哈希命名。

最低响应头：

```text
Content-Security-Policy:
  default-src 'none';
  script-src 'self';
  style-src 'self';
  img-src 'self' data:;
  font-src 'self';
  connect-src 'self' ws://127.0.0.1:<port> ws://[::1]:<port>;
  object-src 'none';
  base-uri 'none';
  form-action 'none';
  frame-ancestors 'none';
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Resource-Policy: same-origin
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Permissions-Policy: camera=(), microphone=(), geolocation=(), payment=()
Cache-Control: no-store  # index/auth/API；hash assets 可 immutable
```

W3C CSP3 的 `connect-src` 覆盖 fetch、XHR、EventSource 与 WebSocket；`frame-ancestors 'none'` 阻止 UI 被嵌入攻击页面。

## 资源与命令授权

- API 只接受 opaque Project/Workflow/Run/Session handle，不提供任意文件路径、PID 或命令执行 endpoint。
- Project 注册必须经系统文件夹选择器或本地 CLI；服务端 canonicalize 后保存 Project root。
- 文件操作需要显式 Project capability，并验证最终路径仍在允许 root 内；symlink/junction 逃逸在打开时复检。
- 终端只能 attach Session Registry 已知 handle；不能 attach 任意 PID。
- Terminal writer 只属于 Controller；binary frame、输入速率、cols/rows、send queue、journal 与每客户端 lag 均有上限。
- 领域命令使用 command id、Controller Lease Epoch、expected aggregate revision 与 CAS。
- 运行、Settlement、安装、Root Mode 不做浏览器乐观成功。

## Secret 与日志

- Provider API Key 是 write-only：浏览器可创建/替换/删除，不能读取明文。
- Rust Secret Store 返回 opaque secret reference；Provider/Agent Snapshot 只含引用与脱敏状态。
- MF_RUN_TOKEN 只进入目标 Agent 子进程环境，永远不进入 Web、事件、terminal control、Handoff 或错误。
- output redaction 在 journal、fan-out、持久化日志之前完成，必须跨 chunk 工作。
- 输入 journal 默认不持久化，避免密码、token、审批文本进入回放。
- 审计不得记录 Secret 环境值；命令 argv 中可能含敏感值时保存脱敏摘要而非原文。

## Root Mode

- 默认关闭，不持久化，不随系统启动；Core 重启必定关闭。
- 只有当前 Controller 可以开启/关闭；开启触发一次 OS 原生授权。
- Root Mode 开启期间，新的 CLI install/update/repair/uninstall job 与新的 Preview/Run/手工 Agent Session 默认获得 OS Administrator/root + MonkeyFence full-access。
- 已运行普通进程不能原地提权；需要重启 Session。
- 关闭 Root Mode 后不杀死已经启动的高权限 Agent，但禁止新的高权限启动；UI/托盘继续标记存活的 Root Agent。
- Root Mode 页面和托盘持续红色提示，Node/Session/Run 均显示管理员徽标。
- 用户明确接受 Root Agent 可以访问和修改整个系统；此模式不承诺文件/网络/进程沙箱。

## Elevated Broker

Core Service 始终以 `asInvoker`/普通用户运行。管理员能力位于独立、最小化的 Elevated Broker：

- Windows：独立带 `requireAdministrator` manifest 的 broker，经 UAC 启动；
- macOS：使用 Service Management/launchd privileged helper 与 Authorization Services，不使用已弃用的任意 `AuthorizationExecuteWithPrivileges` 路径；
- Linux：使用 polkit action/helper，由桌面 Authentication Agent 或受支持的文本 agent 完成认证。

Broker 不监听 TCP，不提供 Web API。IPC 使用随机实例名与显式 OS ACL：

- Windows named pipe DACL 只允许当前 logon SID、Core identity 与 broker；不得使用默认 DACL，因为微软文档指出默认 pipe descriptor 可能给 Everyone/anonymous 读取权限。
- 消息携带 protocol version、Core PID/start identity、broker epoch、一次性 nonce、request id 与 MAC/capability。
- Broker 验证调用者 OS 身份、Core 实例和当前 Root Mode epoch；旧 Core/旧 epoch 请求拒绝。
- 浏览器永远不能直接持有 Broker capability 或连接 IPC。
- Broker 只在 Root Mode 生命周期存在；Core 退出、Root Mode 关闭或 broker/Core channel 断开时停止接受新任务。

Broker 只负责授权与启动，不长期拥有 Root Agent 的 PTY 或安装进程。每个 Root Agent 由独立的、会话级 Elevated Runtime Host 持有进程组与 PTY；每个已启动安装由 job-scoped Elevated Install Host 持有。它们使用只绑定对应 Session/Job 的受保护 IPC 与 Core 通讯，不接受新 Session 或通用安装请求。这样 Root Mode 关闭时 Broker 可以退出并拒绝新启动，已经授权的 Root Session/Install Host 仍可完成或被取消，直到对应 Session/Job 退出。Core 重启后不会自动重获旧 host 的写能力；能否只读重连由后续 Session/Job 恢复契约决定。

Root Mode 的“自由使用”指通过已验证 Core/Broker 通道启动任意安装 recipe 或 Agent command，不意味着插件能在后台、启动时或无 Controller 的情况下自行提权。

## 审计与恢复

记录：Root Mode 开关、OS 授权结果、插件/Agent identity、版本、目标、cwd、脱敏命令摘要、开始/结束时间、退出状态、安装 provenance 与 rollback receipt。

不记录：API Key、MF_RUN_TOKEN、完整 Secret env、终端输入、未脱敏 argv。

Broker/Core 任一方崩溃时：

- 新 Root 请求全部失败关闭；
- 已启动 Root Agent 按独立进程/Job/process-group 记录继续或由既有清理策略结束；
- Core 恢复后不得假定旧 Root capability 仍有效；
- 失去 live PTY 的 Run 进入 Needs You。

## 验收门槛

- public origin、错误 Host/Origin、无 cookie/CSRF、Observer、旧 lease/epoch 全部拒绝；
- DNS rebinding 与 LNA permission 不可绕过 Host/Origin/session；
- 猜测 Project/Session/PID/路径不能访问资源；
- Web 响应和 WS frame 不含 Secret/MF_RUN_TOKEN；跨 chunk redaction 测试通过；
- Root Broker pipe ACL、错误 SID、错误 PID、旧 nonce/epoch、重放 request 全部拒绝；
- Core 保持普通权限；Root Mode 关闭/重启后不能启动新 Root 任务；
- 已存在 Root Agent 在 UI、tray、Run/Session snapshot 中持续可见；
- Root Mode 关闭后 Broker 拒绝新请求并退出，已有 Root Session/Install Host 不被连带杀死，且不能派生新的 Root Job/Session；
- oversize frame、input flood、慢客户端、ACK 丢失不会阻塞工作流事件或 PTY reader。
