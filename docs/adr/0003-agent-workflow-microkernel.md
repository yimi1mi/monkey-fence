# ADR 0003: Agent 工作流采用微内核与能力插件

日期: 2026-08-28
状态: 已接受

MonkeyFence 将工作流模型、DAG 调度状态机、运行事件、插件生命周期和安全边界保留在最小内核中;Agent 适配、节点类型、执行目录、Secret 存储、模板和声明式 UI 均由统一插件贡献。第三方插件通过独立 worker 与版本化 NDJSON 协议运行,只能贡献宿主渲染的声明式 UI,不得注入任意 GPUI 或本地动态库代码。

Task 与 Pipeline Revision 继续遵守 ADR 0001 的 VCS 解耦原则。需要隔离时,Agent Run 通过 Execution Directory Provider 获得路径租约;Git 插件可以在实现内部使用临时 worktree,但内核只识别路径、租约和合并结果,VCS 状态不参与 Task 成功判定。Agent Instance 配置只保存在 MonkeyFence 内部,运行时以进程级参数、环境或临时配置目录注入,绝不改写真正 CLI 的全局配置。

该决策拒绝两种替代方案:把所有行为硬编码进宿主会让新增 CLI 和隔离策略持续扩大核心接口;允许插件注入任意 UI/动态代码则破坏权限审查、跨平台兼容和宿主稳定性。代价是插件界面必须版本化,高级 UI 只能由宿主控件与声明式 Schema 组合。
