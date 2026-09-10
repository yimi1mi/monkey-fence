# ADR 0007: 多文件夹项目（一主多附）

状态: 已接受

 MonkeyFence 的项目不再等同于单一目录：一个项目登记**一个主文件夹（primary）与任意多个附加文件夹（additional）**，主/附加都可用于代码浏览、Git 面板与项目组织。此前"项目 = 一个目录"的定义由 ADR 0005 的 service 注册表继承而来（`project_registry.canonical_root UNIQUE`），本 ADR 将其推广为文件夹集合，同时保持全部执行与存储语义不变。

选择"一主多附"而不是对称的多根：项目数据库（`<primary>/.mf-agent/workflow-v1.db`）、Agent 执行目录租约的默认 cwd、工作流持久 pin key 全部继续锚定单一主文件夹——引入对称多根意味着项目库迁移、执行目录歧义与 pin key 不稳定，收益仅是命名上的对等。附加文件夹解决的是"一个项目跨多个代码目录"的组织与浏览需求，不改变运行语义。

约束与决定：

- **主文件夹即原 canonical_root**：创建项目时选定的目录，不可移除、不可更换（更换=数据迁移，另行决策）。注册表 `project_folders` 表为权威文件夹集合，每项目恰好一行 `primary`（v6 迁移从既有 `canonical_root` 行 backfill），任意多行 `additional`；`canonical_path` 全局 UNIQUE——**一个文件夹至多属于一个项目**，跨项目占用显式报错。
- **挂载幂等语义推广**：`POST /api/v1/projects` 收到的路径若已是某项目的任一文件夹，返回该项目本身（经主文件夹挂载），不为附加文件夹另开新项目；重复添加附加文件夹幂等。重复挂载已装配项目时执行面装配幂等跳过（不重启调度器）。
- **执行语义不变**：Agent 进程 cwd、终端会话目录、`ProjectDirectoryProvider` 租约继续取主文件夹。按节点/按工作流选择执行文件夹是明确的非目标（需要动 LaunchSpec、目录租约、图补丁冻结快照与 pin key，另行 ADR）。
- **授权与命令面不变**：文件夹增删是 service 注册表级操作，经 Controller 门控的 `POST/DELETE /api/v1/projects/{handle}/folders`；项目内命令、快照、run capability 继续按 opaque project handle 寻址，与路径无关。
- **快照 additive**：`WorkspaceProjectSnapshot.folders`（primary 恒在首位）为新增字段，旧客户端忽略；`display_root` 保持为主文件夹路径。

本 ADR 修改 CONTEXT.md 的"项目"词条；不改变 ADR 0001–0006 的其余条款。ADR 0001 中"Agent 在项目工作目录直接执行"指主文件夹，继续成立。
