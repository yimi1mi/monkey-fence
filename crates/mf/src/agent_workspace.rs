//! Agent Workspace:Work 表面的中心职责(替代旧 Cockpit)。
//!
//! - `Agents` 视图:Orca 风格四列看板(需要你 / 工作中 / 已完成 / 空闲),
//!   默认汇总所有打开项目,支持按项目与文本过滤;卡片点开近全屏终端或 transcript。
//! - `Pipeline` 视图:左到右拓扑列布局 + 节点编辑器 + 工具栏
//!   (从模板创建 / AI 生成 / 添加 Step / 校验 / 确认并运行 / 暂停 / 继续 / 取消)。

use crate::app_ctx::AppCtx;
use crate::project_context::{normalize_project_path, ActivationTarget};
use crate::project_overview::{AgentCardOverview, AttentionBucket, ProjectOverviewSnapshot};
use gpui::prelude::*;
use gpui::*;
use gpui::{px, AnyElement, Context, EventEmitter, FontWeight, Window};
use mf_agent::model::*;
use mf_agent::orchestrator::Orchestrator;
use mf_agent::pipeline::{PipelineDraft, SessionPolicy, StepDraft};
use mf_agent::Settlement;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// AgentWorkspace → Workspace 的意图事件(卡片打开走 activation seam)。
pub enum AgentWorkspaceEvent {
    Activate(ActivationTarget),
}

impl EventEmitter<AgentWorkspaceEvent> for AgentWorkspace {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceView {
    Agents,
    Pipeline,
    Instances,
    Workflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    NeedsYou,
    Working,
    Done,
    Idle,
}

impl From<AttentionBucket> for Column {
    fn from(b: AttentionBucket) -> Self {
        match b {
            AttentionBucket::NeedsYou => Column::NeedsYou,
            AttentionBucket::Working => Column::Working,
            AttentionBucket::Done => Column::Done,
            AttentionBucket::Idle => Column::Idle,
        }
    }
}

impl Column {
    fn title(&self) -> &'static str {
        match self {
            Column::NeedsYou => "需要你",
            Column::Working => "工作中",
            Column::Done => "已完成",
            Column::Idle => "空闲",
        }
    }
}

#[derive(Clone)]
enum Overlay {
    Terminal {
        project: PathBuf,
        session_id: i64,
        run_id: Option<i64>,
    },
    Transcript {
        project: PathBuf,
        session_id: i64,
        run_id: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    None,
    FilterText,
    Title,
    Instructions,
    SessionKey,
    PromptInput,
}

pub struct AgentWorkspace {
    app: Arc<AppCtx>,
    view: WorkspaceView,
    cards: Vec<AgentCardOverview>,
    /// 快照项目根列表(filter_project 循环用,来自统一快照)。
    project_roots: Vec<PathBuf>,
    /// 快照的全局活动 run 数(工具栏显示)。
    global_active_runs: usize,
    filter_project: Option<PathBuf>,
    filter_text: String,
    overlay: Option<Overlay>,
    overlay_input: String,
    selected_task: Option<(PathBuf, i64)>,
    task: Option<TaskView>,
    steps: Vec<StepView>,
    draft: Vec<StepDraft>,
    dirty: bool,
    loaded_revision: Option<i64>,
    selected_step: Option<String>,
    profile_popover: bool,
    validation: Vec<String>,
    status_message: String,
    templates: Vec<(String, String, PipelineDraft)>,
    template_popover: bool,
    active_field: Field,
    field_buffer: String,
    focus_handle: FocusHandle,
    pending_focus: bool,
    /// Agent 实例页(独立实体;设计 §11.1)。
    instances_page: gpui::Entity<crate::agent_instances_view::AgentInstancesPage>,
    /// 工作流编辑器页(独立实体;设计 §11.2)。
    workflow_page: gpui::Entity<crate::workflow_canvas::WorkflowCanvas>,
}

impl AgentWorkspace {
    pub fn new(app: Arc<AppCtx>, cx: &mut Context<Self>) -> AgentWorkspace {
        let app_for_pages = app.clone();
        let ws = AgentWorkspace {
            app,
            view: WorkspaceView::Agents,
            cards: Vec::new(),
            project_roots: Vec::new(),
            global_active_runs: 0,
            filter_project: None,
            filter_text: String::new(),
            overlay: None,
            overlay_input: String::new(),
            selected_task: None,
            task: None,
            steps: Vec::new(),
            draft: Vec::new(),
            dirty: false,
            loaded_revision: None,
            selected_step: None,
            profile_popover: false,
            validation: Vec::new(),
            status_message: String::new(),
            templates: Vec::new(),
            template_popover: false,
            active_field: Field::None,
            field_buffer: String::new(),
            focus_handle: cx.focus_handle(),
            pending_focus: false,
            instances_page: cx.new(|cx| {
                crate::agent_instances_view::AgentInstancesPage::new(app_for_pages.clone(), cx)
            }),
            workflow_page: cx
                .new(|cx| crate::workflow_canvas::WorkflowCanvas::new(app_for_pages, cx)),
        };
        ws
    }

    /// Workspace 泵推送统一快照(与 TaskSidebar 同一 revision)。
    /// Pipeline detail 在 revision 变化时刷新;不恢复独立全局轮询。
    pub fn set_overview(&mut self, snapshot: Arc<ProjectOverviewSnapshot>, cx: &mut Context<Self>) {
        self.cards = snapshot.agent_cards.clone();
        self.templates = snapshot.templates.clone();
        self.project_roots = snapshot.projects.iter().map(|p| p.root.clone()).collect();
        self.global_active_runs = snapshot.global_active_runs;
        if !self.dirty {
            self.refresh_pipeline_state();
        }
        cx.notify();
    }

    pub fn show_tab(&mut self, tab: crate::workspace::AgentTab, cx: &mut Context<Self>) {
        self.view = match tab {
            crate::workspace::AgentTab::Agents => WorkspaceView::Agents,
            crate::workspace::AgentTab::Pipeline => WorkspaceView::Pipeline,
        };
        cx.notify();
    }

    pub fn set_selected_task(&mut self, sel: Option<(PathBuf, i64)>, cx: &mut Context<Self>) {
        if self.selected_task != sel {
            self.selected_task = sel.clone();
            // 实例页的默认 CLI 启动挂载点跟随任务选择
            self.instances_page
                .update(cx, |page, cx| page.set_selected_task(sel.clone(), cx));
            self.workflow_page
                .update(cx, |page, cx| page.set_selected_task(sel, cx));
            self.dirty = false;
            self.draft.clear();
            self.loaded_revision = None;
            self.selected_step = None;
            // 切换任务时清空编辑缓冲:防止 A 的草稿文本写进 B 的字段
            self.active_field = Field::None;
            self.field_buffer.clear();
            self.view = WorkspaceView::Pipeline;
            self.refresh_pipeline_state();
            cx.notify();
        }
    }

    fn orchestrator(&self) -> Option<Arc<Orchestrator>> {
        let (root, _) = self.selected_task.as_ref()?;
        self.app.orchestrator_of(root)
    }

    fn refresh_pipeline_state(&mut self) {
        let Some((root, task_id)) = self.selected_task.clone() else {
            self.task = None;
            self.steps.clear();
            return;
        };
        let Some(orch) = self.app.orchestrator_of(&root) else {
            return;
        };
        if let Ok(Some((task, steps, _))) = orch.task_detail(task_id) {
            let revision_changed = task.active_revision != self.loaded_revision;
            self.task = Some(task);
            self.steps = steps;
            if revision_changed || self.draft.is_empty() {
                self.loaded_revision = self.task.as_ref().and_then(|t| t.active_revision);
                self.draft = self
                    .steps
                    .iter()
                    .map(|s| StepDraft {
                        key: s.step_key.clone(),
                        title: s.title.clone(),
                        instructions: s.instructions.clone(),
                        agent_profile: s.agent_profile.clone(),
                        session_policy: SessionPolicy::parse_db(&s.session_policy),
                        deps: s
                            .deps
                            .iter()
                            .filter_map(|d| {
                                self.steps
                                    .iter()
                                    .find(|x| x.id == *d)
                                    .map(|x| x.step_key.clone())
                            })
                            .collect(),
                    })
                    .collect();
            }
        }
    }

    // ---------- 动作 ----------

    fn save_pipeline(&mut self, cx: &mut Context<Self>) {
        let Some((_, task_id)) = self.selected_task.clone() else {
            return;
        };
        let Some(orch) = self.orchestrator() else {
            return;
        };
        let draft = PipelineDraft {
            steps: self.draft.clone(),
        };
        match orch.save_pipeline(task_id, &draft) {
            Ok(_) => {
                self.dirty = false;
                self.validation.clear();
                self.status_message = "流水线已保存".into();
                self.refresh_pipeline_state();
            }
            Err(e) => self.status_message = format!("{e:#}"),
        }
        cx.notify();
    }

    fn validate_draft(&mut self, cx: &mut Context<Self>) {
        let catalog = self.app.catalog.read();
        self.validation = PipelineDraft {
            steps: self.draft.clone(),
        }
        .validate(&catalog.index);
        self.status_message = if self.validation.is_empty() {
            "校验通过".into()
        } else {
            format!("{} 个问题", self.validation.len())
        };
        cx.notify();
    }

    fn confirm_and_run(&mut self, cx: &mut Context<Self>) {
        let Some((_, task_id)) = self.selected_task.clone() else {
            return;
        };
        let Some(orch) = self.orchestrator() else {
            return;
        };
        if self.dirty {
            let draft = PipelineDraft {
                steps: self.draft.clone(),
            };
            if let Err(e) = orch.save_pipeline(task_id, &draft) {
                self.status_message = format!("{e:#}");
                cx.notify();
                return;
            }
            self.dirty = false;
        }
        match orch.confirm_and_run(task_id) {
            Ok(_) => self.status_message = "已确认并开始运行".into(),
            Err(e) => self.status_message = format!("{e:#}"),
        }
        self.refresh_pipeline_state();
        cx.notify();
    }

    fn ai_generate(&mut self, cx: &mut Context<Self>) {
        // Planner 草案(mock 演示;真实 provider 走结构化结果)。Planner 不得绕过用户确认。
        let Some(task) = self.task.clone() else {
            return;
        };
        let title_head: String = task.title.chars().take(20).collect();
        self.draft = vec![
            StepDraft {
                key: "plan".into(),
                title: format!("规划:{title_head}"),
                instructions: "分析任务目标,给出执行清单。".into(),
                agent_profile: "mock".into(),
                session_policy: SessionPolicy::Fresh,
                deps: vec![],
            },
            StepDraft {
                key: "execute".into(),
                title: "执行主工作".into(),
                instructions: task.goal.clone(),
                agent_profile: "mock".into(),
                session_policy: SessionPolicy::Fresh,
                deps: vec!["plan".into()],
            },
        ];
        self.dirty = true;
        self.status_message = "AI 已生成草案(需用户确认后才运行)".into();
        cx.notify();
    }

    fn add_step(&mut self, cx: &mut Context<Self>) {
        // 唯一 key:删除 step-1 后再添加不得复用旧 key(校验会拒绝重复)
        let mut n = self.draft.len() + 1;
        while self.draft.iter().any(|s| s.key == format!("step-{n}")) {
            n += 1;
        }
        self.draft.push(StepDraft {
            key: format!("step-{n}"),
            title: format!("新步骤 {n}"),
            instructions: String::new(),
            agent_profile: "mock".into(),
            session_policy: SessionPolicy::Fresh,
            deps: Vec::new(),
        });
        self.dirty = true;
        cx.notify();
    }

    fn task_action(&mut self, action: &str, cx: &mut Context<Self>) {
        let Some((_, task_id)) = self.selected_task.clone() else {
            return;
        };
        let Some(orch) = self.orchestrator() else {
            return;
        };
        let result = match action {
            "pause" => orch.pause_task(task_id).map(|_| "已暂停".to_string()),
            "resume" => orch.resume_task(task_id).map(|_| "已继续".to_string()),
            "cancel" => orch.cancel_task(task_id).map(|_| "已终止".to_string()),
            _ => Ok(String::new()),
        };
        self.status_message = result.unwrap_or_else(|e| format!("{e:#}"));
        self.refresh_pipeline_state();
        cx.notify();
    }

    fn step_action(
        &mut self,
        step_id: i64,
        action: &str,
        profile: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(orch) = self.orchestrator() else {
            return;
        };
        let result: std::result::Result<String, String> = match action {
            "retry" => orch
                .retry_step(step_id, mf_agent::RetryMode::FreshSession)
                .map(|_| "已重试".into())
                .map_err(|e| format!("{e:#}")),
            "skip" => orch
                .skip_step(step_id, true)
                .map(|_| "已跳过".into())
                .map_err(|e| format!("{e:#}")),
            "replace" => orch
                .replace_agent(step_id, profile.unwrap_or("mock"))
                .map(|_| "已替换 Agent(新 Revision)".into())
                .map_err(|e| format!("{e:#}")),
            _ => Ok(String::new()),
        };
        self.status_message = result.unwrap_or_default();
        self.dirty = false;
        self.refresh_pipeline_state();
        cx.notify();
    }

    /// 手工结算:run id 是各项目数据库行号,必须按项目路由(不能扫第一个命中的库)。
    fn settle_run(&mut self, project: &PathBuf, run_id: i64, ok: bool, cx: &mut Context<Self>) {
        if let Some(orch) = self.app.orchestrator_of(project) {
            let settlement = if ok {
                Settlement::Complete {
                    summary: "人工判定成功".into(),
                }
            } else {
                Settlement::Fail {
                    reason: "人工判定失败".into(),
                }
            };
            let _ = orch.settle_run(run_id, settlement);
        }
        cx.notify();
    }

    fn send_prompt_to_run(
        &mut self,
        project: &PathBuf,
        run_id: i64,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(orch) = self.app.orchestrator_of(project) {
            let _ = orch.send_prompt(run_id, text);
        }
        cx.notify();
    }

    fn confirm_session_by(
        &mut self,
        project: &PathBuf,
        session_id: i64,
        to_idle: bool,
        cx: &mut Context<Self>,
    ) {
        let status = if to_idle {
            SessionStatus::Idle
        } else {
            SessionStatus::Hidden
        };
        if let Some(orch) = self.app.orchestrator_of(project) {
            if let Err(e) = orch.set_session_status(session_id, status) {
                log::warn!("更新会话状态失败: {e:#}");
            }
        }
        cx.notify();
    }

    fn kill_session_by(&mut self, project: &PathBuf, session_id: i64, cx: &mut Context<Self>) {
        let project_str = project.to_string_lossy().to_string();
        self.app.registry.kill_session(&project_str, session_id);
        if let Some(orch) = self.app.orchestrator_of(project) {
            if let Err(e) = orch.set_session_status(session_id, SessionStatus::Dead) {
                log::warn!("更新会话状态失败: {e:#}");
            }
        }
        cx.notify();
    }
}

// ---------- 渲染:Agents 视图 ----------

impl AgentWorkspace {
    fn render_agents_view(&self, cx: &Context<Self>, window: &Window) -> AnyElement {
        let filtered: Vec<usize> = self
            .cards
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                if let Some(p) = &self.filter_project {
                    if &c.project_root != p {
                        return false;
                    }
                }
                if !self.filter_text.is_empty() {
                    let hay = format!(
                        "{} {} {} {}",
                        c.profile_display,
                        c.task_title.clone().unwrap_or_default(),
                        c.session.last_instruction.clone().unwrap_or_default(),
                        c.session.last_reply.clone().unwrap_or_default()
                    );
                    if !hay
                        .to_lowercase()
                        .contains(&self.filter_text.to_lowercase())
                    {
                        return false;
                    }
                }
                true
            })
            .map(|(i, _)| i)
            .collect();

        let filter_label = match &self.filter_project {
            None => "项目:全部".to_string(),
            Some(p) => format!(
                "项目:{}",
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            ),
        };

        let toolbar = div()
            .id("agents-toolbar")
            .flex()
            .items_center()
            .gap_1p5()
            .px_2()
            .h(px(34.))
            .border_b_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .child(
                div()
                    .id("filter-project")
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .text_size(px(10.5))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .child(filter_label)
                    .on_click(cx.listener(|ws: &mut AgentWorkspace, _, _, cx| {
                        let projects = ws.project_roots.clone();
                        ws.filter_project = match &ws.filter_project {
                            None => projects.first().cloned(),
                            Some(cur) => {
                                let idx = projects.iter().position(|p| p == cur);
                                match idx {
                                    Some(i) if i + 1 < projects.len() => {
                                        Some(projects[i + 1].clone())
                                    }
                                    _ => None,
                                }
                            }
                        };
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("filter-text")
                    .flex_1()
                    .max_w(px(320.))
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(
                        if self.active_field == Field::FilterText
                            && self.focus_handle.is_focused(window)
                        {
                            crate::theme::Theme::accent()
                        } else {
                            crate::theme::Theme::border()
                        },
                    ))
                    .text_size(px(10.5))
                    .cursor_pointer()
                    .on_click(cx.listener(|ws: &mut AgentWorkspace, _, window, cx| {
                        ws.active_field = Field::FilterText;
                        ws.field_buffer = ws.filter_text.clone();
                        window.focus(&ws.focus_handle, cx);
                        cx.notify();
                    }))
                    .child(if self.filter_text.is_empty() {
                        div()
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("过滤(Agent/任务/文本)…")
                    } else {
                        div()
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child(self.filter_text.clone())
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child(format!(
                        "{} 会话 · 活动运行 {} · 全局并发上限 {}",
                        filtered.len(),
                        self.global_active_runs,
                        self.app.limiter.max()
                    )),
            );

        let mut columns = div()
            .id("agents-columns")
            .flex_1()
            .min_h_0()
            .flex()
            .gap_2()
            .p_2();
        for col in [
            Column::NeedsYou,
            Column::Working,
            Column::Done,
            Column::Idle,
        ] {
            let col_cards: Vec<usize> = filtered
                .iter()
                .copied()
                .filter(|i| Column::from(self.cards[*i].bucket) == col)
                .collect();
            let accent = match col {
                Column::NeedsYou => crate::theme::Theme::warning(),
                Column::Working => crate::theme::Theme::success(),
                Column::Done => crate::theme::Theme::accent(),
                Column::Idle => crate::theme::Theme::fg_faint(),
            };
            let mut body = div()
                .id(ElementId::Name(format!("col-{}", col.title()).into()))
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap_1p5()
                .overflow_y_scroll();
            for &idx in &col_cards {
                body = body.child(self.render_card(idx, cx));
            }
            if col_cards.is_empty() {
                body = body.child(
                    div()
                        .p_2()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child("—"),
                );
            }
            columns = columns.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(div().size(px(8.)).rounded_full().bg(rgb(accent)))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(col.title()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child(col_cards.len().to_string()),
                            ),
                    )
                    .child(body),
            );
        }

        div()
            .id("agents-view")
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(columns)
            .into_any_element()
    }

    fn render_card(&self, idx: usize, cx: &Context<Self>) -> AnyElement {
        let card = &self.cards[idx];
        let column = Column::from(card.bucket);
        let accent = match column {
            Column::NeedsYou => crate::theme::Theme::warning(),
            Column::Working => crate::theme::Theme::success(),
            Column::Done => crate::theme::Theme::accent(),
            Column::Idle => crate::theme::Theme::fg_faint(),
        };
        let session_id = card.session.id;
        let run_id = card.run.as_ref().map(|r| r.id);
        let card_task_id = card.task_id;
        let is_http = card.is_http;
        let card_project = card.project_root.clone();
        let project = card.project_root.clone();
        let session_title = if card.session.title.is_empty() {
            card.profile_display.clone()
        } else {
            card.session.title.clone()
        };
        let status_label = card
            .run
            .as_ref()
            .map(|r| r.status.as_str().to_string())
            .unwrap_or_else(|| card.session.status.as_str().to_string());
        let status_label = if card.alive {
            status_label
        } else {
            format!("{status_label}(已退出)")
        };
        let last_user: String = card
            .session
            .last_instruction
            .clone()
            .unwrap_or_default()
            .chars()
            .take(60)
            .collect();
        let last_reply: String = card
            .tail
            .last()
            .cloned()
            .or_else(|| card.session.last_reply.clone())
            .unwrap_or_default()
            .chars()
            .take(80)
            .collect();

        let mut actions = div().flex().items_center().gap_1().mt_1();
        match column {
            Column::NeedsYou => {
                if let Some(run_id) = run_id {
                    let p_ok = card_project.clone();
                    let p_fail = card_project.clone();
                    actions = actions
                        .child(small_btn(
                            cx,
                            ("card-ok", session_id as u64),
                            "判定成功",
                            crate::theme::Theme::success(),
                            move |ws, _, _, cx| {
                                ws.settle_run(&p_ok, run_id, true, cx);
                            },
                        ))
                        .child(small_btn(
                            cx,
                            ("card-fail", session_id as u64),
                            "判定失败",
                            crate::theme::Theme::danger(),
                            move |ws, _, _, cx| {
                                ws.settle_run(&p_fail, run_id, false, cx);
                            },
                        ));
                }
            }
            Column::Done => {
                let p = card_project.clone();
                actions = actions.child(small_btn(
                    cx,
                    ("card-confirm", session_id as u64),
                    "确认",
                    crate::theme::Theme::accent(),
                    move |ws, _, _, cx| {
                        ws.confirm_session_by(&p, session_id, true, cx);
                    },
                ));
            }
            Column::Idle => {
                let p_hide = card_project.clone();
                let p_kill = card_project.clone();
                actions = actions
                    .child(small_btn(
                        cx,
                        ("card-hide", session_id as u64),
                        "隐藏(留历史)",
                        0x8a8a8a,
                        move |ws, _, _, cx| {
                            ws.confirm_session_by(&p_hide, session_id, false, cx);
                        },
                    ))
                    .child(small_btn(
                        cx,
                        ("card-kill", session_id as u64),
                        "终止",
                        crate::theme::Theme::danger(),
                        move |ws, _, _, cx| {
                            ws.kill_session_by(&p_kill, session_id, cx);
                        },
                    ));
            }
            Column::Working => {}
        }
        let _ = idx;

        div()
            .id(("agent-card", session_id as u64))
            .p_2()
            .rounded_lg()
            .border_1()
            .border_color(rgb(if column == Column::NeedsYou {
                crate::theme::Theme::warning()
            } else {
                crate::theme::Theme::border()
            }))
            .bg(rgb(crate::theme::Theme::bg_elevated()))
            .cursor_pointer()
            .hover(move |d| d.border_color(rgb(accent)))
            .on_click(cx.listener(move |ws: &mut AgentWorkspace, _, _, cx| {
                // 卡片打开 = 先激活所属项目 + 清除 session 未读(activation seam),
                // 同时在本视图打开终端/transcript overlay。
                let (pid, _) = normalize_project_path(&project);
                cx.emit(AgentWorkspaceEvent::Activate(ActivationTarget::AgentRun {
                    project: pid,
                    task_id: card_task_id,
                    session_id,
                }));
                ws.overlay = Some(match run_id {
                    Some(run_id) if is_http => Overlay::Transcript {
                        project: project.clone(),
                        session_id,
                        run_id,
                    },
                    _ => Overlay::Terminal {
                        project: project.clone(),
                        session_id,
                        run_id,
                    },
                });
                ws.overlay_input.clear();
                ws.active_field = Field::None;
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(div().text_size(px(12.)).child(card.profile_display.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(rgb(crate::theme::Theme::fg()))
                            .child(session_title),
                    )
                    .when(card.session.unread, |d| {
                        d.child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(rgb(crate::theme::Theme::warning())),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(accent))
                            .child(status_label),
                    ),
            )
            .child(
                div()
                    .mt_1()
                    .text_size(px(9.5))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child(format!(
                        "{} · {}",
                        card.project_name,
                        card.task_title.clone().unwrap_or_else(|| "—".into())
                    )),
            )
            .when(!last_user.is_empty(), |d| {
                d.child(
                    div()
                        .mt_1()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::Theme::fg_dim()))
                        .child(format!("▶ {last_user}")),
                )
            })
            .when(!last_reply.is_empty(), |d| {
                d.child(
                    div()
                        .mt_0p5()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                        .child(format!("◀ {last_reply}")),
                )
            })
            .child(actions)
            .into_any_element()
    }
}

// ---------- 渲染:Pipeline 视图 ----------

impl AgentWorkspace {
    fn render_pipeline_view(&self, cx: &Context<Self>, window: &Window) -> AnyElement {
        let Some(task) = self.task.clone() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::Theme::fg_faint()))
                .child("在左侧任务列表选择或新建一个任务")
                .into_any_element();
        };

        let mut toolbar = div()
            .id("pipeline-toolbar")
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(34.))
            .border_b_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(crate::theme::Theme::fg()))
                    .child(format!(
                        "{} · {}",
                        task.title.chars().take(30).collect::<String>(),
                        task.status.label_cn()
                    )),
            )
            .child(div().w(px(8.)));
        if !self.templates.is_empty() {
            toolbar = toolbar.child(small_btn(
                cx,
                "tpl-create",
                "从模板创建",
                crate::theme::Theme::accent(),
                |ws, _, _, cx| {
                    ws.template_popover = !ws.template_popover;
                    cx.notify();
                },
            ));
        }
        toolbar = toolbar
            .child(small_btn(
                cx,
                "ai-gen",
                "AI 生成",
                crate::theme::Theme::accent(),
                |ws, _, _, cx| {
                    ws.ai_generate(cx);
                },
            ))
            .child(small_btn(
                cx,
                "add-step",
                "添加 Step",
                crate::theme::Theme::accent(),
                |ws, _, _, cx| {
                    ws.add_step(cx);
                },
            ))
            .child(small_btn(
                cx,
                "validate",
                "校验",
                0x8a8a8a,
                |ws, _, _, cx| {
                    ws.validate_draft(cx);
                },
            ));
        if self.dirty {
            toolbar = toolbar.child(small_btn(
                cx,
                "save",
                "保存修改",
                crate::theme::Theme::warning(),
                |ws, _, _, cx| {
                    ws.save_pipeline(cx);
                },
            ));
        }
        toolbar = toolbar.child(small_btn(
            cx,
            "run",
            "确认并运行",
            crate::theme::Theme::success(),
            |ws, _, _, cx| {
                ws.confirm_and_run(cx);
            },
        ));
        match task.status {
            TaskStatus::Running if task.paused => {
                toolbar = toolbar.child(small_btn(
                    cx,
                    "resume",
                    "继续",
                    crate::theme::Theme::success(),
                    |ws, _, _, cx| {
                        ws.task_action("resume", cx);
                    },
                ));
            }
            TaskStatus::Running => {
                toolbar = toolbar.child(small_btn(
                    cx,
                    "pause",
                    "暂停",
                    crate::theme::Theme::warning(),
                    |ws, _, _, cx| {
                        ws.task_action("pause", cx);
                    },
                ));
            }
            _ => {}
        }
        toolbar = toolbar.child(small_btn(
            cx,
            "cancel",
            "取消",
            crate::theme::Theme::danger(),
            |ws, _, _, cx| {
                ws.task_action("cancel", cx);
            },
        ));
        if !self.status_message.is_empty() {
            toolbar = toolbar.child(
                div()
                    .id("pipeline-status")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(10.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child(self.status_message.clone()),
            );
        }

        let draft = PipelineDraft {
            steps: self.draft.clone(),
        };
        let levels = draft.topo_levels();
        let by_key: HashMap<String, StepView> = self
            .steps
            .iter()
            .map(|s| (s.step_key.clone(), s.clone()))
            .collect();
        let mut cols = div()
            .id("pipeline-cols")
            .flex_1()
            .min_h_0()
            .flex()
            .gap_3()
            .p_3()
            .overflow_x_scroll();
        if levels.is_empty() {
            cols = cols.child(
                div()
                    .p_3()
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                    .child("还没有步骤;从模板创建、AI 生成或「添加 Step」开始。"),
            );
        }
        let level_list: Vec<(usize, Vec<String>)> = levels
            .iter()
            .enumerate()
            .map(|(i, l)| (i, l.clone()))
            .collect();
        for (li, level) in level_list {
            let mut col = div().flex().flex_col().gap_2().min_w(px(200.));
            for key in level.clone() {
                let Some(dstep) = draft.step(&key).cloned() else {
                    continue;
                };
                let saved = by_key.get(&key).cloned();
                let status = saved
                    .as_ref()
                    .map(|s| s.status)
                    .unwrap_or(StepStatus::Pending);
                let attempts = saved.as_ref().map(|s| s.attempts).unwrap_or(0);
                let selected = self.selected_step.as_deref() == Some(key.as_str());
                let color = step_color(status);
                let step_id = saved.as_ref().map(|s| s.id);
                let needs_actions = matches!(
                    status,
                    StepStatus::Failed | StepStatus::Blocked | StepStatus::AwaitingOutcome
                );
                col = col.child(
                    div()
                        .id(ElementId::Name(format!("step-{key}").into()))
                        .w(px(208.))
                        .p_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(if selected {
                            crate::theme::Theme::accent()
                        } else {
                            crate::theme::Theme::border()
                        }))
                        .bg(rgb(crate::theme::Theme::bg_elevated()))
                        .cursor_pointer()
                        .hover(|d| d.border_color(rgb(crate::theme::Theme::accent_dim())))
                        .on_click({
                            let key_for_click = key.clone();
                            cx.listener(move |ws: &mut AgentWorkspace, _, _, cx| {
                                if ws.selected_step.as_deref() != Some(key_for_click.as_str()) {
                                    // 切换步骤:丢弃未提交的字段缓冲
                                    ws.active_field = Field::None;
                                    ws.field_buffer.clear();
                                }
                                ws.selected_step = Some(key_for_click.clone());
                                ws.profile_popover = false;
                                cx.notify();
                            })
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .child(div().size(px(7.)).rounded_full().bg(rgb(color)))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(11.))
                                        .child(dstep.title.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(9.))
                                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                                        .child(if attempts > 0 {
                                            format!("×{attempts}")
                                        } else {
                                            String::new()
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .mt_1()
                                .text_size(px(9.5))
                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                .child(format!("{} · {}", dstep.agent_profile, status.label_cn())),
                        )
                        .child(
                            div()
                                .mt_0p5()
                                .text_size(px(9.))
                                .text_color(rgb(crate::theme::Theme::fg_faint()))
                                .child(if dstep.deps.is_empty() {
                                    "无依赖".to_string()
                                } else {
                                    format!("← {}", dstep.deps.join(","))
                                }),
                        )
                        .when(needs_actions, |d| {
                            d.child(
                                div()
                                    .mt_1()
                                    .flex()
                                    .gap_1()
                                    .child(small_btn(
                                        cx,
                                        ElementId::Name(format!("step-retry-{key}").into()),
                                        "重试",
                                        crate::theme::Theme::accent(),
                                        move |ws, _, _, cx| {
                                            if let Some(id) = step_id {
                                                ws.step_action(id, "retry", None, cx);
                                            }
                                        },
                                    ))
                                    .child(small_btn(
                                        cx,
                                        ElementId::Name(format!("step-skip-{key}").into()),
                                        "跳过",
                                        0x8a8a8a,
                                        move |ws, _, _, cx| {
                                            if let Some(id) = step_id {
                                                ws.step_action(id, "skip", None, cx);
                                            }
                                        },
                                    )),
                            )
                        }),
                );
            }
            cols = cols.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child(format!("L{li}")),
                    )
                    .child(col),
            );
        }

        let editor: AnyElement = if self.selected_step.is_some() {
            self.render_step_editor(cx, window)
        } else {
            div().into_any_element()
        };
        let validation_panel = if self.validation.is_empty() {
            None
        } else {
            Some(
                div()
                    .id("validation-panel")
                    .max_h(px(110.))
                    .overflow_y_scroll()
                    .px_3()
                    .py_1p5()
                    .border_t_1()
                    .border_color(rgb(crate::theme::Theme::danger()))
                    .text_size(px(10.))
                    .text_color(rgb(crate::theme::Theme::danger()))
                    .children(self.validation.iter().map(|e| div().child(e.clone()))),
            )
        };

        div()
            .id("pipeline-view")
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .child(toolbar)
            .when(self.template_popover, |d| {
                d.child(self.render_template_popover(cx))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(cols)
                            .children(validation_panel),
                    )
                    .child(editor),
            )
            .into_any_element()
    }

    fn render_template_popover(&self, cx: &Context<Self>) -> AnyElement {
        let mut list = div()
            .id("template-popover")
            .absolute()
            .top(px(34.))
            .left(px(120.))
            .w(px(280.))
            .rounded_lg()
            .border_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg_elevated()))
            .p_1()
            .flex()
            .flex_col()
            .gap_0p5();
        for (i, (id, name, tpl)) in self.templates.iter().enumerate() {
            let steps = tpl.steps.len();
            let steps_copy = tpl.steps.clone();
            list = list.child(
                div()
                    .id(("tpl-item", i as u64))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                    .text_size(px(11.))
                    .child(format!("{name} · {steps} 步 · {id}"))
                    .on_click(cx.listener(move |ws: &mut AgentWorkspace, _, _, cx| {
                        ws.draft = steps_copy.clone();
                        ws.dirty = true;
                        ws.template_popover = false;
                        ws.status_message = "模板已载入草案".into();
                        cx.notify();
                    })),
            );
        }
        list.into_any_element()
    }
}

// ---------- 渲染:节点编辑器 ----------

impl AgentWorkspace {
    fn render_step_editor(&self, cx: &Context<Self>, window: &Window) -> AnyElement {
        let Some(key) = self.selected_step.clone() else {
            return div().into_any_element();
        };
        let Some(step) = self.draft.iter().find(|s| s.key == key) else {
            return div().into_any_element();
        };
        let profiles: Vec<String> = {
            let catalog = self.app.catalog.read();
            let mut ids: Vec<String> = catalog.index.entries.keys().cloned().collect();
            ids.sort();
            ids
        };
        let other_keys: Vec<String> = self
            .draft
            .iter()
            .filter(|s| s.key != key)
            .map(|s| s.key.clone())
            .collect();
        let session_key = match &step.session_policy {
            SessionPolicy::Fresh => String::new(),
            SessionPolicy::Reuse { key } => key.clone(),
        };
        let field_focused =
            |f: Field| self.active_field == f && self.focus_handle.is_focused(window);

        let title_value = if field_focused(Field::Title) {
            self.field_buffer.clone()
        } else {
            step.title.clone()
        };
        let instructions_value = if field_focused(Field::Instructions) {
            self.field_buffer.clone()
        } else {
            step.instructions.clone()
        };
        let session_key_value = if field_focused(Field::SessionKey) {
            self.field_buffer.clone()
        } else {
            session_key
        };

        div()
            .id("step-editor")
            .w(px(300.))
            .min_w(px(300.))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .bg(rgb(crate::theme::Theme::bg_panel()))
            .overflow_y_scroll()
            .child(
                div()
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .px_2p5()
                    .border_b_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("Step `{key}`")),
                    ),
            )
            .child(
                div()
                    .id("edit-title-wrap")
                    .px_2p5()
                    .py_1p5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("标题"),
                    )
                    .child(
                        div()
                            .id("edit-title")
                            .h(px(24.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if field_focused(Field::Title) {
                                crate::theme::Theme::accent()
                            } else {
                                crate::theme::Theme::border()
                            }))
                            .text_size(px(10.5))
                            .cursor_pointer()
                            .on_click(cx.listener(|ws: &mut AgentWorkspace, _, window, cx| {
                                ws.active_field = Field::Title;
                                if let Some(k) = ws.selected_step.clone() {
                                    if let Some(s) = ws.draft.iter().find(|s| s.key == k) {
                                        ws.field_buffer = s.title.clone();
                                    }
                                }
                                window.focus(&ws.focus_handle, cx);
                                cx.notify();
                            }))
                            .child(if title_value.is_empty() {
                                div()
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child("—")
                            } else {
                                div()
                                    .text_color(rgb(crate::theme::Theme::fg()))
                                    .child(title_value.clone())
                            }),
                    ),
            )
            .child(
                div()
                    .id("edit-instructions-wrap")
                    .px_2p5()
                    .py_1p5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("工作说明"),
                    )
                    .child(
                        div()
                            .id("edit-instructions")
                            .min_h(px(64.))
                            .p_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if field_focused(Field::Instructions) {
                                crate::theme::Theme::accent()
                            } else {
                                crate::theme::Theme::border()
                            }))
                            .text_size(px(10.5))
                            .cursor_pointer()
                            .on_click(cx.listener(|ws: &mut AgentWorkspace, _, window, cx| {
                                ws.active_field = Field::Instructions;
                                if let Some(k) = ws.selected_step.clone() {
                                    if let Some(s) = ws.draft.iter().find(|s| s.key == k) {
                                        ws.field_buffer = s.instructions.clone();
                                    }
                                }
                                window.focus(&ws.focus_handle, cx);
                                cx.notify();
                            }))
                            .child(if instructions_value.is_empty() {
                                div()
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child("(点击后输入;Enter 换行,Esc 结束)")
                            } else {
                                div()
                                    .text_color(rgb(crate::theme::Theme::fg()))
                                    .child(instructions_value.clone())
                            }),
                    ),
            )
            .child(
                div()
                    .px_2p5()
                    .py_1p5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("Agent Profile"),
                    )
                    .child(
                        div()
                            .id("edit-profile")
                            .h(px(24.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(crate::theme::Theme::border()))
                            .cursor_pointer()
                            .text_size(px(10.5))
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child(step.agent_profile.clone())
                            .on_click(cx.listener(|ws: &mut AgentWorkspace, _, _, cx| {
                                ws.profile_popover = !ws.profile_popover;
                                cx.notify();
                            })),
                    )
                    .when(self.profile_popover, |d| {
                        d.child(
                            div()
                                .id("profile-list")
                                .max_h(px(160.))
                                .overflow_y_scroll()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(crate::theme::Theme::border()))
                                .flex()
                                .flex_col()
                                .children(profiles.iter().map(|p| {
                                    let p = p.clone();
                                    div()
                                        .id(ElementId::Name(format!("profile-{p}").into()))
                                        .px_2()
                                        .py_1()
                                        .text_size(px(10.5))
                                        .cursor_pointer()
                                        .hover(|h| h.bg(rgb(crate::theme::Theme::bg_hover())))
                                        .child(p.clone())
                                        .on_click(cx.listener(
                                            move |ws: &mut AgentWorkspace, _, _, cx| {
                                                if let Some(k) = ws.selected_step.clone() {
                                                    if let Some(s) =
                                                        ws.draft.iter_mut().find(|s| s.key == k)
                                                    {
                                                        s.agent_profile = p.clone();
                                                    }
                                                }
                                                ws.dirty = true;
                                                ws.profile_popover = false;
                                                cx.notify();
                                            },
                                        ))
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .id("session-policy")
                    .px_2p5()
                    .py_1p5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("会话策略(相同 key 串行复用)"),
                    )
                    .child(
                        div()
                            .id("edit-session-fresh")
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .cursor_pointer()
                            .child(
                                div()
                                    .size(px(10.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::fg_dim()))
                                    .when(
                                        matches!(step.session_policy, SessionPolicy::Fresh),
                                        |d| d.bg(rgb(crate::theme::Theme::accent())),
                                    ),
                            )
                            .child(div().text_size(px(10.5)).child("fresh(每步新会话)"))
                            .on_click(cx.listener(move |ws: &mut AgentWorkspace, _, _, cx| {
                                if let Some(k) = ws.selected_step.clone() {
                                    if let Some(s) = ws.draft.iter_mut().find(|s| s.key == k) {
                                        s.session_policy = SessionPolicy::Fresh;
                                    }
                                    ws.dirty = true;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("edit-session-reuse")
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .cursor_pointer()
                            .child(
                                div()
                                    .size(px(10.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::fg_dim()))
                                    .when(
                                        matches!(step.session_policy, SessionPolicy::Reuse { .. }),
                                        |d| d.bg(rgb(crate::theme::Theme::accent())),
                                    ),
                            )
                            .child(div().text_size(px(10.5)).child("reuse:"))
                            .child(
                                div()
                                    .id("edit-session-key")
                                    .w(px(120.))
                                    .h(px(22.))
                                    .px_1p5()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(if field_focused(Field::SessionKey) {
                                        crate::theme::Theme::accent()
                                    } else {
                                        crate::theme::Theme::border()
                                    }))
                                    .text_size(px(10.5))
                                    .child(session_key_value.clone()),
                            )
                            .on_click(cx.listener(|ws: &mut AgentWorkspace, _, window, cx| {
                                ws.active_field = Field::SessionKey;
                                if let Some(k) = ws.selected_step.clone() {
                                    if let Some(s) = ws.draft.iter().find(|s| s.key == k) {
                                        ws.field_buffer = match &s.session_policy {
                                            SessionPolicy::Fresh => String::new(),
                                            SessionPolicy::Reuse { key } => key.clone(),
                                        };
                                    }
                                }
                                window.focus(&ws.focus_handle, cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .px_2p5()
                    .py_1p5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgb(crate::theme::Theme::fg_faint()))
                            .child("前置步骤"),
                    )
                    .children(other_keys.clone().into_iter().map(|other| {
                        let checked = step.deps.contains(&other);
                        div()
                            .id(ElementId::Name(format!("dep-{other}").into()))
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .cursor_pointer()
                            .py_0p5()
                            .child(
                                div()
                                    .size(px(10.))
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(rgb(crate::theme::Theme::fg_dim()))
                                    .when(checked, |d| d.bg(rgb(crate::theme::Theme::accent()))),
                            )
                            .child(div().text_size(px(10.5)).child(other.clone()))
                            .on_click(cx.listener(move |ws: &mut AgentWorkspace, _, _, cx| {
                                if let Some(k) = ws.selected_step.clone() {
                                    if let Some(s) = ws.draft.iter_mut().find(|s| s.key == k) {
                                        if s.deps.contains(&other) {
                                            s.deps.retain(|d| d != &other);
                                        } else {
                                            s.deps.push(other.clone());
                                        }
                                    }
                                    ws.dirty = true;
                                }
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .id("editor-delete-wrap")
                    .px_2p5()
                    .py_1p5()
                    .flex()
                    .gap_1()
                    .child(small_btn(
                        cx,
                        "editor-delete",
                        "删除此 Step",
                        crate::theme::Theme::danger(),
                        move |ws: &mut AgentWorkspace, _, _, cx| {
                            ws.draft.retain(|s| s.key != key);
                            ws.selected_step = None;
                            ws.dirty = true;
                            cx.notify();
                        },
                    )),
            )
            .into_any_element()
    }

    // ---------- 渲染:详情覆盖层 ----------

    fn render_overlay(&self, cx: &Context<Self>, window: &Window) -> AnyElement {
        let Some(overlay) = &self.overlay else {
            return div().into_any_element();
        };
        let (project, session_id, run_id, is_terminal) = match overlay {
            Overlay::Terminal {
                project,
                session_id,
                run_id,
            } => (project.clone(), *session_id, *run_id, true),
            Overlay::Transcript {
                project,
                session_id,
                run_id,
            } => (project.clone(), *session_id, Some(*run_id), false),
        };
        let project_str = project.to_string_lossy().to_string();
        let snapshot = self.app.registry.snapshot(&project_str, session_id);
        let title = snapshot
            .as_ref()
            .map(|s| s.title.clone())
            .unwrap_or_else(|| format!("会话 #{session_id}"));
        let body: AnyElement = if is_terminal {
            let rows = snapshot.map(|s| s.screen_rows).unwrap_or_default();
            div()
                .id("overlay-terminal")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .bg(rgb(0x0c0c0c))
                .p_2()
                .flex()
                .flex_col()
                .track_focus(&self.focus_handle)
                .children(
                    rows.iter()
                        .map(|line| {
                            div()
                                .id(ElementId::Name(format!("trow-{}", line.len()).into()))
                                .text_size(px(11.5))
                                .font_family("Consolas")
                                .text_color(rgb(0xd4d4d4))
                                .line_height(px(15.))
                                .child(line.replace(' ', "\u{00a0}"))
                        })
                        .collect::<Vec<_>>(),
                )
                .into_any_element()
        } else {
            let transcript = snapshot.map(|s| s.transcript).unwrap_or_default();
            div()
                .id("overlay-transcript")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p_3()
                .flex()
                .flex_col()
                .gap_2()
                .children(transcript.iter().map(|(role, text)| {
                    let (bg, mine) = match role.as_str() {
                        "user" => (crate::theme::Theme::bg_elevated(), true),
                        "assistant" => (crate::theme::Theme::bg_active(), false),
                        _ => (crate::theme::Theme::bg_hover(), false),
                    };
                    div()
                        .id(ElementId::Name(
                            format!("msg-{}-{}", role, text.len()).into(),
                        ))
                        .max_w(px(720.))
                        .when(mine, |d| d.ml_auto())
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(rgb(bg))
                        .text_size(px(11.5))
                        .child(format!("[{role}] {text}"))
                }))
                .into_any_element()
        };
        let awaiting = self
            .cards
            .iter()
            .find(|c| c.session.id == session_id)
            .map(|c| {
                c.run
                    .as_ref()
                    .map(|r| r.status == RunStatus::AwaitingOutcome)
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        div()
            .id("agent-overlay")
            .absolute()
            .size_full()
            .top_0()
            .left_0()
            .flex()
            .flex_col()
            .bg(rgb(crate::theme::Theme::bg()))
            .child(
                div()
                    .id("overlay-header")
                    .h(px(34.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .border_b_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .when(awaiting, |d| {
                        d.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(rgb(crate::theme::Theme::warning()))
                                .child("待结算:Agent 已结束但未显式结算"),
                        )
                    })
                    .child(div().flex_1())
                    .when(awaiting, |d| {
                        let ov_project = project.clone();
                        let ov_project2 = project.clone();
                        d.child(small_btn(
                            cx,
                            "ov-ok",
                            "判定成功",
                            crate::theme::Theme::success(),
                            move |ws: &mut AgentWorkspace, _, _, cx| {
                                if let Some(rid) = run_id {
                                    ws.settle_run(&ov_project, rid, true, cx);
                                }
                            },
                        ))
                        .child(small_btn(
                            cx,
                            "ov-fail",
                            "判定失败",
                            crate::theme::Theme::danger(),
                            move |ws: &mut AgentWorkspace, _, _, cx| {
                                if let Some(rid) = run_id {
                                    ws.settle_run(&ov_project2, rid, false, cx);
                                }
                            },
                        ))
                    })
                    .child(
                        div()
                            .id("ov-close")
                            .px_2()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(crate::theme::Theme::fg_dim()))
                            .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                            .child("✕ 关闭(Esc)")
                            .on_click(cx.listener(|ws: &mut AgentWorkspace, _, _, cx| {
                                ws.overlay = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(body)
            .child(
                div()
                    .id("overlay-input")
                    .h(px(36.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .border_t_1()
                    .border_color(rgb(crate::theme::Theme::border()))
                    .child(
                        div()
                            .id("ov-input-box")
                            .flex_1()
                            .h(px(24.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(
                                if self.active_field == Field::PromptInput
                                    && self.focus_handle.is_focused(window)
                                {
                                    crate::theme::Theme::accent()
                                } else {
                                    crate::theme::Theme::border()
                                },
                            ))
                            .text_size(px(11.))
                            .cursor_pointer()
                            .on_click(cx.listener(|ws: &mut AgentWorkspace, _, window, cx| {
                                ws.active_field = Field::PromptInput;
                                window.focus(&ws.focus_handle, cx);
                                cx.notify();
                            }))
                            .child(if self.overlay_input.is_empty() {
                                div()
                                    .text_color(rgb(crate::theme::Theme::fg_faint()))
                                    .child(if is_terminal {
                                        "输入发送到终端(终端本身支持键盘直通)…"
                                    } else {
                                        "输入发送给 Agent…"
                                    })
                            } else {
                                div()
                                    .text_color(rgb(crate::theme::Theme::fg()))
                                    .child(self.overlay_input.clone())
                            }),
                    )
                    .child({
                        let ov_project_send = project.clone();
                        small_btn(
                            cx,
                            "ov-send",
                            "发送",
                            crate::theme::Theme::accent(),
                            move |ws: &mut AgentWorkspace, _, _, cx| {
                                if let Some(rid) = run_id {
                                    if !ws.overlay_input.trim().is_empty() {
                                        let text = ws.overlay_input.clone();
                                        ws.send_prompt_to_run(&ov_project_send, rid, &text, cx);
                                        ws.overlay_input.clear();
                                    }
                                }
                            },
                        )
                    }),
            )
            .into_any_element()
    }
}

// ---------- 轮询聚合与辅助 ----------

fn step_color(status: StepStatus) -> u32 {
    match status {
        StepStatus::Pending => 0x8a8a8a,
        StepStatus::Ready => crate::theme::Theme::accent(),
        StepStatus::Running => crate::theme::Theme::success(),
        StepStatus::AwaitingOutcome | StepStatus::NeedsInput => crate::theme::Theme::warning(),
        StepStatus::Succeeded => crate::theme::Theme::success(),
        StepStatus::Failed | StepStatus::Blocked => crate::theme::Theme::danger(),
        StepStatus::Skipped | StepStatus::Cancelled => 0x8a8a8a,
    }
}

fn small_btn<F>(
    cx: &Context<AgentWorkspace>,
    id: impl Into<gpui::ElementId>,
    label: &str,
    color: u32,
    handler: F,
) -> impl IntoElement
where
    F: Fn(&mut AgentWorkspace, &gpui::ClickEvent, &mut Window, &mut Context<AgentWorkspace>)
        + 'static,
{
    div()
        .id(id)
        .px_2()
        .h(px(20.))
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(color))
        .text_size(px(9.5))
        .text_color(rgb(color))
        .cursor_pointer()
        .hover(move |d| d.bg(rgb(color)).text_color(rgb(crate::theme::Theme::bg())))
        .child(label.to_string())
        .on_click(cx.listener(handler))
}

fn terminal_key_bytes(ev: &gpui::KeyDownEvent) -> Option<Vec<u8>> {
    let k = &ev.keystroke;
    if let Some(ch) = &k.key_char {
        return Some(ch.as_bytes().to_vec());
    }
    // Ctrl+字母 → 控制字节(ctrl-w/c/a/u/l/k/e/d/z 等 CLI 常用)
    if k.modifiers.control && !k.modifiers.alt && !k.modifiers.shift {
        let key = k.key.as_str();
        if key.len() == 1 {
            let c = key.chars().next().unwrap().to_ascii_lowercase();
            if ('a'..='z').contains(&c) {
                return Some(vec![(c as u8) - b'a' + 1]);
            }
        }
        if key == "space" {
            return Some(vec![0]);
        }
    }
    let mods = &k.modifiers;
    let mod_code = if mods.control && mods.shift {
        "4"
    } else if mods.control && mods.alt {
        "5"
    } else if mods.control {
        "1"
    } else if mods.shift {
        "2"
    } else if mods.alt {
        "3"
    } else {
        "1"
    };
    let csi = |letter: char| -> Vec<u8> { format!("\x1b[1;{mod_code}{letter}").into_bytes() };
    Some(match k.key.as_str() {
        "enter" => b"\r".to_vec(),
        "backspace" => vec![0x7f],
        "tab" => b"\t".to_vec(),
        "escape" => vec![0x1b],
        "up" => csi('A'),
        "down" => csi('B'),
        "right" => csi('C'),
        "left" => csi('D'),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        _ => return None,
    })
}

// ---------- Render ----------

impl Render for AgentWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_focus {
            self.pending_focus = false;
            window.focus(&self.focus_handle, cx);
        }
        let tabs = div()
            .id("ws-tabs")
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(30.))
            .border_b_1()
            .border_color(rgb(crate::theme::Theme::border()))
            .child(tab_btn(cx, WorkspaceView::Agents, "Agents", self.view))
            .child(tab_btn(cx, WorkspaceView::Pipeline, "Pipeline", self.view))
            .child(tab_btn(cx, WorkspaceView::Instances, "实例", self.view))
            .child(tab_btn(cx, WorkspaceView::Workflow, "工作流", self.view));
        let body = match self.view {
            WorkspaceView::Agents => self.render_agents_view(cx, window),
            WorkspaceView::Pipeline => self.render_pipeline_view(cx, window),
            WorkspaceView::Instances => self.instances_page.clone().into_any_element(),
            WorkspaceView::Workflow => self.workflow_page.clone().into_any_element(),
        };
        div()
            .id("agent-workspace")
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .key_context("AgentWorkspace")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(
                |ws: &mut AgentWorkspace, ev: &gpui::KeyDownEvent, window, cx| {
                    let focused = ws.focus_handle.is_focused(window);
                    if ev.keystroke.key.as_str() == "escape" {
                        // 终端覆盖层且无输入焦点:Esc 是 TUI 程序按键,直达 PTY(关闭用 ✕)
                        if matches!(ws.overlay, Some(Overlay::Terminal { .. }))
                            && ws.active_field == Field::None
                        {
                            if let Some(Overlay::Terminal {
                                project,
                                session_id,
                                ..
                            }) = ws.overlay.clone()
                            {
                                let p = project.to_string_lossy().to_string();
                                let _ = ws.app.registry.send_prompt_raw(&p, session_id, &[0x1b]);
                            }
                            cx.stop_propagation();
                            return;
                        }
                        if ws.overlay.is_some() {
                            ws.overlay = None;
                            ws.active_field = Field::None;
                            cx.notify();
                            return;
                        }
                        ws.profile_popover = false;
                        ws.template_popover = false;
                        if ws.active_field != Field::None {
                            ws.active_field = Field::None;
                        }
                        cx.notify();
                        return;
                    }
                    let editing = |f: Field| ws.active_field == f && focused;
                    // 覆盖层 prompt 输入
                    if editing(Field::PromptInput) {
                        match ev.keystroke.key.as_str() {
                            "enter" => {
                                if !ws.overlay_input.trim().is_empty() {
                                    let rid = match &ws.overlay {
                                        Some(Overlay::Transcript {
                                            project, run_id, ..
                                        }) => Some((project.clone(), *run_id)),
                                        Some(Overlay::Terminal {
                                            project,
                                            run_id: Some(run_id),
                                            ..
                                        }) => Some((project.clone(), *run_id)),
                                        _ => None,
                                    };
                                    if let Some((project, rid)) = rid {
                                        let text = ws.overlay_input.clone();
                                        ws.send_prompt_to_run(&project, rid, &text, cx);
                                    }
                                    ws.overlay_input.clear();
                                }
                            }
                            "backspace" => {
                                ws.overlay_input.pop();
                            }
                            _ => {
                                if let Some(ch) = ev.keystroke.key_char.as_ref() {
                                    ws.overlay_input.push_str(ch);
                                }
                            }
                        }
                        cx.notify();
                        return;
                    }
                    // 过滤输入
                    if editing(Field::FilterText) {
                        match ev.keystroke.key.as_str() {
                            "backspace" => {
                                ws.filter_text.pop();
                            }
                            _ => {
                                if let Some(ch) = ev.keystroke.key_char.as_ref() {
                                    ws.filter_text.push_str(ch);
                                }
                            }
                        }
                        cx.notify();
                        return;
                    }
                    // 终端覆盖层:键盘直通(Esc/Ctrl 组合直达 PTY;关闭用 ✕)
                    if let Some(Overlay::Terminal {
                        project,
                        session_id,
                        ..
                    }) = ws.overlay.clone()
                    {
                        if focused && ws.active_field == Field::None {
                            if let Some(seq) = terminal_key_bytes(ev) {
                                let p = project.to_string_lossy().to_string();
                                let _ = ws.app.registry.send_prompt_raw(&p, session_id, &seq);
                            }
                            cx.stop_propagation();
                            return;
                        }
                    }
                    // Step 编辑字段
                    if let Some(key) = ws.selected_step.clone() {
                        let f = ws.active_field;
                        if (editing(Field::Title)
                            || editing(Field::Instructions)
                            || editing(Field::SessionKey))
                            && f != Field::None
                        {
                            match ev.keystroke.key.as_str() {
                                "enter" => {
                                    if f == Field::Instructions {
                                        ws.field_buffer.push('\n');
                                    } else {
                                        ws.active_field = Field::None;
                                    }
                                }
                                "backspace" => {
                                    ws.field_buffer.pop();
                                }
                                _ => {
                                    if let Some(ch) = ev.keystroke.key_char.as_ref() {
                                        ws.field_buffer.push_str(ch);
                                    }
                                }
                            }
                            if let Some(s) = ws.draft.iter_mut().find(|s| s.key == key) {
                                match f {
                                    Field::Title => s.title = ws.field_buffer.clone(),
                                    Field::Instructions => s.instructions = ws.field_buffer.clone(),
                                    Field::SessionKey => {
                                        let k = ws.field_buffer.trim().to_string();
                                        s.session_policy = if k.is_empty() {
                                            SessionPolicy::Fresh
                                        } else {
                                            SessionPolicy::Reuse { key: k }
                                        };
                                    }
                                    _ => {}
                                }
                            }
                            ws.dirty = true;
                            cx.notify();
                        }
                    }
                },
            ))
            .child(tabs)
            .child(div().flex_1().min_h_0().flex().child(body))
            .when(self.overlay.is_some(), |d| {
                d.child(self.render_overlay(cx, window))
            })
    }
}

fn tab_btn(
    cx: &Context<AgentWorkspace>,
    target: WorkspaceView,
    label: &str,
    current: WorkspaceView,
) -> impl IntoElement {
    let active = current == target;
    div()
        .id(ElementId::Name(format!("ws-tab-{label}").into()))
        .h(px(22.))
        .px_3()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(11.))
        .font_weight(if active {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        })
        .text_color(rgb(if active {
            crate::theme::Theme::fg()
        } else {
            crate::theme::Theme::fg_dim()
        }))
        .when(active, |d| d.bg(rgb(crate::theme::Theme::bg_active())))
        .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
        .child(label.to_string())
        .on_click(cx.listener(move |ws: &mut AgentWorkspace, _, _, cx| {
            ws.view = target;
            cx.notify();
        }))
}
