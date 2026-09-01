# Issue tracker: GitHub

本仓库的需求、PRD、Wayfinder 地图和决策 tickets 使用 GitHub Issues。优先使用已连接的 GitHub 插件；本机安装 `gh` CLI 时也可使用下列等价命令。

## 常规操作

- 创建：`gh issue create --title "..." --body "..."`
- 读取：`gh issue view <number> --comments`
- 列表：`gh issue list --state open --json number,title,body,labels,assignees`
- 评论：`gh issue comment <number> --body "..."`
- 标签：`gh issue edit <number> --add-label "..."` 或 `--remove-label "..."`
- 关闭：`gh issue close <number> --comment "..."`

GitHub 插件调用必须显式使用仓库 `yimi1mi/monkey-fence`。`gh` 在仓库 checkout 内可根据 `git remote -v` 自动识别它。

## Pull Requests 作为需求入口

**否。** PR 不进入默认 triage 队列。

## Skill 约定

- Skill 要求“发布到 issue tracker”时，创建 GitHub Issue。
- Skill 要求“读取 ticket”时，使用 `gh issue view <number> --comments` 并读取标签。
- Spec 和实现 ticket 使用 `ready-for-agent` 表示已可由 AFK agent 执行。

## Wayfinding operations

- **Map**：单个 GitHub Issue，标签为 `wayfinder:map`，保存 Destination、Notes、Decisions so far、Not yet specified 和 Out of scope。
- **Child ticket**：优先使用 GitHub sub-issue 关联到 map；若仓库未启用 sub-issue，则在 map 的 task list 中列出，并在 ticket 顶部写 `Part of #<map>`。
- **Ticket labels**：使用 `wayfinder:research`、`wayfinder:prototype`、`wayfinder:grilling` 或 `wayfinder:task`。
- **Blocking**：优先使用 GitHub 原生 issue dependencies。若不可用，在 ticket 顶部使用 `Blocked by: #<n>, #<n>`。
- **Frontier**：map 下所有 open、无未关闭 blocker、无 assignee 的 tickets。
- **Claim**：`gh issue edit <n> --add-assignee @me`，必须是一次决策会话的第一项写操作。
- **Resolve**：把答案写入 resolution comment，关闭 ticket，再把一行摘要链接追加到 map 的 Decisions so far。

GitHub 原生 dependency 通过 API 建立：

```text
gh api --method POST repos/<owner>/<repo>/issues/<child>/dependencies/blocked_by -F issue_id=<blocker-database-id>
```

其中 `<blocker-database-id>` 来自：

```text
gh api repos/<owner>/<repo>/issues/<number> --jq .id
```

若 GitHub API 对 sub-issue 或 dependency 返回不支持，立即使用正文约定回退，不要伪造关系。
