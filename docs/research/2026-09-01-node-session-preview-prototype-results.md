# Node Session Panel 与 Preview Session 技术原型结果

> 后续决策覆盖：本文当时把 Rust checkpoint 原型列为正式实现前置；`2026-09-01-web-api-terminal-protocol-v1.md` 已收敛为 v1 不发送 checkpoint，改用 replay ring + durable read-only transcript + explicit restart。因此 checkpoint 不再是 v1/GPUI 退役 gate，本文的真实 CLI/IME/resize/洪泛与千节点结论继续有效。

- 日期：2026-09-01
- Wayfinder ticket：[验证节点会话面板与 Preview Session 交互](https://github.com/yimi1mi/monkey-fence/issues/7)
- 平台：Windows / ConPTY / Node.js 24
- 结论：Web Terminal + 真实 Agent CLI 路径可行；React Flow 对 MonkeyFence 预期 DAG 规模有余量；人工 IME 已验证通过，Terminal v1 的正式恢复契约以后续 API 决策为准。

## 原型资产

- 信息架构与交互：`crates/mf/prototypes/workflow-dag-editor-prototype/`
- 真实 PTY/xterm：`crates/mf/prototypes/web-terminal-pty-prototype/`
- React Flow/Dagre 规模基准：`crates/mf/prototypes/react-flow-benchmark-prototype/`

所有目录均为 throwaway prototype，不是生产实现。

## 真实 Agent PTY

使用 production Vite bundle、`@xterm/xterm` 6.0.0、`@xterm/addon-fit` 0.11.0、`node-pty` 1.1.0 和 `ws` 8.21.0 建立临时 loopback PTY 桥。正式实现仍由 Rust Core Service 持有 PTY。

### Production build

```text
vite v7.3.1
6 modules transformed
JS 334.28 kB / gzip 84.99 kB
build 约 0.5s
```

xterm.js 6.0.0 没有在本原型的 Vite/esbuild production minification 路径中触发构建或启动崩溃。该结果只覆盖本原型 bundle，不能替代正式应用回归。

### Codex

- 启动真实 Codex CLI 0.151.0：通过。
- 原生 alternate-screen/TUI、颜色、光标：通过。
- 在 xterm 输入 `/skills`：Codex 自己显示 Skills 选择器；Web 未解析命令。
- 初始 resize：xterm `180×79` 已传入 ConPTY。
- 输出 seq/ACK 同步推进：通过。

### Claude Code

- 启动真实 Claude Code 2.1.248：通过。
- 经用户确认项目 trust prompt 后进入主界面：通过。
- 在 xterm 输入 `/`：Claude 自己显示 Slash/Skill 列表；Web 未解析命令。
- 已安装 Superpowers skills 可在原生列表中显示：通过。

### 单 Writer、多观察者

```text
第二标签 attach：第一标签 observer，第二标签 writer
第一标签 Take writer：第一标签 writer，第二标签 observer
```

角色切换无需杀死 PTY，满足 Node Session Panel 多观察者 + 单输入所有者模型。

### Resize

临时调整浏览器 viewport：

```text
180×79 → 116×34 → 180×79
```

每次尺寸变化均触发真实 CLI 重绘和 seq 增长，证明 FitAddon → WebSocket control → node-pty/ConPTY resize 路径有效。

### Replay 与 Agent 切换

```text
Codex 刷新后从 seq 0 replay 到 1867
切换 Claude 后从该 Session 的 seq 0 replay 到 28
同 Claude 断连重连只补增量到 29
```

原型中发现并修复了一个错误：单一全局 cursor 会把 Codex 的 `lastSeq` 带给 Claude。修复后 cursor/重放语义按 Agent Session 隔离；同 Session 重连保留 xterm buffer，只补增量；切换 Session 或刷新页面从 seq 0 重放。

### 输出洪泛与中断

Stress Fixture 以受控速率持续输出，不调用模型。

```text
洪泛期间：seq 510 == ack 510
继续洪泛：seq 1255 == ack 1255
长时间运行后：seq 18319 == ack 18319
发送原始 0x03 前：seq 18319
进程停止后：seq 33447 == ack 33447，随后 600ms 保持不变
```

页面在洪泛期间仍可交互，ACK 没有落后。浏览器自动化的 `Control+C` 被快捷键层截获；原型增加仅用于验证的 “Send Ctrl+C” 按钮，发送原始 `0x03` 后 Node 进程以 Windows Control-C 状态退出。

人工验收仍需在真实浏览器中按 Ctrl+C，不能只以按钮路径替代。

## React Flow / Dagre 规模

使用 production Vite bundle、React 19.2.0、React Flow 12.11.0、Dagre 1.1.8。节点为 220×88 的 MonkeyFence 风格富 DOM 卡片，启用 `onlyRenderVisibleElements`。

| 规模 | Dagre layout | 实际 DOM 节点 | 边 |
| ---: | ---: | ---: | ---: |
| 100 | 11.4–16.3ms | 100 | 99 |
| 500 | 49.5ms | 105 | 499 |
| 1000 | 104.6–124.7ms | 105 | 999 |

1000 节点 fit-view 下只有约 105 个节点进入 DOM。自动化中一次可见节点拖动操作约 38ms（包含 Browser CUA 调度，不能视作纯前端 frame time），交互完成且 DOM 数不增长。

React production build 的 Profiler `actualDuration` 为 0，不可作为渲染指标；正式性能门槛应使用浏览器 Performance trace、requestAnimationFrame drag latency 和 heap，而不是当前页面的 Profiler 值。

## 已验证的产品交互

- 顶层“工作流 / 运行”，Needs You 是运行过滤入口。
- 三栏 Workbench：Agent 库、DAG、Inspector。
- 单击节点选中；端口连线；拖动与双击冲突已修复并有浏览器回归。
- 编辑节点双击：Preview Session Terminal。
- 运行节点双击：正式 Agent Session Terminal。
- Provider Profile → Agent Instance → Active Session 配置层次。
- CC Switch 风格 Provider 配置与模型获取下拉交互。

## 验证状态与正式实现项

1. Windows 真实键盘路径与中文 IME 已由用户验收；macOS/Linux 输入法留到对应真实平台 bring-up，不阻塞 Windows 首发。
2. node-pty 回调是 JavaScript string，不是生产 Rust raw-byte data plane；正式协议仍须 binary frame + seq/ACK。
3. Terminal v1 已决定不提供 checkpoint；超出 retained history 时走 `history_gap`、read-only transcript 与 explicit restart。
4. 原型无鉴权、CSP、Secret、Root Mode 或权限隔离，不能暴露到 loopback 之外。
5. React Flow 正式性能验收需要 Performance trace 和内存基准；当前数据只证明技术方向可行。

## 决策建议

- Web DAG：继续采用 React Flow + Dagre，保留 LayoutEngine/ELK 升级接口。
- Web Terminal：继续采用锁定版本的 xterm.js。
- PTY：正式实现复用并服务化 Rust `pty_spawn` / Session Registry。
- 协议：workflow event 与 terminal data plane 分离；terminal 使用 binary + seq/ACK/control。
- 恢复：浏览器 detach/reconnect 必须保证；Core 服务崩溃后的 live PTY reattach 不承诺。
- #7 已完成人工 IME/真实键盘确认、记录 Resolution 并关闭。
