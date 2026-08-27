# MonkeyFence 变更交付

MonkeyFence 围绕一个用户可见的工作单元组织 AI 辅助开发，使它从意图、执行、审阅一直到版本控制交付保持同一上下文。

## 统一语言

**项目（Project）**:
MonkeyFence 打开的 Git 仓库或 Perforce client root。
_避免_: 用“仓库”指代 Perforce 项目

**工作项（Work Item）**:
一项需要交付的用户目标，例如功能、修复或重构；它是用户创建、选择、审阅和完成的一级对象。
_避免_: Task、Run、Worktree、Change

**工作区（Workspace）**:
工作项拥有的版本控制隔离位置：Git worktree 与分支，或 Perforce changelist 与可选 client。
_避免_: 工作项、项目

**执行（Run）**:
Agent 尝试完成一个工作项的一次执行。
_避免_: Task、Session

**步骤（Step）**:
一次执行计划中具有依赖关系的一个内部操作。
_避免_: Task

**变更集（Change Set）**:
工作项产生并等待审阅或交付的文件与 hunks。
_避免_: 工作区、Changelist

**交付（Delivery）**:
完成工作项的版本控制结果：Git commit、Perforce shelve 或 Perforce submit。
_避免_: 执行完成、Run result

**Agent 会话（Agent Session）**:
一次执行相关的消息、问题和工具活动。
_避免_: 工作项、终端
