# ADR 0002: Agent、流水线模板、技能和工具使用统一插件扩展缝隙

日期: 2026-08-27
状态: 已接受

## 背景

MonkeyFence 需要同时支持本地 CLI Agent(Codex、Claude、OpenCode、Cursor、Kimi)、API Agent(OpenAI 兼容/Anthropic/mock)以及未来第三方扩展。旧实现中:providers 硬编码在 config、skills 是独立目录约定、没有工具扩展点。若各走各的扩展机制,权限审查、启停管理、版本更新会各自为政。

## 决策

所有可扩展内容(Agent、流水线模板、技能、工具)统一通过插件(`mf-plugins` crate)贡献:

- 插件根清单 `monkeyfence-plugin.toml` 声明 publisher/id/version、MonkeyFence 最低版本、能力声明、可选后台 worker,以及 `agents / pipelines / skills / tools` 四类贡献。
- 内置内容(API providers、内置 CLI Agent、现有 skills)以**合成插件**(synthetic plugin)形式暴露,与第三方插件走同一注册表与同一权限模型。
- 安装流程统一:临时目录复制/克隆 → 清单与路径校验(拒绝逃逸) → 内容哈希 → 原子发布 → 锁文件记录(来源、版本、commit、哈希、授权指纹)。
- 新插件默认禁用;用户审查权限后才启用;worker、钩子、能力或说明变化改变授权指纹,要求重新授权;插件代码在授权前不得运行。
- 插件后台 worker 以独立进程 + NDJSON 协议运行;第三方插件首版不能注入任意 GPUI 界面。

## 权限边界

插件的权限声明只约束其与 MonkeyFence 宿主接口(工具调用、worker RPC)的交互;worker 进程与本地 CLI Agent 始终以当前 Windows 用户权限运行,设置页必须明示这一点。

## 后果

- 添加新 Agent = 添加一个插件条目(内置合成插件仅需声明),无宿主代码改动。
- 权限审查、启停、更新、删除对所有扩展类型一致。
- 现有 `~/.monkeyfence/config.toml` 的 providers 与 skills 目录继续可用,由兼容层转为内置 Agent Profile。
