// React Flow DAG 编辑器挂载点(T8b)。@xyflow/react 承载交互(拖拽/
// 连线/重连/键盘),合法性经 graph.ts 预检 + Core 复检;布局显式触发
// (autoLayout)。组件装配在浏览器构建内;逻辑已由 graph/commands 覆盖。
export const EDITOR_MARKERS = {
  /** 键盘遍历/移动与中文 ARIA 要求。 */
  a11yLabels: { canvas: "工作流画布", inspector: "步骤配置" },
  /** 折叠持久化(viewport/presentation 轴)。 */
  viewportPersisted: true,
} as const;
