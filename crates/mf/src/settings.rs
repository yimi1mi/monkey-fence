use gpui::prelude::*;
use gpui::*;
use mf_agent::{Config, ProviderConfig, ProviderKind};

pub struct Saved(pub Config);
pub struct Dismissed;

/// 设置界面(模态):提供方/角色/引擎/编辑器字体,保存到 ~/.monkeyfence/config.toml
/// 设置页(两级导航)
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Page {
    Appearance,
    EditorTerm,
    VersionControl,
    Providers,
    Roles,
    Engine,
    Agents,
    Plugins,
}

pub struct SettingsView {
    draft: Config,
    page: Page,
    /// 应用上下文(插件注册表/检测);测试与无 GUI 场景可为 None
    app: Option<std::sync::Arc<crate::app_ctx::AppCtx>>,
    /// 嵌入式 Agent 配置页(Agent Type / 保存实例;ADR 0004 后
    /// 实例配置的唯一入口在设置 → 智能体)。
    pub(crate) agent_instances:
        Option<gpui::Entity<crate::agent_instances_view::AgentInstancesPage>>,
    /// 智能体页:当前展开详情的 profile id
    agent_expanded: Option<String>,
    /// 智能体页:命令/参数覆盖缓冲
    agent_cmd_s: String,
    agent_args_s: String,
    agent_perm_args_s: String,
    agent_env_s: String,
    /// 插件页:等待用户确认重新授权的插件 id(两步式,不做一键自动重授权)
    pending_reauth: Option<String>,
    /// 智能体页:一键安装进行中
    installing: bool,
    /// 插件页:本地目录 / Git URL 安装输入
    plugin_local_s: String,
    plugin_git_s: String,
    plugin_status: SharedString,
    /// 版本控制页:当前选中的插件贡献完整 ID 与测试状态。
    selected_vcs_provider: String,
    vcs_test_status: SharedString,
    vcs_test_root: Option<std::path::PathBuf>,
    /// Provider 管理页当前选中的提供方名
    selected_provider_name: String,
    test_status: SharedString,
    /// 当前编辑其提供方的角色
    selected_role: SharedString,
    /// 数值字段以字符串编辑,保存时解析
    workers_s: String,
    max_iters_s: String,
    max_failures_s: String,
    font_size_s: String,
    status: SharedString,
    focus_handle: FocusHandle,
    /// 所有轻量输入共用窗口焦点；该字段负责把键盘输入路由到实际点击的输入框。
    active_field: Option<Field>,
}

actions!(settings, [Dismiss]);

const ROLES: &[&str] = &["planner", "worker", "reviewer"];

#[derive(Clone, PartialEq, Debug)]
enum Field {
    RoleProvider,
    BaseUrl,
    ApiKey,
    Model,
    Workers,
    MaxIters,
    MaxFailures,
    FontFamily,
    FontSize,
    TerminalCommand,
    AgentCommand,
    AgentArgs,
    AgentPermArgs,
    AgentEnv,
    PluginLocal,
    PluginGit,
    /// 插件声明式设置字段(完整贡献 ID + 字段 ID)。
    PluginSetting(String, String),
}

impl SettingsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let draft = Config::load().unwrap_or_default();
        let workers_s = draft.engine.workers.to_string();
        let max_iters_s = draft.engine.max_iterations.to_string();
        let max_failures_s = draft.engine.max_failures.to_string();
        let font_size_s = format!("{:.1}", draft.editor.font_size);
        let selected_provider_name = draft
            .roles
            .get("planner")
            .cloned()
            .or_else(|| draft.providers.keys().next().cloned())
            .unwrap_or_else(fallback_provider_name);
        Self {
            draft,
            page: Page::Appearance,
            app: None,
            agent_instances: None,
            agent_expanded: None,
            agent_cmd_s: String::new(),
            agent_args_s: String::new(),
            agent_perm_args_s: String::new(),
            agent_env_s: String::new(),
            pending_reauth: None,
            installing: false,
            plugin_local_s: String::new(),
            plugin_git_s: String::new(),
            plugin_status: "".into(),
            selected_vcs_provider: String::new(),
            vcs_test_status: "".into(),
            vcs_test_root: None,
            selected_provider_name,
            test_status: "".into(),
            selected_role: "planner".into(),
            workers_s,
            max_iters_s,
            max_failures_s,
            font_size_s,
            status: "".into(),
            focus_handle: cx.focus_handle(),
            active_field: None,
        }
    }

    pub fn new_with_app(
        app: std::sync::Arc<crate::app_ctx::AppCtx>,
        active_project_root: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::new(cx);
        view.selected_vcs_provider = app
            .plugins()
            .contributions()
            .vcs_providers()
            .into_iter()
            .next()
            .map(|(full_id, _, _)| full_id)
            .unwrap_or_default();
        // 嵌入式 Agent 配置页(创建一次;关闭设置不销毁工作流状态)
        view.agent_instances = Some(cx.new(|cx| {
            crate::agent_instances_view::AgentInstancesPage::new_embedded(app.clone(), cx)
        }));
        view.app = Some(app);
        view.vcs_test_root = active_project_root;
        view
    }

    /// 选中角色的提供方配置(不存在时按需创建,便于新名字直接生效)
    fn selected_provider_mut(&mut self) -> &mut ProviderConfig {
        let role = self.selected_role.to_string();
        let name = self
            .draft
            .roles
            .get(&role)
            .cloned()
            .unwrap_or_else(fallback_provider_name);
        self.draft
            .providers
            .entry(name)
            .or_insert_with(fallback_provider_config)
    }

    fn selected_provider(&self) -> ProviderConfig {
        let name = self
            .draft
            .roles
            .get(self.selected_role.as_ref())
            .cloned()
            .unwrap_or_else(fallback_provider_name);
        self.draft
            .providers
            .get(&name)
            .cloned()
            .unwrap_or_else(fallback_provider_config)
    }

    fn field_text(&self, f: &Field) -> String {
        match f {
            Field::RoleProvider => self
                .draft
                .roles
                .get(self.selected_role.as_ref())
                .cloned()
                .unwrap_or_default(),
            Field::BaseUrl => self.cur_provider().base_url.clone(),
            Field::ApiKey => self.cur_provider().api_key.clone(),
            Field::Model => self.cur_provider().model.clone(),
            Field::Workers => self.workers_s.clone(),
            Field::MaxIters => self.max_iters_s.clone(),
            Field::MaxFailures => self.max_failures_s.clone(),
            Field::FontFamily => self.draft.editor.font_family.clone(),
            Field::FontSize => self.font_size_s.clone(),
            Field::TerminalCommand => self.draft.terminal.command.clone().unwrap_or_default(),
            Field::AgentCommand => self.agent_cmd_s.clone(),
            Field::AgentArgs => self.agent_args_s.clone(),
            Field::AgentPermArgs => self.agent_perm_args_s.clone(),
            Field::AgentEnv => self.agent_env_s.clone(),
            Field::PluginLocal => self.plugin_local_s.clone(),
            Field::PluginGit => self.plugin_git_s.clone(),
            Field::PluginSetting(contribution_id, field_id) => self
                .vcs_provider(contribution_id)
                .and_then(|provider| {
                    provider
                        .settings
                        .iter()
                        .find(|field| field.id == *field_id)
                        .map(|field| {
                            self.draft
                                .plugin_value(contribution_id, field_id, &field.default)
                        })
                })
                .unwrap_or_default(),
        }
    }

    fn do_save(&mut self, _: &ClickEvent, _w: &mut Window, cx: &mut Context<Self>) {
        self.draft.engine.workers = self.workers_s.trim().parse().unwrap_or(2).max(1).min(8);
        self.draft.engine.max_iterations = self.max_iters_s.trim().parse().unwrap_or(24).max(1);
        self.draft.engine.max_failures = self.max_failures_s.trim().parse().unwrap_or(3).max(1);
        self.draft.editor.font_size = self
            .font_size_s
            .trim()
            .parse::<f32>()
            .unwrap_or(13.0)
            .clamp(8.0, 32.0);
        match self.draft.save() {
            Ok(()) => {
                self.status = "已保存".into();
                if let Some(app) = &self.app {
                    app.refresh_catalog();
                }
                cx.emit(Saved(self.draft.clone()));
            }
            Err(e) => self.status = format!("保存失败: {e}").into(),
        }
        cx.notify();
    }

    fn do_reset(&mut self, _: &ClickEvent, _w: &mut Window, cx: &mut Context<Self>) {
        self.draft = Config::default();
        self.workers_s = self.draft.engine.workers.to_string();
        self.max_iters_s = self.draft.engine.max_iterations.to_string();
        self.max_failures_s = self.draft.engine.max_failures.to_string();
        self.font_size_s = format!("{:.1}", self.draft.editor.font_size);
        crate::theme::set_theme_id(&self.draft.editor.theme);
        self.status = "已恢复默认(未保存)".into();
        self.active_field = None;
        cx.notify();
    }

    fn do_dismiss(&mut self, _: &ClickEvent, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(Dismissed);
    }

    fn act_dismiss(&mut self, _: &Dismiss, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(Dismissed);
    }

    fn select_role(&mut self, role: &str, cx: &mut Context<Self>) {
        self.selected_role = role.into();
        self.active_field = None;
        cx.notify();
    }

    fn set_kind(&mut self, kind: ProviderKind, cx: &mut Context<Self>) {
        self.cur_provider_mut().kind = kind;
        cx.notify();
    }

    // ---------- Provider 管理页 ----------
    fn cur_provider(&self) -> ProviderConfig {
        self.draft
            .providers
            .get(&self.selected_provider_name)
            .cloned()
            .unwrap_or_else(fallback_provider_config)
    }
    fn cur_provider_mut(&mut self) -> &mut ProviderConfig {
        self.draft
            .providers
            .entry(self.selected_provider_name.clone())
            .or_insert_with(fallback_provider_config)
    }
    fn select_provider(&mut self, name: &str, cx: &mut Context<Self>) {
        self.selected_provider_name = name.to_string();
        self.test_status = "".into();
        self.active_field = None;
        cx.notify();
    }
    fn new_provider(&mut self, cx: &mut Context<Self>) {
        let mut i = 1;
        while self.draft.providers.contains_key(&format!("provider-{i}")) {
            i += 1;
        }
        let name = format!("provider-{i}");
        self.draft.providers.insert(
            name.clone(),
            ProviderConfig {
                kind: ProviderKind::Openai,
                base_url: String::new(),
                api_key: String::new(),
                model: String::new(),
            },
        );
        self.selected_provider_name = name;
        self.test_status = "".into();
        self.active_field = None;
        cx.notify();
    }
    fn delete_provider(&mut self, cx: &mut Context<Self>) {
        let name = self.selected_provider_name.clone();
        self.draft.providers.remove(&name);
        if mf_agent::config::mock_available() {
            // 调试构建:角色回退到 mock
            for (_, v) in self.draft.roles.iter_mut() {
                if *v == name {
                    *v = "mock".into();
                }
            }
        } else {
            // 发布构建不把 mock 写进配置:直接解除角色绑定
            self.draft.roles.retain(|_, v| *v != name);
        }
        self.selected_provider_name = self
            .draft
            .providers
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(fallback_provider_name);
        self.test_status = format!("已删除 {name}").into();
        self.active_field = None;
        cx.notify();
    }
    fn test_conn(&mut self, cx: &mut Context<Self>) {
        let prov = self.cur_provider();
        self.test_status = "测试中…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move {
                mf_agent::provider::test_connection(&prov.base_url, &prov.api_key)
            });
            let r = res.await;
            this.update(cx, |s, cx| {
                s.test_status = match r {
                    Ok(n) => format!("✓ 连接成功,{} 个模型", n).into(),
                    Err(e) => format!("✕ {e:#}").into(),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    fn apply_theme(&mut self, theme_id: &'static str, cx: &mut Context<Self>) {
        let applied = crate::theme::set_theme_id(theme_id);
        self.draft.editor.theme = applied.into();
        self.status = if applied == crate::theme::MORNING_MIST_ID {
            "已切换「晨雾」亮色主题(即时生效，保存后记住)"
        } else {
            "已切换「夜航」暗色主题(即时生效，保存后记住)"
        }
        .into();
        cx.notify();
    }
}

impl EventEmitter<Saved> for SettingsView {}
impl EventEmitter<Dismissed> for SettingsView {}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 遮罩:点击空白关闭
        div()
            .id("settings-overlay")
            .key_context("Settings")
            .size_full()
            .absolute()
            .top_0()
            .left_0()
            .bg(gpui::rgba(0x000000CC))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(60.))
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.emit(Dismissed);
                }),
            )
            .on_key_down(
                cx.listener(|s: &mut SettingsView, e: &KeyDownEvent, window, cx| {
                    // 所有轻量输入共用窗口焦点;active_field 决定字符去向(用户既有模式)
                    let Some(field) = s.active_field.clone() else {
                        return;
                    };
                    if !s.focus_handle.is_focused(window) {
                        return;
                    }
                    match e.keystroke.key.as_str() {
                        "backspace" => s.pop_field(field, cx),
                        "enter" | "escape" => {
                            s.active_field = None;
                        }
                        _ => {
                            if let Some(chars) = e.keystroke.key_char.clone() {
                                let printable: String =
                                    chars.chars().filter(|c| !c.is_control()).collect();
                                if !printable.is_empty() {
                                    s.push_field(field.clone(), &printable, cx);
                                }
                            }
                        }
                    }
                    cx.notify();
                }),
            )
            .on_action(cx.listener(Self::act_dismiss))
            .child(self.render_card(window, cx))
    }
}

impl SettingsView {
    fn render_card(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title_bar = div()
            .id("settings-title")
            .h(px(36.))
            .flex()
            .items_center()
            .px_4()
            .border_b_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .text_size(crate::theme::ui_px(13.))
            .font_weight(FontWeight::SEMIBOLD)
            .child("设置");

        div()
            .id("settings-card")
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(760.))
            .max_h(px(640.))
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .shadow_lg()
            .overflow_hidden()
            .child(title_bar)
            .child(self.render_body(window, cx))
            .child(self.render_footer(cx))
            .into_any_element()
    }

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("settings-body")
            .flex_1()
            .min_h_0()
            .flex()
            .child(self.render_nav(cx))
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(self.render_page(window, cx))
                    .child(
                        div()
                            .id("settings-hint")
                            .text_size(crate::theme::ui_px(11.))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("引擎/提供方改动在下次打开项目或新建运行时生效；VCS 环境保存后立即刷新当前项目。"),
                    ),
            )
            .into_any_element()
    }

    /// 左侧两级导航:通用[外观/编辑器与终端] · Agent 与模型[Provider 管理/角色绑定] · 引擎
    fn render_nav(&self, cx: &Context<Self>) -> AnyElement {
        let general = [Page::Appearance, Page::EditorTerm];
        let agent = [Page::Agents, Page::Providers, Page::Roles, Page::Plugins];
        let vcs = [Page::VersionControl];
        let engine = [Page::Engine];
        let groups: Vec<(&str, &[Page])> = vec![
            ("通用", &general),
            ("Agent 与模型", &agent),
            ("项目工具", &vcs),
            ("引擎", &engine),
        ];
        let page_name = |p: Page| match p {
            Page::Appearance => "外观",
            Page::EditorTerm => "编辑器与终端",
            Page::VersionControl => "版本控制",
            Page::Agents => "智能体",
            Page::Providers => "Provider 管理",
            Page::Roles => "角色绑定",
            Page::Plugins => "插件",
            Page::Engine => "引擎",
        };
        let mut items: Vec<AnyElement> = Vec::new();
        for (gname, pages) in &groups {
            if !gname.is_empty() {
                items.push(
                    div()
                        .id(ElementId::Name(format!("nav-g-{}", gname).into()))
                        .px_2()
                        .pt_3()
                        .pb_1()
                        .text_size(crate::theme::ui_px(9.5))
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child(gname.to_string())
                        .into_any_element(),
                );
            }
            for &pg in *pages {
                let cur = self.page == pg;
                items.push(
                    div()
                        .id(ElementId::Name(format!("nav-{:?}", pg).into()))
                        .px_2()
                        .py(px(4.))
                        .pl_3()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_size(crate::theme::ui_px(12.))
                        .when(cur, |d| {
                            d.bg(rgb(crate::theme::Theme::accent_dim()))
                                .text_color(rgb(crate::theme::Theme::fg()))
                        })
                        .when(!cur, |d| {
                            d.text_color(rgb(crate::theme::Theme::fg_dim()))
                                .hover(|h| h.bg(rgb(crate::theme::Theme::bg_hover())))
                        })
                        .child(page_name(pg))
                        .on_click(cx.listener(move |s, _, _, cx| {
                            s.page = pg;
                            s.active_field = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
        }
        div()
            .id("settings-nav")
            .w(px(150.))
            .min_h_0()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg_elevated()))
            .p_1()
            .flex()
            .flex_col()
            .children(items)
            .into_any_element()
    }

    fn render_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.page {
            Page::Appearance => self.render_appearance_section(window, cx),
            Page::EditorTerm => self.render_editor_section(window, cx),
            Page::VersionControl => self.render_vcs_page(window, cx),
            Page::Providers => self.render_providers_page(window, cx),
            Page::Roles => self.render_roles_page(window, cx),
            Page::Engine => self.render_engine_section(window, cx),
            Page::Agents => self.render_agent_policy_page(cx),
            Page::Plugins => self.render_plugins_page(window, cx),
        }
    }

    fn render_appearance_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme_id = crate::theme::current_theme_id();
        div()
            .id("appearance-fields")
            .flex()
            .flex_col()
            .gap_2()
            .child(section("外观"))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(theme_btn(
                        "夜航",
                        "暗色 · 低眩光海军蓝",
                        theme_id == crate::theme::NIGHT_VOYAGE_ID,
                        crate::theme::NIGHT_VOYAGE_ID,
                        0x151b24,
                        0x62aaf7,
                        cx,
                    ))
                    .child(theme_btn(
                        "晨雾",
                        "亮色 · 暖灰纸面",
                        theme_id == crate::theme::MORNING_MIST_ID,
                        crate::theme::MORNING_MIST_ID,
                        0xf3f2ee,
                        0x356fc4,
                        cx,
                    )),
            )
            .child(field_row(
                "字号(px)",
                self.text_input(Field::FontSize, window, cx),
            ))
            .into_any_element()
    }

    /// 角色绑定页 = 角色列表 + 选中角色的 provider 名编辑
    fn render_roles_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("roles-page")
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_roles_section(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section("绑定提供方(输入名字,新名字会自动创建)"))
                    .child(field_row(
                        "提供方名称",
                        self.text_input(Field::RoleProvider, window, cx),
                    )),
            )
            .into_any_element()
    }

    /// Provider 管理页:列表 + 行内编辑 + 新建/删除/测试连接
    fn render_providers_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mut names: Vec<String> = self.draft.providers.keys().cloned().collect();
        names.sort();
        let sel = self.selected_provider_name.clone();
        let mut list_rows: Vec<AnyElement> = vec![section("Provider 列表").into_any_element()];
        for name in &names {
            let p = &self.draft.providers[name];
            let selected = *name == sel;
            list_rows.push(
                div()
                    .id(ElementId::Name(format!("prov-{}", name).into()))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(4.))
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |d| {
                        d.bg(rgb(crate::theme::Theme::bg_active()))
                            .border_l_2()
                            .border_color(rgb(crate::theme::Theme::accent()))
                    })
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .on_click({
                        let nm = name.clone();
                        cx.listener(move |s, _, _, cx| {
                            s.select_provider(&nm, cx);
                        })
                    })
                    .child(
                        div()
                            .text_size(crate::theme::ui_px(12.))
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child(name.clone()),
                    )
                    .child(kind_badge(p.kind.kind_str()))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(crate::theme::ui_px(10.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(p.model.clone()),
                    )
                    .into_any_element(),
            );
        }
        div()
            .id("providers-page")
            .flex()
            .flex_col()
            .gap_3()
            .children(list_rows)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(small_btn(
                        "＋ 新建",
                        "prov-new",
                        cx,
                        cx.listener(|s, _, _, cx| {
                            s.new_provider(cx);
                        }),
                    ))
                    .child(small_btn(
                        "🗑 删除",
                        "prov-del",
                        cx,
                        cx.listener(|s, _, _, cx| {
                            s.delete_provider(cx);
                        }),
                    ))
                    .child(small_btn(
                        "⚡ 测试连接",
                        "prov-test",
                        cx,
                        cx.listener(|s, _, _, cx| {
                            s.test_conn(cx);
                        }),
                    ))
                    .child(div().flex_1())
                    .when(!self.test_status.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(crate::theme::ui_px(11.))
                                .text_color(rgb(if self.test_status.starts_with('✓') {
                                    crate::theme::Theme::success()
                                } else {
                                    crate::theme::Theme::danger()
                                }))
                                .child(self.test_status.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section(format!("编辑 [{}]", sel)))
                    .child(field_row_kind("类型", self.cur_provider().kind, cx))
                    .child(field_row(
                        "base_url",
                        self.text_input(Field::BaseUrl, window, cx),
                    ))
                    .child(field_row(
                        "api_key",
                        self.text_input(Field::ApiKey, window, cx),
                    ))
                    .child(field_row(
                        "model",
                        self.text_input(Field::Model, window, cx),
                    )),
            )
            .into_any_element()
    }

    fn render_roles_section(&self, cx: &Context<Self>) -> AnyElement {
        let draft = self.draft.clone();
        let sel_role = self.selected_role.clone();
        let mut rows = vec![section("角色 → 提供方").into_any_element()];
        for role in ROLES {
            let selected = sel_role.as_ref() == *role;
            // 发布构建未绑定的角色显示占位,不显示 mock
            let prov_name = draft.roles.get(*role).cloned().unwrap_or_else(|| {
                if mf_agent::config::mock_available() {
                    "mock".into()
                } else {
                    "(未绑定)".into()
                }
            });
            let kind = draft
                .providers
                .get(&prov_name)
                .map(|p| p.kind.kind_str())
                .unwrap_or("?");
            rows.push(
                div()
                    .id(ElementId::Name(format!("role-{}", role).into()))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py(px(4.))
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(crate::theme::Theme::bg_active())))
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.select_role(role, cx);
                    }))
                    .child(self.role_label(*role, selected))
                    .child(self.role_value(&prov_name))
                    .child(kind_badge(kind))
                    .into_any_element(),
            );
        }
        div()
            .id("role-rows")
            .flex()
            .flex_col()
            .gap_1()
            .children(rows)
            .into_any_element()
    }

    fn role_label(&self, role: &str, selected: bool) -> Div {
        div()
            .w(px(80.))
            .text_size(crate::theme::ui_px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(if selected {
                crate::theme::Theme::accent()
            } else {
                crate::theme::Theme::fg_dim()
            }))
            .child(role.to_string())
    }

    fn role_value(&self, prov_name: &str) -> Div {
        div()
            .flex_1()
            .text_size(crate::theme::ui_px(12.))
            .text_color(rgb(crate::theme::Theme::fg()))
            .child(prov_name.to_string())
    }

    fn vcs_provider(&self, full_id: &str) -> Option<mf_plugins::manifest::VcsProviderContribution> {
        self.app
            .as_ref()?
            .plugins()
            .contributions()
            .find_vcs_provider(full_id)
            .map(|(_, provider)| provider)
    }

    fn test_vcs_provider(&mut self, cx: &mut Context<Self>) {
        let Some(app) = self.app.clone() else {
            self.vcs_test_status = "应用上下文不可用".into();
            cx.notify();
            return;
        };
        let full_id = self.selected_vcs_provider.clone();
        if full_id.is_empty() {
            self.vcs_test_status = "没有已启用的 VCS Provider 插件".into();
            cx.notify();
            return;
        }
        let registry = app.plugins().contributions();
        let config = self.draft.clone();
        let cwd = self
            .vcs_test_root
            .clone()
            .or_else(|| app.project_roots().into_iter().next())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        self.vcs_test_status = "正在测试插件实例…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    mf_plugins::vcs_provider::test_provider(&registry, &config, &full_id, &cwd)
                })
                .await;
            this.update(cx, |settings, cx| {
                settings.vcs_test_status = match result {
                    Ok(detail) => format!("✓ {detail}").into(),
                    Err(error) => format!("✕ {error:#}").into(),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 通用 VCS 插件设置页。列表、字段、默认值和适配器说明全部来自
    /// ContributionRegistry；增加第三方 Provider 不需要修改这里的字段代码。
    fn render_vcs_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(app) = self.app.clone() else {
            return div()
                .p_3()
                .text_size(crate::theme::ui_px(12.))
                .child("此页面需要应用上下文")
                .into_any_element();
        };
        let providers = app.plugins().contributions().vcs_providers();
        if providers.is_empty() {
            return div()
                .flex()
                .flex_col()
                .gap_2()
                .child(section("版本控制"))
                .child("没有已启用且贡献 VCS Provider 的插件。")
                .into_any_element();
        }
        if !providers
            .iter()
            .any(|(full_id, _, _)| *full_id == self.selected_vcs_provider)
        {
            self.selected_vcs_provider = providers[0].0.clone();
        }
        let selected_id = self.selected_vcs_provider.clone();
        let selected_provider = providers
            .iter()
            .find(|(full_id, _, _)| *full_id == selected_id)
            .map(|(_, _, provider)| provider.clone())
            .unwrap_or_else(|| providers[0].2.clone());

        let mut provider_list = div()
            .id("vcs-provider-list")
            .flex()
            .flex_col()
            .gap_1()
            .child(section("已启用的 VCS Provider 插件"));
        for (full_id, source, provider) in &providers {
            let selected = *full_id == selected_id;
            let id = full_id.clone();
            provider_list = provider_list.child(
                div()
                    .id(ElementId::Name(format!("vcs-provider-{full_id}").into()))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |row| {
                        row.bg(rgb(crate::theme::Theme::bg_active()))
                            .border_l_2()
                            .border_color(rgb(crate::theme::Theme::accent()))
                    })
                    .hover(|row| row.bg(rgb(crate::theme::Theme::bg_hover())))
                    .on_click(cx.listener(move |settings, _, _, cx| {
                        settings.selected_vcs_provider = id.clone();
                        settings.vcs_test_status = "".into();
                        settings.active_field = None;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(90.))
                            .text_size(crate::theme::ui_px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(provider.name.clone()),
                    )
                    .child(kind_badge(&provider.adapter))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(crate::theme::ui_px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(format!(
                                "{} {}",
                                source.plugin_full_id, source.plugin_version
                            )),
                    ),
            );
        }

        let mut fields = div()
            .id("vcs-provider-settings")
            .flex()
            .flex_col()
            .gap_2()
            .child(section(format!("{} 环境", selected_provider.name)))
            .child(
                div()
                    .text_size(crate::theme::ui_px(10.5))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child(selected_provider.description.clone()),
            );
        for field in selected_provider.settings.clone() {
            let value = self
                .draft
                .plugin_value(&selected_id, &field.id, &field.default);
            match field.kind.as_str() {
                "boolean" => {
                    let on = value.parse::<bool>().unwrap_or(false);
                    let contribution_id = selected_id.clone();
                    let field_id = field.id.clone();
                    fields = fields.child(toggle_row(
                        cx,
                        ElementId::Name(format!("vcs-setting-{contribution_id}-{field_id}").into()),
                        &field.label,
                        on,
                        move |settings, _, _, cx| {
                            settings.draft.set_plugin_value(
                                contribution_id.clone(),
                                field_id.clone(),
                                (!on).to_string(),
                            );
                            settings.vcs_test_status = "".into();
                            cx.notify();
                        },
                    ));
                }
                "select" => {
                    let mut choices = div().flex().items_center().gap_1();
                    for option in field.options.clone() {
                        let active = value == option.value;
                        let contribution_id = selected_id.clone();
                        let field_id = field.id.clone();
                        let option_value = option.value.clone();
                        choices = choices.child(
                            div()
                                .id(ElementId::Name(
                                    format!(
                                        "vcs-setting-{contribution_id}-{field_id}-{option_value}"
                                    )
                                    .into(),
                                ))
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(if active {
                                    crate::theme::Theme::accent()
                                } else {
                                    crate::theme::Theme::border()
                                }))
                                .text_size(crate::theme::ui_px(10.5))
                                .text_color(rgb(if active {
                                    crate::theme::Theme::accent()
                                } else {
                                    crate::theme::Theme::fg_dim()
                                }))
                                .cursor_pointer()
                                .hover(|item| item.bg(rgb(crate::theme::Theme::bg_hover())))
                                .child(option.label)
                                .on_click(cx.listener(move |settings, _, _, cx| {
                                    settings.draft.set_plugin_value(
                                        contribution_id.clone(),
                                        field_id.clone(),
                                        option_value.clone(),
                                    );
                                    settings.vcs_test_status = "".into();
                                    cx.notify();
                                })),
                        );
                    }
                    fields = fields.child(field_row(&field.label, choices));
                }
                _ => {
                    fields = fields.child(field_row(
                        &field.label,
                        self.text_input(
                            Field::PluginSetting(selected_id.clone(), field.id.clone()),
                            window,
                            cx,
                        ),
                    ));
                }
            }
            if !field.description.is_empty() {
                fields = fields.child(
                    div()
                        .ml(px(118.))
                        .text_size(crate::theme::ui_px(9.5))
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child(field.description),
                );
            }
        }

        div()
            .id("vcs-settings-page")
            .flex()
            .flex_col()
            .gap_3()
            .child(provider_list)
            .child(fields)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(small_btn(
                        "测试环境",
                        "vcs-test-environment",
                        cx,
                        cx.listener(|settings, _, _, cx| settings.test_vcs_provider(cx)),
                    ))
                    .child(
                        div()
                            .text_size(crate::theme::ui_px(10.5))
                            .text_color(rgb(if self.vcs_test_status.starts_with('✓') {
                                crate::theme::Theme::success()
                            } else if self.vcs_test_status.starts_with('✕') {
                                crate::theme::Theme::danger()
                            } else {
                                crate::theme::Theme::fg_dim()
                            }))
                            .child(self.vcs_test_status.clone()),
                    ),
            )
            .child(
                div()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(crate::theme::Theme::bg_elevated()))
                    .text_size(crate::theme::ui_px(10.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("这些值只保存在 MonkeyFence 的单一插件实例中，不会改写 Git 或 Perforce 的全局配置。"),
            )
            .into_any_element()
    }

    fn render_engine_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("engine-fields")
            .flex()
            .flex_col()
            .gap_2()
            .child(section("引擎"))
            .child(field_row(
                "并行 worker 数",
                self.text_input(Field::Workers, window, cx),
            ))
            .child(field_row(
                "工具循环轮数",
                self.text_input(Field::MaxIters, window, cx),
            ))
            .child(field_row(
                "失败熔断次数",
                self.text_input(Field::MaxFailures, window, cx),
            ))
            .into_any_element()
    }

    fn render_editor_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("editor-fields")
            .flex()
            .flex_col()
            .gap_2()
            .child(section("编辑器"))
            .child(field_row(
                "字体",
                self.text_input(Field::FontFamily, window, cx),
            ))
            .child(field_row(
                "字号(px)",
                self.text_input(Field::FontSize, window, cx),
            ))
            .child(field_row(
                "终端命令(空=cmd;可填 codex 等 CLI)",
                self.text_input(Field::TerminalCommand, window, cx),
            ))
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("settings-footer")
            .h(px(44.))
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .border_t_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .child(
                div()
                    .id("settings-status")
                    .flex_1()
                    .text_size(crate::theme::ui_px(11.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child(self.status.clone()),
            )
            .child(primary_btn("保存", cx.listener(Self::do_save)))
            .child(secondary_btn("恢复默认", cx.listener(Self::do_reset)))
            .child(secondary_btn("关闭", cx.listener(Self::do_dismiss)))
            .into_any_element()
    }
}

impl SettingsView {
    /// 单行文本输入(点击聚焦后直接键入,与 VCS 提交描述一致的轻量输入)
    fn text_input(
        &self,
        field: Field,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let text = self.field_text(&field);
        let is_active =
            self.active_field.as_ref() == Some(&field) && self.focus_handle.is_focused(window);
        let element_id = format!("settings-input-{field:?}");
        div()
            .id(ElementId::Name(element_id.into()))
            .flex_1()
            .h(px(24.))
            .px_2()
            .rounded_sm()
            .border_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg()))
            .when(is_active, |d| {
                d.border_color(rgb(crate::theme::Theme::accent()))
            })
            .text_size(crate::theme::ui_px(12.))
            .text_color(rgb(crate::theme::Theme::fg()))
            .overflow_hidden()
            .on_click(cx.listener(move |s, _, window, cx| {
                s.active_field = Some(field.clone());
                let focus_handle = s.focus_handle.clone();
                window.focus(&focus_handle, cx);
                cx.notify();
            }))
            .child(if text.is_empty() {
                SharedString::from("输入…")
            } else {
                SharedString::from(text)
            })
    }

    fn push_field(&mut self, field: Field, text: &str, cx: &mut Context<Self>) {
        match field {
            Field::RoleProvider => {
                let role = self.selected_role.as_ref().to_string();
                let old = self.draft.roles.get(&role).cloned().unwrap_or_default();
                let new = format!("{}{}", old, text);
                // 同名迁移旧配置,避免改名后丢失密钥等
                if !old.is_empty() && old != new && !self.draft.providers.contains_key(&new) {
                    if let Some(p) = self.draft.providers.get(&old).cloned() {
                        self.draft.providers.insert(new.clone(), p);
                    }
                }
                self.draft.roles.insert(role, new);
            }
            Field::BaseUrl => self.cur_provider_mut().base_url.push_str(text),
            Field::ApiKey => self.cur_provider_mut().api_key.push_str(text),
            Field::Model => self.cur_provider_mut().model.push_str(text),
            Field::Workers => self.workers_s.push_str(text),
            Field::MaxIters => self.max_iters_s.push_str(text),
            Field::MaxFailures => self.max_failures_s.push_str(text),
            Field::FontFamily => self.draft.editor.font_family.push_str(text),
            Field::FontSize => self.font_size_s.push_str(text),
            Field::TerminalCommand => self
                .draft
                .terminal
                .command
                .get_or_insert_with(String::new)
                .push_str(text),
            Field::AgentCommand => self.agent_cmd_s.push_str(text),
            Field::AgentArgs => self.agent_args_s.push_str(text),
            Field::AgentPermArgs => self.agent_perm_args_s.push_str(text),
            Field::AgentEnv => self.agent_env_s.push_str(text),
            Field::PluginLocal => self.plugin_local_s.push_str(text),
            Field::PluginGit => self.plugin_git_s.push_str(text),
            Field::PluginSetting(contribution_id, field_id) => {
                let default = self
                    .vcs_provider(&contribution_id)
                    .and_then(|provider| {
                        provider
                            .settings
                            .into_iter()
                            .find(|field| field.id == field_id)
                            .map(|field| field.default)
                    })
                    .unwrap_or_default();
                let current = self
                    .draft
                    .plugin_value(&contribution_id, &field_id, &default);
                self.draft
                    .set_plugin_value(contribution_id, field_id, format!("{current}{text}"));
                self.vcs_test_status = "".into();
            }
        }
        cx.notify();
    }

    fn pop_field(&mut self, field: Field, cx: &mut Context<Self>) {
        match field {
            Field::RoleProvider => {
                if let Some(v) = self.draft.roles.get_mut(self.selected_role.as_ref()) {
                    v.pop();
                }
            }
            Field::BaseUrl => {
                self.cur_provider_mut().base_url.pop();
            }
            Field::ApiKey => {
                self.cur_provider_mut().api_key.pop();
            }
            Field::Model => {
                self.cur_provider_mut().model.pop();
            }
            Field::Workers => {
                self.workers_s.pop();
            }
            Field::MaxIters => {
                self.max_iters_s.pop();
            }
            Field::MaxFailures => {
                self.max_failures_s.pop();
            }
            Field::FontFamily => {
                self.draft.editor.font_family.pop();
            }
            Field::FontSize => {
                self.font_size_s.pop();
            }
            Field::TerminalCommand => {
                if let Some(c) = self.draft.terminal.command.as_mut() {
                    c.pop();
                }
            }
            Field::AgentCommand => {
                self.agent_cmd_s.pop();
            }
            Field::AgentArgs => {
                self.agent_args_s.pop();
            }
            Field::AgentPermArgs => {
                self.agent_perm_args_s.pop();
            }
            Field::AgentEnv => {
                self.agent_env_s.pop();
            }
            Field::PluginLocal => {
                self.plugin_local_s.pop();
            }
            Field::PluginGit => {
                self.plugin_git_s.pop();
            }
            Field::PluginSetting(contribution_id, field_id) => {
                let default = self
                    .vcs_provider(&contribution_id)
                    .and_then(|provider| {
                        provider
                            .settings
                            .into_iter()
                            .find(|field| field.id == field_id)
                            .map(|field| field.default)
                    })
                    .unwrap_or_default();
                let mut current = self
                    .draft
                    .plugin_value(&contribution_id, &field_id, &default);
                current.pop();
                self.draft
                    .set_plugin_value(contribution_id, field_id, current);
                self.vcs_test_status = "".into();
            }
        }
        cx.notify();
    }
}

fn section(title: impl Into<SharedString>) -> Div {
    div()
        .text_size(crate::theme::ui_px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(crate::theme::Theme::accent()))
        .child(title.into())
}

fn field_row(label: &str, input: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(110.))
                .text_size(crate::theme::ui_px(12.))
                .text_color(rgb(crate::theme::Theme::fg_dim()))
                .child(label.to_string()),
        )
        .child(input)
}

/// 选中项缺失时的回退名:调试用 mock,发布用 provider-1
/// (一个合法、可保存的名字,绝不把 mock 写进配置)。
fn fallback_provider_name() -> String {
    if mf_agent::config::mock_available() {
        "mock".into()
    } else {
        "provider-1".into()
    }
}

/// 按需创建提供方时的回退配置:调试用 mock,发布用 openai 占位。
fn fallback_provider_config() -> ProviderConfig {
    ProviderConfig {
        kind: if mf_agent::config::mock_available() {
            ProviderKind::Mock
        } else {
            ProviderKind::Openai
        },
        base_url: String::new(),
        api_key: String::new(),
        model: String::new(),
    }
}

fn field_row_kind(label: &str, current: ProviderKind, cx: &Context<SettingsView>) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(110.))
                .text_size(crate::theme::ui_px(12.))
                .text_color(rgb(crate::theme::Theme::fg_dim()))
                .child(label.to_string()),
        )
        // mock 仅调试构建显示(发布构建不出现,也不进配置文件)
        .when(mf_agent::config::mock_available(), |d| {
            d.child(kind_btn(
                "mock",
                current == ProviderKind::Mock,
                ProviderKind::Mock,
                cx,
            ))
        })
        .child(kind_btn(
            "openai",
            current == ProviderKind::Openai,
            ProviderKind::Openai,
            cx,
        ))
        .child(kind_btn(
            "anthropic",
            current == ProviderKind::Anthropic,
            ProviderKind::Anthropic,
            cx,
        ))
}

fn kind_btn(
    label: &str,
    active: bool,
    kind: ProviderKind,
    cx: &Context<SettingsView>,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("kind-{}", label).into()))
        .px_2()
        .py(px(2.))
        .rounded_sm()
        .border_1()
        .border_color(rgb(if active {
            crate::theme::Theme::accent()
        } else {
            crate::theme::Theme::border()
        }))
        .text_size(crate::theme::ui_px(11.))
        .text_color(rgb(if active {
            crate::theme::Theme::accent()
        } else {
            crate::theme::Theme::fg_dim()
        }))
        .when(active, |d| d.bg(rgb(crate::theme::Theme::bg_active())))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
        .child(label.to_string())
        .on_click(cx.listener(move |s, _, _, cx| {
            s.set_kind(kind, cx);
        }))
}

fn primary_btn(
    label: &str,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("settings-btn-{}", label).into()))
        .px_3()
        .py(px(4.))
        .rounded_sm()
        .bg(rgb(crate::theme::Theme::accent()))
        .text_size(crate::theme::ui_px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(0xffffff))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(crate::theme::Theme::accent_dim())))
        .child(label.to_string())
        .on_click(move |e, w, cx| listener(e, w, cx))
}

fn secondary_btn(
    label: &str,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("settings-btn-{}", label).into()))
        .px_3()
        .py(px(4.))
        .rounded_sm()
        .border_1()
        .border_color(rgb(crate::theme::Theme::border()))
        .bg(rgb(crate::theme::Theme::bg_elevated()))
        .text_size(crate::theme::ui_px(12.))
        .text_color(rgb(crate::theme::Theme::fg_dim()))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
        .child(label.to_string())
        .on_click(move |e, w, cx| listener(e, w, cx))
}

fn kind_badge(kind: &str) -> SharedString {
    format!("[{}]", kind).into()
}

fn small_btn(
    label: &str,
    id: &str,
    _cx: &Context<SettingsView>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let _ = _cx;
    div()
        .id(ElementId::Name(id.into()))
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(crate::theme::Theme::border()))
        .cursor_pointer()
        .text_size(crate::theme::ui_px(11.))
        .text_color(rgb(crate::theme::Theme::fg_dim()))
        .hover(|d| d.border_color(rgb(crate::theme::Theme::accent())))
        .child(label.to_string())
        .on_click(move |e, w, cx| on_click(e, w, cx))
}

fn theme_btn(
    label: &str,
    description: &str,
    active: bool,
    theme_id: &'static str,
    preview_bg: u32,
    preview_accent: u32,
    cx: &Context<SettingsView>,
) -> impl IntoElement {
    let label = label.to_string();
    let description = description.to_string();
    div()
        .id(ElementId::Name(format!("theme-{}", label).into()))
        .flex_1()
        .p_2()
        .rounded_sm()
        .border_1()
        .cursor_pointer()
        .border_color(rgb(if active {
            crate::theme::Theme::accent()
        } else {
            crate::theme::Theme::border()
        }))
        .when(active, |d| d.bg(rgb(crate::theme::Theme::bg_active())))
        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .size(px(24.))
                        .rounded_sm()
                        .bg(rgb(preview_bg))
                        .border_1()
                        .border_color(rgb(preview_accent))
                        .child(
                            div()
                                .m(px(7.))
                                .size(px(8.))
                                .rounded_full()
                                .bg(rgb(preview_accent)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(crate::theme::ui_px(11.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(if active {
                                    crate::theme::Theme::accent()
                                } else {
                                    crate::theme::Theme::fg()
                                }))
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(crate::theme::ui_px(9.5))
                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                .child(description),
                        ),
                ),
        )
        .on_click(cx.listener(move |s, _, _, cx| {
            s.apply_theme(theme_id, cx);
        }))
}

// ---------- 智能体设置页 / 插件管理页 ----------

impl SettingsView {
    /// 设置页只保留跨实例的全局策略；命令、参数、环境、Secret 与隔离配置
    /// 全部归 AgentInstancesPage，工作流只保存实例引用。
    fn render_agent_policy_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let manual = self.draft.agents.permission_mode == "manual";
        let selected_default = self.draft.agents.default_agent.clone();
        let mut default_rows = div().flex().flex_col().gap_1();
        for (id, label) in [
            ("", "自动（由工作流节点决定）"),
            ("blank-terminal", "空白终端"),
        ] {
            let id_owned = id.to_string();
            default_rows = default_rows.child(toggle_row(
                cx,
                format!("default-agent-{id}"),
                label,
                selected_default == id,
                move |settings, _, _, cx| {
                    settings.draft.agents.default_agent = id_owned.clone();
                    cx.notify();
                },
            ));
        }
        if let Some(app) = self.app.as_ref() {
            for profile in app
                .plugins()
                .agent_profiles()
                .into_iter()
                .filter(|profile| {
                    profile.id != "blank-terminal"
                        && (profile.runtime != mf_agent::RuntimeKind::Pty
                            || mf_plugins::builtin::detect_on_path(&profile.command).is_some())
                })
            {
                let id = profile.id.clone();
                let label = profile.display_name.clone();
                let selected = selected_default == id;
                default_rows = default_rows.child(toggle_row(
                    cx,
                    gpui::ElementId::Name(format!("default-agent-{id}").into()),
                    &label,
                    selected,
                    move |settings, _, _, cx| {
                        settings.draft.agents.default_agent = id.clone();
                        cx.notify();
                    },
                ));
            }
        }
        div()
            .id("agent-policy-page")
            .flex()
            .flex_col()
            .gap_3()
            .child(section("职责边界"))
            .child(responsibility_row(
                "智能体",
                "创建和编辑实例：CLI、参数、环境变量、Secret、隔离配置与启停。",
                "唯一配置入口",
            ))
            .child(responsibility_row(
                "工作流",
                "编排节点、依赖和指令；节点只引用现有实例，不复制实例配置。",
                "只做编排",
            ))
            .child(responsibility_row(
                "会话 / 运行",
                "查看实时 CLI、Transcript 和运行状态；不承载配置。",
                "只做观察",
            ))
            .child(responsibility_row(
                "本页",
                "只控制所有实例共同遵守的安全与设备策略。",
                "全局策略",
            ))
            .child(section("默认智能体"))
            .child(default_rows)
            .child(section("权限参数总策略"))
            .child(toggle_row(
                cx,
                "perm-manual",
                "手动确认（不自动附加权限参数）",
                manual,
                |settings, _, _, cx| {
                    settings.draft.agents.permission_mode = "manual".into();
                    cx.notify();
                },
            ))
            .child(toggle_row(
                cx,
                "perm-yolo",
                "自动批准（使用实例适配器生成的权限参数）",
                !manual,
                |settings, _, _, cx| {
                    settings.draft.agents.permission_mode = "yolo".into();
                    cx.notify();
                },
            ))
            .child(
                div()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(crate::theme::Theme::bg_elevated()))
                    .text_size(crate::theme::ui_px(9.5))
                    .text_color(rgb(if manual {
                        crate::theme::Theme::fg_faint()
                    } else {
                        crate::theme::Theme::warning()
                    }))
                    .child(if manual {
                        "实例仍可定义自己的权限模式，但 MonkeyFence 不会自动把权限参数附加到 CLI。"
                    } else {
                        "自动批准会减少交互中断；具体参数仍由每个 Agent 实例所属的适配器决定。"
                    }),
            )
            .child(section("设备策略"))
            .child(toggle_row(
                cx,
                "keep-awake",
                "Agent 工作时保持系统唤醒",
                self.draft.agents.keep_awake,
                |settings, _, _, cx| {
                    settings.draft.agents.keep_awake.toggle();
                    cx.notify();
                },
            ))
            .child(
                div()
                    .text_size(crate::theme::ui_px(9.5))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("插件安装与启停统一在「插件」页;Agent 类型与保存配置在下方管理。"),
            )
            // 同一页:先全局策略,再 Agent Type 与保存配置(嵌入式实例页)
            .children(self.agent_instances.clone().map(|page| {
                div()
                    .id("settings-agents-instances")
                    .h(px(520.))
                    .flex()
                    .child(page)
                    .into_any_element()
            }))
            .into_any_element()
    }

    #[allow(dead_code)]
    fn render_agents_page_legacy(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(app) = self.app.clone() else {
            return div()
                .p_3()
                .text_size(crate::theme::ui_px(12.))
                .child("此页面需要应用上下文")
                .into_any_element();
        };
        let profiles = app.plugins().agent_profiles();
        let summaries = app.plugins().summaries();
        let default_agent = self.draft.agents.default_agent.clone();

        let mut col = div().flex().flex_col().gap_2().child(section("默认智能体"));
        // 默认:Auto / 空白终端 / 已启用且检测到的 Agent
        for (id, label) in [("", "Auto(按 Step 指派)"), ("blank-terminal", "空白终端")] {
            let selected = default_agent == id;
            col = col.child(toggle_row(cx, format!("default-{id}"), label, selected, {
                let id = id.to_string();
                move |s: &mut SettingsView, _, _, cx| {
                    s.draft.agents.default_agent = id.clone();
                    cx.notify();
                }
            }));
        }
        for p in profiles.iter().filter(|p| p.id != "blank-terminal") {
            let detected = mf_plugins::builtin::detect_on_path(&p.command).is_some()
                || p.runtime != mf_agent::RuntimeKind::Pty;
            if !detected {
                continue;
            }
            let selected = default_agent == p.id;
            let id = p.id.clone();
            col = col.child(toggle_row(
                cx,
                format!("default-{id}"),
                &format!("{}({id})", p.display_name),
                selected,
                move |s: &mut SettingsView, _, _, cx| {
                    s.draft.agents.default_agent = id.clone();
                    cx.notify();
                },
            ));
        }

        // Runtime 行 + 全局开关
        col = col
            .child(section("Runtime"))
            .child(field_row("平台", label_value("Windows · 可用")))
            .child(field_row("WSL", label_value("未支持(首版仅本地 Windows)")))
            .child(section("行为"))
            .child(toggle_row(cx, "hooks-master", "状态钩子总开关(写入本地 Agent 配置)", self.draft.agents.hooks_enabled, |s: &mut SettingsView, _, _, cx| {
                s.draft.agents.hooks_enabled.toggle();
                cx.notify();
            }))
            .child(toggle_row(cx, "auto-title", "自动生成标签(会话)标题", self.draft.agents.auto_title, |s: &mut SettingsView, _, _, cx| {
                s.draft.agents.auto_title.toggle();
                cx.notify();
            }))
            .child(toggle_row(cx, "keep-awake", "Agent 工作时保持唤醒", self.draft.agents.keep_awake, |s: &mut SettingsView, _, _, cx| {
                s.draft.agents.keep_awake.toggle();
                cx.notify();
            }))
            .child(section("权限模式"))
            .child(toggle_row(cx, "perm-yolo", "Yolo(自动附加 permission args,自动批准)", self.draft.agents.permission_mode != "manual", |s: &mut SettingsView, _, _, cx| {
                s.draft.agents.permission_mode = "yolo".into();
                cx.notify();
            }))
            .child(toggle_row(cx, "perm-manual", "Manual(不附加,在终端里手动批准)", self.draft.agents.permission_mode == "manual", |s: &mut SettingsView, _, _, cx| {
                s.draft.agents.permission_mode = "manual".into();
                cx.notify();
            }))
            .child(
                div()
                    .text_size(crate::theme::ui_px(10.))
                    .text_color(rgb(crate::theme::Theme::warning()))
                    .child("⚠ 权限模式只约束 MonkeyFence 传给 Agent 的参数;worker 进程与 CLI 仍以当前 Windows 用户权限运行。"),
            );

        // 已安装/已检测列表 + 可安装列表
        col = col.child(section("已安装 / 已检测"));
        for p in &profiles {
            let detected = mf_plugins::builtin::detect_on_path(&p.command).is_some()
                || p.runtime != mf_agent::RuntimeKind::Pty;
            let plugin = summaries.iter().find(|s| s.agents.contains(&p.id));
            let expanded = self.agent_expanded.as_deref() == Some(p.id.as_str());
            let pid = p.id.clone();
            let pid2 = p.id.clone();
            let pid3 = p.id.clone();
            let homepage = p.homepage.clone().unwrap_or_default();
            let has_hook = p.hook.is_some();
            let hook_cfg = p.hook.clone();
            let is_default = default_agent == p.id;
            col = col.child(
                div()
                    .id(ElementId::Name(format!("agent-row-{pid}").into()))
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if detected {
                        crate::theme::Theme::border()
                    } else {
                        crate::theme::Theme::danger()
                    }))
                    .px_2()
                    .py_1p5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(crate::theme::ui_px(12.))
                                    .child(p.icon.clone().unwrap_or_default()),
                            )
                            .child(
                                div()
                                    .text_size(crate::theme::ui_px(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(p.display_name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(crate::theme::ui_px(9.5))
                                    .text_color(rgb(if detected {
                                        crate::theme::Theme::success()
                                    } else {
                                        crate::theme::Theme::fg_faint()
                                    }))
                                    .child(if detected {
                                        if p.runtime == mf_agent::RuntimeKind::Pty {
                                            "● 已检测(在 PATH)".to_string()
                                        } else {
                                            "● API".to_string()
                                        }
                                    } else {
                                        "○ 未检测到(PATH)".to_string()
                                    }),
                            )
                            .child(div().flex_1())
                            .when(!homepage.is_empty(), |d| {
                                d.child(link_btn(
                                    cx,
                                    format!("agent-home-{pid}"),
                                    "官方主页",
                                    homepage.clone(),
                                ))
                            })
                            .when(is_default, |d| {
                                d.child(mini_btn_disabled(
                                    format!("agent-default-{pid}"),
                                    "✓ 当前默认",
                                ))
                            })
                            .when(!is_default, |d| {
                                d.child(mini_btn(
                                    cx,
                                    format!("agent-default-{pid}"),
                                    "设为默认",
                                    crate::theme::Theme::accent(),
                                    move |s: &mut SettingsView, _, _, cx| {
                                        s.draft.agents.default_agent = pid2.clone();
                                        cx.notify();
                                    },
                                ))
                            })
                            .child(mini_btn(
                                cx,
                                format!("agent-expand-{pid}"),
                                if expanded { "收起" } else { "详情" },
                                0x8a8a8a,
                                move |s: &mut SettingsView, _, _, cx| {
                                    let pid = pid3.clone();
                                    if s.agent_expanded.as_deref() == Some(pid.as_str()) {
                                        s.agent_expanded = None;
                                    } else {
                                        s.agent_expanded = Some(pid.clone());
                                        // 载入当前覆盖缓冲
                                        if let Some(app) = &s.app {
                                            if let Some(spec) = app
                                                .plugins()
                                                .agent_profiles()
                                                .into_iter()
                                                .find(|x| x.id == pid)
                                            {
                                                s.agent_cmd_s = spec.command.clone();
                                                s.agent_args_s = spec.args.join(" ");
                                                s.agent_perm_args_s =
                                                    spec.permission_args.join(" ");
                                                s.agent_env_s = spec
                                                    .env
                                                    .iter()
                                                    .map(|(k, v)| format!("{k}={v}"))
                                                    .collect::<Vec<_>>()
                                                    .join(";");
                                            }
                                        }
                                    }
                                    cx.notify();
                                },
                            )),
                    )
                    .when(expanded, |d| {
                        d.child(
                            div()
                                .id(ElementId::Name(format!("agent-detail-{pid}").into()))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(field_row(
                                    "Command",
                                    self.text_input(Field::AgentCommand, window, cx),
                                ))
                                .child(field_row(
                                    "Arguments",
                                    self.text_input(Field::AgentArgs, window, cx),
                                ))
                                .child(field_row(
                                    "Environment(KEY=V;…)",
                                    self.text_input(Field::AgentEnv, window, cx),
                                ))
                                .child(field_row(
                                    "Permission arguments",
                                    self.text_input(Field::AgentPermArgs, window, cx),
                                ))
                                .child(field_row(
                                    "Hook 安装状态",
                                    label_value(if has_hook {
                                        if self.draft.agents.hooks_enabled {
                                            "可安装(总开关开启;命名空间内写入,备份可恢复)"
                                        } else {
                                            "总开关已关闭"
                                        }
                                    } else {
                                        "此 Agent 无钩子配置"
                                    }),
                                ))
                                .child(field_row(
                                    "插件来源",
                                    label_value(&format!(
                                        "{} v{}",
                                        plugin.map(|s| s.source_kind.as_str()).unwrap_or("builtin"),
                                        plugin.map(|s| s.version.as_str()).unwrap_or("?")
                                    )),
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .gap_1()
                                        .child({
                                            let pid_apply = pid.clone();
                                            mini_btn(
                                                cx,
                                                format!("agent-apply-{pid}"),
                                                "应用覆盖",
                                                crate::theme::Theme::accent(),
                                                move |s: &mut SettingsView, _, _, cx| {
                                                    let pid = pid_apply.clone();
                                                    let cmd = s.agent_cmd_s.trim().to_string();
                                                    let args = s
                                                        .agent_args_s
                                                        .split_whitespace()
                                                        .map(str::to_string)
                                                        .collect();
                                                    let perm = s
                                                        .agent_perm_args_s
                                                        .split_whitespace()
                                                        .map(str::to_string)
                                                        .collect();
                                                    let env = s
                                                        .agent_env_s
                                                        .split(';')
                                                        .filter_map(|kv| {
                                                            kv.split_once('=').map(|(k, v)| {
                                                                (
                                                                    k.trim().to_string(),
                                                                    v.trim().to_string(),
                                                                )
                                                            })
                                                        })
                                                        .collect();
                                                    if let Some(app) = &s.app {
                                                        if let Some(mut spec) = app
                                                            .plugins()
                                                            .agent_profiles()
                                                            .into_iter()
                                                            .find(|x| x.id == pid)
                                                        {
                                                            spec.command = cmd;
                                                            spec.args = args;
                                                            spec.permission_args = perm;
                                                            spec.env = env;
                                                            app.plugins().set_agent_override(spec);
                                                            app.refresh_catalog();
                                                            s.status = "已应用(会话内生效)".into();
                                                        }
                                                    }
                                                    cx.notify();
                                                },
                                            )
                                        })
                                        .when_some(hook_cfg.clone(), |d, hook| {
                                            let hk = hook.clone();
                                            let hk2 = hook.clone();
                                            d.child(mini_btn(
                                                cx,
                                                format!("hook-install-{pid}"),
                                                "安装状态钩子",
                                                crate::theme::Theme::warning(),
                                                move |s: &mut SettingsView, _, _, cx| {
                                                    if !s.draft.agents.hooks_enabled {
                                                        s.status = "请先开启状态钩子总开关".into();
                                                        cx.notify();
                                                        return;
                                                    }
                                                    match mf_plugins::hooks::install_hook(
                                                        &hk.config_path,
                                                        &hk.namespace,
                                                        &hk.command_template,
                                                    ) {
                                                        Ok(backup) => {
                                                            s.status = format!(
                                                                "钩子已写入(备份: {})",
                                                                backup.display()
                                                            )
                                                            .into();
                                                        }
                                                        Err(e) => {
                                                            s.status = format!("{e:#}").into()
                                                        }
                                                    }
                                                    cx.notify();
                                                },
                                            ))
                                            .child(
                                                mini_btn(
                                                    cx,
                                                    format!("hook-remove-{pid}"),
                                                    "移除钩子",
                                                    0x8a8a8a,
                                                    move |s: &mut SettingsView, _, _, cx| {
                                                        match mf_plugins::hooks::remove_hook(
                                                            &hk2.config_path,
                                                            &hk2.namespace,
                                                        ) {
                                                            Ok(()) => {
                                                                s.status =
                                                                    "钩子已移除(用户配置保留)"
                                                                        .into()
                                                            }
                                                            Err(e) => {
                                                                s.status = format!("{e:#}").into()
                                                            }
                                                        }
                                                        cx.notify();
                                                    },
                                                ),
                                            )
                                        }),
                                ),
                        )
                    }),
            );
        }

        // 可安装区:始终显示;未检测到的内置 CLI Agent 逐个列出。
        // 官方 npm 包已核实的一键安装(codex/claude/opencode);
        // cursor/kimi 走官方安装页(npm 同名包非官方,不做自动执行)。
        let all_cli = mf_plugins::builtin::builtin_cli_agents();
        let installable: Vec<_> = all_cli
            .iter()
            .filter(|a| mf_plugins::builtin::detect_on_path(&a.command).is_none())
            .collect();
        col = col.child(section("可安装 Agent 插件"));
        if installable.is_empty() {
            col = col.child(
                div()
                    .text_size(crate::theme::ui_px(10.5))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("全部内置 Agent 均已检测到(在 PATH)。"),
            );
        }
        for a in installable {
            let homepage = a.homepage.clone();
            let spec = mf_plugins::builtin::install_spec_of(&a.profile_id);
            // 按安装器程序检测可用性(npm / python 各自检查 PATH)
            let can_auto = spec
                .as_ref()
                .is_some_and(|sp| mf_plugins::builtin::detect_on_path(&sp.program).is_some());
            let missing_tool = match &spec {
                Some(sp) if !can_auto => Some(sp.program.clone()),
                _ => None,
            };
            let aid = a.profile_id.clone();
            col = col.child(
                div()
                    .id(ElementId::Name(
                        format!("installable-{}", a.profile_id).into(),
                    ))
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .flex_wrap()
                    .child(
                        div()
                            .text_size(crate::theme::ui_px(11.5))
                            .child(a.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(crate::theme::ui_px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(format!("`{}`", a.command)),
                    )
                    .when_some(spec.clone(), |d, sp| {
                        d.child(
                            div()
                                .text_size(crate::theme::ui_px(9.))
                                .px_1()
                                .rounded_sm()
                                .bg(rgb(crate::theme::Theme::bg_active()))
                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                .child(sp.display.clone()),
                        )
                    })
                    .child(div().flex_1())
                    .when_some(missing_tool, |d, tool| {
                        d.child(
                            div()
                                .text_size(crate::theme::ui_px(9.5))
                                .text_color(rgb(crate::theme::Theme::warning()))
                                .child(format!("缺少 {tool}")),
                        )
                    })
                    .when(can_auto, |d| {
                        let sp = spec.clone().unwrap();
                        let aid_btn = aid.clone();
                        d.child(mini_btn(
                            cx,
                            format!("install-run-{aid}"),
                            "一键安装",
                            crate::theme::Theme::success(),
                            move |s: &mut SettingsView, _, _, cx| {
                                s.run_agent_install(&sp.program, &sp.args, &aid_btn, cx);
                            },
                        ))
                    })
                    .when(spec.is_none(), |d| {
                        d.child(
                            div()
                                .text_size(crate::theme::ui_px(9.))
                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                .child("官方独立安装器"),
                        )
                    })
                    .when(!homepage.is_empty(), |d| {
                        d.child(link_btn(
                            cx,
                            format!("install-open-{aid}"),
                            "官方安装页",
                            homepage,
                        ))
                    }),
            );
        }
        col = col.child(div().id("agents-refresh").child(mini_btn(
            cx,
            "agents-refresh",
            "刷新检测",
            crate::theme::Theme::accent(),
            move |s: &mut SettingsView, _, _, cx| {
                if let Some(app) = &s.app {
                    app.refresh_catalog();
                    s.status = "已刷新检测".into();
                }
                cx.notify();
            },
        )));
        col.into_any_element()
    }

    /// 一键安装 CLI Agent:后台运行官方包管理器命令,输出尾部回显,完成后刷新检测。
    fn run_agent_install(
        &mut self,
        program: &str,
        args: &[String],
        agent_id: &str,
        cx: &mut Context<Self>,
    ) {
        if self.installing {
            self.plugin_status = "已有安装任务进行中…".into();
            cx.notify();
            return;
        }
        self.installing = true;
        self.plugin_status = format!("正在安装 {agent_id}…").into();
        let program = program.to_string();
        let args = args.to_vec();
        let agent_id = agent_id.to_string();
        cx.spawn(async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move {
                    // Windows 下 npm 是 npm.cmd,经 cmd /c 调用;5 分钟超时
                    let mut cmd = if cfg!(windows) {
                        let mut c = std::process::Command::new("cmd");
                        c.arg("/c").arg(&program).args(&args);
                        c
                    } else {
                        let mut c = std::process::Command::new(&program);
                        c.args(&args);
                        c
                    };
                    if let Some(home) = dirs::home_dir() {
                        cmd.current_dir(home);
                    }
                    match cmd.output() {
                        Ok(o) => {
                            let ok = o.status.success();
                            let tail = |b: &[u8]| -> String {
                                let text = String::from_utf8_lossy(b);
                                text.lines()
                                    .rev()
                                    .take(3)
                                    .collect::<Vec<_>>()
                                    .into_iter()
                                    .rev()
                                    .collect::<Vec<_>>()
                                    .join(" | ")
                            };
                            (ok, tail(&o.stdout), tail(&o.stderr))
                        }
                        Err(e) => (false, String::new(), e.to_string()),
                    }
                })
                .await;
            this.update(cx, move |s: &mut SettingsView, cx| {
                s.installing = false;
                let (ok, out_tail, err_tail) = out;
                if ok {
                    s.plugin_status = format!("{agent_id} 安装完成:{out_tail}").into();
                } else {
                    s.plugin_status = format!(
                        "{agent_id} 安装失败:{err_tail} {out_tail}(可从官方安装页手动安装)"
                    )
                    .into();
                }
                if let Some(app) = &s.app {
                    app.refresh_catalog();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn render_plugins_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(app) = self.app.clone() else {
            return div()
                .p_3()
                .text_size(crate::theme::ui_px(12.))
                .child("此页面需要应用上下文")
                .into_any_element();
        };
        let summaries = app.plugins().summaries();
        let mut col = div().flex().flex_col().gap_2().child(section("已安装插件"));
        // 贡献视图:类型/权限/固定版本/哈希(设计 §11.5)
        let contribution_rows =
            crate::plugin_contribution_view::summaries_from_registry(&app.plugins());
        if summaries.is_empty() {
            col = col.child(label_value("暂无插件"));
        }
        for s in &summaries {
            let full_id = s.full_id.clone();
            let enabled = s.enabled;
            let is_builtin = s.builtin;
            col = col.child(
                div()
                    .id(ElementId::Name(format!("plugin-{full_id}").into()))
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if enabled {
                        crate::theme::Theme::border()
                    } else {
                        crate::theme::Theme::danger()
                    }))
                    .px_2()
                    .py_1p5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(crate::theme::ui_px(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(s.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(crate::theme::ui_px(9.5))
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child(format!(
                                        "v{} · {} · {}",
                                        s.version, s.source_kind, full_id
                                    )),
                            )
                            .when(s.has_worker, |d| {
                                d.child(
                                    div()
                                        .text_size(crate::theme::ui_px(9.))
                                        .px_1()
                                        .rounded_sm()
                                        .bg(rgb(crate::theme::Theme::bg_active()))
                                        .child("worker"),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_size(crate::theme::ui_px(9.5))
                                    .text_color(rgb(if enabled {
                                        crate::theme::Theme::success()
                                    } else {
                                        crate::theme::Theme::fg_faint()
                                    }))
                                    .child(if enabled { "已启用" } else { "已禁用" }),
                            ),
                    )
                    .when(
                        contribution_rows
                            .iter()
                            .find(|r| r.full_id == full_id)
                            .map(|r| !r.contribution_counts.is_empty() || !r.requested_permissions.is_empty())
                            .unwrap_or(false),
                        |d| {
                            let row = contribution_rows
                                .iter()
                                .find(|r| r.full_id == full_id)
                                .unwrap();
                            let counts = row
                                .contribution_counts
                                .iter()
                                .map(|(k, c)| format!("{k}: {c}"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            let perms = row.requested_permissions.join(", ");
                            d.child(
                                div()
                                    .text_size(crate::theme::ui_px(9.))
                                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                                    .child(format!(
                                        "贡献({counts})权限: {perms} · worker/CLI 以当前系统用户运行"
                                    )),
                            )
                        },
                    )
                    .when(
                        contribution_rows
                            .iter()
                            .find(|r| r.full_id == full_id)
                            .map(|r| !r.compatible || r.active_pins > 0)
                            .unwrap_or(false),
                        |d| {
                            let row = contribution_rows
                                .iter()
                                .find(|r| r.full_id == full_id)
                                .unwrap();
                            d.child(
                                div()
                                    .text_size(crate::theme::ui_px(9.))
                                    .text_color(rgb(if row.compatible {
                                        crate::theme::Theme::fg_dim()
                                    } else {
                                        crate::theme::Theme::warning()
                                    }))
                                    .child(format!(
                                        "{}· 活动 pin: {}",
                                        if row.compatible {
                                            String::new()
                                        } else {
                                            "⚠ 与当前版本不兼容 ".to_string()
                                        },
                                        row.active_pins
                                    )),
                            )
                        },
                    )
                    .child(
                        div()
                            .text_size(crate::theme::ui_px(10.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .child(if s.description.is_empty() {
                                "—".to_string()
                            } else {
                                s.description.clone()
                            }),
                    )
                    .child(
                        div()
                            .text_size(crate::theme::ui_px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(format!(
                                "权限: fs_read={} fs_write={} net={} spawn={} hooks={} · 授权:{}",
                                s.capabilities.fs_read,
                                s.capabilities.fs_write,
                                s.capabilities.net,
                                s.capabilities.spawn,
                                s.capabilities.hooks,
                                s.authorized_at.as_deref().map(|_| "是").unwrap_or("否"),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child({
                                let fid_enable = full_id.clone();
                                mini_btn(
                                    cx,
                                    format!("plugin-enable-{full_id}"),
                                    "启用/授权",
                                    crate::theme::Theme::success(),
                                    move |s2: &mut SettingsView, _, _, cx| {
                                        let app = s2.app.clone();
                                        if let Some(app) = app {
                                            // 只做常规启用;需要重新授权时置为待确认,
                                            // 绝不在同一次点击里自动重授权
                                            match app.plugins().enable(&fid_enable, false) {
                                                Ok(()) => {
                                                    s2.pending_reauth = None;
                                                    s2.plugin_status = "已启用".into();
                                                }
                                                Err(e) => {
                                                    if format!("{e}").contains("重新授权") {
                                                        s2.pending_reauth = Some(fid_enable.clone());
                                                        s2.plugin_status = format!(
                                                            "权限/内容发生变化({e});请核对权限与 worker/钩子声明后点击「确认重新授权」"
                                                        )
                                                        .into();
                                                    } else {
                                                        s2.plugin_status = format!("{e}").into();
                                                    }
                                                }
                                            }
                                            app.refresh_catalog();
                                        }
                                        cx.notify();
                                    },
                                )
                            })
                            .child({
                                let fid_disable = full_id.clone();
                                mini_btn(
                                    cx,
                                    format!("plugin-disable-{full_id}"),
                                    "禁用",
                                    0x8a8a8a,
                                    move |s2: &mut SettingsView, _, _, cx| {
                                        let app = s2.app.clone();
                                        if let Some(app) = app {
                                            s2.plugin_status =
                                                match app.plugins().disable(&fid_disable) {
                                                    Ok(()) => "已禁用".into(),
                                                    Err(e) => format!("{e}").into(),
                                                };
                                            app.refresh_catalog();
                                        }
                                        cx.notify();
                                    },
                                )
                            })
                            .when(!is_builtin, |d| {
                                let fid = full_id.clone();
                                d.child(mini_btn(
                                    cx,
                                    format!("plugin-uninstall-{full_id}"),
                                    "删除",
                                    crate::theme::Theme::danger(),
                                    move |s2: &mut SettingsView, _, _, cx| {
                                        let app = s2.app.clone();
                                        s2.plugin_status = match app {
                                            Some(app) => match app.plugins().uninstall(&fid) {
                                                Ok(()) => "已删除(重载后生效)".into(),
                                                Err(e) => format!("{e:#}").into(),
                                            },
                                            None => "应用上下文不可用".into(),
                                        };
                                        if let Some(app) = s2.app.clone() {
                                            app.refresh_catalog();
                                        }
                                        cx.notify();
                                    },
                                ))
                            })
                            .when(
                                self.pending_reauth
                                    .as_deref()
                                    .is_some_and(|id| id == full_id.as_str()),
                                |d| {
                                    let fid_reauth = full_id.clone();
                                    d.child(
                                        div()
                                            .id("plugin-reauth-row")
                                            .mt_1()
                                            .p_1p5()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(crate::theme::Theme::warning()))
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .text_size(crate::theme::ui_px(10.))
                                                    .text_color(rgb(crate::theme::Theme::warning()))
                                                    .child("重新授权将接受该插件当前的能力声明、worker 命令与钩子配置。"),
                                            )
                                            .child(mini_btn(
                                                cx,
                                                format!("plugin-reauth-{full_id}"),
                                                "确认重新授权",
                                                crate::theme::Theme::danger(),
                                                move |s2: &mut SettingsView, _, _, cx| {
                                                    let app = s2.app.clone();
                                                    if let Some(app) = app {
                                                        let fid = fid_reauth.clone();
                                                        s2.plugin_status =
                                                            match app.plugins().enable(&fid, true) {
                                                                Ok(()) => "已重新授权并启用".into(),
                                                                Err(e) => format!("{e}").into(),
                                                            };
                                                        s2.pending_reauth = None;
                                                        app.refresh_catalog();
                                                    }
                                                    cx.notify();
                                                },
                                            )),
                                    )
                                },
                            ),
                    ),
            );
        }

        // 安装来源:本地目录 / Git URL
        col = col
            .child(section("安装插件"))
            .child(field_row("本地目录", self.text_input(Field::PluginLocal, window, cx)))
            .child(
                div()
                    .id("plugin-install-local")
                    .child(mini_btn(cx, "plugin-install-local", "从本地目录安装(复制→校验→哈希→原子发布)", crate::theme::Theme::accent(), move |s: &mut SettingsView, _, _, cx| {
                        let path = s.plugin_local_s.trim().to_string();
                        if path.is_empty() {
                            s.plugin_status = "请输入插件目录路径".into();
                            cx.notify();
                            return;
                        }
                        let source = mf_plugins::install::InstallSource::Local { path: path.clone() };
                        s.plugin_status = match mf_plugins::install::install_from_dir(std::path::Path::new(&path), source) {
                            Ok(e) => format!("已安装 {}(默认禁用,需审查权限后启用)", e.full_id).into(),
                            Err(e) => format!("{e:#}").into(),
                        };
                        if let Some(app) = &s.app {
                            app.refresh_catalog();
                        }
                        cx.notify();
                    })),
            )
            .child(field_row("Git URL", self.text_input(Field::PluginGit, window, cx)))
            .child(
                div()
                    .id("plugin-install-git")
                    .child(mini_btn(cx, "plugin-install-git", "从 Git URL 安装(git clone → 校验)", crate::theme::Theme::accent(), move |s: &mut SettingsView, _, _, cx| {
                        let url = s.plugin_git_s.trim().to_string();
                        if url.is_empty() {
                            s.plugin_status = "请输入 Git URL".into();
                            cx.notify();
                            return;
                        }
                        s.plugin_status = match mf_plugins::install::install_from_git(&url) {
                            Ok(e) => format!("已安装 {}(默认禁用)", e.full_id).into(),
                            Err(e) => format!("{e:#}").into(),
                        };
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_size(crate::theme::ui_px(10.))
                    .text_color(rgb(crate::theme::Theme::warning()))
                    .child("⚠ 插件权限只约束其与 MonkeyFence 宿主接口的交互;worker 进程与 CLI 仍以当前 Windows 用户权限运行。"),
            )
            .child(
                div()
                    .id("plugin-status-line")
                    .text_size(crate::theme::ui_px(10.5))
                    .text_color(rgb(crate::theme::Theme::fg_dim()))
                    .child(self.plugin_status.clone()),
            );
        col.into_any_element()
    }
}

trait Toggle {
    fn toggle(&mut self);
}
impl Toggle for bool {
    fn toggle(&mut self) {
        *self = !*self;
    }
}

fn label_value(text: &str) -> impl IntoElement {
    div()
        .text_size(crate::theme::ui_px(11.5))
        .text_color(rgb(crate::theme::Theme::fg()))
        .child(text.to_string())
}

fn responsibility_row(title: &str, description: &str, badge: &str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .p_2()
        .rounded_sm()
        .border_1()
        .border_color(rgb(crate::theme::Theme::border()))
        .child(
            div()
                .w(px(72.))
                .text_size(crate::theme::ui_px(10.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(crate::theme::Theme::fg()))
                .child(title.to_string()),
        )
        .child(
            div()
                .flex_1()
                .text_size(crate::theme::ui_px(9.5))
                .text_color(rgb(crate::theme::Theme::fg_dim()))
                .child(description.to_string()),
        )
        .child(
            div()
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .bg(rgb(crate::theme::Theme::bg_active()))
                .text_size(crate::theme::ui_px(8.5))
                .text_color(rgb(crate::theme::Theme::accent()))
                .child(badge.to_string()),
        )
}

fn toggle_row(
    cx: &Context<SettingsView>,
    id: impl Into<gpui::ElementId>,
    label: &str,
    on: bool,
    handler: impl Fn(&mut SettingsView, &gpui::ClickEvent, &mut Window, &mut Context<SettingsView>)
        + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .py_0p5()
        .cursor_pointer()
        .child(
            div()
                .w(px(34.))
                .h(px(18.))
                .rounded_full()
                .bg(rgb(if on {
                    crate::theme::Theme::accent()
                } else {
                    crate::theme::Theme::bg_active()
                }))
                .relative()
                .child(
                    div()
                        .absolute()
                        .top(px(2.))
                        .left(px(if on { 18. } else { 2. }))
                        .size(px(14.))
                        .rounded_full()
                        .bg(rgb(crate::theme::Theme::bg_elevated())),
                ),
        )
        .child(
            div()
                .text_size(crate::theme::ui_px(11.5))
                .child(label.to_string()),
        )
        .on_click(cx.listener(handler))
}

fn mini_btn(
    cx: &Context<SettingsView>,
    id: impl Into<gpui::ElementId>,
    label: &str,
    color: u32,
    handler: impl Fn(&mut SettingsView, &gpui::ClickEvent, &mut Window, &mut Context<SettingsView>)
        + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px_2()
        .h(px(20.))
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(color))
        .text_size(crate::theme::ui_px(9.5))
        .text_color(rgb(color))
        .cursor_pointer()
        .hover(move |d| d.bg(rgb(color)).text_color(rgb(crate::theme::Theme::bg())))
        .child(label.to_string())
        .on_click(cx.listener(handler))
}

fn mini_btn_disabled(id: impl Into<gpui::ElementId>, label: &str) -> impl IntoElement {
    div()
        .id(id)
        .px_2()
        .h(px(20.))
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(crate::theme::Theme::border()))
        .text_size(crate::theme::ui_px(9.5))
        .text_color(rgb(crate::theme::Theme::fg_faint()))
        .child(label.to_string())
}

fn link_btn(
    _cx: &Context<SettingsView>,
    id: impl Into<gpui::ElementId>,
    label: &str,
    url: String,
) -> impl IntoElement {
    div()
        .id(id)
        .px_2()
        .h(px(20.))
        .flex()
        .items_center()
        .rounded_md()
        .text_size(crate::theme::ui_px(9.5))
        .text_color(rgb(crate::theme::Theme::accent()))
        .cursor_pointer()
        .hover(|d| d.underline())
        .child(label.to_string())
        .on_click(move |_, _, _| {
            let _ = std::process::Command::new(if cfg!(windows) {
                "explorer"
            } else {
                "xdg-open"
            })
            .arg(&url)
            .spawn();
        })
}
