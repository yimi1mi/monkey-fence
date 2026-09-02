//! 运行级「需要你」投影与徽标(ADR 0004 / Task 7):
//! 唯一判定口径 `direct_attention_for_step` / `attention_for_task`;
//! blocked 后代不单独计数、跨项目按运行数计数、focus 优先级稳定排序、
/// 处理完唯一直接原因后徽标清零、interrupted 恢复可见、直达定位。
use crate::project_overview::{
    attention_for_task as project_attention_for_task, direct_attention_for_step,
    DirectAttentionReason, WorkflowRunAttention,
};
use mf_agent::model::{RunStatus, RunView, StepStatus, TaskStatus, TaskView};
use mf_agent::store::Store;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn step(id: i64, key: &str, status: StepStatus) -> mf_agent::StepView {
    mf_agent::StepView {
        id,
        public_handle: format!("step-{id}"),
        revision: 1,
        revision_id: 1,
        task_id: 1,
        step_key: key.into(),
        title: key.into(),
        instructions: String::new(),
        agent_profile: "p".into(),
        session_policy: "fresh".into(),
        status,
        attempts: 1,
        auto_retry: 0,
        result: None,
        started_at: None,
        ended_at: None,
        deps: vec![],
    }
}

fn run(id: i64, step_id: i64, status: RunStatus) -> RunView {
    RunView {
        id,
        public_handle: format!("run-{id}"),
        revision: 1,
        task_id: 1,
        step_id,
        revision_id: 1,
        session_id: None,
        status,
        agent_state: mf_agent::AgentState::Idle,
        capability_token: String::new(),
        outcome: None,
        outcome_payload: None,
        started_at: String::new(),
        ended_at: None,
    }
}

fn task(status: TaskStatus) -> TaskView {
    TaskView {
        id: 1,
        public_handle: "run-1".into(),
        revision: 1,
        title: "运行".into(),
        goal: String::new(),
        status,
        paused: false,
        unread: false,
        active_revision: Some(1),
        revision_count: 1,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn attention_for_task(
    task: &TaskView,
    steps: &[mf_agent::StepView],
    latest: &HashMap<i64, RunView>,
    has_merge_conflict: bool,
    open_questions: usize,
) -> Option<WorkflowRunAttention> {
    let merge_steps: HashSet<i64> = if has_merge_conflict {
        steps
            .iter()
            .filter(|step| step.status == StepStatus::Blocked)
            .take(1)
            .map(|step| step.id)
            .collect()
    } else {
        HashSet::new()
    };
    project_attention_for_task(
        task,
        steps,
        latest,
        &HashSet::new(),
        &merge_steps,
        has_merge_conflict && merge_steps.is_empty(),
        open_questions,
    )
}

/// 同一运行:1 个失败根因 + 3 个 blocked 后代 → 只计一次
/// (blocked 后代不是直接可操作原因)。
#[test]
fn blocked_descendants_do_not_add_attention_count() {
    let steps = vec![
        step(1, "build", StepStatus::Failed),
        step(2, "test-a", StepStatus::Blocked),
        step(3, "test-b", StepStatus::Blocked),
        step(4, "report", StepStatus::Blocked),
    ];
    let mut latest = HashMap::new();
    latest.insert(1i64, run(10, 1, RunStatus::Failed));
    let attention = attention_for_task(&task(TaskStatus::NeedsYou), &steps, &latest, false, 0)
        .expect("失败根因必须产生徽标");
    assert_eq!(attention.reason_count, 1, "blocked 后代不得单独计数");
    assert_eq!(attention.focus_step_id, Some(1));
    // 徽标口径:一个运行最多一项 → 项目徽标 +1(由 len 决定)
    assert_eq!(vec![attention].len(), 1, "一个运行只贡献一个徽标计数");
}

/// 两个项目各一个 Needs You 运行 → 徽标为 2(运行数,非节点数)。
#[test]
fn two_projects_each_needs_you_badge_counts_runs() {
    let make = |project: &str| {
        attention_for_task(
            &task(TaskStatus::NeedsYou),
            &[step(1, "a", StepStatus::NeedsInput)],
            &HashMap::new(),
            false,
            0,
        )
        .map(|mut a: WorkflowRunAttention| {
            a.project_root = PathBuf::from(project);
            a
        })
    };
    let runs: Vec<_> = ["D:/proj-a", "D:/proj-b"]
        .iter()
        .filter_map(|p| make(p))
        .collect();
    assert_eq!(runs.len(), 2, "两个项目各计一次(按运行数)");
}

/// 处理完唯一直接原因后徽标变 0(重算即消失,无手工减计数)。
#[test]
fn resolving_only_direct_reason_clears_attention() {
    let steps = vec![step(1, "a", StepStatus::AwaitingOutcome)];
    let attention = attention_for_task(
        &task(TaskStatus::Running),
        &steps,
        &HashMap::new(),
        false,
        0,
    );
    assert!(attention.is_some(), "待结算必须需要你");
    // 人工确认成功后:重算 → None
    let resolved = vec![step(1, "a", StepStatus::Succeeded)];
    assert!(
        attention_for_task(
            &task(TaskStatus::Succeeded),
            &resolved,
            &HashMap::new(),
            false,
            0
        )
        .is_none(),
        "唯一直接原因处理后徽标必须清零"
    );
}

/// focus_step_id 优先级(等待输入 → 待结算 → 冲突 → 失败/中断)
/// 与同优先级 Step ID 稳定排序。
#[test]
fn focus_step_priority_and_stable_ordering() {
    // needs-input(0)优先于 awaiting-outcome(1)与 failed(3)
    let steps = vec![
        step(5, "fail-late", StepStatus::Failed),
        step(3, "await-mid", StepStatus::AwaitingOutcome),
        step(7, "input-high", StepStatus::NeedsInput),
    ];
    let attention = attention_for_task(
        &task(TaskStatus::NeedsYou),
        &steps,
        &HashMap::new(),
        false,
        0,
    )
    .unwrap();
    assert_eq!(attention.focus_step_id, Some(7), "等待输入优先");
    assert_eq!(attention.reason_count, 3);

    // 同优先级:按 Step ID 稳定排序取最小
    let steps = vec![
        step(9, "await-b", StepStatus::AwaitingOutcome),
        step(4, "await-a", StepStatus::AwaitingOutcome),
    ];
    let attention = attention_for_task(
        &task(TaskStatus::NeedsYou),
        &steps,
        &HashMap::new(),
        false,
        0,
    )
    .unwrap();
    assert_eq!(
        attention.focus_step_id,
        Some(4),
        "同优先级按 Step ID 稳定排序"
    );

    // 合并冲突(2)介于待结算(1)与失败(3)之间
    let steps = vec![
        step(2, "join", StepStatus::Blocked),
        step(6, "fail", StepStatus::Failed),
    ];
    let attention = attention_for_task(
        &task(TaskStatus::NeedsYou),
        &steps,
        &HashMap::new(),
        true,
        0,
    )
    .unwrap();
    assert_eq!(attention.focus_step_id, Some(2), "冲突优先于失败节点");

    // 运行级原因(interrupted)从 latest_run 推导
    let attention = direct_attention_for_step(
        &step(8, "r", StepStatus::Running),
        Some(&run(20, 8, RunStatus::Interrupted)),
        false,
        false,
    )
    .unwrap();
    assert_eq!(attention.reason, DirectAttentionReason::Interrupted);
    // 进程退出未结算(会话死/Run 未结算)
    let attention = direct_attention_for_step(
        &step(8, "r", StepStatus::Running),
        Some(&run(21, 8, RunStatus::AwaitingOutcome)),
        false,
        false,
    )
    .unwrap();
    assert_eq!(attention.reason, DirectAttentionReason::AwaitingOutcome);

    // Run 仍标 running，但会话已死亡/丢失时也必须进入待结算提醒。
    let attention = direct_attention_for_step(
        &step(10, "dead", StepStatus::Running),
        Some(&run(22, 10, RunStatus::Running)),
        true,
        false,
    )
    .unwrap();
    assert_eq!(attention.reason, DirectAttentionReason::AwaitingOutcome);
}

#[test]
fn merge_conflict_marks_only_the_owning_step() {
    let steps = vec![
        step(2, "blocked-descendant", StepStatus::Blocked),
        step(9, "join", StepStatus::Succeeded),
    ];
    let merge_steps = HashSet::from([9]);
    let attention = project_attention_for_task(
        &task(TaskStatus::NeedsYou),
        &steps,
        &HashMap::new(),
        &HashSet::new(),
        &merge_steps,
        false,
        0,
    )
    .unwrap();
    assert_eq!(attention.reason_count, 1);
    assert_eq!(attention.focus_step_id, Some(9));
}

/// 不计数的运行:已取消/已归档;普通 Draft Task(无 Revision)不进入列表。
#[test]
fn cancelled_archived_and_revisionless_runs_are_excluded() {
    let steps = vec![step(1, "a", StepStatus::Failed)];
    assert!(
        attention_for_task(
            &task(TaskStatus::Cancelled),
            &steps,
            &HashMap::new(),
            false,
            0
        )
        .is_none(),
        "已取消运行不计数"
    );
    assert!(
        attention_for_task(
            &task(TaskStatus::Archived),
            &steps,
            &HashMap::new(),
            false,
            0
        )
        .is_none(),
        "已归档运行不计数"
    );
    let mut draft = task(TaskStatus::Draft);
    draft.revision_count = 0;
    draft.active_revision = None;
    assert!(
        attention_for_task(&draft, &steps, &HashMap::new(), false, 0).is_none(),
        "无 Pipeline Revision 的普通 Draft Task 不是工作流运行"
    );
}

/// 重启恢复:interrupted 运行(Store 持久化)重新出现在徽标中。
#[test]
fn interrupted_run_recovers_into_attention_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("workflow-v1.db");
    let task_id = {
        let store = Store::open(&db).unwrap();
        let t = store.create_task("恢复", "g").unwrap();
        // 手工把步骤与 run 置成 interrupted(模拟崩溃恢复后的持久化状态)
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO pipeline_revisions (task_id, revision, status, snapshot_json, created_at)
                     VALUES (?1, 1, 'active', '{}', 't')",
                    (t.id,),
                )?;
                let rev_id = c.last_insert_rowid();
                c.execute(
                    "INSERT INTO steps (revision_id, task_id, step_key, title, agent_profile,
                         status, attempts, created_at, updated_at)
                     VALUES (?1, ?2, 'a', 'A', 'p', 'running', 1, 't', 't')",
                    (rev_id, t.id),
                )?;
                let step_id = c.last_insert_rowid();
                c.execute(
                    "INSERT INTO agent_runs (task_id, step_id, revision_id, status,
                         capability_token, agent_state, started_at)
                     VALUES (?1, ?2, ?3, 'interrupted', 'tok-r', 'working', 't')",
                    (t.id, step_id, rev_id),
                )?;
                Ok(())
            })
            .unwrap();
        t.id
    };
    // 「重启」:重新打开同一数据库
    let store = Store::open(&db).unwrap();
    let t = store.task_view(task_id).unwrap().unwrap();
    let steps = store.task_steps(task_id).unwrap();
    let runs = store.list_runs_of_task(task_id).unwrap();
    let latest: HashMap<i64, RunView> = runs.into_iter().fold(HashMap::new(), |mut acc, r| {
        let e = acc.entry(r.step_id).or_insert(r.clone());
        if r.id > e.id {
            *e = r;
        }
        acc
    });
    let attention = attention_for_task(&t, &steps, &latest, false, 0)
        .expect("interrupted 运行重启后必须重新出现在徽标中");
    assert_eq!(attention.focus_step_id, steps.first().map(|s| s.id));
}

/// 跨项目直达:AgentWorkspace 打开 attention → Runs 页选中该运行
/// 并定位 RunMonitor 的优先处理节点(原子激活由 ActivationTarget::Task 完成)。
#[gpui::test]
fn clicking_cross_project_attention_locates_run_and_focus_step(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let project = tempfile::tempdir().unwrap();
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    // 构造一次带 NeedsInput 步骤的运行(经真实 Store)
    let t = orch.create_task("跨项目", "g").unwrap();
    orch.store
        .with_conn(|c| {
            c.execute(
                "INSERT INTO pipeline_revisions (task_id, revision, status, snapshot_json, created_at)
                 VALUES (?1, 1, 'active', '{}', 't')",
                (t.id,),
            )?;
            let rev_id = c.last_insert_rowid();
            c.execute(
                "INSERT INTO steps (revision_id, task_id, step_key, title, agent_profile,
                     status, attempts, created_at, updated_at)
                 VALUES (?1, ?2, 'a', 'A', 'p', 'needs-input', 1, 't', 't')",
                (rev_id, t.id),
            )?;
            Ok(())
        })
        .unwrap();
    orch.store
        .set_task_status(t.id, TaskStatus::NeedsYou)
        .unwrap();
    let steps = orch.store.task_steps(t.id).unwrap();
    let mut attention = attention_for_task(
        &orch.store.task_view(t.id).unwrap().unwrap(),
        &steps,
        &HashMap::new(),
        false,
        0,
    )
    .unwrap();
    attention.project_root = project.path().to_path_buf();
    assert_eq!(attention.focus_step_id, Some(steps[0].id));

    // AgentWorkspace 直达:Runs 页 + 选中运行 + focus step
    let ws = cx.new(|cx| crate::agent_workspace::AgentWorkspace::new(ctx.clone(), cx));
    cx.update_entity(&ws, |aw, cx| {
        aw.open_attention_run(&attention, cx);
        assert_eq!(
            aw.active_tab(),
            crate::workspace::AgentTab::Runs,
            "直达必须进入 Runs"
        );
        let selected = aw.runs_page.read(cx).selected.clone();
        let focused = aw.runs_page.read(cx).monitor.read(cx).focused_step();
        assert_eq!(
            selected,
            Some((project.path().to_path_buf(), t.id)),
            "跨项目运行被选中"
        );
        assert_eq!(focused, Some(steps[0].id), "RunMonitor 定位到优先处理节点");
    });
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());
}

/// 徽标计数进入 AgentWorkspace(Runs 页签同一计数)。
#[gpui::test]
fn attention_count_flows_to_workspace_tabs(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    let ctx = crate::app_ctx::AppCtx::with_catalog_for_tests(catalog);
    let ws = cx.new(|cx| crate::agent_workspace::AgentWorkspace::new(ctx.clone(), cx));
    let attention = WorkflowRunAttention {
        project_root: PathBuf::from("D:/p"),
        task_id: 3,
        task_title: "运行".into(),
        reason_count: 2,
        focus_step_id: Some(11),
    };
    let make_snapshot = |count: usize, attention: Vec<WorkflowRunAttention>| {
        crate::project_overview::ProjectOverviewSnapshot {
            revision: 1,
            projects: vec![],
            agent_cards: vec![],
            global_active_runs: 0,
            templates: vec![],
            attention_run_count: count,
            attention_runs: attention,
        }
    };
    let attention = WorkflowRunAttention {
        project_root: PathBuf::from("D:/p"),
        task_id: 3,
        task_title: "运行".into(),
        reason_count: 2,
        focus_step_id: Some(11),
    };
    cx.update_entity(&ws, |aw, cx| {
        aw.set_overview(
            std::sync::Arc::new(make_snapshot(1, vec![attention.clone()])),
            cx,
        );
        assert_eq!(aw.attention_run_count, 1);
        aw.enter_from_activity(cx);
        assert_eq!(aw.active_tab(), crate::workspace::AgentTab::Runs);
    });
    // 处理完唯一直接原因:新快照计数 0 → 徽标消失
    cx.update_entity(&ws, |aw, cx| {
        aw.set_overview(std::sync::Arc::new(make_snapshot(0, vec![])), cx);
        assert_eq!(aw.attention_run_count, 0, "统一快照口径清零徽标");
        let _ = &attention;
    });
}

// ---------- Issue #26:「需要你」事实迁到 Core Kernel 摘要 ----------

fn kernel_summary(
    status: &str,
    reason_count: usize,
    focus: Option<&str>,
) -> mf_kernel::projection::WorkflowRunSummarySnapshot {
    mf_kernel::projection::WorkflowRunSummarySnapshot {
        workflow_run: mf_kernel::handles::WorkflowRunHandle::parse(
            "018f1e61-a197-7b4d-8f4e-6f4e6f4e6f41",
        )
        .unwrap(),
        revision: mf_kernel::projection::ScalarRevision { revision: 3 },
        title: "运行".into(),
        status: status.into(),
        paused: false,
        unread: false,
        needs_you: status == "needs-you",
        reason_count,
        focus_step: focus.map(|handle| mf_kernel::handles::StepHandle::parse(handle).unwrap()),
        active_agent_runs: 0,
    }
}

/// Kernel 摘要 → 「需要你」:reason_count>0 即命中;一个运行最多一项;
/// focus_step 由 Kernel 给出优先处理节点 handle(UI 侧 join 回 rowid)。
#[test]
fn kernel_summary_maps_to_single_attention_per_run() {
    let root = PathBuf::from("D:/p");
    let summary = kernel_summary("running", 2, Some("018f1e61-a197-7b4d-8f4e-6f4e6f4e6f42"));
    let attention = crate::project_overview::attention_from_summary(&root, &summary, 7, Some(11))
        .expect("Kernel 直接原因必须产生徽标");
    assert_eq!(attention.task_id, 7);
    assert_eq!(attention.reason_count, 2);
    assert_eq!(attention.focus_step_id, Some(11));
    assert_eq!(attention.task_title, "运行");

    // 无直接原因 → 无徽标(即使 Task 状态机仍在 needs-you 也不凭状态计数)
    assert!(
        crate::project_overview::attention_from_summary(
            &root,
            &kernel_summary("needs-you", 0, None),
            7,
            None
        )
        .is_none(),
        "Kernel reason_count 为 0 不得产生徽标"
    );
}

/// 已取消/已归档运行不产生徽标(Kernel 会为终态运行计算历史原因数,
/// UI 必须按运行状态过滤)。
#[test]
fn kernel_attention_excludes_cancelled_and_archived_runs() {
    let root = PathBuf::from("D:/p");
    for status in ["cancelled", "archived"] {
        assert!(
            crate::project_overview::attention_from_summary(
                &root,
                &kernel_summary(status, 3, None),
                7,
                None
            )
            .is_none(),
            "{status} 运行不得产生徽标"
        );
    }
}

/// Kernel 摘要驱动的跨项目徽标:两个运行(不同项目)各一项,
/// 徽标计数 = 运行数(不按被阻塞节点数)。
#[test]
fn kernel_attention_badge_counts_runs_across_projects() {
    let roots = [PathBuf::from("D:/a"), PathBuf::from("D:/b")];
    let attentions: Vec<_> = roots
        .iter()
        .enumerate()
        .filter_map(|(index, root)| {
            crate::project_overview::attention_from_summary(
                root,
                &kernel_summary("needs-you", 1, None),
                index as i64,
                None,
            )
        })
        .collect();
    assert_eq!(attentions.len(), 2, "徽标按运行数计数");
    assert_eq!(
        attentions.iter().map(|a| a.task_id).collect::<Vec<_>>(),
        vec![0, 1]
    );
}
