//! Workflow 画布与节点检查器的 GPUI 视图(ADR 0004 / Task 5 起为
//! 项目工作流编辑器)。
//!
//! 布局 B(默认):左侧 Agent 库(检测到的默认 CLI + 保存配置) /
//! 中间分层 DAG 画布 / 右侧可折叠检查器;布局 A:画布在上。
//! 画布编辑的是项目工作流(project_workflows,跨重启保留),
//! 每个原子编辑动作后自动保存;「运行工作流」只表达意图
//! (WorkflowCanvasEvent::RunRequested),运行本体由 AgentWorkspace
//! 经 Run Composer 发起。

use crate::workflow_editor::{EditorNode, WorkflowEditorState, WorkflowLayout};
use gpui::prelude::*;
use gpui::{px, rgb, AnyElement, Context, EventEmitter, FocusHandle, Window};
use std::sync::Arc;

/// 画布 → AgentWorkspace 的单向意图事件(画布不自行运行/不建设置页)。
pub enum WorkflowCanvasEvent {
    /// 运行当前项目工作流(已确认保存成功后才会发出)。
    RunRequested {
        project_root: std::path::PathBuf,
        workflow_key: String,
    },
    /// 打开设置页的智能体配置(编辑状态不丢失)。
    OpenAgentSettings,
}

impl EventEmitter<WorkflowCanvasEvent> for WorkflowCanvas {}

/// 检查器状态(独立小模型,便于测试标题/指令编辑)。
#[derive(Clone)]
pub struct WorkflowNodeInspector {
    pub node_key: String,
    pub title_buffer: String,
    /// 节点工作说明缓冲(确认后作为原子编辑保存)。
    pub instructions_buffer: String,
    /// 折叠状态(可折叠检查器)。
    pub collapsed: bool,
}

impl WorkflowNodeInspector {
    pub fn new(node: &EditorNode) -> WorkflowNodeInspector {
        WorkflowNodeInspector {
            node_key: node.key.clone(),
            title_buffer: node.title.clone(),
            instructions_buffer: node.instructions.clone(),
            collapsed: false,
        }
    }
}

/// 左侧 Agent 库条目:默认 CLI 与保存配置视觉分组,拖入/选择都产生节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLibraryEntry {
    /// 检测到且启用的默认 CLI(引用 `default-cli:<完整贡献 ID>`)。
    DefaultCli {
        full_contribution_id: String,
        name: String,
    },
    /// 已启用的保存配置(引用实例 id)。
    Instance { id: String, name: String },
    /// 「管理智能体配置……」入口(单向事件链打开设置)。
    ManageSettings,
}

impl AgentLibraryEntry {
    /// 拖入/选择后产生的节点引用。
    pub fn node_reference(&self) -> Option<String> {
        match self {
            AgentLibraryEntry::DefaultCli {
                full_contribution_id,
                ..
            } => Some(format!(
                "{}{full_contribution_id}",
                crate::app_ctx::DEFAULT_CLI_REFERENCE_PREFIX
            )),
            AgentLibraryEntry::Instance { id, .. } => Some(id.clone()),
            AgentLibraryEntry::ManageSettings => None,
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            AgentLibraryEntry::DefaultCli { name, .. } => name,
            AgentLibraryEntry::Instance { name, .. } => name,
            AgentLibraryEntry::ManageSettings => "管理智能体配置……",
        }
    }
}

/// 检测到且启用的默认 CLI 投影(完整贡献 ID + 用户可见名)。
/// 未检测到的 CLI 不出现在画布选择器(设置页会解释原因)。
pub fn detected_default_cli_entries(app: &Arc<crate::app_ctx::AppCtx>) -> Vec<(String, String)> {
    let contributions = app.plugins.contributions();
    let summaries = app.plugins.summaries();
    contributions
        .agent_types()
        .into_iter()
        .filter(|(full_id, _, contribution)| {
            // 显示用户名,不把 contribution id 当主标题;命令为空不是 CLI
            !contribution.command.is_empty()
                && mf_plugins::builtin::detect_on_path(&contribution.command).is_some()
                && summaries
                    .iter()
                    .any(|s| s.enabled && s.agents.contains(&contribution.id))
                && full_id.rsplit('.').next() != Some("blank-terminal")
        })
        .map(|(full_id, _, contribution)| (full_id, contribution.name))
        .collect()
}

/// 工作流画布页(独立 GPUI 实体;项目工作流编辑器)。
pub struct WorkflowCanvas {
    pub app: Arc<crate::app_ctx::AppCtx>,
    pub editor: WorkflowEditorState,
    /// Agent 库:默认 CLI 分组在前、保存配置在后、管理入口收尾。
    pub library: Vec<AgentLibraryEntry>,
    pub inspector: Option<WorkflowNodeInspector>,
    pub status: String,
    /// 当前项目(项目工作流作用域);None = 尚未打开项目。
    pub project_root: Option<std::path::PathBuf>,
    /// 项目工作流列表投影:(key, 名称)。
    pub workflows: Vec<(String, String)>,
    /// 当前编辑的项目工作流 key(None = 未选择)。
    pub current_key: Option<String>,
    /// 当前工作流名称(重命名入口编辑)。
    pub workflow_name: String,
    /// 保存状态:None = 干净;Some(错误) = dirty 保存失败(Run 被阻止)。
    pub save_error: Option<String>,
    /// 全局模板列表 ((key, 名称));「从全局模板创建」入口。
    pub templates: Vec<(String, String)>,
    /// 从模板创建的弹层(打开时为待选模板 key 列表索引)。
    pub template_popover: bool,
    /// 当前工作流的共享目录并行风险开关(保存进项目工作流)。
    pub unsafe_parallel: bool,
    /// 重命名输入模式(键盘字符直接进入工作流名)。
    pub rename_mode: bool,
    /// 节点检查器当前文本输入；None 表示未编辑。
    inspector_field: Option<InspectorField>,
    focus_handle: FocusHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectorField {
    Title,
    Instructions,
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
            project_root: None,
            workflows: Vec::new(),
            current_key: None,
            workflow_name: String::new(),
            save_error: None,
            templates: Vec::new(),
            template_popover: false,
            unsafe_parallel: false,
            rename_mode: false,
            inspector_field: None,
            focus_handle: cx.focus_handle(),
        };
        canvas.refresh_library();
        canvas
    }

    pub fn refresh_library(&mut self) {
        let mut library: Vec<AgentLibraryEntry> = detected_default_cli_entries(&self.app)
            .into_iter()
            .map(
                |(full_contribution_id, name)| AgentLibraryEntry::DefaultCli {
                    full_contribution_id,
                    name,
                },
            )
            .collect();
        library.extend(
            self.app
                .catalog_store
                .list_agent_instances(None)
                .map(|rows| {
                    rows.into_iter()
                        .filter(|instance| instance.enabled)
                        .map(|instance| AgentLibraryEntry::Instance {
                            id: instance.id,
                            name: instance.name,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        library.push(AgentLibraryEntry::ManageSettings);
        self.library = library;
    }

    /// 兼容入口:任务选择变化只改项目作用域(项目工作流与 Task 无关)。
    pub fn set_selected_task(
        &mut self,
        task: Option<(std::path::PathBuf, i64)>,
        cx: &mut Context<Self>,
    ) {
        self.set_project(task.map(|(root, _)| root), cx);
    }

    /// 接收当前项目:加载项目工作流列表并选中第一个(无 Task 也可用)。
    pub fn set_project(&mut self, root: Option<std::path::PathBuf>, cx: &mut Context<Self>) {
        if self.project_root == root {
            cx.notify();
            return;
        }
        self.project_root = root;
        // 项目切换不能把上一个项目的草稿带进新 Store；读取失败时
        // 保持本项目的空编辑态并以 save_error 阻止写入/运行。
        self.workflows.clear();
        self.current_key = None;
        self.workflow_name.clear();
        self.editor.load_nodes(Vec::new());
        self.inspector = None;
        self.inspector_field = None;
        self.unsafe_parallel = false;
        self.save_error = None;
        self.refresh_library();
        self.refresh_templates();
        self.reload_workflows();
        cx.notify();
    }

    fn store(&self) -> Option<std::sync::Arc<mf_agent::store::Store>> {
        self.project_root
            .as_ref()
            .and_then(|root| self.app.orchestrator_of(root))
            .map(|orch| orch.store.clone())
    }

    /// 重载项目工作流列表;保持当前选择(消失则回落第一项)。
    pub fn reload_workflows(&mut self) {
        let Some(store) = self.store() else {
            self.workflows.clear();
            self.current_key = None;
            self.workflow_name.clear();
            self.editor.load_nodes(Vec::new());
            self.inspector = None;
            self.inspector_field = None;
            self.unsafe_parallel = false;
            return;
        };
        let records = match store.list_project_workflows() {
            Ok(records) => records,
            Err(error) => {
                let message = format!("读取项目工作流列表失败: {error:#}");
                self.status = message.clone();
                self.save_error = Some(message);
                // 同项目刷新失败时保留当前编辑态；禁止把错误伪装成空列表。
                return;
            }
        };
        self.workflows = records.into_iter().map(|r| (r.key, r.name)).collect();
        if !self
            .workflows
            .iter()
            .any(|(k, _)| Some(k) == self.current_key.as_ref())
        {
            let first = self.workflows.first().cloned();
            if let Some((key, _)) = first {
                self.load_workflow(&key);
            } else {
                self.current_key = None;
                self.workflow_name.clear();
                self.editor.load_nodes(Vec::new());
                self.inspector = None;
                self.inspector_field = None;
                self.unsafe_parallel = false;
                self.save_error = None;
            }
        }
    }

    fn refresh_templates(&mut self) {
        self.templates = self
            .app
            .catalog_store
            .list_templates(false)
            .map(|rows| rows.into_iter().map(|t| (t.key, t.name)).collect())
            .unwrap_or_default();
    }

    /// 选中并加载一个项目工作流(名称/节点/并行开关来自 Store)。
    pub fn load_workflow(&mut self, key: &str) {
        let Some(store) = self.store() else {
            return;
        };
        match store.load_project_workflow(key) {
            Ok(Some(record)) => {
                self.current_key = Some(record.key.clone());
                self.workflow_name = record.name.clone();
                self.unsafe_parallel = record.allow_unsafe_parallel;
                self.editor.load_nodes(
                    record
                        .nodes
                        .iter()
                        .map(|n| EditorNode {
                            key: n.key.clone(),
                            title: n.title.clone(),
                            instance_id: n.agent_instance_id.clone(),
                            deps: n.deps.clone(),
                            instructions: n.instructions.clone(),
                        })
                        .collect(),
                );
                self.inspector = None;
                self.inspector_field = None;
                self.save_error = None;
            }
            Ok(None) => {
                let message = format!("项目工作流 `{key}` 不存在");
                self.status = message.clone();
                self.save_error = Some(message);
            }
            Err(e) => {
                let message = format!("读取工作流失败: {e:#}");
                self.status = message.clone();
                self.save_error = Some(message);
            }
        }
    }

    /// 当前编辑状态 → 项目工作流草案。
    fn current_draft(&self) -> Option<mf_agent::workflow::ProjectWorkflowDraft> {
        let key = self.current_key.clone()?;
        Some(mf_agent::workflow::ProjectWorkflowDraft {
            key,
            name: self.workflow_name.clone(),
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
            allow_unsafe_parallel: self.unsafe_parallel,
        })
    }

    /// 保存当前项目工作流(原子编辑动作后的自动保存)。
    /// 失败保留 dirty(save_error)并显示错误;调用方据此阻止运行。
    pub fn save_current(&mut self) -> anyhow::Result<()> {
        let Some(draft) = self.current_draft() else {
            // 未选择工作流且无节点:无事可保存(新建入口负责建)
            return Ok(());
        };
        // 全新工作流还没有第一个节点:不写库(第一个节点加入时自动保存)。
        // 已持久化的工作流被清空:保持 dirty 交给 Store 报错,阻止运行旧内容。
        let persisted = self
            .workflows
            .iter()
            .any(|(k, _)| Some(k) == self.current_key.as_ref());
        if draft.nodes.is_empty() && !persisted {
            return Ok(());
        }
        let Some(store) = self.store() else {
            let msg = "项目未打开,无法保存工作流".to_string();
            self.save_error = Some(msg.clone());
            anyhow::bail!("{msg}");
        };
        match store.save_project_workflow(&draft) {
            Ok(record) => {
                self.save_error = None;
                self.status = format!("已保存「{}」", record.name);
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.save_error = Some(msg.clone());
                anyhow::bail!("{msg}");
            }
        }
    }

    /// 原子编辑后的统一收口:保存 + 刷新列表投影。
    pub fn save_after_edit(&mut self) {
        if self.save_current().is_ok() {
            self.reload_workflows();
        }
    }

    /// 新建项目工作流:稳定 key + 默认名称(用户重命名);
    /// 第一个节点加入时自动保存(空工作流不落库)。
    pub fn new_workflow(&mut self, cx: &mut Context<Self>) {
        if self.store().is_none() {
            self.status = "请先打开项目".into();
            cx.notify();
            return;
        }
        if let Some(error) = self.save_error.clone() {
            self.status = format!("当前工作流存储异常，不能新建: {error}");
            cx.notify();
            return;
        }
        let existing: Vec<String> = self.workflows.iter().map(|(k, _)| k.clone()).collect();
        let key = crate::workflow_editor::next_workflow_key(&existing);
        self.current_key = Some(key);
        self.workflow_name = format!("工作流 {}", existing.len() + 1);
        self.unsafe_parallel = false;
        self.editor.load_nodes(Vec::new());
        self.inspector = None;
        self.inspector_field = None;
        self.save_error = None;
        self.status = "已新建工作流(添加节点后自动保存)".into();
        cx.notify();
    }

    /// 重命名(原子编辑:保存)。
    pub fn rename_workflow(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.current_key.is_none() {
            return;
        }
        self.workflow_name = name.to_string();
        self.save_after_edit();
        cx.notify();
    }

    /// 复制当前工作流为新 key(副本独立保存)。
    pub fn duplicate_workflow(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store() else {
            return;
        };
        let Some(draft) = self.current_draft() else {
            return;
        };
        if draft.nodes.is_empty() {
            self.status = "空工作流没有可复制的内容".into();
            cx.notify();
            return;
        }
        let existing: Vec<String> = self.workflows.iter().map(|(k, _)| k.clone()).collect();
        let base = format!("{}-copy", draft.key);
        let mut key = base.clone();
        let mut n = 2;
        while existing.contains(&key) {
            key = format!("{base}-{n}");
            n += 1;
        }
        let mut copy = draft;
        copy.key = key;
        copy.name = format!("{}(副本)", self.workflow_name);
        match store.save_project_workflow(&copy) {
            Ok(record) => {
                let key = record.key.clone();
                self.reload_workflows();
                self.load_workflow(&key);
                self.status = format!("已复制为「{}」", record.name);
            }
            Err(e) => self.status = format!("复制失败: {e:#}"),
        }
        cx.notify();
    }

    /// 删除当前项目工作流(回落到列表第一项;不动已冻结 Revision)。
    pub fn delete_workflow(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store() else {
            return;
        };
        let Some(key) = self.current_key.clone() else {
            return;
        };
        match store.delete_project_workflow(&key) {
            Ok(true) => {
                self.current_key = None;
                self.reload_workflows();
                self.status = format!("已删除「{key}」");
            }
            Ok(false) => self.status = format!("项目工作流 `{key}` 不存在"),
            Err(e) => self.status = format!("删除失败: {e:#}"),
        }
        cx.notify();
    }

    /// 从全局模板创建:复制模板当前版本为新的项目工作流(之后互不联动)。
    pub fn create_from_template(&mut self, template_key: &str, cx: &mut Context<Self>) {
        let Some(store) = self.store() else {
            self.status = "请先打开项目".into();
            cx.notify();
            return;
        };
        // 解析模板当前版本(复制其节点;失败给出稳定错误)
        let version = match self.app.catalog_store.template_versions(template_key) {
            Ok(versions) => versions.into_iter().next_back(),
            Err(e) => {
                self.status = format!("读取模板失败: {e:#}");
                cx.notify();
                return;
            }
        };
        let Some(version) = version else {
            self.status = format!("全局模板 `{template_key}` 不存在");
            cx.notify();
            return;
        };
        let existing: Vec<String> = self.workflows.iter().map(|(k, _)| k.clone()).collect();
        let draft = mf_agent::workflow::ProjectWorkflowDraft {
            key: crate::workflow_editor::next_workflow_key(&existing),
            name: self
                .templates
                .iter()
                .find(|(k, _)| k == template_key)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| template_key.to_string()),
            nodes: version.nodes.clone(),
            allow_unsafe_parallel: false,
        };
        let created_key = draft.key.clone();
        match store.save_project_workflow(&draft) {
            Ok(record) => {
                self.reload_workflows();
                self.load_workflow(&created_key);
                self.template_popover = false;
                self.status = format!(
                    "已从模板「{template_key}」创建「{}」(副本独立,不再联动)",
                    record.name
                );
            }
            Err(e) => self.status = format!("创建失败: {e:#}"),
        }
        cx.notify();
    }

    /// 另存为全局模板(次级动作;项目工作流本体不变)。
    pub fn save_as_template(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.current_draft() else {
            self.status = "请先选择或新建一个工作流".into();
            cx.notify();
            return;
        };
        if draft.nodes.is_empty() {
            self.status = "空工作流无法另存为模板".into();
            cx.notify();
            return;
        }
        let template = mf_agent::workflow::WorkflowTemplateDraft {
            key: format!("{}-template", draft.key),
            name: format!("{}(模板)", draft.name),
            task_local: false,
            nodes: draft.nodes,
        };
        match self.app.catalog_store.save_template(&template) {
            Ok(version) => {
                self.refresh_templates();
                self.status = format!("已另存为全局模板(版本 {})", version.version);
            }
            Err(e) => self.status = format!("另存失败: {e:#}"),
        }
        cx.notify();
    }

    /// 切换共享目录并行(原子编辑:立即保存进项目工作流)。
    pub fn toggle_unsafe_parallel(&mut self, cx: &mut Context<Self>) {
        self.unsafe_parallel = !self.unsafe_parallel;
        self.save_after_edit();
        cx.notify();
    }

    /// 确认检查器编辑(标题/指令;原子编辑动作)。
    pub fn apply_inspector_edits(&mut self, cx: &mut Context<Self>) {
        let Some(inspector) = self.inspector.clone() else {
            return;
        };
        self.editor.select(&inspector.node_key);
        self.editor
            .set_selected_title(inspector.title_buffer.trim());
        self.editor
            .set_selected_instructions(inspector.instructions_buffer.trim());
        self.inspector_field = None;
        self.save_after_edit();
        cx.notify();
    }

    pub fn is_text_editing(&self) -> bool {
        self.rename_mode || self.inspector_field.is_some()
    }

    /// 「运行工作流」:先确认当前项目工作流已成功保存,再表达意图
    /// (运行本体由 AgentWorkspace 经 Run Composer 发起)。
    pub fn request_run(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let Some(root) = self.project_root.clone() else {
            let msg = "请先打开项目";
            self.status = msg.into();
            cx.notify();
            anyhow::bail!("{msg}");
        };
        let Some(key) = self.current_key.clone() else {
            let msg = "请先选择或新建一个工作流";
            self.status = msg.into();
            cx.notify();
            anyhow::bail!("{msg}");
        };
        // 保存失败(dirty)必须阻止:绝不能运行旧 Store 内容
        if let Some(err) = self.save_error.clone() {
            let msg = format!("工作流有未保存的修改({err}),先解决保存错误");
            self.status = msg.clone();
            cx.notify();
            anyhow::bail!("{msg}");
        }
        if self.editor.nodes().is_empty() {
            let msg = "空工作流无法运行";
            self.status = msg.into();
            cx.notify();
            anyhow::bail!("{msg}");
        }
        // 运行前再保存一次(把缓冲中的编辑落库;失败即中止)
        self.save_current()?;
        cx.emit(WorkflowCanvasEvent::RunRequested {
            project_root: root,
            workflow_key: key,
        });
        cx.notify();
        Ok(())
    }

    /// 键盘流:重命名模式接收字符,Enter 确认(原子编辑保存),Esc 取消。
    /// 宿主(AgentWorkspace)在画布聚焦时转发按键。
    pub fn handle_key(&mut self, ev: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        if let Some(field) = self.inspector_field {
            match ev.keystroke.key.as_str() {
                "escape" => {
                    if let Some(key) = self.inspector.as_ref().map(|i| i.node_key.clone()) {
                        if let Some(node) = self.editor.nodes().iter().find(|n| n.key == key) {
                            self.inspector = Some(WorkflowNodeInspector::new(node));
                        }
                    }
                    self.inspector_field = None;
                }
                "enter" => self.apply_inspector_edits(cx),
                "backspace" => {
                    if let Some(inspector) = self.inspector.as_mut() {
                        match field {
                            InspectorField::Title => {
                                inspector.title_buffer.pop();
                            }
                            InspectorField::Instructions => {
                                inspector.instructions_buffer.pop();
                            }
                        }
                    }
                }
                _ => {
                    if let (Some(ch), Some(inspector)) =
                        (ev.keystroke.key_char.as_ref(), self.inspector.as_mut())
                    {
                        match field {
                            InspectorField::Title => inspector.title_buffer.push_str(ch),
                            InspectorField::Instructions => {
                                inspector.instructions_buffer.push_str(ch)
                            }
                        }
                    }
                }
            }
            cx.notify();
            return;
        }
        if !self.rename_mode {
            return;
        }
        match ev.keystroke.key.as_str() {
            "escape" => {
                self.rename_mode = false;
                if let Some(key) = self.current_key.clone() {
                    self.load_workflow(&key);
                }
            }
            "enter" => {
                self.rename_mode = false;
                let name = self.workflow_name.trim().to_string();
                if !name.is_empty() {
                    self.rename_workflow(&name, cx);
                }
            }
            "backspace" => {
                self.workflow_name.pop();
            }
            _ => {
                if let Some(ch) = ev.keystroke.key_char.as_ref() {
                    self.workflow_name.push_str(ch);
                }
            }
        }
        cx.notify();
    }

    fn render_library(&self, cx: &Context<Self>) -> AnyElement {
        let mut group_default: Vec<&AgentLibraryEntry> = Vec::new();
        let mut group_instances: Vec<&AgentLibraryEntry> = Vec::new();
        let mut manage = false;
        for entry in &self.library {
            match entry {
                AgentLibraryEntry::DefaultCli { .. } => group_default.push(entry),
                AgentLibraryEntry::Instance { .. } => group_instances.push(entry),
                AgentLibraryEntry::ManageSettings => manage = true,
            }
        }
        // 分组渲染:默认 CLI 与保存配置视觉分组,同一点击行为产生节点
        let render_group =
            |title: &str, entries: &[&AgentLibraryEntry], id_prefix: &str| -> gpui::Div {
                let mut group = gpui::div().flex().flex_col().gap_1();
                if entries.is_empty() {
                    return group;
                }
                group = group.child(
                    gpui::div()
                        .text_size(crate::theme::ui_px(9.))
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child(title.to_string()),
                );
                for (idx, entry) in entries.iter().enumerate() {
                    let reference = entry.node_reference();
                    let name = entry.display_name().to_string();
                    let is_default = matches!(entry, AgentLibraryEntry::DefaultCli { .. });
                    group = group.child(
                        gpui::div()
                            .id(gpui::ElementId::Name(format!("{id_prefix}-{idx}").into()))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .cursor_pointer()
                            .text_size(crate::theme::ui_px(10.))
                            .child(name)
                            .when(is_default, |d| {
                                d.child(
                                    gpui::div()
                                        .text_size(crate::theme::ui_px(8.))
                                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                                        .child(" 默认 CLI(沿用外部配置)"),
                                )
                            })
                            .on_click(cx.listener(
                                move |canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                    if let Some(reference) = &reference {
                                        canvas.editor.drag_from_library(reference);
                                        // 原子编辑:添加节点后自动保存
                                        canvas.save_after_edit();
                                        canvas.status = "已添加节点(自动保存)".into();
                                    } else {
                                        cx.emit(WorkflowCanvasEvent::OpenAgentSettings);
                                    }
                                    cx.notify();
                                },
                            )),
                    );
                }
                group
            };
        let mut list = gpui::div().flex().flex_col().gap_2();
        list = list.child(render_group(
            "检测到的默认 CLI",
            &group_default,
            "wf-lib-cli",
        ));
        list = list.child(render_group(
            "保存的智能体配置",
            &group_instances,
            "wf-lib-inst",
        ));
        if group_default.is_empty() && group_instances.is_empty() {
            list = list.child(
                gpui::div()
                    .text_size(crate::theme::ui_px(9.))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child("还没有可用的默认 CLI 或保存配置"),
            );
        }
        if manage {
            list = list.child(
                gpui::div()
                    .id("wf-lib-manage")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .cursor_pointer()
                    .text_size(crate::theme::ui_px(10.))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child("管理智能体配置……")
                    .on_click(cx.listener(|_canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                        cx.emit(WorkflowCanvasEvent::OpenAgentSettings);
                        cx.notify();
                    })),
            );
        }
        gpui::div()
            .id("wf-library")
            .flex()
            .flex_col()
            .gap_2()
            .child(
                gpui::div()
                    .text_size(crate::theme::ui_px(11.))
                    .text_color(rgb(crate::theme::Theme::fg()))
                    .child("Agent 库"),
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
                    .text_size(crate::theme::ui_px(10.))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child("画布为空:点击左侧实例添加节点,节点详情里连接依赖"),
            );
        }
        for (layer_idx, layer) in layers.iter().enumerate() {
            let mut row = gpui::div().flex().gap_2().items_center();
            row = row.child(
                gpui::div()
                    .text_size(crate::theme::ui_px(8.))
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
                                .child(
                                    gpui::div()
                                        .text_size(crate::theme::ui_px(11.))
                                        .child(node.title.clone()),
                                )
                                .child(
                                    gpui::div()
                                        .text_size(crate::theme::ui_px(8.))
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
                                    canvas.inspector_field = None;
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
                .text_size(crate::theme::ui_px(10.))
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
                        .text_size(crate::theme::ui_px(9.))
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
                            .text_size(crate::theme::ui_px(9.))
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
                            .text_size(crate::theme::ui_px(8.))
                            .cursor_pointer()
                            .child(if has_dep { "断开" } else { "依赖" })
                            .on_click(cx.listener(
                                move |canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                    if has_dep {
                                        canvas.editor.remove_dependency(&node_key, &key2);
                                        canvas.status = format!("已断开 {node_key} ← {key2}");
                                        // 原子编辑:依赖变化自动保存
                                        canvas.save_after_edit();
                                    } else {
                                        match canvas.editor.add_dependency(&node_key, &key2) {
                                            Ok(()) => {
                                                canvas.status =
                                                    format!("已连接 {node_key} ← {key2}");
                                                canvas.save_after_edit();
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
        let instructions = inspector.instructions_buffer.clone();
        let node_key = node.key.clone();
        let instance_name = self
            .library
            .iter()
            .find_map(|entry| match entry {
                AgentLibraryEntry::DefaultCli {
                    full_contribution_id,
                    name,
                } if node.instance_id
                    == format!(
                        "{}{full_contribution_id}",
                        crate::app_ctx::DEFAULT_CLI_REFERENCE_PREFIX
                    ) =>
                {
                    Some(format!("{name}(默认 CLI)"))
                }
                AgentLibraryEntry::Instance { id, name } if *id == node.instance_id => {
                    Some(name.clone())
                }
                _ => None,
            })
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
                            .text_size(crate::theme::ui_px(11.))
                            .child(format!("节点 {}", node.key)),
                    )
                    .child(
                        gpui::div()
                            .id("wf-inspector-collapse")
                            .text_size(crate::theme::ui_px(9.))
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
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child("标题"),
                    )
                    .child(
                        gpui::div()
                            .id("wf-inspector-title")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(
                                if self.inspector_field == Some(InspectorField::Title) {
                                    crate::theme::Theme::accent()
                                } else {
                                    crate::theme::Theme::border()
                                },
                            ))
                            .text_size(crate::theme::ui_px(10.))
                            .cursor_pointer()
                            .child(title)
                            .on_click(cx.listener(
                                |canvas: &mut WorkflowCanvas, _ev, window, cx| {
                                    canvas.inspector_field = Some(InspectorField::Title);
                                    window.focus(&canvas.focus_handle, cx);
                                    cx.notify();
                                },
                            )),
                    ),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        gpui::div()
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child("工作说明"),
                    )
                    .child(
                        gpui::div()
                            .id("wf-inspector-instructions")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(
                                if self.inspector_field == Some(InspectorField::Instructions) {
                                    crate::theme::Theme::accent()
                                } else {
                                    crate::theme::Theme::border()
                                },
                            ))
                            .text_size(crate::theme::ui_px(10.))
                            .cursor_pointer()
                            .child(if instructions.is_empty() {
                                "(无补充说明)".to_string()
                            } else {
                                instructions
                            })
                            .on_click(cx.listener(
                                |canvas: &mut WorkflowCanvas, _ev, window, cx| {
                                    canvas.inspector_field = Some(InspectorField::Instructions);
                                    window.focus(&canvas.focus_handle, cx);
                                    cx.notify();
                                },
                            )),
                    ),
            )
            .child(
                gpui::div()
                    .id("wf-inspector-apply")
                    .px_2()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::accent()))
                    .text_size(crate::theme::ui_px(9.))
                    .text_color(rgb(crate::theme::Theme::accent()))
                    .cursor_pointer()
                    .child("确认编辑(标题/说明)")
                    .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                        canvas.apply_inspector_edits(cx);
                    })),
            )
            .child({
                // 改绑 Agent:点击即应用 + 自动保存(原子编辑动作)
                let bindings: Vec<(String, String)> = self
                    .library
                    .iter()
                    .filter_map(|entry| {
                        entry
                            .node_reference()
                            .map(|r| (r, entry.display_name().to_string()))
                    })
                    .collect();
                let mut panel = gpui::div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        gpui::div()
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child(format!("当前绑定:{}", instance_name)),
                    )
                    .child(
                        gpui::div()
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("改绑 Agent(点击应用):"),
                    );
                for (reference, name) in bindings {
                    let node_key = node_key.clone();
                    panel = panel.child(
                        gpui::div()
                            .id(gpui::ElementId::Name(
                                format!("wf-bind-{}", reference).into(),
                            ))
                            .px_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(crate::theme::ui_px(8.))
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child(name)
                            .on_click(cx.listener(
                                move |canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                    canvas.editor.select(&node_key);
                                    canvas.editor.set_selected_instance(&reference);
                                    canvas.save_after_edit();
                                    canvas.status = format!("已改绑为 {reference}");
                                    cx.notify();
                                },
                            )),
                    );
                }
                panel
            })
            .child(
                gpui::div()
                    .text_size(crate::theme::ui_px(9.))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child("依赖(点击连接/断开):"),
            )
            .child(deps_panel)
            .child(
                gpui::div()
                    .id("wf-inspector-delete")
                    .px_2()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::danger()))
                    .text_size(crate::theme::ui_px(9.))
                    .text_color(rgb(crate::theme::Theme::danger()))
                    .cursor_pointer()
                    .child("删除节点")
                    .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                        canvas.editor.delete_selected();
                        canvas.inspector = None;
                        canvas.inspector_field = None;
                        // 原子编辑:删除节点自动保存
                        canvas.save_after_edit();
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
            .track_focus(&self.focus_handle)
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        gpui::div()
                            .text_size(crate::theme::ui_px(12.))
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child("工作流编排"),
                    )
                    .child(
                        gpui::div()
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(
                                "默认 CLI 沿用外部配置;保存配置与 Secret 在设置 → 智能体统一管理",
                            ),
                    )
                    .child(
                        gpui::div()
                            .id("wf-layout-toggle")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(crate::theme::ui_px(9.))
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
                            .id("wf-workflow-select")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child(if self.current_key.is_some() {
                                format!("工作流:{}", self.workflow_name)
                            } else {
                                "工作流:(无)".to_string()
                            })
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                // 循环选择下一个项目工作流
                                let keys: Vec<String> =
                                    canvas.workflows.iter().map(|(k, _)| k.clone()).collect();
                                if keys.is_empty() {
                                    canvas.status = "还没有项目工作流,点击「新建」".into();
                                } else {
                                    let idx = canvas
                                        .current_key
                                        .as_ref()
                                        .and_then(|k| keys.iter().position(|x| x == k))
                                        .map(|i| (i + 1) % keys.len())
                                        .unwrap_or(0);
                                    canvas.load_workflow(&keys[idx]);
                                    canvas.status = format!("已切换到「{}」", canvas.workflow_name);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-new-workflow")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child("新建")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.new_workflow(cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-rename-workflow")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if self.rename_mode {
                                crate::theme::Theme::accent()
                            } else {
                                crate::theme::Theme::border()
                            }))
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child(if self.rename_mode {
                                "重命名中(输入,Enter 确认)"
                            } else {
                                "重命名"
                            })
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.rename_mode = !canvas.rename_mode;
                                if canvas.rename_mode {
                                    canvas.status = "重命名:直接输入,Enter 确认".into();
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-duplicate-workflow")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child("复制")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.duplicate_workflow(cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-delete-workflow")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::danger()))
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::danger()))
                            .cursor_pointer()
                            .child("删除")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.delete_workflow(cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-from-template")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child(if self.templates.is_empty() {
                                "从全局模板创建(无模板)"
                            } else {
                                "从全局模板创建"
                            })
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                if canvas.templates.is_empty() {
                                    canvas.status = "还没有全局模板".into();
                                    cx.notify();
                                    return;
                                }
                                // 无弹层依赖:循环选择模板并直接创建
                                let key = canvas.templates[0].0.clone();
                                canvas.create_from_template(&key, cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-save-template")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child("另存为全局模板")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                canvas.save_as_template(cx);
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-run-workflow")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if self.save_error.is_some() {
                                crate::theme::Theme::danger()
                            } else {
                                crate::theme::Theme::accent()
                            }))
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(if self.save_error.is_some() {
                                crate::theme::Theme::danger()
                            } else {
                                crate::theme::Theme::accent()
                            }))
                            .cursor_pointer()
                            .child("运行工作流")
                            .on_click(cx.listener(|canvas: &mut WorkflowCanvas, _ev, _w, cx| {
                                // 只表达意图;运行本体由 AgentWorkspace 经 Run Composer 发起
                                if let Err(e) = canvas.request_run(cx) {
                                    log::warn!("运行请求被拒绝: {e:#}");
                                }
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("wf-unsafe-parallel")
                            .px_2()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if self.unsafe_parallel {
                                crate::theme::Theme::warning()
                            } else {
                                crate::theme::Theme::border()
                            }))
                            .text_size(crate::theme::ui_px(9.))
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
                            .text_size(crate::theme::ui_px(9.))
                            .text_color(rgb(crate::theme::Theme::warning()))
                            .child(d.clone())
                    }))
                    .when(self.save_error.is_some(), |d| {
                        d.child(
                            gpui::div()
                                .id("wf-save-error")
                                .text_size(crate::theme::ui_px(9.))
                                .text_color(rgb(crate::theme::Theme::danger()))
                                .child(format!(
                                    "有未保存的修改:{}(运行被阻止)",
                                    self.save_error.clone().unwrap_or_default()
                                )),
                        )
                    })
                    .when(
                        self.save_error.is_none() && self.current_key.is_some(),
                        |d| {
                            d.child(
                                gpui::div()
                                    .id("wf-save-state")
                                    .text_size(crate::theme::ui_px(9.))
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child("已自动保存"),
                            )
                        },
                    ),
            )
            .child(body)
            .when(!status.is_empty(), |d| {
                d.child(
                    gpui::div()
                        .text_size(crate::theme::ui_px(9.))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .child(status),
                )
            })
    }
}
