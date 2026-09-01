# Web API v1 与 Terminal Protocol v1 契约

- 日期：2026-09-01
- Wayfinder ticket：[确定 Web API v1 与终端协议的版本化契约](https://github.com/yimi1mi/monkey-fence/issues/9)
- 依赖：#4 状态所有权、#5 信息架构、#8 Web 安全、#10 Agent Plugin/CLI 安装
- 状态：可进入规格化与实现

## 决策摘要

Rust Core Service 是唯一事实源。Web v1 使用三条隔离的数据面，不提供 GraphQL、通用 CRUD、JSON Patch 或 SSE：

| 数据面 | 协议 | 用途 |
| --- | --- | --- |
| HTTP JSON | `/api/v1/*` | 一致 Snapshot、封闭领域命令、安装预览、Operation/Job 查询 |
| Workflow WebSocket | `mf-workflow.v1` | 跨项目领域事件、Controller 变化、Operation 进度与恢复 |
| Terminal WebSocket | `mf-terminal.v1` | 单个 Agent Session 的 VT 字节、replay、ACK、input、resize、writer lease |

终端数据不进入 workflow event，领域命令不走 Terminal WebSocket。Web 不从日志或事件重新运行 Rust 状态机；事件携带 Rust 已确认的权威投影替换、tombstone 或封闭 typed delta。

## HTTP 资源边界

最低路由：

```text
POST /auth/exchange
GET  /api/v1/meta
GET  /api/v1/snapshots/workspace
GET  /api/v1/projects/{project_handle}/workflows/{workflow_handle}
GET  /api/v1/runs/{run_handle}
GET  /api/v1/agent-catalog
GET  /api/v1/installations/{installation_handle}
GET  /api/v1/operations/{operation_handle}
GET  /api/v1/sessions/{session_handle}/terminal-transcript
POST /api/v1/commands
POST /api/v1/controller/takeover
WS   /api/v1/events?client_id={opaque_client_id}
WS   /api/v1/terminal?client_id={opaque_client_id}
```

`client_id` 不是认证凭据，只用于把同源、已认证 cookie session 映射到 bootstrap client；服务端必须同时验证 cookie、精确 Host/Origin 和 client 归属。URL/query 不包含 bootstrap nonce、CSRF、Controller lease、Secret 或 Broker capability。已认证 HTTP resource 可以在 path 中使用 opaque Session handle；但 Terminal WebSocket URL/query 不含 Session handle，live attach 目标只在升级后的首个 control frame 中发送。

所有资源引用都是 Core 生成的 opaque handle。Web API 不接受数据库 rowid、PID、用户任意提交的 executable/path/raw command 作为授权目标。已授权详情页可以返回脱敏 provenance（例如 Project/CLI 的 `display_path`、安装 argv 摘要和 receipt canonical executable），但这些字符串只用于展示，不能在后续命令中代替 opaque handle。错误 Project/Session/PID/path 猜测对外统一成不可区分的 404。

除静态 UI asset 与 `/auth/exchange` 外，`/api/v1/meta`、所有 Snapshot/Operation 和两个 WebSocket 均要求已认证 session；meta 不是绕过 bootstrap 的匿名探测口。

## 认证与版本协商

`/auth/exchange` 沿用 #8 的 URL fragment 一次性 nonce：交换后 nonce 失效，设置 HttpOnly + SameSite=Strict cookie，并返回内存态 CSRF token、client id 与协议元数据。

```json
{
  "schema": "mf.auth-bootstrap.v1",
  "client_id": "opaque",
  "csrf_token": "memory-only",
  "controller": { "role": "controller", "lease_epoch": "17" },
  "api_versions": ["v1"],
  "ws_subprotocols": ["mf-workflow.v1", "mf-terminal.v1"],
  "core_build": "...",
  "ui_asset_build": "..."
}
```

- 所有写 HTTP 请求要求 cookie、Origin/Host、CSRF header、client id 与 Controller lease epoch。
- WebSocket upgrade 验证 cookie、Origin、client id 归属和精确 subprotocol；不接受通配协议或跨源连接。
- HTTP major 放在 `/api/v1`；WS major 放在 subprotocol。
- Web 先用 bootstrap/meta 求版本交集；无交集时不尝试 WS upgrade。浏览器在 upgrade 失败时通常读不到服务端 HTTP problem body，不能靠握手错误承载友好升级说明。
- v1 内只增加可选字段、新资源和新非关键事件。改变字段语义、幂等/CAS、terminal binary header 或恢复规则必须升 v2。
- 未识别且标记 `projection_critical=true` 的事件必须 resync，不能忽略后继续显示旧状态。
- Core/UI bundle 通常同版本发布；协议仍保留至少一个迁移发布周期的明确拒绝/升级提示，不在 v1 subprotocol 中偷换布局。

## Snapshot 一致性

统一 envelope：

```json
{
  "schema": "mf.snapshot.v1",
  "server_instance_id": "opaque",
  "cursor": {
    "stream_epoch": "opaque",
    "through_seq": "1842"
  },
  "data": {}
}
```

所有可能超过 JavaScript safe integer 的 row-independent sequence、epoch 和 revision 在 JSON 中使用十进制字符串。

每个 `data` 内的 aggregate 自带 revision；Workspace Snapshot 包含多个资源时不得伪造一个顶层 resource revision。单资源 Snapshot 也沿用相同结构，避免两套恢复代码。

Snapshot 必须在 Core 的 projection barrier 下形成一致 cut。一次领域事务的顺序是：

```text
validate/CAS → Store commit → append event journal → fan-out
```

事务提交后若 journal publication 无法完成，Core 旋转 `stream_epoch` 并强制所有客户端 resync；不能继续用缺事件的旧 epoch。

CAS、Controller/Root lease 复验、Store commit、journal seq 分配/append 与 epoch rotate 位于同一串行 publication barrier；Snapshot 也在该 barrier 上读取 cursor。它不能观察到“Store 已提交、但 through_seq 仍指向旧投影”的中间状态。HTTP 请求在进入 Store transaction 的线性化点再次复验 lease，而不是只在路由入口检查。

客户端恢复：

1. 完成 auth exchange；
2. 获取所需 Snapshot，保存 `stream_epoch + through_seq`；
3. 连接 `mf-workflow.v1`，发送 `resume(after_seq=through_seq)`；
4. journal 覆盖时补发所有 `seq > through_seq` 的事件；
5. epoch 不同或 journal gap 时返回 `resync_required` 并关闭连接；
6. 客户端重新获取 Snapshot，不能靠局部缓存猜测缺失状态。

Core 不是 Event Sourcing。Workflow event journal 是有界的投影恢复流，持久业务状态仍在 Store。

## Revision 与 CAS

每个 aggregate 独立单调 revision：Workflow Run、Agent Session、Agent Catalog、CLI Installation、Installation Job、Provider Profile、Agent Instance、Root State 各自维护 `revision`。

Project 还维护 `workflow_collection_revision`，作为创建/删除工作流与列表排序的父 aggregate CAS。工作流 rename 归入 Presentation Revision；它不改变执行语义。创建命令 CAS Project collection，删除同时 CAS collection 与目标 Workflow。

Project Workflow 拆成两个轴：

- `semantic_revision`：节点类型、配置、依赖、Agent 指派、运行策略；
- `presentation_revision`：节点坐标、viewport、折叠/布局等不改变执行语义的内容。

命令必须为每个将被修改的轴提供 expected revision；同时修改多个 aggregate/axis 时全部 CAS 成功后才原子提交。`workflow.run` 只接受 Core 已确认的 `semantic_revision`，presentation 变化不阻止运行。

当前 `project_workflows` 只有 graph/content digest，没有双 revision 与 presentation metadata；实现前需要增加存储 seam，不能把 digest 伪装成并发控制 revision。

## 领域命令

统一入口只承载封闭的、版本化 Rust enum：

```json
{
  "schema": "mf.command.v1",
  "command_id": "uuid-v7",
  "client_id": "opaque",
  "controller_lease_epoch": "17",
  "target": {
    "kind": "project_workflow",
    "handle": "opaque"
  },
  "expected": [
    {
      "aggregate": { "kind": "project_workflow", "handle": "opaque" },
      "revisions": {
        "presentation_revision": "91"
      }
    }
  ],
  "type": "workflow.move_node",
  "payload": {
    "node_handle": "opaque",
    "x": 420,
    "y": 180
  }
}
```

命令族：

- Workflow：create/rename/delete、add/update/remove/move node、connect/disconnect、viewport、unsafe-parallel policy；
- Run：start/cancel、retry step、respond、settle；
- Session：start/stop Preview/Ad-hoc Session，terminal attach/input 不走该入口；
- Catalog：refresh discovery、Provider model probe、Provider Profile、Agent Instance；
- CLI：preview/install/update/repair/uninstall/cancel installation job；
- Root：enable/disable；
- Controller：takeover 是特殊认证操作，不需要旧 Controller lease，但需要该 authenticated observer 的 CSRF。

`expected` 始终是 aggregate 列表；只改一个对象也保留数组形态。命令如果会原子修改或以 Run、Step、Session 等对象作为并发前提，必须列出每个已存在 aggregate 的 expected revision；遗漏或任一不匹配都拒绝整条命令。新建对象没有 expected revision，但创建它的父 aggregate 仍需 CAS。

响应：

- `200 applied`：事务已持久化，响应带最新 resource revision；
- `202 accepted`：长任务已创建，返回 `operation_handle`，结果通过 HTTP 查询和 workflow events 投影；
- `409`：revision/command reuse/operation conflict；
- 浏览器超时不等于命令失败。

幂等规则：

- 相同 `command_id + canonical request digest` 返回原结果；
- 相同 command id、不同 digest 返回 `command_id_reused`；
- 安装、运行、Settlement、Root enable 在结果不确定时只能用原 command id 重试，不能自动生成新 id；
- command receipt 持久化跨 Core 重启，最终结果至少保留 30 天；未终结 Operation 与被审计引用的 receipt 不清理。

canonical request digest 只覆盖稳定业务语义：`schema/type/target/expected/payload` 的规范编码；明确排除 cookie/CSRF、client id、Controller lease epoch、trace id 和请求时间。服务端每次重试都先校验当前认证、Controller/Root capability，再查幂等 receipt；旧 client 不能借 command id 绕过新 lease。

Secret 创建/替换命令的专用请求 body 可以包含明文 Secret，这是 write-only 输入的唯一例外：请求禁止 access-log/cache/回显，解析后立即交给 Secret Store。计算 semantic digest 时把 Secret 字段替换为使用持久 service idempotency key 的 HMAC；receipt 只存 HMAC 和脱敏结果，不存原文或可复用 secret ref。响应、事件、日志、journal、Snapshot 和 receipt 永远不含 API Key/installer credential 明文。

同一 command id 在 Operation 仍运行时返回同一个持久 `operation_handle` 与当前状态；终结后返回同一最终结果。Operation handle、command receipt 和 install plan execution record 跨 Core 重启稳定，重启时进入 `reconciling` 而不是换 handle。Root Mode 等进程生命周期能力的旧结果会携带原 `server_instance_id/root_epoch`，重启后不能被解释为当前仍有效；需要新语义动作时必须使用新 command id。

CLI 执行命令只能引用 Core 预览生成的短期 `install_plan_handle + recipe_digest + catalog_revision`，浏览器不能提交解析后的 argv。Root 命令只引用领域对象，不携带 broker nonce/MAC/capability。

## Workflow Event v1

连接握手：

```text
Client → events.resume.v1(stream_epoch?, after_seq)
Server → events.hello.v1(stream_epoch, first_available_seq, next_seq, controller_role/epoch)
Server → mf.event.v1 ...
Server → events.problem.v1(resync_required)? + close
```

首帧必须是 resume，且必须携带刚获取 Snapshot 的 cursor；空 epoch/未知 cursor 返回 `resync_required`。Server 在 hello 前不发送领域事件。workflow WS 是全局跨项目流；v1 的同一 OS-user session 可见全部已登记 Project，Server 仍验证 handle 属于该用户 Core，但不因 Project 过滤事件或制造 seq 空洞。

```json
{
  "schema": "mf.event.v1",
  "stream_epoch": "opaque",
  "seq": "1843",
  "occurred_at": "2026-09-01T08:00:00Z",
  "aggregate": {
    "kind": "project_workflow",
    "handle": "opaque"
  },
  "base_revision": {
    "semantic_revision": "12",
    "presentation_revision": "91"
  },
  "aggregate_revision": {
    "semantic_revision": "12",
    "presentation_revision": "92"
  },
  "caused_by_command_id": "uuid-or-null",
  "type": "workflow.node_position_set",
  "projection_critical": true,
  "projection": {
    "mode": "typed_delta",
    "delta_type": "workflow.node_position_set",
    "data": { "node_handle": "opaque", "x": 420, "y": 180 }
  }
}
```

- `projection.mode` 首版支持 `replace`、`tombstone` 和封闭白名单的 `typed_delta`；typed delta 携带完整新 aggregate revision，只表达 Rust 已确认的精确投影变化，不能使用通用 JSON Patch。
- `aggregate_revision` 是按 aggregate 类型区分的 revision vector：Project Workflow 携带 semantic + presentation，普通 aggregate 携带单一 `revision`。typed delta 另带客户端必须已经拥有的 `base_revision`。
- Workflow 节点移动等高频展示更新使用小型 typed delta，避免千节点整图重发；未知 `delta_type`、本地 revision 不等于 base，或新 revision 不连续时立即 resync。
- Web 收到 `run.step_changed` 只替换/应用 Rust 权威投影，不根据 Step 成功自行解锁下游。
- 每客户端 send queue 有界；慢客户端被要求 resync 或断开，不能阻塞其他客户端、Scheduler 或 PTY reader。
- seq 只在当前 `stream_epoch` 内单调；新 Core instance/不可修复 journal fault 旋转 epoch。v1 是“每 OS 用户一个 Core、该用户的已认证 Client 可见全部已登记 Project”的单一可见性域，因此同一 stream 不过滤隐藏事件、不制造 seq 空洞。未来若增加 Project ACL，必须引入 per-visibility stream 或显式 watermark 协议，不能直接过滤全局序号。
- tombstone 携带删除前 aggregate 的最终 revision；opaque handle 永不复用。
- keepalive/ping 不消耗领域 seq。

## Controller Lease

- Controller lease epoch 由 Core 单调生成，不是浏览器 token。
- 新 bootstrap client 成为 Controller，旧 Controller 降为 Observer；新客户端不会关闭旧连接。
- 同一 client 自动重连时，仅当 Core 仍记录它是 Controller 才恢复写权限，否则保持 Observer。
- Observer 可以显式调用 `/controller/takeover`；成功后 epoch 增加，旧 HTTP 写请求、Terminal writer lease 和尚未启动的手工高权限 job 立即失效。
- takeover 请求必须携带调用者最后观察到的 controller lease epoch；并发接管时仅一个 CAS 成功，其余收到 `controller_lease_expired` 后刷新。
- 已授权 Root Run 的后续节点仍按 #10 规则校验 active Root epoch；Controller 换手不自动取消 Run，但旧 client 不能继续写。
- Observer 可读取 Snapshot、workflow events、terminal output、installation progress/receipt；不能写 DAG、输入/resize 终端、Settlement、安装或 Root Mode。

## Terminal WebSocket v1

每个 WebSocket 只 attach 一个 Agent Session。`session_handle` 是 Agent Session record 的持久 opaque handle，在 transcript retention 期内跨 Core 重启仍可解析；`terminal_epoch` 只代表某次真实 PTY 生命周期。Preview Session 到期清理后 handle 才失效，且永不复用。连接顺序：

```text
Client → JSON attach(session_handle, terminal_epoch?, after_seq)
Server → JSON hello(terminal_epoch, first_available_seq, next_seq, alive, writer_state, cols, rows, limits)
Server → binary output replay
Client → JSON ack(through_seq)  # 仅在 xterm.write callback 完成后
Client → JSON request_writer
Server → JSON writer_granted(writer_lease_id, ttl_ms, renew_after_ms)
Client → JSON resize + binary input
Client → JSON writer_renew
Client → JSON release_writer?
Server → JSON input_ack / writer_renewed / writer_revoked / exit
```

### Binary frame

固定 32-byte network-order header：

```text
0..4    magic = "MFT1"
4       kind: 1=output, 2=input；3..255 保留
5       flags
6..8    reserved = 0
8..16   seq: u64 big-endian
16..32  writer_lease_id: UUID bytes；output 全 0
32..    raw bytes
```

- output 的 seq 属于 `terminal_epoch`；input 的 seq 是当前 writer lease 内单调 `input_seq`。
- output sequence 在跨 chunk Secret redaction 完成后、写 journal/fan-out 之前分配；每个 binary output message 占一个连续 seq。
- Server 必须拒绝 permessage-deflate；浏览器 JS 无法主动关闭该扩展。frame、input rate、outstanding bytes、journal 与每客户端 replay 都有硬上限。
- v1 不发送 checkpoint/kind 3。只有未来协议定义并验证完整的 VT state format、能力协商和 ACK 语义后才能启用，不能把纯文本 Screen 假装成原生 TUI checkpoint。

### Output、ACK 与恢复

- ACK 是 cumulative，只能确认服务端已发送且 xterm 已消费的连续 output seq；Web 必须在 `xterm.write(data, callback)` 的 callback 后 ACK。
- output seq 从 1 开始；空 Session 的 `next_seq=1`、`last_seq=0`、`first_available_seq=1`。`after_seq`/ACK 等于 0 表示尚未消费任何输出。
- `after_seq > last_seq` 或 ACK 高于该 client 已发送的最高 seq 是协议错误并关闭；重复 ACK 幂等，更小的旧 ACK 忽略。ACK 释放该 client 直到 through_seq 的 outstanding byte budget。
- PTY reader 永远持续 drain。ACK 只限制单客户端 send queue，不能把慢浏览器反压到 ConPTY/PTY reader 或 Scheduler。
- 相同 terminal epoch 且 journal 覆盖时增量 replay。
- 当 `after_seq < first_available_seq - 1` 时，Server 发送 `terminal_history_gap`（含 first/last/as_of），随后用 close code 4409 关闭；该连接不能申请 writer 或继续 live replay。
- Web 改读 `/terminal-transcript`：它返回已脱敏、UTF-8/行边界安全的只读 Screen/Transcript 投影、`as_of_seq` 与 `complete` 标记，不把任意 raw tail 填入 xterm。若 live Session 已发生 gap，用户必须显式重启 Session 生成新 PTY/epoch 才能恢复完整可写终端。
- 新 PTY 必须生成新 `terminal_epoch`，不能跨进程复用 seq。
- Core 重启后，除非 versioned Session Host reattach 明确成功，否则 PTY 标记 lost/Needs You，只保留只读转录，不伪装 live。

### Writer、input 与 resize

- writer lease 绑定 `client_id + controller_lease_epoch + WS connection + session_handle`，不可转移。
- 只有当前 Controller 可以申请。`writer_granted` 返回 server policy 的 `ttl_ms/renew_after_ms`；Client 用 `writer_renew` 显式续租，每次续租复验当前 Controller epoch。新 Controller、connection close、显式 release、未续租或 lease 超时都会撤销旧 writer。
- `release_writer(writer_lease_id)` 幂等；Server 用 `writer_revoked(reason="released")` 确认。重复 release 返回相同终态，不会误伤后来颁发的新 lease。
- attach/Observer 传入的浏览器尺寸不改变真实 PTY；只有 writer_granted 之后的 resize control frame 可以调整 PTY。
- input seq 每个 writer lease 从 1 开始。相同 seq + 相同 payload digest 的重发幂等返回原 input_ack；相同 seq + 不同 payload 是 `input_seq_conflict`，撤销 lease 并关闭。future/out-of-order seq 返回 expected seq，不能写入 PTY。
- Core 在把 input 放入该 Session 的单线程有序写队列时，原子复验 Controller epoch 与 writer lease；这是 Terminal input 的线性化点。takeover 不撤销此前已线性化的字节，但此后旧 lease 的任何 input/resize/renew 都拒绝。
- 网络结果不确定或断线后**绝不自动重放未确认 input**，避免重复执行命令、审批或 `/xxx`。
- `input_ack` 只在底层 PTY writer 完整 `write_all` 成功后发送，证明 OS writer 接受字节，不证明 Agent 执行成功。部分写/错误不 ACK，撤销 writer 并返回 terminal problem。
- resize 只有 writer 可发，包含单调 resize seq、cols/rows；陈旧值丢弃，尺寸有上下界。
- CLI 的 `/model`、`/skills`、审批、TUI、IME 和 Unicode 都是原始 PTY 输入/VT 输出，Web 不解释命令。
- PTY EOF/exit 时先调用 streaming redactor `finish`，把最后脱敏字节分配 seq、写 journal；随后把 transcript through final_seq 与 exit metadata durable commit，成功后才 fan-out 最后输出并串行发送 `exit(final_seq, code, signal)`。无输出时 final_seq=0。退出后的重连仍先 replay 到 final_seq，再发送相同 exit；持久化失败时不发送可恢复的正常 exit，而进入 terminal/session failure。
- exit 表示进程结束，不等于 Settlement；对应 Run 按现有状态机进入 awaiting-outcome/Needs You。
- live TerminalJournal 由“内存 replay ring + 用户 ACL 保护的增量 Transcript Store”组成。脱敏输出按 seq 周期性 durable flush，输入永不持久化；Core 崩溃后恢复到 `durable_through_seq`，Transcript 标记 `complete=false` 并明确可能缺少崩溃前尾部。正常 exit 则在释放 `PtySession` 前原子提交 through final_seq + exit metadata，标记 `complete=true`。具体容量、flush 周期和保留策略由 hello limits/meta 暴露并在 `/to-spec` 固定。

## Control frame

Control frame 使用 UTF-8 JSON，统一包含 `schema` 与 `type`。最低集合：

```text
terminal.attach.v1
terminal.hello.v1
terminal.ack.v1
terminal.request_writer.v1
terminal.writer_granted.v1
terminal.writer_denied.v1
terminal.writer_revoked.v1
terminal.release_writer.v1
terminal.writer_renew.v1
terminal.writer_renewed.v1
terminal.input_ack.v1
terminal.resize.v1
terminal.exit.v1
terminal.problem.v1
```

Server 必须先收到合法 attach 才发送 PTY 数据。未知 control type 在 v1 中返回 `invalid_envelope` 并关闭；不能忽略可能改变 writer/恢复语义的 frame。WebSocket ping/pong 由 Server 发起和统计，浏览器 JS 不承担发送或观察原生 ping/pong。

## Error v1

统一安全裁剪后的 problem body/frame：

```json
{
  "schema": "mf.problem.v1",
  "code": "revision_conflict",
  "message": "工作流已被更新",
  "trace_id": "opaque",
  "command_id": "uuid-or-null",
  "retry": "after_resync",
  "current": { "semantic_revision": "13" }
}
```

`retry` 固定枚举：`never`、`same_command_id`、`after_resync`、`after_reauth`、`after_retry_after`。

稳定错误码至少包括：

- 协议：`unsupported_api_version`、`unsupported_ws_subprotocol`、`invalid_envelope`；
- 认证：`unauthenticated`、`origin_rejected`、`csrf_rejected`；
- 角色：`controller_required`、`controller_lease_expired`；
- 资源：公开只返回 `resource_not_found`；内部可审计 `resource_scope_mismatch`，但不能把对象存在性暴露给 Web；
- CAS：`revision_conflict`、`command_id_reused`、`command_in_progress`；
- DAG：`validation_failed`、`workflow_cycle`、`unknown_dependency`；
- Agent：`agent_instance_unavailable`、`plugin_version_unavailable`、`cli_version_mismatch`；
- Terminal：`writer_required`、`writer_lease_expired`、`input_seq_conflict`、`terminal_epoch_mismatch`、`terminal_history_gap`、`frame_too_large`、`rate_limited`；
- Root/安装：`root_mode_required`、`root_epoch_expired`、`root_authorization_denied`、`broker_unavailable`、`elevation_required`、`installation_failed`；
- 服务：`resync_required`、`service_unavailable`、`internal_error`。

错误不返回内部 path、argv、Secret、MF_RUN_TOKEN、broker identity 或对象存在性差异。HTTP status 表达大类，`code + retry` 才是客户端分支依据。

已升级的 WebSocket 使用 application close code：4400 invalid envelope、4401 unauthenticated、4403 role/lease、4409 resync/history gap、4413 frame too large、4429 rate limited、4500 internal。没有共同 subprotocol 时 UI 根据 bootstrap/meta 直接显示 Core/UI 升级动作，不发起 WebSocket。

## 当前代码迁移点

- `runtime_host.rs` 当前 `PtySession` 只有固定 `Screen`、256 KiB `output_tail` 和 raw writer；需要新增 TerminalJournal、terminal epoch/seq、真实 resize、per-client replay/ACK/writer lease。
- 当前 legacy PTY reader 仍存在直接 `screen.feed/output_tail` 的未脱敏路径；另外两条 redactor 主要从 `plan.secret_env` 构造，而 `MF_RUN_TOKEN` 在之后单独注入，若 CLI echo 仍会进入终端。实现必须把所有 PTY 路径统一为 `raw PTY → streaming redactor(all Secret/capability values) → seq/journal → Screen/transcript → fan-out`，并删除任何旁路。
- `PtySession` 当前退出后即从 Registry 移除，Store 也没有 Terminal Transcript。需要新增增量 Transcript Store 与 crash-incomplete 标记；正常退出先原子持久化 through final_seq + exit metadata，再释放进程对象。
- `SessionRegistry` 当前用 `project path + rowid` 组合 key；Web 层必须增加随机 public handle 映射，不能泄露 path/rowid。
- `project_workflows` 当前只保存 graph/content digest；新增 semantic/presentation revision 与 presentation metadata。
- `pipe_server.rs`/`mfctl` 的 `MF_RUN_TOKEN` 本地 IPC 保持独立，绝不复用到 Web cookie、HTTP command 或 WS。
- throwaway Node 原型的 seq/replay/writer 结果只作为验证证据；生产实现必须在 Rust Core 中重新实现并做 contract tests。

## 验收门槛

- 并发命令下 Snapshot/event barrier 不丢更新；commit 后 journal fault 会旋转 epoch；
- 同 epoch replay、epoch mismatch、journal gap、慢客户端全部有确定结果；
- 相同 command id 跨断线/重启幂等，不同 payload 冲突；
- semantic/presentation CAS 独立，workflow.run 只认已确认 semantic revision；
- Observer、旧 Controller epoch、跨项目 opaque handle 全部拒绝写入；
- Root/install 只返回 Operation，浏览器拿不到 broker capability；install plan digest/catalog revision 有 TOCTOU 测试；
- Terminal output seq/ACK、input dedupe/不重放、writer revoke、resize、exit final seq 全覆盖；
- takeover 与已进入的 HTTP/PTY 写请求做线性化竞态测试；旧 lease 在线性化点之后不能写入；
- history gap 必须关闭 live WS、返回行边界安全的只读 transcript，且在 v1 绝不发送未协商 checkpoint；
- live transcript 增量持久化、Core crash 后 `complete=false/durable_through_seq`、正常 exit durable-before-notify 均有故障注入测试；
- writer TTL/renew、同 seq 异 payload、partial PTY write、redactor finish → final output → exit 顺序全覆盖；
- Codex/Claude/GLM 原生 slash/skill/TUI/IME/Unicode/resize/reconnect/洪泛通过；
- redaction 跨 chunk 且发生在 journal 之前；除专用 write-only Secret 请求 body 外，所有响应、事件、日志、journal、Snapshot、receipt 与 WS payload 不含 API Key、installer credential、MF_RUN_TOKEN；
- write-only Secret command 的 semantic HMAC 幂等、无 access-log/cache/回显和跨重启同 command id 测试通过；
- 错误码、HTTP status、retry class 与 envelope 做 golden contract tests；
- 不支持的 HTTP/WS major 明确拒绝并提示升级；
- 1000 节点 Snapshot/事件大小、事件风暴与 Terminal 洪泛互不饿死。
