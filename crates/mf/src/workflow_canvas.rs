//! Workflow 画布与节点检查器的 GPUI 视图(UI 计划 Task 2;设计 §11.2)。
//!
//! 布局 B(默认):左侧实例库 / 中间分层 DAG 画布 / 右侧可折叠检查器;
//! 布局 A:画布在上,库与检查器在下。保存草稿写入任务本地模板
//! (默认私有,不进全局列表;设计 §9.1)。

use crate::workflow_editor::{EditorNode, WorkflowEditorState, WorkflowLayout};
use gpui::prelude::*;
use gpui::{px, rgb, AnyElement, Context, FocusHandle, Window};
use std::sync::Arc;

/// 检查器状态(独立小模型,便于测试标题编辑)。
pub struct WorkflowNodeInspector {
    pub node_key: String,
    pub title_buffer: String,
    /// 折叠状态(可折叠检查器)。
    pub collapsed: bool,
}

impl WorkflowNodeInspector {
    pub fn new(node: &EditorNode) -> WorkflowNodeInspector {
        WorkflowNodeInspector {
            node_key: node.key.clone(),
            title_buffer: node.title.clone(),
            collapsed: false,
        }
    }
}

/// 工作流画布页(独立 GPUI 实体)。
pub struct WorkflowCanvas {
    pub app: Arc<crate::app_ctx::AppCtx>,
    pub editor: WorkflowEditorState,
    /// 实例库:(id, 名称)。
    pub library: Vec<(String, String)>,
    pub inspector: Option<WorkflowNodeInspector>,
    pub status: String,
    /// 当前任务(保存草稿的任务本地模板归属)。
    pub selected_task: Option<(std::path::PathBuf, i64)>,
    /// 非 Git 根的"共享目录并行"风险开关(持久化到项目 Store)。
    pub unsafe_parallel: bool,
    focus_handle: FocusHandle,
}

impl WorkflowCanvas {
    pub fn new(app: Arc<crate::app_ctx::AppCtx>, cx: &mut Context<Self>) -> WorkflowCanvas {
        let mut prefs = crate::workflow_editor::FilePrefs::default_path();
        let editor = WorkflowEditorState::load(&mut prefs);
        let mut canvas = WorkflowCanvas {
            app,
            editor,
            library: Vec::new(),
            inspector: None,
            status: String::new(),
            selected_task: None,
            unsafe_parallel: false,
            focus_handle: cx.focus_handle(),
        };
        canvas.refresh_library();
        canvas
    }

    pub fn refresh_library(&mut self) {
        self.library = self
            .app
            .catalog_store
            .list_agent_instances(None)
            .map(|rows| rows.into_iter().map(|i| (i.id, i.name)).collect::<Vec<_>>())
            .unwrap_or_default();
    }

    pub fn set_selected_task(
        &mut self,
        task: Option<(std::path::PathBuf, i64)>,
        cx: &mut Context<Self>,
    ) {
        self.selected_task = task.clone();
        // 加载该任务在该项目下的本地工作流草稿(project+task 双键,
        // 跨项目同 task id 互不串扰)
        if let Some((root, task_id)) = task {
            if let Some(orch) = self.app.orchestrator_of(&root) {
                self.unsafe_parallel = orch
                    .store
                    .task_workflow_unsafe_parallel(&root.to_string_lossy(), task_id)
                    .unwrap_or(false);
                match orch
                    .store
                    .load_task_workflow(&root.to_string_lossy(), task_id)
                {
                    Ok(Some(draft)) => {
                        self.editor.load_nodes(
                            draft
                                .nodes
                                .iter()
                                .map(|n| crate::workflow_editor::EditorNode {
                                    key: n.key.clone(),
                                    title: n.title.clone(),
                                    instance_id: n.agent_instance_id.clone(),
                                    deps: n.deps.clone(),
                                    instructions: n.instructions.clone(),
                                })
                                .collect(),
                        );
                        self.status = format!("已加载任务 {task_id} 的本地工作流草稿");
                    }
                    Ok(None) => {
                        self.editor.load_nodes(Vec::new());
                        self.status = format!("任务 {task_id} 暂无本地草稿(空白画布)");
                    }
                    Err(e) => self.status = format!("读取草稿失败: {e:#}"),
                }
            }
        }
        cx.notify();
    }

    /// 由编辑器状态构建任务本地草稿。
    fn current_draft(&self, task_id: i64) -> mf_agent::workflow::WorkflowTemplateDraft {
        mf_agent::workflow::WorkflowTemplateDraft {
            key: format!("task-{task_id}"),
            name: format!("任务 {task_id} 工作流"),
            task_local: true,
            nodes: self
                .editor
                .nodes()
                .iter()
                .map(|n| mf_agent::workflow::WorkflowNodeDraft {
                    key: n.key.clone(),
                    title: n.title.clone(),
                    instructions: n.instructions.clone(),
                    agent_instance_id: n.instance_id.clone(),
                    deps: n.deps.clone(),
                })
                .collect(),
        }
    }

    /// 保存草稿:写入项目 Store 的任务本地工作流(默认私有),
    /// 连同"共享目录并行"风险开关一起持久化。
    /// **返回 Result**:保存失败必须让调用方(编译/分配/确认运行)
    /// 中止 —— 绝不能继续读取旧 Store 草稿并运行。
    pub fn save_draft(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        if self.selected_task.is_none() {
            let msg = "请先选择一个任务(草稿保存为该任务的工作流)";
            self.status = msg.into();
            cx.notify();
            anyhow::bail!("{msg}");
        }
        if self.editor.nodes().is_empty() {
            let msg = "空工作流无法保存";
            self.status = msg.into();
            cx.notify();
            anyhow::bail!("{msg}");
        }
        let Some((root, task_id)) = self.selected_task.clone() else {
            unreachable!("上方已检查 selected_task");
        };
        // 任务本地草稿存项目 Store(project+task 双键,不进全局目录库)
        let Some(orch) = self.app.orchestrator_of(&root) else {
            let msg = "项目未打开,无法保存草稿";
            self.status = msg.into();
            cx.notify();
            anyhow::bail!("{msg}");
        };
        let draft = self.current_draft(task_id);
        let unsafe_parallel = self.unsafe_parallel;
        match orch.store.save_task_workflow(
            &root.to_string_lossy(),
            task_id,
            &draft,
            unsafe_parallel,
        ) {
            Ok(()) => {
                self.status = format!("已保存任务 {task_id} 本地草稿(项目内;可另存为全局模板)");
            }
            Err(e) => {
                self.status = format!("保存失败: {e:#}");
                cx.notify();
                anyhow::bail!("保存失败: {e:#}");
            }
        }
        cx.notify();
        Ok(())
    }

    /// 切换"共享目录并行"风险开关(立即持久化;非 Git 根并行需要它)。
    /// 持久化失败不再静默吞掉(错误进状态栏)。
    pub fn toggle_unsafe_parallel(&mut self, cx: &mut Context<Self>) {
        self.unsafe_parallel = !self.unsafe_parallel;
        if let Some((root, task_id)) = self.selected_task.clone() {
            if !self.editor.nodes().is_empty() {
                if let Some(orch) = self.app.orchestrator_of(&root) {
                    let draft = self.current_draft(task_id);
                    let flag = self.unsafe_parallel;
                    if let Err(e) = orch.store.save_task_workflow(
                        &root.to_string_lossy(),
                        task_id,
                        &draft,
                        flag,
                    ) {
                        self.status = format!("保存失败: {e:#}");
                        cx.notify();
                        return;
                    }
                }
            }
        }
        self.status = if self.unsafe_parallel {
            "已开启共享目录并行(自担冲突风险;Git 项目无需开启)".into()
        } else {
            "已关闭共享目录并行".into()
        };
        cx.notify();
    }

    /// 编译检查:按项目 Store 草稿干跑 Workflow Compiler(不写库)。
    pub fn compile_task_local(&mut self, cx: &mut Context<Self>) {
        let Some((root, task_id)) = self.selected_task.clone() else {
            self.status = "请先选择一个任务".into();
            cx.notify();
            return;
        };
        // 先保存当前编辑状态:编译检查的就是即将冻结的内容;
        // 保存失败必须中止(不得检查/读取旧 Store 草稿)
        if self.save_draft(cx).is_err() {
            return;
        }
        match self.app.compile_task_local_workflow(&root, task_id) {
            Ok(snapshot) => {
                self.status = format!("编译通过:{} 个节点已可冻结", snapshot.nodes.len());
            }
            Err(e) => self.status = format!("{e:#}"),
        }
        cx.notify();
    }

    /// 分配:冻结项目 Store 草稿为 Revision(编译 + 插件 pin + Step 投影)。
    pub fn assign_task_local(&mut self, cx: &mut Context<Self>) {
        let Some((root, task_id)) = self.selected_task.clone() else {
            self.status = "请先选择一个任务".into();
            cx.notify();
            return;
        };
        // 保存失败必须中止分配(不得基于旧 Store 草稿冻结 Revision)
        if self.save_draft(cx).is_err() {
            return;
        }
        match self.app.assign_task_local_workflow(&root, task_id) {
            Ok(rev) => {
                self.status =
                    format!("已冻结任务 {task_id} 工作流(Revision #{rev});确认运行后开始调度");
            }
            Err(e) => self.status = format!("{e:#}"),
        }
        cx.notify();
    }

    /// 确认运行:冻结项目 Store 草稿并立即开始调度(显式用户动作)。
    pub fn confirm_and_run_task_local(&mut self, cx: &mut Context<Self>) {
        let Some((root, task_id)) = self.selected_task.clone() else {
            self.status = "请先选择一个任务".into();
            cx.notify();
            return;
        };
        // 先保存当前编辑(脏内容进项目 Store),再走原子
        // 「分配并确认」:草稿未变时不重复冻结,assign 产生的 draft
        // 在确认前 active_revision 仍为 none —— 不用它判断是否已分配。
        // 保存失败必须中止:绝不能继续读取旧 Store 草稿并运行
        if self.save_draft(cx).is_err() {
            return;
        }
        match self.app.assign_and_confirm_task_local(&root, task_id) {
            Ok(()) => {
                self.status = format!("任务 {task_id} 已确认运行");
            }
            Err(e) => self.status = format!("{e:#}"),
        }
        cx.notify();
    }

    /// 另存为全局模板(提升到用户目录库,进入可分配列表)。
    pub fn save_as_template(&mut self, cx: &mut Context<Self>) {
        let Some((_root, task_id)) = self.selected_task.clone() else {
            self.status = "请先选择一个任务".into();
            cx.notify();
            return;
        };
        if self.editor.nodes().is_empty() {
            self.status = "空工作流无法另存为模板".into();
            cx.notify();
            return;
        }
        let draft = mf_agent::workflow::WorkflowTemplateDraft {
            key: format!("task-{task_id}-template"),
            name: format!("任务 {task_id} 工作流(模板)"),
            task_local: false,
            nodes: self
                .editor
                .nodes()
                .iter()
                .map(|n| mf_agent::workflow::WorkflowNodeDraft {
                    key: n.key.clone(),
                    title: n.title.clone(),
                    instructions: n.instructions.clone(),
                    agent_instance_id: n.instance_id.clone(),
                    deps: n.deps.clone(),
                })
                .collect(),
        };
        match self.app.catalog_store.save_template(&draft) {
            Ok(version) => {
                self.status = format!("已另存为全局模板(版本 {})", version.version);
            }
            Err(e) => self.status = format!("另存失败: {e:#}"),
        }
        cx.notify();
    }

    fn render_library(&self, cx: &Context<Self>) -> AnyElement {
        let mut list = gpui::div().flex().flex_col().gap_1();
        if self.library.is_empty() {
            list = list.child(
                gpui::div()
                    .text_size(px(9.))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child("还没有 Agent 实例:到「实例」页创建"),
            );
        }
        for (idx, (id, name)) in self.library.iter().enumerate() {
            let id2 = id.clone();
            list = list.child(
                gpui::div()
                    .id(gpui::ElementId::Name(format!("wf-lib-{idx}").into()))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .cursor_pointer()
                    .text_size(px(10.))
                    .child(name.clone())
                    .on_click(
                        cx.listener(move |canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                            canvas.editor.drag_from_library(&id2);
                            canvas.status = "已从实例库添加节点".into();
                            cx.notify();
                        }),
                    ),
            );
        }
        gpui::div()
            .id("wf-library")
            .flex()
            .flex_col()
            .gap_2()
            .child(
                gpui::div()
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::Theme::fg()))
                    .child("Agent 实例库"),
            )
            .child(
                gpui::div()
                    .id("wf-library-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    fn render_canvas(&self, cx: &Context<Self>) -> AnyElement {
        let layers = self.editor.autolayout();
        let mut rows = gpui::div().flex().flex_col().gap_3().p_2();
        if layers.is_empty() {
            rows = rows.child(
                gpui::div()
                    .text_size(px(10.))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child("画布为空:点击左侧实例添加节点,节点详情里连接依赖"),
            );
        }
        for (layer_idx, layer) in layers.iter().enumerate() {
            let mut row = gpui::div().flex().gap_2().items_center();
            row = row.child(
                gpui::div()
                    .text_size(px(8.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child(format!("层 {}", layer_idx + 1)),
            );
            for (key, node_idx) in layer {
                let selected = self.editor.selected() == Some(key.as_str());
                let node = &self.editor.nodes()[*node_idx];
                let deps_note = if node.deps.is_empty() {
                    String::new()
                } else {
                    format!(" ← {}", node.deps.join(","))
                };
                let key2 = key.clone();
                row = row.child(
                    gpui::div()
                        .id(gpui::ElementId::Name(format!("wf-node-{key}").into()))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(if selected {
                            crate::theme::Theme::accent()
                        } else {
                            crate::theme::Theme::border()
                        }))
                        .bg(rgb(if selected {
                            crate::theme::Theme::bg_active()
                        } else {
                            crate::theme::Theme::bg_elevated()
                        }))
                        .cursor_pointer()
                        .child(
                            gpui::div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(gpui::div().text_size(px(11.)).child(node.title.clone()))
                                .child(
                                    gpui::div()
                                        .text_size(px(8.))
                                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                                        .child(format!("{}{}", node.key, deps_note)),
                                ),
                        )
                        .on_click(
                            cx.listener(move |canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.editor.select(&key2);
                                if let Some(node) = canvas
                                    .editor
                                    .nodes()
                                    .iter()
                                    .find(|n| n.key == key2)
                                    .cloned()
                                {
                                    canvas.inspector = Some(WorkflowNodeInspector::new(&node));
                                }
                                cx.notify();
                            }),
                        ),
                );
            }
            rows = rows.child(row);
        }
        gpui::div()
            .id("wf-canvas")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .child(rows)
            .into_any_element()
    }

    fn render_inspector(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(inspector) = self.inspector.as_mut() else {
            return gpui::div()
                .w(px(260.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.))
                .text_color(rgb(crate::theme::Theme::fg_dim()))
                .child("点击画布节点查看详情")
                .into_any_element();
        };
        let node = self
            .editor
            .nodes()
            .iter()
            .find(|n| n.key == inspector.node_key)
            .cloned();
        let Some(node) = node else {
            self.inspector = None;
            return gpui::div().into_any_element();
        };
        if inspector.collapsed {
            return gpui::div()
                .id("wf-inspector")
                .w(px(28.))
                .flex()
                .flex_col()
                .items_center()
                .child(
                    gpui::div()
                        .id("wf-inspector-expand")
                        .text_size(px(9.))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .cursor_pointer()
                        .child("展开")
                        .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                            if let Some(i) = canvas.inspector.as_mut() {
                                i.collapsed = false;
                            }
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        // 依赖管理:选中节点可从其余节点中添加/移除依赖
        let other_keys: Vec<String> = self
            .editor
            .nodes()
            .iter()
            .filter(|n| n.key != node.key)
            .map(|n| n.key.clone())
            .collect();
        let mut deps_panel = gpui::div().flex().flex_col().gap_1();
        for key in &other_keys {
            let has_dep = node.deps.iter().any(|d| d == key);
            let key2 = key.clone();
            let node_key = node.key.clone();
            deps_panel = deps_panel.child(
                gpui::div()
                    .id(gpui::ElementId::Name(format!("wf-dep-{key}").into()))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .child(
                        gpui::div()
                            .text_size(px(9.))
                            .text_color(rgb(if has_dep {
                                crate::theme::Theme::accent()
                            } else {
                                crate::theme::Theme::fg_dim()
                            }))
                            .child(key.clone()),
                    )
                    .child(
                        gpui::div()
                            .id(gpui::ElementId::Name(format!("wf-dep-btn-{key}").into()))
                            .px_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(px(8.))
                            .cursor_pointer()
                            .child(if has_dep { "断开" } else { "依赖" })
                            .on_click(cx.listener(
                                move |canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                    if has_dep {
                                        canvas.editor.remove_dependency(&node_key, &key2);
                                        canvas.status = format!("已断开 {node_key} ← {key2}");
                                    } else {
                                        match canvas.editor.add_dependency(&node_key, &key2) {
                                            Ok(()) => {
                                                canvas.status =
                                                    format!("已连接 {node_key} ← {key2}");
                                            }
                                            Err(e) => canvas.status = e,
                                        }
                                    }
                                    cx.notify();
                                },
                            )),
                    ),
            );
        }
        let title = inspector.title_buffer.clone();
        let node_key = node.key.clone();
        let instance_name = self
            .library
            .iter()
            .find(|(id, _)| *id == node.instance_id)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| node.instance_id.clone());
        gpui::div()
            .id("wf-inspector")
            .w(px(260.))
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        gpui::div()
                            .text_size(px(11.))
                            .child(format!("节点 {}", node.key)),
                    )
                    .child(
                        gpui::div()
                            .id("wf-inspector-collapse")
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child("折叠")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                if let Some(i) = canvas.inspector.as_mut() {
                                    i.collapsed = true;
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        gpui::div()
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child("标题(点击后输入,Enter 确认)"),
                    )
                    .child(
                        gpui::div()
                            .id("wf-inspector-title")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(px(10.))
                            .cursor_pointer()
                            .child(title)
                            .on_click(cx.listener(
                                move |canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                    // 标题编辑经命令面板式弹层过重;这里直接进入
                                    // 键盘流(画布 key 处理 Enter 提交)
                                    if let Some(i) = canvas.inspector.as_mut() {
                                        i.title_buffer = i.title_buffer.clone();
                                    }
                                    let _ = &node_key;
                                    canvas.status = "标题编辑:输入后按 Enter".into();
                                    cx.notify();
                                },
                            )),
                    ),
            )
            .child(
                gpui::div()
                    .text_size(px(9.))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child(format!("实例:{}", instance_name)),
            )
            .child(
                gpui::div()
                    .text_size(px(9.))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child("依赖(点击连接/断开):"),
            )
            .child(deps_panel)
            .child(
                gpui::div()
                    .id("wf-inspector-delete")
                    .px_2()
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::danger()))
                    .text_size(px(9.))
                    .text_color(rgb(crate::theme::Theme::danger()))
                    .cursor_pointer()
                    .child("删除节点")
                    .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                        canvas.editor.delete_selected();
                        canvas.inspector = None;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}

impl Render for WorkflowCanvas {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layout = self.editor.layout();
        let diagnostics: Vec<String> = self.editor.diagnostics();
        let status = self.status.clone();
        let (library, canvas, inspector) = (self.render_library(cx), self.render_canvas(cx), {
            let any: AnyElement = self.render_inspector(cx);
            any
        });
        let body = match layout {
            WorkflowLayout::Sidebar => gpui::div()
                .flex_1()
                .min_h_0()
                .flex()
                .gap_2()
                .child(gpui::div().w(px(220.)).flex().flex_col().child(library))
                .child(canvas)
                .child(inspector),
            WorkflowLayout::Stacked => gpui::div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap_2()
                .child(canvas)
                .child(
                    gpui::div()
                        .h(px(180.))
                        .flex()
                        .gap_2()
                        .child(gpui::div().w(px(220.)).flex().flex_col().child(library))
                        .child(inspector),
                ),
        };
        let next_layout = match layout {
            WorkflowLayout::Sidebar => WorkflowLayout::Stacked,
            WorkflowLayout::Stacked => WorkflowLayout::Sidebar,
        };
        let toggle_label = match layout {
            WorkflowLayout::Sidebar => "切换为上下布局(A)",
            WorkflowLayout::Stacked => "切换为侧栏布局(B)",
        };
        let _ = self.focus_handle;
        gpui::div()
            .id("workflow-canvas-page")
            .size_full()
            .flex()
            .flex_col()
            .p_2()
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child("工作流编辑器"),
                    )
                    .child(
                        gpui::div()
                            .id("wf-layout-toggle")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child(toggle_label)
                            .on_click(cx.listener(
                                move |canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                    let mut prefs =
                                        crate::workflow_editor::FilePrefs::default_path();
                                    canvas.editor.set_layout(next_layout, &mut prefs);
                                    cx.notify();
                                },
                            )),
                    )
                    .child(
                        gpui::div()
                            .id("wf-save-template")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child("另存为全局模板")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.save_as_template(cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-save-draft")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::accent()))
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::accent()))
                            .cursor_pointer()
                            .child("保存草稿(任务本地)")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                // 保存结果已由 save_draft 写入状态栏展示
                                let _ = canvas.save_draft(cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-compile-local")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child("编译检查")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.compile_task_local(cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-assign-local")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::accent_dim()))
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child("分配(冻结 Revision)")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.assign_task_local(cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-confirm-local")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::accent()))
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::accent()))
                            .cursor_pointer()
                            .child("确认运行")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.confirm_and_run_task_local(cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-unsafe-parallel")
                            .px_2()
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if self.unsafe_parallel {
                                crate::theme::Theme::warning()
                            } else {
                                crate::theme::Theme::border()
                            }))
                            .text_size(px(9.))
                            .text_color(rgb(if self.unsafe_parallel {
                                crate::theme::Theme::warning()
                            } else {
                                crate::theme::Theme::fg_faint()
                            }))
                            .cursor_pointer()
                            .child(if self.unsafe_parallel {
                                "共享目录并行:已开启(风险)"
                            } else {
                                "共享目录并行:关闭"
                            })
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.toggle_unsafe_parallel(cx);
                            })),
                    )
                    .children(diagnostics.iter().map(|d| {
                        gpui::div()
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::warning()))
                            .child(d.clone())
                    })),
            )
            .child(body)
            .when(!status.is_empty(), |d| {
                d.child(
                    gpui::div()
                        .text_size(px(9.))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .child(status),
                )
            })
    }
}
