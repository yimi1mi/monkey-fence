use crate::config::{Config, ProviderKind};
use crate::db::Db;
use crate::provider::{self, AssistantBlock, ChatMessage, ToolCall};
use crate::tools::{planner_tools, worker_tools, ToolCtx, ToolOutcome};
use crate::types::*;
use anyhow::Result;
use mf_skills::Skill;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 编排引擎:规划者线程 + 工作者线程池,事件经 crossbeam 通道推给 UI
pub struct Engine {
    db: Arc<Db>,
    events: crossbeam_channel::Sender<EngineEvent>,
    pub events_rx: crossbeam_channel::Receiver<EngineEvent>,
    config: Config,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    workers: usize,
    skills: Vec<Skill>,
}

impl Engine {
    pub fn start(
        db_path: impl AsRef<std::path::Path>,
        root: PathBuf,
        config: Config,
        skills: Vec<Skill>,
    ) -> Result<Self> {
        let db = Arc::new(Db::open(db_path)?);
        let (tx, rx) = crossbeam_channel::bounded(4096);
        let workers = config.engine.workers.clamp(1, 8);
        let engine = Self {
            db,
            events: tx,
            events_rx: rx,
            config,
            root,
            stop: Arc::new(AtomicBool::new(false)),
            workers,
            skills,
        };
        engine.spawn_workers();
        Ok(engine)
    }

    fn spawn_workers(&self) {
        for i in 0..self.workers {
            let db = self.db.clone();
            let events = self.events.clone();
            let config = self.config.clone();
            let root = self.root.clone();
            let stop = self.stop.clone();
            let skills = self.skills.clone();
            let name = format!("worker-{}", i + 1);
            std::thread::Builder::new()
                .name(name.clone())
                .spawn(move || {
                    worker_loop(&name, db, events, config, root, stop, &skills);
                })
                .expect("spawn worker");
        }
    }

    /// 启动一次目标运行:规划者在独立线程分解任务
    pub fn start_run(&self, objective: &str) -> Result<i64> {
        let run_id = self.db.create_run(objective)?;
        let _ = self.events.send(EngineEvent::RunStarted(RunView {
            id: run_id,
            objective: objective.to_string(),
            status: "active".into(),
        }));
        self.db.push_message(run_id, "user", "coordinator", "objective", objective)?;

        let db = self.db.clone();
        let events = self.events.clone();
        let config = self.config.clone();
        let root = self.root.clone();
        let objective = objective.to_string();
        std::thread::Builder::new()
            .name("planner".into())
            .spawn(move || {
                planner_loop(run_id, &objective, &db, &events, &config, &root);
            })
            .expect("spawn planner");
        Ok(run_id)
    }

    /// UI 回答问题
    pub fn answer_question(&self, question_id: i64, answer: &str) -> Result<()> {
        self.db.answer_question(question_id, answer)?;
        Ok(())
    }

    /// UI 手动解除熔断
    pub fn unblock_task(&self, task_id: i64) -> Result<()> {
        self.db.unblock_task(task_id)
    }

    pub fn tasks_of_run(&self, run_id: i64) -> Result<Vec<TaskView>> {
        self.db.tasks_of_run(run_id)
    }

    pub fn open_questions(&self, run_id: i64) -> Result<Vec<QuestionView>> {
        self.db.open_questions(run_id)
    }

    /// 最近一次活跃 run
    pub fn latest_active_run(&self) -> Result<Option<RunView>> {
        self.db.latest_run()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---------- 规划者 ----------

fn planner_loop(
    run_id: i64,
    objective: &str,
    db: &Arc<Db>,
    events: &crossbeam_channel::Sender<EngineEvent>,
    config: &Config,
    root: &PathBuf,
) {
    let provider_cfg = config.provider_for_role("planner");
    let tools = planner_tools();
    let tree = workspace_tree_snippet(root);
    let system = "你是 MonkeyFence 的任务规划者(coordinator)。\
将用户目标分解为可独立执行的任务 DAG。规则:\n\
1. 每个任务用 create_task 创建,spec 写清楚做什么、验收标准是什么。\n\
2. 有依赖关系的任务用 deps 传入前置任务 id;无依赖留空。\n\
3. 任务粒度适中:一个任务 = 一次可验证的改动。\n\
4. 全部创建完毕后调用 finalize_plan 结束规划。\n\
不要执行任何具体修改,那是工作者的职责。";
    let mut messages = vec![
        ChatMessage::system(system),
        ChatMessage::user(format!(
            "PLANNER_OBJECTIVE: {}\n\n工作区结构:\n{}",
            objective, tree
        )),
    ];
    let max_iters = config.engine.max_iterations.min(12);
    for _ in 0..max_iters {
        let blocks = match provider::complete(&provider_cfg, &messages, &tools) {
            Ok(b) => b,
            Err(e) => {
                let _ = events.send(EngineEvent::EngineError(format!("planner: {e}")));
                // 规划失败:兜底创建单任务,保证 run 不悬空
                let _ = db.create_task(run_id, None, &format!("(规划失败兜底) {}", objective), &[]);
                let _ = db.finish_run(run_id, "planner-error");
                return;
            }
        };
        let mut tool_calls = Vec::new();
        let mut text_out = String::new();
        for b in &blocks {
            match b {
                AssistantBlock::Text(t) => text_out.push_str(t),
                AssistantBlock::ToolUse(c) => tool_calls.push(c.clone()),
            }
        }
        if !text_out.trim().is_empty() {
            let _ = events.send(EngineEvent::WorkerLog {
                task_id: 0,
                worker: "planner".into(),
                text: text_out.clone(),
            });
        }
        if tool_calls.is_empty() {
            // 纯文本回复视作规划结束
            break;
        }
        messages.push(ChatMessage::assistant(text_out, tool_calls.clone()));
        let mut finalized = false;
        for call in &tool_calls {
            let outcome = exec_planner_tool(run_id, db, events, call);
            match outcome {
                ToolOutcome::Complete(_) => finalized = true,
                ToolOutcome::Fail(reason) => {
                    let _ = db.finish_run(run_id, "planner-error");
                    let _ = events.send(EngineEvent::RunFinished(run_id, format!("规划失败: {reason}")));
                    return;
                }
                ToolOutcome::Result(text) => {
                    messages.push(ChatMessage::tool_result(call.id.clone(), text));
                }
            }
        }
        if finalized {
            let _ = db.push_message(run_id, "planner", "coordinator", "status", "规划完成");
            let _ = events.send(EngineEvent::WorkerLog {
                task_id: 0,
                worker: "planner".into(),
                text: "规划完成,开始派发".into(),
            });
            return;
        }
    }
}

fn exec_planner_tool(
    run_id: i64,
    db: &Arc<Db>,
    events: &crossbeam_channel::Sender<EngineEvent>,
    call: &ToolCall,
) -> ToolOutcome {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    match call.name.as_str() {
        "create_task" => {
            let spec = args
                .get("spec")
                .and_then(|v| v.as_str())
                .unwrap_or("(空任务)")
                .to_string();
            let deps: Vec<i64> = args
                .get("deps")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default();
            match db.create_task(run_id, None, &spec, &deps) {
                Ok(view) => {
                    let _ = events.send(EngineEvent::TaskCreated(view.clone()));
                    ToolOutcome::Result(format!("任务已创建,id={}", view.id))
                }
                Err(e) => ToolOutcome::Result(format!("错误: {e}")),
            }
        }
        "finalize_plan" => ToolOutcome::Complete("ok".into()),
        _ => ToolOutcome::Result(format!("未知规划工具: {}", call.name)),
    }
}

// ---------- 工作者 ----------

fn worker_loop(
    name: &str,
    db: Arc<Db>,
    events: crossbeam_channel::Sender<EngineEvent>,
    config: Config,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    skills: &[Skill],
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match db.claim_next_ready(name) {
            Ok(Some((task, dispatch_id))) => {
                execute_task(name, task, dispatch_id, &db, &events, &config, &root, skills);
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(300));
            }
            Err(e) => {
                let _ = events.send(EngineEvent::EngineError(format!("claim: {e}")));
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
    }
}

fn execute_task(
    worker: &str,
    task: TaskView,
    dispatch_id: i64,
    db: &Arc<Db>,
    events: &crossbeam_channel::Sender<EngineEvent>,
    config: &Config,
    root: &PathBuf,
    skills: &[Skill],
) {
    let _ = events.send(EngineEvent::TaskStatus(task.clone()));
    let provider_cfg = config.provider_for_role("worker");
    // 技能匹配:注入说明 + 收紧工具白名单
    let matched = mf_skills::match_skills(skills, &task.spec);
    let all = worker_tools();
    let all_names: Vec<&str> = all.iter().map(|t| t.name).collect();
    let allowed = mf_skills::allowed_tools(&matched, &all_names);
    let tools: Vec<_> = all
        .into_iter()
        .filter(|t| allowed.contains(t.name))
        .collect();
    let ctx = ToolCtx {
        root: root.clone(),
        db: db.clone(),
        run_id: task.run_id,
        task_id: task.id,
        worker: worker.to_string(),
        events: events.clone(),
    };

    let system = worker_system_prompt(&task, root, &matched);
    let mut messages = vec![
        ChatMessage::system(system),
        ChatMessage::user(format!(
            "TASK_ID: {}\nTASK_SPEC:\n{}\n\n工作区根: {}\n开始执行。可用工具见工具列表。",
            task.id,
            task.spec,
            root.display()
        )),
    ];

    let mut failure_reason = String::from("达到最大迭代次数");
    let mut settled = false;
    let mut iterations = 0usize;
    'outer: while iterations < config.engine.max_iterations {
        iterations += 1;
        let blocks = match provider::complete(&provider_cfg, &messages, &tools) {
            Ok(b) => b,
            Err(e) => {
                failure_reason = format!("提供方调用失败: {e}");
                break;
            }
        };
        let mut tool_calls = Vec::new();
        let mut text_out = String::new();
        for b in &blocks {
            match b {
                AssistantBlock::Text(t) => text_out.push_str(t),
                AssistantBlock::ToolUse(c) => tool_calls.push(c.clone()),
            }
        }
        if !text_out.trim().is_empty() {
            let _ = events.send(EngineEvent::WorkerLog {
                task_id: task.id,
                worker: worker.to_string(),
                text: text_out.clone(),
            });
        }
        if tool_calls.is_empty() {
            // 无工具调用的文本回复 = 自然结束
            failure_reason = String::new();
            let summary = text_out.trim().chars().take(400).collect::<String>();
            settle(db, events, &task, dispatch_id, true, &summary, config);
            return;
        }
        messages.push(ChatMessage::assistant(text_out, tool_calls.clone()));
        for call in &tool_calls {
            let _ = events.send(EngineEvent::WorkerTool {
                task_id: task.id,
                worker: worker.to_string(),
                tool: call.name.clone(),
                summary: call.arguments.chars().take(120).collect::<String>(),
            });
            match ctx.execute(&call.name, &call.arguments) {
                ToolOutcome::Result(text) => {
                    messages.push(ChatMessage::tool_result(call.id.clone(), text));
                }
                ToolOutcome::Complete(summary) => {
                    failure_reason = String::new();
                    settled = true;
                    settle(db, events, &task, dispatch_id, true, &summary, config);
                    break 'outer;
                }
                ToolOutcome::Fail(reason) => {
                    failure_reason = reason;
                    break 'outer;
                }
            }
        }
    }

    // 循环自然结束(未显式结算)才走兜底路径
    if settled {
        return;
    }
    if failure_reason.is_empty() {
        settle(db, events, &task, dispatch_id, true, "(无总结)", config);
    } else {
        settle(db, events, &task, dispatch_id, false, &failure_reason, config);
    }
}

fn settle(
    db: &Arc<Db>,
    events: &crossbeam_channel::Sender<EngineEvent>,
    task: &TaskView,
    dispatch_id: i64,
    ok: bool,
    summary: &str,
    config: &Config,
) {
    let changed = if ok {
        db.complete_task(task.id, dispatch_id, summary)
    } else {
        db.fail_task(task.id, dispatch_id, summary, config.engine.max_failures)
    }
    .unwrap_or_default();
    let _ = db.push_message(
        task.run_id,
        &format!("worker-for-#{}", task.id),
        "coordinator",
        if ok { "worker_done" } else { "escalation" },
        &format!("任务 #{} {}: {}", task.id, if ok { "完成" } else { "失败" }, summary),
    );
    // 刷新本任务与被提升任务的视图
    if let Ok(view) = db.task_view(task.id) {
        let _ = events.send(EngineEvent::TaskStatus(view));
    }
    for c in changed {
        let _ = events.send(EngineEvent::TaskStatus(c));
    }
    // 收敛检查
    if let Ok(true) = db.run_converged(task.run_id) {
        let ok_run = db
            .tasks_of_run(task.run_id)
            .map(|ts| ts.iter().all(|t| t.status != TaskStatus::Blocked && t.status != TaskStatus::Failed))
            .unwrap_or(false);
        let status = if ok_run { "done" } else { "done-with-failures" };
        let _ = db.finish_run(task.run_id, status);
        let _ = events.send(EngineEvent::RunFinished(
            task.run_id,
            if ok_run { "全部任务完成".into() } else { "存在失败/熔断任务".into() },
        ));
    }
}

// ---------- 提示词 ----------

fn workspace_tree_snippet(root: &PathBuf) -> String {
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, depth: usize, out: &mut Vec<String>) {
        if depth > 2 {
            return;
        }
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(format!("{}{}{}", "  ".repeat(depth), if is_dir { "📁 " } else { "   " }, name));
            if is_dir {
                walk(&e.path(), depth + 1, out);
            }
            if out.len() > 200 {
                return;
            }
        }
    }
    walk(root, 0, &mut out);
    out.into_iter().take(200).collect::<Vec<_>>().join("\n")
}

fn worker_system_prompt(task: &TaskView, _root: &PathBuf, skills: &[&Skill]) -> String {
    let skill_section = if skills.is_empty() {
        String::new()
    } else {
        let bodies: Vec<String> = skills
            .iter()
            .map(|s| format!("## {}(来自技能 {})\n{}", s.meta.title, s.meta.id, s.body))
            .collect();
        format!("\n\n# 适用技能\n\n{}\n", bodies.join("\n\n"))
    };
    format!(
        "你是 MonkeyFence 的工作者 agent(worker),负责完成任务 #{}。\n\
执行纪律:\n\
1. 修改文件前先用 fs_read 看过原文。\n\
2. 小步修改,每次 fs_write/fs_patch 后自检(必要时 run_cmd 运行构建/测试)。\n\
3. 需要进一步拆分工作时用 spawn_subtask 创建子任务并等待其完成(在依赖里声明)。\n\
4. 遇到必须由人决策的问题用 ask_human,不要自行猜测关键决策。\n\
5. 完成后调用 complete_task 提交总结;无法完成调用 report_failure 说明原因。\n\
6. 只操作工作区内的文件。{}\n\n\
任务说明:\n{}",
        task.id, skill_section, task.spec
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EngineConfig, ProviderConfig};

    /// mock 提供方驱动的端到端任务流转:规划 → 2 任务(DAG) → 全部完成
    #[test]
    fn mock_run_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let mut config = Config::default();
        config.engine = EngineConfig {
            workers: 2,
            max_iterations: 8,
            max_failures: 3,
        };
        let engine = Engine::start(
            tmp.path().join("orchestration.db"),
            root.clone(),
            config,
            Vec::new(),
        )
        .unwrap();
        let run_id = engine
            .start_run("整理 .mf-agent 目录并写一个说明文件")
            .unwrap();

        // 轮询事件直到 run 结束(上限 30s)
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut finished = false;
        let mut saw_fs_write = false;
        while std::time::Instant::now() < deadline {
            match engine.events_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(ev) => match ev {
                    EngineEvent::RunFinished(_, _) => {
                        finished = true;
                        break;
                    }
                    EngineEvent::WorkerTool { tool, .. } => {
                        if tool == "fs_write" {
                            saw_fs_write = true;
                        }
                    }
                    _ => {}
                },
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
        engine.stop();
        assert!(finished, "run 应在时限内完成");
        assert!(saw_fs_write, "mock 工作者应写入文件");
        let tasks = engine.tasks_of_run(run_id).unwrap();
        assert!(tasks.len() >= 2, "mock 规划者应创建至少 2 个任务,实际 {}", tasks.len());
        assert!(
            tasks.iter().all(|t| t.status == TaskStatus::Completed),
            "所有任务应完成,实际 {:?}",
            tasks.iter().map(|t| t.status).collect::<Vec<_>>()
        );
        // 依赖关系生效:第二个任务依赖第一个
        let with_deps: Vec<_> = tasks.iter().filter(|t| !t.deps.is_empty()).collect();
        assert!(!with_deps.is_empty());
        // 文件确实写入
        let wrote = tasks.iter().any(|t| root.join(format!(".mf-agent/task-{}.md", t.id)).exists());
        assert!(wrote, ".mf-agent/task-N.md 应存在");
    }

    #[test]
    fn sandbox_rejects_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, _rx) = crossbeam_channel::bounded(16);
        let db = Db::memory().unwrap();
        let ctx = ToolCtx {
            root: tmp.path().to_path_buf(),
            db: Arc::new(db),
            run_id: 1,
            task_id: 1,
            worker: "t".into(),
            events: tx,
        };
        assert!(ctx.sandbox("../outside.txt").is_err());
        assert!(ctx.sandbox("C:/Windows/system32").is_err());
        assert!(ctx.sandbox("sub/dir/file.rs").is_ok());
    }
}
