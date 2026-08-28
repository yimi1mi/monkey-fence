# ADR 0001: Task 生命周期与版本控制完全解耦

日期: 2026-08-27
状态: 已接受

## 背景

旧模型(MonkeyFence v1)以「工作项(Work Item)」为一级对象,每个工作项绑定一个版本控制隔离位置(Git worktree + 分支,或 Perforce changelist),Agent 执行、变更集与交付都挂在该绑定上。这带来三个问题:

1. 创建任务的先决条件是建立 VCS 隔离(worktree/CL),在没有仓库或仓库状态复杂时任务无法开展。
2. 任务状态机与 VCS 状态纠缠,任务"完成"被隐式定义为"变更已交付"。
3. 多项目并行时,每项目各自不同的 VCS 后端让调度逻辑分叉。

## 决策

Task 是项目内的一级目标,其生命周期(`draft / ready / running / needs-you / succeeded / failed / cancelled / archived`)由 Agent Run 的结算与人工决策驱动,**与 Git、P4、worktree、分支、变更集零耦合**:

- 创建 Task 不触碰任何 VCS 状态。
- Step 结算(成功/失败)不检查变更集或提交。
- 变更集(Change Set)与版控面板作为独立视图继续存在,只读地观察文件系统变化,不参与 Task 成程条件。
- 交付(Delivery)是用户在版控面板中的显式动作,与 Task 终态之间没有自动流转。

## 迁移

- 旧 `runs` 表迁移为新 Task,旧 `tasks` 迁移为新 Step,旧 `dispatches` 迁移为新 Agent Run。
- `work-items.json` 兼容导入一次,忽略 `vcs_ref`;JSON 原文件保留但迁移后停止写入。

## 后果

- 任务可以在任何目录(含非仓库)创建。
- Agent 在项目工作目录直接执行,文件变化由变更集视图事后观察。
- 旧 worktree 管理功能退出任务流,保留在版控面板一侧。
