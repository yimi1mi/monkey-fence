//! Agent Instance 列表视图模型(UI 计划 Task 1)。
//!
//! Agent Type(默认 CLI 引导入口)与用户实例同页展示、视觉区分:
//! 默认 CLI 条目只是"快速打开全部已检测 Agent"的入口(不落库);
//! 持久化实例独立列出。缺失 CLI 置灰并解释原因(设计 §11.1)。

use crate::agent_instance_editor::AgentTypeInfo;
use mf_agent::agent_instance::AgentInstance;
use mf_agent::{InstanceScope, RunMode};

/// 列表行(渲染投影):default-cli = 类型引导入口;instance = 已存实例。
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceListEntry {
    pub kind: &'static str,
    pub title: String,
    pub subtitle: String,
    pub available: bool,
    pub id: Option<String>,
}

/// 实例条目数据(从目录库装载)。
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceListInstance {
    pub id: String,
    pub name: String,
    pub agent_type: String,
    pub type_name: String,
    pub enabled: bool,
    pub current_version: i64,
    pub scope: InstanceScope,
    pub executable: String,
    pub run_mode: RunMode,
}

pub type InstanceListEntryLegacy = InstanceListInstance;

/// 列表视图模型(纯状态)。
#[derive(Debug, Default)]
pub struct AgentInstancesViewModel {
    types: Vec<AgentTypeInfo>,
    instances: Vec<InstanceListInstance>,
}

impl AgentInstancesViewModel {
    pub fn push_type(&mut self, info: AgentTypeInfo) {
        self.types.push(info);
    }

    pub fn type_infos(&self) -> &[AgentTypeInfo] {
        &self.types
    }

    pub fn instances(&self) -> &[InstanceListInstance] {
        &self.instances
    }

    pub fn push_instance(&mut self, instance: InstanceListInstance) {
        self.instances.push(instance);
    }

    /// 从目录库行装载(类型名解析失败的显示原始 id)。
    pub fn load_instances(&mut self, rows: &[AgentInstance]) {
        for row in rows {
            let type_name = self
                .types
                .iter()
                .find(|t| t.id == row.agent_type)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| row.agent_type.clone());
            self.instances.push(InstanceListInstance {
                id: row.id.clone(),
                name: row.name.clone(),
                agent_type: row.agent_type.clone(),
                type_name,
                enabled: row.enabled,
                current_version: row.current_version,
                scope: row.scope,
                executable: String::new(),
                run_mode: RunMode::Interactive,
            });
        }
    }

    /// 全部条目:默认 CLI 引导入口在前,实例在后。
    pub fn entries(&self) -> Vec<InstanceListEntry> {
        let mut out = Vec::new();
        for t in &self.types {
            out.push(InstanceListEntry {
                kind: "default-cli",
                title: t.name.clone(),
                subtitle: if t.detected {
                    format!("{} · 类型入口（点击创建实例）", t.plugin_name)
                } else {
                    format!("{} · 未检测到 {}", t.plugin_name, t.default_command)
                },
                available: t.detected,
                id: None,
            });
        }
        for i in &self.instances {
            out.push(InstanceListEntry {
                kind: "instance",
                title: i.name.clone(),
                subtitle: format!(
                    "{} · v{} · {}{}",
                    i.type_name,
                    i.current_version,
                    if i.enabled { "已启用" } else { "已禁用" },
                    match i.scope {
                        InstanceScope::User => String::new(),
                        InstanceScope::Project => " · 项目作用域".into(),
                    }
                ),
                available: true,
                id: Some(i.id.clone()),
            });
        }
        out
    }

    /// 文本过滤(标题 + 副标题,大小写不敏感)。
    pub fn filtered(&self, text: &str) -> Vec<InstanceListEntry> {
        let needle = text.trim().to_lowercase();
        self.entries()
            .into_iter()
            .filter(|e| {
                needle.is_empty()
                    || e.title.to_lowercase().contains(&needle)
                    || e.subtitle.to_lowercase().contains(&needle)
            })
            .collect()
    }
}

// ---------- GPUI 页面(设计 §11.1)----------

use gpui::prelude::*;
use gpui::{px, rgb, AnyElement, Context, FocusHandle, Window};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageField {
    None,
    Filter,
    Name,
    Executable,
    Argv,
    Env,
    ProjectKey,
    /// config_schema 表单字段(索引)。
    ConfigField(usize),
    SecretName,
    SecretValue,
    /// 结构化 secret_env 行的 ENV 名编辑(行索引)。
    SecretEnvName(usize),
}

/// Agent 实例页(独立 GPUI 实体,模式同 TaskComposer):
/// 左侧类型/实例列表,右侧编辑器;默认 CLI 行可快速启动到当前任务。
pub struct AgentInstancesPage {
    pub app: std::sync::Arc<crate::app_ctx::AppCtx>,
    pub model: AgentInstancesViewModel,
    pub filter: String,
    pub editor: Option<crate::agent_instance_editor::AgentInstanceEditorState>,
    /// 编辑器当前编辑字段。
    field: PageField,
    focus_handle: FocusHandle,
    pub status: String,
    /// 当前选中任务(默认 CLI 启动的离散会话挂载点)。
    pub selected_task: Option<(std::path::PathBuf, i64)>,
    /// Secret 管理:新 Secret 名称输入与值缓冲。值缓冲是 Zeroizing:
    /// 按键间短暂存在,seal 后清零,drop 时擦除(明文不驻留内存)。
    secret_name_input: String,
    secret_value_input: zeroize::Zeroizing<String>,
}

impl AgentInstancesPage {
    pub fn new(
        app: std::sync::Arc<crate::app_ctx::AppCtx>,
        cx: &mut Context<Self>,
    ) -> AgentInstancesPage {
        let mut page = AgentInstancesPage {
            app,
            model: AgentInstancesViewModel::default(),
            filter: String::new(),
            editor: None,
            field: PageField::None,
            focus_handle: cx.focus_handle(),
            status: String::new(),
            selected_task: None,
            secret_name_input: String::new(),
            secret_value_input: zeroize::Zeroizing::new(String::new()),
        };
        page.refresh();
        page
    }

    /// 从插件贡献 + 目录库刷新。
    pub fn refresh(&mut self) {
        let mut model = AgentInstancesViewModel::default();
        let contributions = self.app.plugins.contributions();
        let mut types: Vec<crate::agent_instance_editor::AgentTypeInfo> = contributions
            .agent_types()
            .into_iter()
            .map(|(full_contribution_id, src, a)| {
                let detected = mf_plugins::builtin::detect_on_path(&a.command).is_some()
                    || a.command.is_empty();
                crate::agent_instance_editor::AgentTypeInfo {
                    id: a.id.clone(),
                    // 完整贡献 ID(publisher.plugin.agent-type):
                    // 列表/编辑器的身份与 pin 解析都用它
                    full_contribution_id: full_contribution_id.clone(),
                    name: a.name.clone(),
                    plugin_name: src.plugin_full_id.clone(),
                    plugin_version: src.plugin_version.clone(),
                    content_hash: src.content_hash.clone(),
                    config_schema_fields: crate::adapter_launch::config_schema_fields(
                        &self.app.plugins,
                        &full_contribution_id,
                    ),
                    detected,
                    supports_isolated_config: a.supports_isolated_config,
                    default_command: a.command.clone(),
                    adapter: a.adapter.clone(),
                    modes: a
                        .modes
                        .iter()
                        .filter_map(|m| mf_agent::RunMode::parse(m))
                        .collect(),
                }
            })
            .collect();
        types.sort_by(|a, b| a.name.cmp(&b.name));
        for info in types {
            model.push_type(info);
        }
        if let Ok(rows) = self.app.catalog_store.list_agent_instances(None) {
            // 快照解析 executable/run_mode(列表投影不再丢字段)
            for row in &rows {
                let snapshot = self
                    .app
                    .catalog_store
                    .snapshot_agent_instance(&row.id, None)
                    .ok();
                model.push_instance(InstanceListInstance {
                    id: row.id.clone(),
                    name: row.name.clone(),
                    agent_type: row.agent_type.clone(),
                    type_name: crate::agent_instance_editor::resolve_type_info(
                        model.type_infos(),
                        &row.agent_type,
                    )
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| row.agent_type.clone()),
                    enabled: row.enabled,
                    current_version: row.current_version,
                    scope: row.scope,
                    executable: snapshot
                        .as_ref()
                        .map(|s| s.executable.clone())
                        .unwrap_or_default(),
                    run_mode: snapshot
                        .as_ref()
                        .map(|s| s.run_mode)
                        .unwrap_or(mf_agent::RunMode::Interactive),
                });
            }
        }
        self.model = model;
    }

    /// 任务选择变化(workspace 推送)。
    pub fn set_selected_task(
        &mut self,
        task: Option<(std::path::PathBuf, i64)>,
        cx: &mut Context<Self>,
    ) {
        self.selected_task = task;
        cx.notify();
    }

    fn type_info_of(
        &self,
        agent_type: &str,
    ) -> Option<crate::agent_instance_editor::AgentTypeInfo> {
        crate::agent_instance_editor::resolve_type_info(self.model.type_infos(), agent_type)
            .cloned()
    }

    /// 默认 CLI 快速启动:在当前选中任务下创建离散会话(不改变任务状态)。
    pub fn launch_default_cli(&mut self, agent_type: &str, cx: &mut Context<Self>) {
        let Some(info) = self.type_info_of(agent_type) else {
            self.status = format!("未找到 Agent 类型 {agent_type}");
            cx.notify();
            return;
        };
        let Some((root, task_id)) = self.selected_task.clone() else {
            self.status = "请先选择一个任务(默认 CLI 在任务下以离散会话启动)".into();
            cx.notify();
            return;
        };
        let snapshot = mf_agent::AgentInstanceSnapshot {
            id: format!("default-{}", info.id),
            name: format!("{} 默认 CLI", info.name),
            // 完整贡献 ID:第三方类型短 id 无法被 resolve_adapter 解析
            agent_type: info.full_contribution_id.clone(),
            version: 0,
            enabled: true,
            run_mode: mf_agent::RunMode::Interactive,
            executable: info.default_command.clone(),
            argv: vec![],
            env: vec![],
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({ "completion": "manual" }),
            sealed_secret_ids: vec![],
        };
        match self.app.create_ad_hoc_session(
            &root,
            task_id,
            &snapshot,
            mf_agent::RunMode::Interactive,
            // 默认 CLI 启动:只读外部已有配置
            true,
        ) {
            Ok(view) => self.status = format!("已在任务 {task_id} 下启动 {}", view.title),
            Err(e) => self.status = format!("启动失败: {e:#}"),
        }
        cx.notify();
    }

    /// 以类型预填打开新建编辑器。
    pub fn open_editor_for_type(&mut self, agent_type: &str, cx: &mut Context<Self>) {
        if let Some(info) = self.type_info_of(agent_type) {
            self.editor = Some(crate::agent_instance_editor::AgentInstanceEditorState::new(
                info,
            ));
            self.field = PageField::Name;
            cx.notify();
        }
    }

    pub fn open_editor_for_instance(&mut self, instance_id: &str, cx: &mut Context<Self>) {
        match self
            .app
            .catalog_store
            .snapshot_agent_instance(instance_id, None)
        {
            Ok(snapshot) => {
                let row = self
                    .app
                    .catalog_store
                    .get_agent_instance(instance_id)
                    .ok()
                    .flatten();
                let info = self
                    .type_info_of(&snapshot.agent_type)
                    .unwrap_or_else(|| fallback_type_info(&snapshot.agent_type));
                let (scope, project_key, enabled) = match &row {
                    Some(row) => (row.scope, row.project_key.clone(), row.enabled),
                    None => (mf_agent::InstanceScope::User, None, true),
                };
                self.editor = Some(
                    crate::agent_instance_editor::AgentInstanceEditorState::from_instance(
                        info,
                        &snapshot,
                        scope,
                        project_key.as_deref(),
                        enabled,
                    ),
                );
                self.field = PageField::None;
                cx.notify();
            }
            Err(e) => {
                self.status = format!("读取实例失败: {e:#}");
                cx.notify();
            }
        }
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.editor.clone() else {
            return;
        };
        if !state.can_save() {
            self.status = "配置无效,无法保存".into();
            cx.notify();
            return;
        }
        let draft = state.to_draft();
        let result = match &state.editing_instance_id {
            Some(id) => self
                .app
                .catalog_store
                .update_agent_instance(id, draft)
                .map(|i| i.id),
            None => self
                .app
                .catalog_store
                .create_agent_instance(draft)
                .map(|i| i.id),
        };
        match result {
            Ok(id) => {
                self.status = format!("已保存实例 {id}");
                self.editor = None;
                self.field = PageField::None;
                self.refresh();
            }
            Err(e) => self.status = format!("保存失败: {e:#}"),
        }
        cx.notify();
    }

    /// 密封输入的 Secret(值缓冲立即清空;成功后可附加引用)。
    pub fn seal_secret_input(&mut self, cx: &mut Context<Self>) {
        let name = self.secret_name_input.trim().to_string();
        if name.is_empty() || self.secret_value_input.is_empty() {
            self.status = "Secret 名称与值都不能为空".into();
            cx.notify();
            return;
        }
        match self.app.seal_secret(&name, &self.secret_value_input) {
            Ok(id) => {
                self.status = format!("已密封 Secret {id}(引用它而不再保存明文)");
                if let Some(state) = self.editor.as_mut() {
                    state.add_secret_ref(&id);
                }
            }
            Err(e) => self.status = format!("密封失败: {e:#}"),
        }
        // 无论成败都立即清零值缓冲(Zeroizing:clear 后逐字节擦除)
        use zeroize::Zeroize;
        self.secret_value_input.zeroize();
        self.field = PageField::None;
        cx.notify();
    }

    /// 删除 Secret(仍被实例引用时后端拒绝)。
    pub fn delete_secret_by_id(&mut self, id: String, cx: &mut Context<Self>) {
        match self.app.delete_secret(&id) {
            Ok(true) => {
                self.status = format!("已删除 Secret {id}");
                if let Some(state) = self.editor.as_mut() {
                    state.remove_secret_ref(&id);
                }
            }
            Ok(false) => self.status = format!("Secret {id} 不存在"),
            Err(e) => self.status = format!("删除失败: {e:#}"),
        }
        cx.notify();
    }

    pub fn delete_current(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.editor.clone() else {
            return;
        };
        let Some(id) = state.editing_instance_id.clone() else {
            return;
        };
        match self.app.catalog_store.delete_agent_instance(&id) {
            Ok(true) => {
                self.status = format!("已删除实例 {id}");
                self.editor = None;
                self.refresh();
            }
            Ok(false) => self.status = "实例不存在".into(),
            Err(e) => self.status = format!("删除失败: {e:#}"),
        }
        cx.notify();
    }

    fn handle_key(&mut self, ev: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        if self.field == PageField::None {
            return;
        }
        let key = ev.keystroke.key.as_str();
        if key == "escape" {
            self.field = PageField::None;
            cx.notify();
            return;
        }
        if self.field == PageField::Filter {
            match key {
                "backspace" => {
                    self.filter.pop();
                }
                "enter" => self.field = PageField::None,
                _ => {
                    if let Some(ch) = ev.keystroke.key_char.as_ref() {
                        self.filter.push_str(ch);
                    }
                }
            }
            cx.notify();
            return;
        }
        // 结构化 secret_env 行:ENV 名内联编辑(编辑器状态内)
        if let PageField::SecretEnvName(row) = self.field {
            if let Some(state) = self.editor.as_mut() {
                match key {
                    "escape" => self.field = PageField::None,
                    "enter" => self.field = PageField::None,
                    "backspace" => {
                        if let Some((env, id)) = state.secret_env_map.get(row).cloned() {
                            let mut text = env;
                            text.pop();
                            state.set_secret_env(&text, &id);
                        }
                    }
                    _ => {
                        if let Some(ch) = ev.keystroke.key_char.as_ref() {
                            if let Some((env, id)) = state.secret_env_map.get(row).cloned() {
                                let mut text = env.clone();
                                text.push_str(ch);
                                state.set_secret_env(&text, &id);
                            }
                        }
                    }
                }
            }
            cx.notify();
            return;
        }
        // Secret 管理输入(页面层缓冲;seal 后清零)
        match self.field {
            PageField::SecretName | PageField::SecretValue => {
                let buffer = if self.field == PageField::SecretName {
                    &mut self.secret_name_input
                } else {
                    &mut self.secret_value_input
                };
                match key {
                    "backspace" => {
                        buffer.pop();
                    }
                    "enter" => self.field = PageField::None,
                    _ => {
                        if let Some(ch) = ev.keystroke.key_char.as_ref() {
                            buffer.push_str(ch);
                        }
                    }
                }
                cx.notify();
                return;
            }
            _ => {}
        }
        let Some(state) = self.editor.as_mut() else {
            self.field = PageField::None;
            return;
        };
        match key {
            "enter" => {
                if self.field == PageField::Env {
                    if let Some(ch) = ev.keystroke.key_char.as_ref() {
                        push_field(state, self.field, ch);
                    }
                } else {
                    self.field = PageField::None;
                }
            }
            "backspace" => {
                if let Some(mut text) = field_text(state, self.field) {
                    text.pop();
                    set_field_text(state, self.field, &text);
                }
            }
            _ => {
                if let Some(ch) = ev.keystroke.key_char.as_ref() {
                    push_field(state, self.field, ch);
                }
            }
        }
        cx.notify();
    }
}

fn field_text(
    state: &crate::agent_instance_editor::AgentInstanceEditorState,
    field: PageField,
) -> Option<String> {
    match field {
        PageField::Name => Some(state.name.clone()),
        PageField::Executable => Some(state.executable.clone()),
        PageField::Argv => Some(state.argv_text.clone()),
        PageField::Env => Some(state.env_text.clone()),
        PageField::ProjectKey => Some(state.project_key.clone()),
        PageField::ConfigField(i) => state
            .config_form()
            .fields()
            .get(i)
            .map(|f| state.config_form().masked_value(&f.id)),
        PageField::SecretEnvName(row) => state.secret_env_map.get(row).map(|(env, _)| env.clone()),
        // Secret 输入在页面层(非编辑器状态)
        PageField::SecretName | PageField::SecretValue | PageField::None | PageField::Filter => {
            None
        }
    }
}

fn set_field_text(
    state: &mut crate::agent_instance_editor::AgentInstanceEditorState,
    field: PageField,
    text: &str,
) {
    match field {
        PageField::Name => state.set_name(text),
        PageField::Executable => state.set_executable(text),
        PageField::Argv => state.set_argv(text),
        PageField::Env => state.set_env_lines(text),
        PageField::ProjectKey => state.set_project_key(text),
        PageField::ConfigField(i) => {
            if let Some(f) = state.config_form().fields().get(i).cloned() {
                if f.kind != "secret" {
                    state.set_config_value(&f.id, text);
                }
            }
        }
        _ => {}
    }
}

fn push_field(
    state: &mut crate::agent_instance_editor::AgentInstanceEditorState,
    field: PageField,
    ch: &str,
) {
    if let Some(mut text) = field_text(state, field) {
        text.push_str(ch);
        set_field_text(state, field, &text);
    }
}

fn fallback_type_info(agent_type: &str) -> crate::agent_instance_editor::AgentTypeInfo {
    crate::agent_instance_editor::AgentTypeInfo {
        id: agent_type.to_string(),
        full_contribution_id: agent_type.to_string(),
        name: agent_type.to_string(),
        plugin_name: "未知来源".into(),
        plugin_version: String::new(),
        content_hash: String::new(),
        config_schema_fields: Vec::new(),
        detected: false,
        supports_isolated_config: false,
        default_command: String::new(),
        adapter: "generic-command".into(),
        modes: vec![mf_agent::RunMode::Interactive],
    }
}

fn action_chip(
    id: gpui::ElementId,
    label: &str,
    color: u32,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let mut chip = gpui::div()
        .id(id)
        .px_2()
        .h(px(20.))
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(if enabled {
            color
        } else {
            crate::theme::Theme::border()
        }))
        .text_size(px(9.5))
        .text_color(rgb(if enabled {
            color
        } else {
            crate::theme::Theme::fg_dim()
        }))
        .child(label.to_string());
    if enabled {
        chip = chip.cursor_pointer();
    }
    chip
}

fn instance_list_section(label: &str) -> impl IntoElement {
    gpui::div()
        .pt_1()
        .pb_0p5()
        .text_size(px(8.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(crate::theme::Theme::fg_faint()))
        .child(label.to_string())
}

impl Render for AgentInstancesPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.filter.clone();
        let entries = self.model.filtered(&filter);
        let mut list = gpui::div()
            .flex()
            .flex_col()
            .gap_1()
            .child(instance_list_section("可用 Agent 类型"));
        let mut instance_header_added = false;
        for (idx, entry) in entries.iter().enumerate() {
            let is_default = entry.kind == "default-cli";
            if !is_default && !instance_header_added {
                list = list.child(instance_list_section("已配置实例"));
                instance_header_added = true;
            }
            let (title_color, sub_color) = if entry.available {
                (crate::theme::Theme::fg(), crate::theme::Theme::fg_dim())
            } else {
                (
                    crate::theme::Theme::fg_dim(),
                    crate::theme::Theme::warning(),
                )
            };
            let entry_id = entry.id.clone();
            let title = entry.title.clone();
            let subtitle = entry.subtitle.clone();
            let available = entry.available;
            let idx2 = idx;
            list = list.child(
                gpui::div()
                    .id(gpui::ElementId::Name(format!("inst-entry-{idx}").into()))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_panel())))
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |page: &mut AgentInstancesPage, _ev, _w, cx| {
                            let entry = page.model.filtered(&page.filter).get(idx2).cloned();
                            if let Some(entry) = entry {
                                if entry.kind == "default-cli" {
                                    if let Some(info) = page
                                        .model
                                        .type_infos()
                                        .iter()
                                        .find(|t| t.name == entry.title)
                                    {
                                        let id = info.id.clone();
                                        page.open_editor_for_type(&id, cx);
                                    }
                                } else if let Some(id) = entry.id {
                                    page.open_editor_for_instance(&id, cx);
                                }
                            }
                        }),
                    )
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .text_size(px(11.))
                                    .text_color(rgb(title_color))
                                    .child(title),
                            )
                            .child(
                                gpui::div()
                                    .flex_1()
                                    .text_size(px(9.))
                                    .text_color(rgb(sub_color))
                                    .child(subtitle),
                            )
                            .when(is_default && available, |d| {
                                d.child(
                                    action_chip(
                                        gpui::ElementId::Name(format!("inst-launch-{idx}").into()),
                                        "启动临时会话",
                                        crate::theme::Theme::accent(),
                                        true,
                                    )
                                    .on_click(cx.listener(
                                        move |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                            let entry = page
                                                .model
                                                .filtered(&page.filter)
                                                .get(idx2)
                                                .cloned();
                                            if let Some(entry) = entry {
                                                if let Some(info) = page
                                                    .model
                                                    .type_infos()
                                                    .iter()
                                                    .find(|t| t.name == entry.title)
                                                {
                                                    let id = info.id.clone();
                                                    page.launch_default_cli(&id, cx);
                                                }
                                            }
                                        },
                                    )),
                                )
                            }),
                    ),
            );
            let _ = entry_id;
        }

        let editor: AnyElement = match &self.editor {
            None => gpui::div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(crate::theme::Theme::fg_dim()))
                .text_size(px(11.))
                .child("从左侧选择 Agent 类型创建实例，或选择已有实例继续编辑")
                .into_any_element(),
            Some(state) => {
                let errors = state.validation();
                let can_save = state.can_save();
                let editing = state.editing_instance_id.is_some();
                let error_text = errors
                    .iter()
                    .map(|e| e.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                let secret_display = state.secret_display();
                let secrets_list = self.app.list_secrets().unwrap_or_default();
                let header = if editing {
                    format!("编辑实例 · {}", state.info.name)
                } else {
                    format!("新建实例 · {}", state.info.name)
                };
                let adapter_note = format!(
                    "适配器 {} · 贡献 {} · {}",
                    state.info.adapter,
                    state.info.full_contribution_id,
                    if state.info.supports_isolated_config {
                        "支持隔离配置"
                    } else {
                        "不支持隔离配置"
                    }
                );
                let values = [
                    (PageField::Name, "名称", state.name.clone()),
                    (
                        PageField::Executable,
                        "可执行文件",
                        state.executable.clone(),
                    ),
                    (PageField::Argv, "参数(空格分隔)", state.argv_text.clone()),
                    (
                        PageField::Env,
                        "环境变量(每行 KEY=VALUE)",
                        state.env_text.clone(),
                    ),
                ];
                gpui::div()
                    .id("inst-editor-scroll")
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .child(
                        gpui::div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_2()
                            .child(
                                gpui::div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(gpui::div().text_size(px(12.)).child(header))
                                    .child(
                                        gpui::div()
                                            .text_size(px(9.))
                                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                                            .child(adapter_note),
                                    ),
                            )
                            .children(values.iter().map(|(field, label, value)| {
                                let display = if value.is_empty() {
                                    "(空)".to_string()
                                } else {
                                    value.replace('\n', " ⏎ ")
                                };
                                let focused = self.field == *field;
                                let f = *field;
                                gpui::div()
                                    .id(gpui::ElementId::Name(
                                        format!("inst-field-{label}").into(),
                                    ))
                                    .flex()
                                    .gap_2()
                                    .items_start()
                                    .child(
                                        gpui::div()
                                            .w(px(140.))
                                            .text_size(px(10.))
                                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                                            .child(label.to_string()),
                                    )
                                    .child(
                                        gpui::div()
                                            .id(gpui::ElementId::Name(
                                                format!("inst-field-box-{label}").into(),
                                            ))
                                            .flex_1()
                                            .px_2()
                                            .py_1()
                                            .min_h(px(22.))
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(if focused {
                                                crate::theme::Theme::accent()
                                            } else {
                                                crate::theme::Theme::border()
                                            }))
                                            .text_size(px(10.))
                                            .cursor_pointer()
                                            .child(display)
                                            .on_click(cx.listener(
                                                move |page: &mut AgentInstancesPage,
                                                      _ev,
                                                      _w,
                                                      cx| {
                                                    page.field = f;
                                                    cx.notify();
                                                },
                                            )),
                                    )
                            }))
                            // 作用域 / 项目键 / 运行模式 / 启用(复审阻塞项 5)
                            .child(
                                gpui::div()
                                    .id("inst-scope-toggle")
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .cursor_pointer()
                                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                                    .on_click(cx.listener(
                                        |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                            if let Some(state) = page.editor.as_mut() {
                                                state.toggle_scope();
                                            }
                                            cx.notify();
                                        },
                                    ))
                                    .child(
                                        gpui::div()
                                            .w(px(140.))
                                            .text_size(px(10.))
                                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                                            .child("作用域(点击切换)"),
                                    )
                                    .child(
                                        gpui::div()
                                            .text_size(px(10.))
                                            .child(match state.scope {
                                                mf_agent::InstanceScope::User => {
                                                    "User(全局可见)".to_string()
                                                }
                                                mf_agent::InstanceScope::Project => {
                                                    "Project(绑定项目)".to_string()
                                                }
                                            }),
                                    ),
                            )
                            .when(state.scope == mf_agent::InstanceScope::Project, |d| {
                                d.child(
                                    gpui::div()
                                        .flex()
                                        .gap_2()
                                        .items_start()
                                        .child(
                                            gpui::div()
                                                .w(px(140.))
                                                .text_size(px(10.))
                                                .text_color(rgb(crate::theme::Theme::fg_dim()))
                                                .child("project_key"),
                                        )
                                        .child(
                                            gpui::div()
                                                .id("inst-project-key")
                                                .flex_1()
                                                .px_2()
                                                .py_1()
                                                .rounded_md()
                                                .border_1()
                                                .border_color(rgb(
                                                    if self.field == PageField::ProjectKey {
                                                        crate::theme::Theme::accent()
                                                    } else {
                                                        crate::theme::Theme::border()
                                                    },
                                                ))
                                                .text_size(px(10.))
                                                .cursor_pointer()
                                                .on_click(cx.listener(
                                                    |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                        page.field = PageField::ProjectKey;
                                                        cx.notify();
                                                    },
                                                ))
                                                .child(if state.project_key.is_empty() {
                                                    "(必填;如 my-project)".to_string()
                                                } else {
                                                    state.project_key.clone()
                                                }),
                                        ),
                                )
                            })
                            .child(
                                gpui::div()
                                    .id("inst-run-mode-toggle")
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .cursor_pointer()
                                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                                    .on_click(cx.listener(
                                        |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                            if let Some(state) = page.editor.as_mut() {
                                                state.toggle_run_mode();
                                            }
                                            cx.notify();
                                        },
                                    ))
                                    .child(
                                        gpui::div()
                                            .w(px(140.))
                                            .text_size(px(10.))
                                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                                            .child("运行模式(点击切换)"),
                                    )
                                    .child(
                                        gpui::div().text_size(px(10.)).child(match state.run_mode {
                                            mf_agent::RunMode::Interactive => "交互".to_string(),
                                            mf_agent::RunMode::OneShot => "一次性".to_string(),
                                        }),
                                    ),
                            )
                            .when(editing, |d| {
                                d.child(
                                    gpui::div()
                                        .id("inst-enabled-toggle")
                                        .flex()
                                        .gap_2()
                                        .items_center()
                                        .cursor_pointer()
                                        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                                        .on_click(cx.listener(
                                            |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                if let Some(state) = page.editor.as_mut() {
                                                    state.toggle_enabled();
                                                }
                                                cx.notify();
                                            },
                                        ))
                                        .child(
                                            gpui::div()
                                                .w(px(140.))
                                                .text_size(px(10.))
                                                .text_color(rgb(crate::theme::Theme::fg_dim()))
                                                .child("启用(点击切换)"),
                                        )
                                        .child(
                                            gpui::div().text_size(px(10.)).child(
                                                if state.enabled {
                                                    "已启用".to_string()
                                                } else {
                                                    "已禁用".to_string()
                                                },
                                            ),
                                        ),
                                )
                            })
                            // 插件 config_schema 声明式表单(真渲染)
                            .children(
                                state
                                    .config_form()
                                    .fields()
                                    .iter()
                                    .enumerate()
                                    .map(|(idx, field)| {
                                        let label = format!(
                                            "{}{}",
                                            field.label,
                                            if field.required { "(必填)" } else { "" }
                                        );
                                        let is_secret = field.kind == "secret";
                                        let options_note = if field.kind == "select" {
                                            format!("(可选:{})", field.options.join(" / "))
                                        } else {
                                            String::new()
                                        };
                                        gpui::div()
                                            .flex()
                                            .gap_2()
                                            .items_start()
                                            .child(
                                                gpui::div()
                                                    .w(px(140.))
                                                    .text_size(px(10.))
                                                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                                                    .child(label),
                                            )
                                            .child(
                                                gpui::div()
                                                    .id(gpui::ElementId::Name(
                                                        format!("inst-config-{}", field.id).into(),
                                                    ))
                                                    .flex_1()
                                                    .px_2()
                                                    .py_1()
                                                    .rounded_md()
                                                    .border_1()
                                                    .border_color(rgb(
                                                        if self.field
                                                            == PageField::ConfigField(idx)
                                                        {
                                                            crate::theme::Theme::accent()
                                                        } else {
                                                            crate::theme::Theme::border()
                                                        },
                                                    ))
                                                    .text_size(px(10.))
                                                    .cursor_pointer()
                                                    .on_click(cx.listener(
                                                        move |page: &mut AgentInstancesPage,
                                                              _ev,
                                                              _w,
                                                              cx| {
                                                            page.field =
                                                                PageField::ConfigField(idx);
                                                            cx.notify();
                                                        },
                                                    ))
                                                    .child(format!(
                                                        "{}{}",
                                                        state.config_form().masked_value(&field.id),
                                                        options_note
                                                    ))
                                                    .when(is_secret, |d| {
                                                        d.child(
                                                            gpui::div().text_size(px(9.)).child(
                                                                "(Secret 引用;由下方 Secret 管理选择)",
                                                            ),
                                                        )
                                                    }),
                                            )
                                    })
                                    .collect::<Vec<_>>(),
                            )
                            // Secret 引用(只显示引用 id + 掩码,不输入明文)
                            .child(
                                gpui::div()
                                    .flex()
                                    .gap_2()
                                    .items_start()
                                    .child(
                                        gpui::div()
                                            .w(px(140.))
                                            .text_size(px(10.))
                                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                                            .child("Secret 引用"),
                                    )
                                    .child(
                                        gpui::div()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                gpui::div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(
                                                        crate::theme::Theme::fg_dim(),
                                                    ))
                                                    .child(if secret_display.is_empty() {
                                                        "(无)".to_string()
                                                    } else {
                                                        secret_display
                                                    }),
                                            )
                                            .child(
                                                // 目录库 Secret 列表:点击附加引用
                                                gpui::div()
                                                    .flex()
                                                    .flex_col()
                                                    .gap_1()
                                                    .children(secrets_list.iter().map(
                                                        |desc| {
                                                            let id = desc.id.clone();
                                                            let referenced =
                                                                state.secret_refs.contains(&id);
                                                            gpui::div()
                                                                .id(gpui::ElementId::Name(
                                                                    format!(
                                                                        "inst-secret-{id}"
                                                                    )
                                                                    .into(),
                                                                ))
                                                                .flex()
                                                                .gap_2()
                                                                .items_center()
                                                                .text_size(px(9.))
                                                                .cursor_pointer()
                                                                .hover(|d| {
                                                                    d.bg(rgb(
                                                                        crate::theme::Theme::bg_hover(),
                                                                    ))
                                                                })
                                                                .child(format!(
                                                                    "{} = •••• ({}B)",
                                                                    desc.name, desc.byte_len
                                                                ))
                                                                .child(if referenced {
                                                                    "解除引用".to_string()
                                                                } else {
                                                                    "附加引用".to_string()
                                                                })
                                                                .on_click(cx.listener(
                                                                    move |page: &mut AgentInstancesPage,
                                                                          _ev,
                                                                          _w,
                                                                          cx| {
                                                                        if let Some(state) =
                                                                            page.editor.as_mut()
                                                                        {
                                                                            if referenced {
                                                                                state.remove_secret_ref(&id);
                                                                            } else {
                                                                                state.add_secret_ref(&id);
                                                                            }
                                                                        }
                                                                        cx.notify();
                                                                    },
                                                                ))
                                                        },
                                                    )),
                                            ),
                                    ),
                            )
                            // Secret 管理:seal(名称+值)/ 删除
                            .child(
                                gpui::div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        gpui::div()
                                            .w(px(140.))
                                            .text_size(px(10.))
                                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                                            .child("Secret 管理"),
                                    )
                                    .child(
                                        gpui::div()
                                            .id("inst-secret-name")
                                            .w(px(110.))
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(
                                                if self.field == PageField::SecretName {
                                                    crate::theme::Theme::accent()
                                                } else {
                                                    crate::theme::Theme::border()
                                                },
                                            ))
                                            .text_size(px(9.))
                                            .cursor_pointer()
                                            .on_click(cx.listener(
                                                |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                    page.field = PageField::SecretName;
                                                    cx.notify();
                                                },
                                            ))
                                            .child(if self.secret_name_input.is_empty() {
                                                "名称…".to_string()
                                            } else {
                                                self.secret_name_input.clone()
                                            }),
                                    )
                                    .child(
                                        gpui::div()
                                            .id("inst-secret-value")
                                            .w(px(110.))
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(
                                                if self.field == PageField::SecretValue {
                                                    crate::theme::Theme::accent()
                                                } else {
                                                    crate::theme::Theme::border()
                                                },
                                            ))
                                            .text_size(px(9.))
                                            .cursor_pointer()
                                            .on_click(cx.listener(
                                                |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                    page.field = PageField::SecretValue;
                                                    cx.notify();
                                                },
                                            ))
                                            .child(if self.secret_value_input.is_empty() {
                                                "值(密封后即清空)…".to_string()
                                            } else {
                                                "••••".to_string()
                                            }),
                                    )
                                    .child(
                                        action_chip(
                                            gpui::ElementId::Name("inst-secret-seal".into()),
                                            "密封",
                                            crate::theme::Theme::accent(),
                                            true,
                                        )
                                        .on_click(cx.listener(
                                            |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                page.seal_secret_input(cx);
                                            },
                                        )),
                                    )
                                    .children(secrets_list.iter().map(|desc| {
                                        let id = desc.id.clone();
                                        action_chip(
                                            gpui::ElementId::Name(
                                                format!("inst-secret-del-{}", desc.id).into(),
                                            ),
                                            "删除",
                                            crate::theme::Theme::danger(),
                                            true,
                                        )
                                        .on_click(cx.listener(
                                            move |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                page.delete_secret_by_id(id.clone(), cx);
                                            },
                                        ))
                                    }).collect::<Vec<_>>()),
                            )
                            // 结构化 secret_env:ENV 名 → Secret 引用行
                            .child(
                                gpui::div()
                                    .id("inst-secret-env")
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        gpui::div()
                                            .text_size(px(10.))
                                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                                            .child("Secret 环境变量(ENV → 引用)"),
                                    )
                                    .children(
                                        (0..state.secret_env_map.len()).map(|row| {
                                            let (env, ref_id) = state.secret_env_map[row].clone();
                                            gpui::div()
                                                .id(gpui::ElementId::Name(
                                                    format!("inst-secret-env-{row}").into(),
                                                ))
                                                .flex()
                                                .gap_1()
                                                .items_center()
                                                .child(
                                                    gpui::div()
                                                        .id(gpui::ElementId::Name(
                                                            format!("inst-secret-env-name-{row}").into(),
                                                        ))
                                                        .px_2()
                                                        .py_1()
                                                        .rounded_md()
                                                        .border_1()
                                                        .border_color(rgb(
                                                            if self.field == PageField::SecretEnvName(row) {
                                                                crate::theme::Theme::accent()
                                                            } else {
                                                                crate::theme::Theme::border()
                                                            },
                                                        ))
                                                        .text_size(px(9.))
                                                        .cursor_pointer()
                                                        .child(if env.is_empty() {
                                                            "ENV_NAME…".to_string()
                                                        } else {
                                                            env.clone()
                                                        })
                                                        .on_click(cx.listener(
                                                            move |page: &mut AgentInstancesPage,
                                                                  _ev,
                                                                  _w,
                                                                  cx| {
                                                                page.field =
                                                                    PageField::SecretEnvName(row);
                                                                cx.notify();
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    gpui::div()
                                                        .text_size(px(9.))
                                                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                                                        .child(format!("→ {ref_id}")),
                                                )
                                                .child(
                                                    action_chip(
                                                        gpui::ElementId::Name(
                                                            format!("inst-secret-env-cycle-{row}").into(),
                                                        ),
                                                        "换引用",
                                                        crate::theme::Theme::accent_dim(),
                                                        !state.secret_refs.is_empty(),
                                                    )
                                                    .on_click(cx.listener(
                                                        move |page: &mut AgentInstancesPage,
                                                              _ev,
                                                              _w,
                                                              cx| {
                                                            if let Some(state) = page.editor.as_mut() {
                                                                if let Some((env, cur)) =
                                                                    state.secret_env_map.get(row).cloned()
                                                                {
                                                                    let next = state
                                                                        .secret_refs
                                                                        .iter()
                                                                        .cycle()
                                                                        .skip(
                                                                            state
                                                                                .secret_refs
                                                                                .iter()
                                                                                .position(|r| *r == cur)
                                                                                .map(|p| p + 1)
                                                                                .unwrap_or(0),
                                                                        )
                                                                        .next()
                                                                        .cloned();
                                                                    if let Some(next) = next {
                                                                        state.set_secret_env(&env, &next);
                                                                    }
                                                                }
                                                            }
                                                            cx.notify();
                                                        },
                                                    )),
                                                )
                                                .child(
                                                    action_chip(
                                                        gpui::ElementId::Name(
                                                            format!("inst-secret-env-del-{row}").into(),
                                                        ),
                                                        "移除",
                                                        crate::theme::Theme::danger(),
                                                        true,
                                                    )
                                                    .on_click(cx.listener(
                                                        move |page: &mut AgentInstancesPage,
                                                              _ev,
                                                              _w,
                                                              cx| {
                                                            if let Some(state) = page.editor.as_mut() {
                                                                if let Some((env, _)) =
                                                                    state.secret_env_map.get(row).cloned()
                                                                {
                                                                    state.remove_secret_env(&env);
                                                                }
                                                            }
                                                            cx.notify();
                                                        },
                                                    )),
                                                )
                                        }),
                                    )
                                    .when(!state.secret_refs.is_empty(), |d| {
                                        d.child(
                                            action_chip(
                                                gpui::ElementId::Name("inst-secret-env-add".into()),
                                                "添加 ENV 映射",
                                                crate::theme::Theme::accent_dim(),
                                                true,
                                            )
                                            .on_click(cx.listener(
                                                |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                    if let Some(state) = page.editor.as_mut() {
                                                        if let Some(first) =
                                                            state.secret_refs.first().cloned()
                                                        {
                                                            let n = state.secret_env_map.len() + 1;
                                                            state.set_secret_env(
                                                                &format!("SECRET_ENV_{n}"),
                                                                &first,
                                                            );
                                                        }
                                                    }
                                                    cx.notify();
                                                },
                                            )),
                                        )
                                    }),
                            )
                            .child(
                                gpui::div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        action_chip(
                                            gpui::ElementId::Name("inst-validate".into()),
                                            "校验",
                                            crate::theme::Theme::accent(),
                                            true,
                                        )
                                        .on_click(cx.listener(
                                            |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                if let Some(state) = page.editor.as_ref() {
                                                    let errors = state.validation();
                                                    page.status = if errors.is_empty() {
                                                        "配置有效".into()
                                                    } else {
                                                        errors
                                                            .iter()
                                                            .map(|e| e.message.clone())
                                                            .collect::<Vec<_>>()
                                                            .join("; ")
                                                    };
                                                }
                                                cx.notify();
                                            },
                                        )),
                                    )
                                    .child(
                                        action_chip(
                                            gpui::ElementId::Name("inst-save".into()),
                                            if editing { "保存修改" } else { "保存实例" },
                                            crate::theme::Theme::accent(),
                                            can_save,
                                        )
                                        .on_click(cx.listener(
                                            |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                page.save(cx);
                                            },
                                        )),
                                    )
                                    .child(
                                        action_chip(
                                            gpui::ElementId::Name("inst-close".into()),
                                            "关闭",
                                            crate::theme::Theme::fg_dim(),
                                            true,
                                        )
                                        .on_click(cx.listener(
                                            |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                page.editor = None;
                                                page.field = PageField::None;
                                                cx.notify();
                                            },
                                        )),
                                    )
                                    .when(editing, |d| {
                                        d.child(
                                            action_chip(
                                                gpui::ElementId::Name("inst-delete".into()),
                                                "删除",
                                                crate::theme::Theme::danger(),
                                                true,
                                            )
                                            .on_click(cx.listener(
                                                |page: &mut AgentInstancesPage, _ev, _w, cx| {
                                                    page.delete_current(cx);
                                                },
                                            )),
                                        )
                                    }),
                            )
                            .when(!errors.is_empty(), |d| {
                                d.child(
                                    gpui::div()
                                        .text_size(px(9.))
                                        .text_color(rgb(crate::theme::Theme::danger()))
                                        .child(error_text),
                                )
                            }),
                    )
                    .into_any_element()
            }
        };

        let status = self.status.clone();
        let filter_focused = self.field == PageField::Filter;
        let filter_display = if self.filter.is_empty() {
            "过滤:点击后输入名称…".to_string()
        } else {
            self.filter.clone()
        };
        gpui::div()
            .id("instances-page")
            .size_full()
            .flex()
            .gap_2()
            .p_2()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(
                |page: &mut AgentInstancesPage, ev: &gpui::KeyDownEvent, _w, cx| {
                    page.handle_key(ev, cx);
                },
            ))
            .child(
                gpui::div()
                    .w(px(320.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child("智能体 · 实例配置"),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(9.))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("CLI、参数、环境、Secret 与隔离配置的唯一编辑入口"),
                    )
                    .child(
                        gpui::div()
                            .id("inst-filter-box")
                            .px_2()
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if filter_focused {
                                crate::theme::Theme::accent()
                            } else {
                                crate::theme::Theme::border()
                            }))
                            .text_size(px(10.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .cursor_pointer()
                            .child(filter_display)
                            .on_click(cx.listener(|page: &mut AgentInstancesPage, _ev, _w, cx| {
                                page.field = PageField::Filter;
                                cx.notify();
                            })),
                    )
                    .child(
                        gpui::div()
                            .id("inst-list-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(list),
                    ),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(editor)
                    .when(!status.is_empty(), |d| {
                        d.child(
                            gpui::div()
                                .text_size(px(9.))
                                .text_color(rgb(crate::theme::Theme::fg_dim()))
                                .child(status),
                        )
                    }),
            )
    }
}
