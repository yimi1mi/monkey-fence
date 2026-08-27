use gpui::prelude::*;
use gpui::*;
use mf_agent::{Config, ProviderConfig, ProviderKind};


pub struct Saved(pub Config);
pub struct Dismissed;

/// 设置界面(模态):提供方/角色/引擎/编辑器字体,保存到 ~/.monkeyfence/config.toml
pub struct SettingsView {
    draft: Config,
    /// 当前编辑其提供方的角色
    selected_role: SharedString,
    /// 数值字段以字符串编辑,保存时解析
    workers_s: String,
    max_iters_s: String,
    max_failures_s: String,
    font_size_s: String,
    status: SharedString,
    focus_handle: FocusHandle,
}

actions!(settings, [Dismiss]);

const ROLES: &[&str] = &["planner", "worker", "reviewer"];

#[derive(Clone, Copy, PartialEq, Debug)]
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
}

impl SettingsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let draft = Config::load().unwrap_or_default();
        let workers_s = draft.engine.workers.to_string();
        let max_iters_s = draft.engine.max_iterations.to_string();
        let max_failures_s = draft.engine.max_failures.to_string();
        let font_size_s = format!("{:.1}", draft.editor.font_size);
        Self {
            draft,
            selected_role: "planner".into(),
            workers_s,
            max_iters_s,
            max_failures_s,
            font_size_s,
            status: "".into(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// 选中角色的提供方配置(不存在时按需创建,便于新名字直接生效)
    fn selected_provider_mut(&mut self) -> &mut ProviderConfig {
        let role = self.selected_role.to_string();
        let name = self
            .draft
            .roles
            .get(&role)
            .cloned()
            .unwrap_or_else(|| "mock".into());
        self.draft
            .providers
            .entry(name)
            .or_insert_with(|| ProviderConfig {
                kind: ProviderKind::Mock,
                base_url: String::new(),
                api_key: String::new(),
                model: String::new(),
            })
    }

    fn selected_provider(&self) -> ProviderConfig {
        let name = self
            .draft
            .roles
            .get(self.selected_role.as_ref())
            .cloned()
            .unwrap_or_else(|| "mock".into());
        self.draft.providers.get(&name).cloned().unwrap_or(ProviderConfig {
            kind: ProviderKind::Mock,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        })
    }

    fn field_text(&self, f: Field) -> String {
        match f {
            Field::RoleProvider => self
                .draft
                .roles
                .get(self.selected_role.as_ref())
                .cloned()
                .unwrap_or_default(),
            Field::BaseUrl => self.selected_provider().base_url.clone(),
            Field::ApiKey => self.selected_provider().api_key.clone(),
            Field::Model => self.selected_provider().model.clone(),
            Field::Workers => self.workers_s.clone(),
            Field::MaxIters => self.max_iters_s.clone(),
            Field::MaxFailures => self.max_failures_s.clone(),
            Field::FontFamily => self.draft.editor.font_family.clone(),
            Field::FontSize => self.font_size_s.clone(),
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
        self.status = "已恢复默认(未保存)".into();
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
        cx.notify();
    }

    fn set_kind(&mut self, kind: ProviderKind, cx: &mut Context<Self>) {
        self.selected_provider_mut().kind = kind;
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
            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
                cx.emit(Dismissed);
            }))
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
            .border_color(rgb(crate::theme::Theme::BORDER))
            .text_size(px(13.))
            .font_weight(FontWeight::SEMIBOLD)
            .child("设置");

        div()
            .id("settings-card")
            .on_mouse_down(MouseButton::Left, |_, _, _| {})
            .w(px(620.))
            .max_h(px(640.))
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(rgb(crate::theme::Theme::BORDER))
            .bg(rgb(crate::theme::Theme::BG_PANEL))
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
            .overflow_y_scroll()
            .p_4()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.render_roles_section(cx))
            .child(self.render_provider_section(window, cx))
            .child(self.render_engine_section(window, cx))
            .child(self.render_editor_section(window, cx))
            .child(
                div()
                    .id("settings-hint")
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::Theme::FG_FAINT))
                    .child("引擎/提供方改动在下次打开项目或新建运行时生效;字体立即应用到所有编辑器。"),
            )
            .into_any_element()
    }

    fn render_roles_section(&self, cx: &Context<Self>) -> AnyElement {
        let draft = self.draft.clone();
        let sel_role = self.selected_role.clone();
        let mut rows = vec![section("角色 → 提供方").into_any_element()];
        for role in ROLES {
            let selected = sel_role.as_ref() == *role;
            let prov_name = draft
                .roles
                .get(*role)
                .cloned()
                .unwrap_or_else(|| "mock".into());
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
                    .when(selected, |d| d.bg(rgb(crate::theme::Theme::BG_ACTIVE)))
                    .hover(|d| d.bg(rgb(crate::theme::Theme::BG_HOVER)))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.select_role(role, cx);
                    }))
                    .child(self.role_label(*role, selected))
                    .child(self.role_value(&prov_name))
                    .child(kind_badge(kind))
                    .into_any_element(),
            );
        }
        div().id("role-rows").flex().flex_col().gap_1().children(rows).into_any_element()
    }

    fn role_label(&self, role: &str, selected: bool) -> Div {
        div()
            .w(px(80.))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(if selected {
                crate::theme::Theme::ACCENT
            } else {
                crate::theme::Theme::FG_DIM
            }))
            .child(role.to_string())
    }

    fn role_value(&self, prov_name: &str) -> Div {
        div()
            .flex_1()
            .text_size(px(12.))
            .text_color(rgb(crate::theme::Theme::FG))
            .child(prov_name.to_string())
    }

    fn render_provider_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let prov = self.selected_provider();
        let name = self
            .draft
            .roles
            .get(self.selected_role.as_ref())
            .cloned()
            .unwrap_or_default();
        let header = section(format!("提供方 [{}]({})", self.selected_role, name));
        let kind_row = field_row_kind("类型", prov.kind, cx);
        div()
            .id("provider-fields")
            .flex()
            .flex_col()
            .gap_2()
            .child(header)
            .child(field_row("名称", self.text_input(Field::RoleProvider, window, cx)))
            .child(kind_row)
            .child(field_row("base_url", self.text_input(Field::BaseUrl, window, cx)))
            .child(field_row("api_key", self.text_input(Field::ApiKey, window, cx)))
            .child(field_row("model", self.text_input(Field::Model, window, cx)))
            .into_any_element()
    }

    fn render_engine_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("engine-fields")
            .flex()
            .flex_col()
            .gap_2()
            .child(section("引擎"))
            .child(field_row("并行 worker 数", self.text_input(Field::Workers, window, cx)))
            .child(field_row("工具循环轮数", self.text_input(Field::MaxIters, window, cx)))
            .child(field_row("失败熔断次数", self.text_input(Field::MaxFailures, window, cx)))
            .into_any_element()
    }

    fn render_editor_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("editor-fields")
            .flex()
            .flex_col()
            .gap_2()
            .child(section("编辑器"))
            .child(field_row("字体", self.text_input(Field::FontFamily, window, cx)))
            .child(field_row("字号(px)", self.text_input(Field::FontSize, window, cx)))
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
            .border_color(rgb(crate::theme::Theme::BORDER))
            .child(
                div()
                    .id("settings-status")
                    .flex_1()
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::Theme::FG_FAINT))
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
    fn text_input(&self, field: Field, window: &mut Window, cx: &Context<Self>) -> impl IntoElement {
        let text = self.field_text(field);
        div()
            .id(ElementId::Name(format!("settings-input-{:?}", field).into()))
            .flex_1()
            .h(px(24.))
            .px_2()
            .rounded_sm()
            .border_1()
            .border_color(rgb(crate::theme::Theme::BORDER))
            .bg(rgb(crate::theme::Theme::BG))
            .track_focus(&self.focus_handle)
            .when(self.focus_handle.is_focused(window), |d| {
                d.border_color(rgb(crate::theme::Theme::ACCENT))
            })
            .text_size(px(12.))
            .text_color(rgb(crate::theme::Theme::FG))
            .overflow_hidden()
            .on_key_down(cx.listener(move |s, e: &KeyDownEvent, _w, cx| {
                if let Some(chars) = e.keystroke.key_char.clone() {
                    let printable: String = chars.chars().filter(|c| !c.is_control()).collect();
                    if !printable.is_empty() {
                        s.push_field(field, &printable, cx);
                    }
                } else if e.keystroke.key == "backspace" {
                    s.pop_field(field, cx);
                }
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
            Field::BaseUrl => self.selected_provider_mut().base_url.push_str(text),
            Field::ApiKey => self.selected_provider_mut().api_key.push_str(text),
            Field::Model => self.selected_provider_mut().model.push_str(text),
            Field::Workers => self.workers_s.push_str(text),
            Field::MaxIters => self.max_iters_s.push_str(text),
            Field::MaxFailures => self.max_failures_s.push_str(text),
            Field::FontFamily => self.draft.editor.font_family.push_str(text),
            Field::FontSize => self.font_size_s.push_str(text),
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
                self.selected_provider_mut().base_url.pop();
            }
            Field::ApiKey => {
                self.selected_provider_mut().api_key.pop();
            }
            Field::Model => {
                self.selected_provider_mut().model.pop();
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
        };
        cx.notify();
    }
}

fn section(title: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(crate::theme::Theme::ACCENT))
        .child(title.into())
}

fn field_row(label: &str, input: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().w(px(110.)).text_size(px(12.)).text_color(rgb(crate::theme::Theme::FG_DIM)).child(label.to_string()))
        .child(input)
}

fn field_row_kind(label: &str, current: ProviderKind, cx: &Context<SettingsView>) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().w(px(110.)).text_size(px(12.)).text_color(rgb(crate::theme::Theme::FG_DIM)).child(label.to_string()))
        .child(kind_btn("mock", current == ProviderKind::Mock, ProviderKind::Mock, cx))
        .child(kind_btn("openai", current == ProviderKind::Openai, ProviderKind::Openai, cx))
        .child(kind_btn("anthropic", current == ProviderKind::Anthropic, ProviderKind::Anthropic, cx))
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
            crate::theme::Theme::ACCENT
        } else {
            crate::theme::Theme::BORDER
        }))
        .text_size(px(11.))
        .text_color(rgb(if active {
            crate::theme::Theme::ACCENT
        } else {
            crate::theme::Theme::FG_DIM
        }))
        .when(active, |d| d.bg(rgb(crate::theme::Theme::BG_ACTIVE)))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(crate::theme::Theme::BG_HOVER)))
        .child(label.to_string())
        .on_click(cx.listener(move |s, _, _, cx| {
            s.set_kind(kind, cx);
        }))
}

fn primary_btn(label: &str, listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("settings-btn-{}", label).into()))
        .px_3()
        .py(px(4.))
        .rounded_sm()
        .bg(rgb(crate::theme::Theme::ACCENT))
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(0xffffff))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(crate::theme::Theme::ACCENT_DIM)))
        .child(label.to_string())
        .on_click(move |e, w, cx| listener(e, w, cx))
}

fn secondary_btn(label: &str, listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("settings-btn-{}", label).into()))
        .px_3()
        .py(px(4.))
        .rounded_sm()
        .border_1()
        .border_color(rgb(crate::theme::Theme::BORDER))
        .bg(rgb(crate::theme::Theme::BG_ELEVATED))
        .text_size(px(12.))
        .text_color(rgb(crate::theme::Theme::FG_DIM))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(crate::theme::Theme::BG_HOVER)))
        .child(label.to_string())
        .on_click(move |e, w, cx| listener(e, w, cx))
}

fn kind_badge(kind: &str) -> SharedString {
    format!("[{}]", kind).into()
}
