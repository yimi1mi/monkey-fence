//! T0b 生命周期基线(Issue #13):用确定性 fake RuntimeHost 跑通
//! Project Workflow「创建 → 编辑 → 冻结 Pipeline Revision → 运行 →
//! 重试 / 取消 / respond / Settlement / Handoff → 重启恢复」的完整
//! headless 生命周期,并把每个用户动作后的 Store 终态 + 可见状态
//! 投影为规范化 golden(`fixtures/baseline/expected/workflow-*.json`)。
//!
//! - 投影只经公共读取 API,显式挑选稳定业务字段(step_key、状态、
//!   尝试次数、结算结果、Handoff 内容),不整体序列化 DTO:
//!   时间戳、随机能力令牌、自增 rowid、`raw_log_ref` 全部不进入 golden,
//!   T1/T2 迁移增加字段后基线可原样复跑;
//! - 进程退出 / `done` / 终端空闲绝不自动 Settlement(进入 Needs You);
//! - 重启后 Needs You、Settlement、Handoff 均可恢复;
//! - 正常、取消、失败、询问、人工结算路径共用同一 harness。
//!
//! golden 重生成:
//! `MF_REGEN_BASELINE=1 cargo test -p mf-agent --test baseline_workflow_lifecycle
//!  regenerate_workflow_goldens -- --ignored --exact --nocapture`

#[allow(dead_code)]
#[path = "common/baseline.rs"]
mod baseline;
#[allow(dead_code)]
mod common;

use common::*;
use mf_agent::model::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, WorkflowKernel};
use mf_agent::runtime::RuntimeEvent;
use mf_agent::store::Store;
use mf_agent::workflow::{
    ProjectWorkflowDraft, ProjectWorkflowRecord, WorkflowNodeDraft, WorkflowTemplateVersion,
};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// harness:同一套 World 承载全部生命周期场景
// ---------------------------------------------------------------------------

struct ShutdownGuard {
    orch: Arc<Orchestrator>,
}

impl ShutdownGuard {
    fn new(orch: Arc<Orchestrator>) -> ShutdownGuard {
        ShutdownGuard { orch }
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.orch.stop();
        assert!(
            wait_until(Duration::from_secs(5), || Arc::strong_count(&self.orch)
                <= 2),
            "Orchestrator 后台线程未在时限内退出"
        );
    }
}

struct World {
    // guard 必须先于 orch / TempDir 销毁:先停止并等待两个后台线程,
    // 再关闭 Store,最后删除 Windows 上的 SQLite 临时目录。
    shutdown: ShutdownGuard,
    root: PathBuf,
    catalog: Arc<mf_agent::catalog_store::CatalogStore>,
    pins: Arc<FakePins>,
    directory: Arc<ScriptedDirectory>,
    host: Arc<RecordingHost>,
    orch: Arc<Orchestrator>,
    instance_id: String,
    _tmp: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let fx = fixture(tmp.path());
        fx.pins.resolve_ok(true);
        let root = fx.orch.root.clone();
        let orch = fx.orch;
        World {
            shutdown: ShutdownGuard::new(orch.clone()),
            root,
            catalog: fx.catalog,
            pins: fx.pins,
            directory: fx.directory,
            host: fx.host,
            orch,
            instance_id: fx.instance_id,
            _tmp: tmp,
        }
    }

    /// 进程重启:同一批持久 Store/catalog/pin 状态 + 全新宿主
    /// (宿主不再持有任何存活会话,is_session_alive 恒 false)。
    fn restart(self) -> World {
        let World {
            shutdown,
            root,
            catalog,
            pins,
            directory,
            host: _,
            orch,
            instance_id,
            _tmp,
        } = self;
        drop(shutdown);
        drop(orch);
        let store = Store::open(&root.join("workflow-v1.db")).unwrap();
        let host = Arc::new(RecordingHost::default());
        let orch = Orchestrator::start_with_routing(
            store,
            root.clone(),
            mf_agent::config::Config::default(),
            host.clone(),
            empty_profiles(),
            GlobalLimiter::new(4),
            "pipe".into(),
            directory.clone(),
            WorkflowKernel {
                catalog: catalog.clone(),
                pins: Some(pins.clone()),
                instance_resolver: None,
            },
            scripted_routing(),
        )
        .unwrap();
        World {
            shutdown: ShutdownGuard::new(orch.clone()),
            root,
            catalog,
            pins,
            directory,
            host,
            orch,
            instance_id,
            _tmp,
        }
    }

    fn shutdown(self) {
        drop(self);
    }

    // ---- 用户动作(全部走生产公共 API) ----

    fn save_workflow(
        &self,
        key: &str,
        name: &str,
        nodes: Vec<(String, String, String, Vec<String>)>,
    ) -> ProjectWorkflowRecord {
        let nodes = nodes
            .into_iter()
            .map(|(key, title, instructions, deps)| WorkflowNodeDraft {
                key,
                title,
                instructions,
                agent_instance_id: self.instance_id.clone(),
                deps,
            })
            .collect();
        self.orch
            .store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: key.into(),
                name: name.into(),
                nodes,
                allow_unsafe_parallel: false,
            })
            .unwrap()
    }

    /// ProjectWorkflowRecord → 临时模板版本 → 冻结 Pipeline Revision → 运行
    /// (与 mf 层 run_project_workflow 同构的投影路径)。
    fn run_workflow(&self, record: &ProjectWorkflowRecord, title: &str, goal: &str) -> i64 {
        let version = WorkflowTemplateVersion {
            version_id: 0,
            template_key: format!("project-workflow/{}", record.key),
            version: 1,
            nodes: record.nodes.clone(),
            created_at: String::new(),
        };
        let task = self.orch.create_task(title, goal).unwrap();
        self.orch
            .assign_workflow(task.id, &version, &plugin_index(), false)
            .unwrap();
        self.orch.confirm_and_run(task.id).unwrap();
        task.id
    }

    fn settle(&self, task_id: i64, key: &str, settlement: Settlement) -> SettleOutcome {
        let token = token_of_node(&self.orch, task_id, key);
        self.orch.settle_by_token(&token, settlement).unwrap()
    }

    fn emit(&self, task_id: i64, key: &str, ev: RuntimeEvent) {
        let run = self.latest_run(task_id, key);
        self.orch.push_runtime_event(run.id, ev);
    }

    fn respond(&self, task_id: i64, answer: &str) {
        let question = self
            .orch
            .store
            .open_questions(Some(task_id))
            .unwrap()
            .into_iter()
            .next()
            .expect("必须存在待回答问题");
        self.orch.answer_question(question.id, answer).unwrap();
    }

    fn retry_fresh(&self, task_id: i64, key: &str) -> StepView {
        let step_id = self.step(task_id, key).id;
        self.orch
            .retry_step(step_id, RetryMode::FreshSession)
            .unwrap()
    }

    // ---- 等待 / 读取 ----

    fn steps(&self, task_id: i64) -> Vec<StepView> {
        self.orch.store.task_steps(task_id).unwrap()
    }

    fn step(&self, task_id: i64, key: &str) -> StepView {
        self.steps(task_id)
            .into_iter()
            .find(|s| s.step_key == key)
            .unwrap()
    }

    fn latest_run(&self, task_id: i64, key: &str) -> RunView {
        let step = self.step(task_id, key);
        self.orch
            .store
            .list_runs_of_step(step.id)
            .unwrap()
            .into_iter()
            .rev()
            .next()
            .unwrap()
    }

    fn wait_launched(&self, count: usize) {
        assert!(
            wait_until(Duration::from_secs(5), || self.host.workflow.lock().len()
                == count),
            "等待 {count} 个节点派发超时"
        );
    }

    fn wait_run_status(&self, task_id: i64, key: &str, status: RunStatus) {
        assert!(
            wait_until(Duration::from_secs(5), || {
                self.orch
                    .store
                    .run_view(self.latest_run(task_id, key).id)
                    .unwrap()
                    .map(|r| r.status == status)
                    .unwrap_or(false)
            }),
            "等待 {key} 运行进入 {status} 超时"
        );
    }

    fn wait_step_status(&self, task_id: i64, key: &str, status: StepStatus) {
        assert!(
            wait_until(Duration::from_secs(5), || self.step(task_id, key).status
                == status),
            "等待 {key} 进入 {status:?} 超时"
        );
    }

    fn wait_task_status(&self, task_id: i64, status: TaskStatus) {
        assert!(
            wait_until(Duration::from_secs(5), || {
                self.orch
                    .store
                    .task_view(task_id)
                    .unwrap()
                    .map(|t| t.status == status)
                    .unwrap_or(false)
            }),
            "等待任务进入 {status:?} 超时"
        );
    }

    fn wait_task_unread(&self, task_id: i64) {
        assert!(
            wait_until(Duration::from_secs(5), || {
                self.orch
                    .store
                    .task_view(task_id)
                    .unwrap()
                    .map(|task| task.unread)
                    .unwrap_or(false)
            }),
            "等待任务未读标记超时"
        );
    }

    fn instance_name(&self, id: &str) -> String {
        self.catalog
            .list_agent_instances(None)
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .map(|i| i.name)
            .unwrap_or_else(|| id.to_string())
    }
}

// ---------------------------------------------------------------------------
// 规范化投影:只保留稳定业务字段
// ---------------------------------------------------------------------------

fn project(w: &World, wf_key: &str, task_id: Option<i64>) -> Value {
    let store = &w.orch.store;

    // 项目工作流(编辑面)
    let workflows: Vec<Value> = store
        .list_project_workflows()
        .unwrap()
        .iter()
        .filter(|wf| wf.key == wf_key)
        .map(|wf| {
            json!({
                "key": wf.key,
                "name": wf.name,
                "nodes": wf.nodes.iter().map(|n| json!({
                    "key": n.key,
                    "title": n.title,
                    "instructions": n.instructions,
                    "instance": w.instance_name(&n.agent_instance_id),
                    "deps": n.deps,
                })).collect::<Vec<_>>(),
                "allow_unsafe_parallel": wf.allow_unsafe_parallel,
            })
        })
        .collect();

    let mut out = json!({ "workflow": workflows });
    let task_id = match task_id {
        Some(id) => id,
        None => return out,
    };
    let Some(task) = store.task_view(task_id).unwrap() else {
        return out;
    };

    let steps = store.task_steps(task_id).unwrap();
    let key_of = |id: i64| {
        steps
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.step_key.clone())
            .unwrap_or_default()
    };

    // 所有冻结 Pipeline Revision(包括取消后不再 active 的历史行)。
    let revisions: Vec<Value> = store
        .revision_statuses(task_id)
        .unwrap()
        .into_iter()
        .enumerate()
        .map(|(index, (id, status))| {
            let snapshot = store.revision_snapshot(id).unwrap();
            json!({
                "revision": index + 1,
                "status": status,
                "template_key": snapshot.as_ref().map(|s| s.template_key.clone()),
                "directory_provider": snapshot.as_ref().and_then(|s| s.directory_provider.as_ref()).map(|pin| json!({
                    "full_id": pin.full_id,
                    "version": pin.version,
                    "content_hash": pin.content_hash,
                })),
                "nodes": snapshot
                    .as_ref()
                    .map(|s| s.nodes.iter().map(|n| json!({
                        "key": n.key,
                        "title": n.title,
                        "instructions": n.instructions,
                        "instance": n.instance.name,
                        "instance_version": n.instance.version,
                        "plugin": n.plugin.as_ref().map(|pin| json!({
                            "full_id": pin.full_id,
                            "version": pin.version,
                            "content_hash": pin.content_hash,
                        })),
                        "deps": n.deps,
                    })).collect::<Vec<_>>())
                    .unwrap_or_default(),
            })
        })
        .collect();

    // Steps / Agent Runs / Handoff / 租约 / 询问
    let step_projection: Vec<Value> = steps
        .iter()
        .map(|s| {
            json!({
                "step_key": s.step_key,
                "status": s.status.as_str(),
                "attempts": s.attempts,
                "auto_retry": s.auto_retry,
                "result": s.result,
                "deps": s.deps.iter().map(|d| key_of(*d)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let run_rows: Vec<RunView> = steps
        .iter()
        .flat_map(|s| store.list_runs_of_step(s.id).unwrap())
        .collect();
    let mut session_ids: Vec<i64> = run_rows.iter().filter_map(|run| run.session_id).collect();
    session_ids.sort_unstable();
    session_ids.dedup();
    let runs: Vec<Value> = run_rows
        .iter()
        .map(|r| {
            json!({
                "step_key": key_of(r.step_id),
                "session": r.session_id.and_then(|id| session_ids.iter().position(|candidate| *candidate == id)).map(|index| index + 1),
                "status": r.status.as_str(),
                "agent_state": r.agent_state.as_str(),
                "outcome": r.outcome,
                "outcome_payload": r.outcome_payload,
            })
        })
        .collect();
    let handoffs: Vec<Value> = store
        .list_handoff_rows(task_id)
        .unwrap()
        .into_iter()
        .map(|row| {
            json!({
                "step_key": row.step_id.map(|id| key_of(id)),
                "status": row.handoff.status,
                "summary": row.handoff.summary,
                "output": row.handoff.output,
            })
        })
        .collect();
    let leases: Vec<Value> = store
        .list_execution_leases(task_id)
        .unwrap()
        .into_iter()
        .map(|l| {
            json!({
                "step_key": key_of(l.step_id),
                "status": l.status,
            })
        })
        .collect();
    let answers: Vec<Value> = w
        .host
        .answers
        .lock()
        .iter()
        .map(|(run_handle, answer)| {
            let step_key = store
                .run_view_by_handle(run_handle)
                .unwrap()
                .map(|run| key_of(run.step_id))
                .unwrap_or_default();
            json!({
                "step_key": step_key,
                "answer": answer,
            })
        })
        .collect();

    // 可见状态:运行监控面(Needs You 过滤器)能看到的权威投影
    let open_questions: Vec<String> = store
        .open_questions(Some(task_id))
        .unwrap()
        .into_iter()
        .map(|q| q.question)
        .collect();
    let visible = json!({
        "task_status": task.status.as_str(),
        "needs_you": task.status == TaskStatus::NeedsYou,
        "steps": steps
            .iter()
            .map(|s| json!({ "step_key": s.step_key, "status": s.status.as_str() }))
            .collect::<Vec<_>>(),
        "open_questions": open_questions,
        "awaiting_outcome_steps": steps
            .iter()
            .filter(|s| s.status == StepStatus::AwaitingOutcome)
            .map(|s| s.step_key.clone())
            .collect::<Vec<_>>(),
    });

    out["task"] = json!({
        "title": task.title,
        "goal": task.goal,
        "status": task.status.as_str(),
        "paused": task.paused,
        "unread": task.unread,
    });
    out["revisions"] = revisions.into();
    out["steps"] = step_projection.into();
    out["runs"] = runs.into();
    out["handoffs"] = handoffs.into();
    out["leases"] = leases.into();
    out["answers"] = answers.into();
    out["visible"] = visible;
    out
}

fn scenario_json(name: &str, actions: Vec<(&str, Value)>) -> Value {
    json!({
        "scenario": name,
        "actions": actions
            .into_iter()
            .map(|(label, state)| json!({ "action": label, "state": state }))
            .collect::<Vec<_>>(),
    })
}

fn canonical(v: &Value) -> String {
    baseline::canonical_json(v).unwrap()
}

fn baseline_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("baseline")
}

fn expected_dir() -> PathBuf {
    baseline_dir().join("expected")
}

// ---------------------------------------------------------------------------
// 场景:同一 harness 的五条生命周期路径 + 两条重启恢复路径
// ---------------------------------------------------------------------------

/// 正常路径:创建 → 编辑(加节点)→ 运行 → 冻结 Revision →
/// 逐节点 Settlement(带结构化输出)→ 下游解锁 → Handoff 落库 → 成功。
fn scenario_happy_path() -> Value {
    let w = World::new();
    let mut actions: Vec<(&str, Value)> = Vec::new();

    w.save_workflow(
        "release-check",
        "发布检查",
        vec![(
            "probe".into(),
            "探测".into(),
            "固定指令:探测环境".into(),
            vec![],
        )],
    );
    actions.push(("create_workflow", project(&w, "release-check", None)));

    let record = w.save_workflow(
        "release-check",
        "发布检查",
        vec![
            (
                "probe".into(),
                "探测".into(),
                "固定指令:探测环境".into(),
                vec![],
            ),
            (
                "build".into(),
                "构建".into(),
                "固定指令:构建产物".into(),
                vec!["probe".into()],
            ),
        ],
    );
    actions.push(("edit_workflow_add_node", project(&w, "release-check", None)));

    let task_id = w.run_workflow(&record, "发布前检查", "完整目标:发布");
    w.wait_launched(1);
    actions.push(("run_workflow", project(&w, "release-check", Some(task_id))));

    // 运行后继续修改 Project Workflow;已冻结 Revision 仍必须保留
    // 旧 title/instructions/节点集合,不跟随可编辑定义漂移。
    w.save_workflow(
        "release-check",
        "发布检查 v2",
        vec![
            (
                "probe".into(),
                "探测 v2".into(),
                "新指令:探测环境".into(),
                vec![],
            ),
            (
                "build".into(),
                "构建 v2".into(),
                "新指令:构建产物".into(),
                vec!["probe".into()],
            ),
            (
                "audit".into(),
                "审计".into(),
                "新指令:审计".into(),
                vec!["build".into()],
            ),
        ],
    );
    actions.push((
        "edit_workflow_after_freeze",
        project(&w, "release-check", Some(task_id)),
    ));

    w.settle(
        task_id,
        "probe",
        Settlement::complete_with_output(
            "探测完成",
            serde_json::json!({ "env": "baseline", "report_path": "artifacts/probe.json" }),
        ),
    );
    w.wait_launched(2);
    w.wait_step_status(task_id, "build", StepStatus::Running);
    actions.push((
        "settle_probe_complete",
        project(&w, "release-check", Some(task_id)),
    ));

    w.settle(task_id, "build", Settlement::complete("构建完成"));
    w.wait_step_status(task_id, "build", StepStatus::Succeeded);
    actions.push((
        "settle_build_complete",
        project(&w, "release-check", Some(task_id)),
    ));

    w.shutdown();
    scenario_json("happy-path", actions)
}

/// 失败与重试:结算失败 → 下游阻塞 + Needs You → FreshSession 重试 →
/// 成功结算解锁下游 → 汇合成功。
fn scenario_failure_retry() -> Value {
    let w = World::new();
    let mut actions: Vec<(&str, Value)> = Vec::new();

    let record = w.save_workflow(
        "deploy",
        "部署",
        vec![
            (
                "build".into(),
                "构建".into(),
                "固定指令:构建".into(),
                vec![],
            ),
            (
                "deploy".into(),
                "部署".into(),
                "固定指令:部署".into(),
                vec!["build".into()],
            ),
        ],
    );
    let task_id = w.run_workflow(&record, "部署任务", "目标:部署");
    w.wait_launched(1);
    actions.push(("run_workflow", project(&w, "deploy", Some(task_id))));

    w.settle(
        task_id,
        "build",
        Settlement::Fail {
            reason: "构建脚手架损坏".into(),
        },
    );
    w.wait_step_status(task_id, "build", StepStatus::Failed);
    w.wait_task_status(task_id, TaskStatus::NeedsYou);
    actions.push(("settle_build_fail", project(&w, "deploy", Some(task_id))));

    w.retry_fresh(task_id, "build");
    w.wait_launched(2);
    w.wait_step_status(task_id, "build", StepStatus::Running);
    w.wait_task_status(task_id, TaskStatus::Running);
    actions.push((
        "retry_build_fresh_session",
        project(&w, "deploy", Some(task_id)),
    ));

    w.settle(task_id, "build", Settlement::complete("重试成功"));
    w.wait_launched(3);
    w.wait_step_status(task_id, "deploy", StepStatus::Running);
    w.settle(task_id, "deploy", Settlement::complete("部署完成"));
    w.wait_step_status(task_id, "deploy", StepStatus::Succeeded);
    actions.push(("settle_all_complete", project(&w, "deploy", Some(task_id))));

    w.shutdown();
    scenario_json("failure-retry", actions)
}

/// 取消路径:运行中取消任务 → 运行/步骤/任务进入 cancelled,租约释放。
fn scenario_cancel() -> Value {
    let w = World::new();
    let mut actions: Vec<(&str, Value)> = Vec::new();

    let record = w.save_workflow(
        "long",
        "长任务",
        vec![(
            "work".into(),
            "工作".into(),
            "固定指令:长时间工作".into(),
            vec![],
        )],
    );
    let task_id = w.run_workflow(&record, "长任务", "目标:工作");
    w.wait_launched(1);
    actions.push(("run_workflow", project(&w, "long", Some(task_id))));

    w.orch.cancel_task(task_id).unwrap();
    actions.push(("cancel_task", project(&w, "long", Some(task_id))));

    w.shutdown();
    scenario_json("cancel", actions)
}

/// 询问路径:Agent 提问 → needs-input + Needs You → respond →
/// 恢复 running → 结算成功。
fn scenario_question() -> Value {
    let w = World::new();
    let mut actions: Vec<(&str, Value)> = Vec::new();

    let record = w.save_workflow(
        "migrate",
        "迁移",
        vec![(
            "migrate".into(),
            "迁移".into(),
            "固定指令:执行迁移".into(),
            vec![],
        )],
    );
    let task_id = w.run_workflow(&record, "迁移任务", "目标:迁移");
    w.wait_launched(1);
    actions.push(("run_workflow", project(&w, "migrate", Some(task_id))));

    w.emit(
        task_id,
        "migrate",
        RuntimeEvent::Question("是否在生产库执行破坏性迁移?".into()),
    );
    w.wait_step_status(task_id, "migrate", StepStatus::NeedsInput);
    w.wait_task_status(task_id, TaskStatus::NeedsYou);
    actions.push(("agent_asks_question", project(&w, "migrate", Some(task_id))));

    w.respond(task_id, "批准执行");
    w.wait_step_status(task_id, "migrate", StepStatus::Running);
    w.wait_task_status(task_id, TaskStatus::Running);
    actions.push(("respond_question", project(&w, "migrate", Some(task_id))));

    w.settle(task_id, "migrate", Settlement::complete("迁移完成"));
    w.wait_step_status(task_id, "migrate", StepStatus::Succeeded);
    actions.push(("settle_complete", project(&w, "migrate", Some(task_id))));

    w.shutdown();
    scenario_json("question", actions)
}

fn running_signal_world() -> (World, i64) {
    let w = World::new();
    let record = w.save_workflow(
        "oneshot",
        "一次性",
        vec![(
            "run".into(),
            "执行".into(),
            "固定指令:执行一次性命令".into(),
            vec![],
        )],
    );
    let task_id = w.run_workflow(&record, "一次性任务", "目标:执行");
    w.wait_launched(1);
    (w, task_id)
}

/// done / 终端空闲 / 进程退出分别不能自动 Settlement;
/// 只有用户人工结算才终结。三个信号用独立 World 验证,
/// 避免前一个信号已改变状态而掩盖后一个的行为。
fn scenario_exit_no_autosettle() -> Value {
    let mut actions: Vec<(&str, Value)> = Vec::new();

    let (done, done_task) = running_signal_world();
    done.emit(done_task, "run", RuntimeEvent::AgentState(AgentState::Done));
    done.wait_run_status(done_task, "run", RunStatus::AwaitingOutcome);
    done.wait_task_status(done_task, TaskStatus::NeedsYou);
    actions.push((
        "agent_done_no_settlement",
        project(&done, "oneshot", Some(done_task)),
    ));
    done.shutdown();

    let (idle, idle_task) = running_signal_world();
    idle.emit(idle_task, "run", RuntimeEvent::TuiIdle(true));
    // Output 是同一 FIFO runtime channel 上的 barrier;它落库未读标记时,
    // 位于其前的 TuiIdle 已被处理。
    idle.emit(idle_task, "run", RuntimeEvent::Output);
    idle.wait_task_unread(idle_task);
    idle.wait_run_status(idle_task, "run", RunStatus::Running);
    idle.wait_task_status(idle_task, TaskStatus::Running);
    actions.push((
        "terminal_idle_no_settlement",
        project(&idle, "oneshot", Some(idle_task)),
    ));
    idle.shutdown();

    let (exit, exit_task) = running_signal_world();
    exit.emit(exit_task, "run", RuntimeEvent::Exited { code: Some(0) });
    exit.wait_run_status(exit_task, "run", RunStatus::AwaitingOutcome);
    exit.wait_task_status(exit_task, TaskStatus::NeedsYou);
    actions.push((
        "process_exit_no_settlement",
        project(&exit, "oneshot", Some(exit_task)),
    ));

    let outcome = exit.settle(exit_task, "run", Settlement::complete("人工确认完成"));
    assert_eq!(outcome, SettleOutcome::Applied);
    exit.wait_step_status(exit_task, "run", StepStatus::Succeeded);
    actions.push((
        "manual_settlement",
        project(&exit, "oneshot", Some(exit_task)),
    ));

    exit.shutdown();
    scenario_json("exit-no-autosettle", actions)
}

/// 重启恢复(运行中崩溃):未结算运行重启后进入 interrupted +
/// Needs You(绝不判失败),人工结算通道保留并最终成功。
fn scenario_restart_needs_you() -> Value {
    let w = World::new();
    let record = w.save_workflow(
        "crash",
        "崩溃恢复",
        vec![("work".into(), "工作".into(), "固定指令:工作".into(), vec![])],
    );
    let task_id = w.run_workflow(&record, "恢复任务", "目标:恢复");
    w.wait_launched(1);
    let mut actions: Vec<(&str, Value)> =
        vec![("run_workflow", project(&w, "crash", Some(task_id)))];

    let w = w.restart();
    w.wait_run_status(task_id, "work", RunStatus::Interrupted);
    w.wait_task_status(task_id, TaskStatus::NeedsYou);
    actions.push(("restart_process_gone", project(&w, "crash", Some(task_id))));

    let token = token_of_node(&w.orch, task_id, "work");
    let outcome = w
        .orch
        .settle_by_token(&token, Settlement::complete("恢复后人工确认完成"))
        .unwrap();
    assert_eq!(outcome, SettleOutcome::Applied);
    w.wait_step_status(task_id, "work", StepStatus::Succeeded);
    actions.push((
        "manual_settlement_after_restart",
        project(&w, "crash", Some(task_id)),
    ));

    w.shutdown();
    scenario_json("restart-needs-you", actions)
}

/// 重启恢复(已结算):成功结算 + Handoff 落库后重启,终态与
/// Handoff 完整保留。
fn scenario_restart_settled() -> Value {
    let w = World::new();
    let record = w.save_workflow(
        "done",
        "已完成",
        vec![
            ("a".into(), "A".into(), "固定指令:A".into(), vec![]),
            (
                "b".into(),
                "B".into(),
                "固定指令:B".into(),
                vec!["a".into()],
            ),
        ],
    );
    let task_id = w.run_workflow(&record, "已完成任务", "目标:完成");
    w.wait_launched(1);
    w.settle(
        task_id,
        "a",
        Settlement::complete_with_output("A 完成", serde_json::json!({ "artifacts": ["a.txt"] })),
    );
    w.wait_launched(2);
    w.wait_step_status(task_id, "b", StepStatus::Running);
    w.settle(task_id, "b", Settlement::complete("B 完成"));
    w.wait_step_status(task_id, "b", StepStatus::Succeeded);
    let mut actions: Vec<(&str, Value)> = vec![("all_settled", project(&w, "done", Some(task_id)))];

    let w = w.restart();
    actions.push(("restart_after_success", project(&w, "done", Some(task_id))));
    w.shutdown();
    scenario_json("restart-settled", actions)
}

fn all_scenarios() -> Vec<(&'static str, fn() -> Value)> {
    vec![
        ("happy-path", scenario_happy_path),
        ("failure-retry", scenario_failure_retry),
        ("cancel", scenario_cancel),
        ("question", scenario_question),
        ("exit-no-autosettle", scenario_exit_no_autosettle),
        ("restart-needs-you", scenario_restart_needs_you),
        ("restart-settled", scenario_restart_settled),
    ]
}

// ---------------------------------------------------------------------------
// golden 契约测试
// ---------------------------------------------------------------------------

fn golden_name(scenario: &str) -> String {
    format!("workflow-{scenario}.json")
}

fn assert_matches_golden(scenario: &str, actual: Value) {
    let path = expected_dir().join(golden_name(scenario));
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 expected/{} 失败: {e}", golden_name(scenario)))
        .replace("\r\n", "\n");
    assert_eq!(
        canonical(&actual),
        expected,
        "场景 {scenario} 与 golden 不一致(语义变化必须先修订 canonical spec)"
    );
}

#[test]
fn lifecycle_scenarios_match_goldens() {
    for (name, run) in all_scenarios() {
        assert_matches_golden(name, run());
    }
}

/// 生命周期基线是确定性的:同场景独立跑两轮逐字节一致。
#[test]
fn lifecycle_scenarios_are_deterministic() {
    for (name, run) in all_scenarios() {
        let a = canonical(&run());
        let b = canonical(&run());
        assert_eq!(a, b, "场景 {name} 两轮运行不一致");
    }
}

/// 投影不含时间戳/令牌/自增 id 等漂移源:golden 文本中不出现
/// 任何 `mft_` 能力令牌前缀与时间戳形状。
#[test]
fn goldens_contain_no_volatile_fields() {
    for (name, run) in all_scenarios() {
        let text = canonical(&run());
        assert!(!text.contains("mft_"), "场景 {name} 泄漏能力令牌");
        for field in [
            "started_at",
            "ended_at",
            "created_at",
            "updated_at",
            "capability_token",
            "session_id",
            "\"id\"",
            "raw_log_ref",
        ] {
            assert!(
                !text.contains(&format!("\"{field}\"")),
                "场景 {name} 投影含漂移字段 {field}"
            );
        }
    }
}

/// golden 重生成入口(默认 ignore):
/// `MF_REGEN_BASELINE=1 cargo test -p mf-agent --test baseline_workflow_lifecycle
///  regenerate_workflow_goldens -- --ignored --exact --nocapture`
#[test]
#[ignore = "会替换提交的 golden"]
fn regenerate_workflow_goldens() {
    assert_eq!(
        std::env::var("MF_REGEN_BASELINE").as_deref(),
        Ok("1"),
        "必须显式设置 MF_REGEN_BASELINE=1"
    );
    // 先把全部场景渲染到内存;任意一个场景 panic 都不会
    // 留下半套 committed golden。完整集合 + manifest 同一次原子换入。
    let goldens: Vec<(String, Vec<u8>)> = all_scenarios()
        .into_iter()
        .map(|(name, run)| (golden_name(name), canonical(&run()).into_bytes()))
        .collect();
    baseline::write_baseline_with_workflow_goldens(&baseline_dir(), &goldens).unwrap();
    for (name, _) in &goldens {
        eprintln!("已写入 expected/{name}");
    }
}
