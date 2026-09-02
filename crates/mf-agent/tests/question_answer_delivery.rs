//! Issue #26 Respond 契约:question-bound 回答的两阶段持久投递。
//!
//! 状态机(与 `Store::apply_run_mutation_tx` / `confirm_answer_delivery` 对齐):
//! 1. ACCEPT(L-CMD 事务):回答落项目库私有表 `question_answer_deliveries`
//!    (status=pending,nonce 绑定 question+run+revision);question 保持
//!    open、Step 保持 needs-input;产出不含明文的 `AnswerRuntime` action。
//! 2. DELIVER(post-commit,可重放):以私有表为幂等键,复验 nonce/run 绑定
//!    后经 RuntimeHost question-bound 投递;宿主失败 → fail-closed,状态
//!    全部保留(outbox/命令重试可补投)。
//! 3. CONFIRM(宿主确认后事务):CAS pending→delivered + 清空明文 +
//!    question→answered + Step/Task 推进,单事务原子完成。
//!
//! 本文件用带账本的宿主假实现复刻 `SessionRegistry` 的投递守卫,
//! 断言崩溃窗口、并发重放、冲突稳定与明文不泄露。

#[path = "common/run_lifecycle.rs"]
mod run_lifecycle;

use crossbeam_channel::Sender;
use mf_agent::catalog_store::CatalogStore;
use mf_agent::config::Config;
use mf_agent::execution_directory::{
    ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use mf_agent::model::*;
use mf_agent::orchestrator::{
    DirectoryRouting, GlobalLimiter, Orchestrator, ProfileCatalog, WorkflowKernel,
};
use mf_agent::pipeline::{PipelineDraft, SessionPolicy, StepDraft};
use mf_agent::run_mutation::{RunAction, RunMutation, RunMutationOutput};
use mf_agent::runtime::{LaunchSpec, RuntimeEvent, RuntimeHost, WorkflowLaunchSpec};
use mf_agent::store::Store;
use mf_agent::AdHocLaunchSpec;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ---------- Ledger Host:复刻 SessionRegistry 的 question-bound 守卫 ----------

/// 带进程内投递账本与待答槽的宿主假实现:同 id 同答案幂等、异答案冲突、
/// 槽位换题拒绝、无 live sender 拒绝 —— 与生产 `SessionRegistry` 同口径。
/// `session_alive` 默认 true:模拟等待 ask_human 的 HTTP 会话仍存活
/// (重启恢复探测不得把 run 误标 interrupted)。
struct LedgerHost {
    /// ask_human 待答槽:`(run_handle, question_id)`。
    bound: Mutex<Option<(String, i64)>>,
    /// 已真实注入的输入:`(question_id, answer)`。
    injected: Mutex<Vec<(i64, String)>>,
    /// 进程内幂等账本:question_id → 已投递答案。
    ledger: Mutex<HashMap<i64, String>>,
    /// 注入「无 live sender」故障(核心重启后注册表为空)。
    no_live_sender: AtomicBool,
    /// `is_session_alive`(重启恢复探测;默认存活)。
    session_alive: AtomicBool,
}

impl Default for LedgerHost {
    fn default() -> Self {
        LedgerHost {
            bound: Mutex::new(None),
            injected: Mutex::new(Vec::new()),
            ledger: Mutex::new(HashMap::new()),
            no_live_sender: AtomicBool::new(false),
            session_alive: AtomicBool::new(true),
        }
    }
}

impl LedgerHost {
    fn bind_slot(&self, run_handle: &str, question_id: i64) {
        *self.bound.lock() = Some((run_handle.to_string(), question_id));
    }

    fn injected_answers(&self, question_id: i64) -> Vec<String> {
        self.injected
            .lock()
            .iter()
            .filter(|(id, _)| *id == question_id)
            .map(|(_, answer)| answer.clone())
            .collect()
    }
}

impl RuntimeHost for LedgerHost {
    fn launch(&self, _spec: LaunchSpec, _events: Sender<(i64, RuntimeEvent)>) {}
    fn launch_workflow(
        &self,
        _spec: WorkflowLaunchSpec,
        _events: Sender<(i64, RuntimeEvent)>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn launch_ad_hoc(&self, _spec: AdHocLaunchSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn send_prompt(
        &self,
        _run_handle: &str,
        _session_handle: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop_run(&self, _run_handle: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill_session(&self, _session_handle: &str) {}
    fn kill_ad_hoc(&self, _display_session_handle: &str) {}
    fn answer_question(&self, _run_handle: &str, _answer: &str) {
        panic!("question-bound 契约下绝不允许回退 legacy run 级回答");
    }
    fn is_session_alive(&self, _session_handle: &str) -> bool {
        self.session_alive.load(Ordering::SeqCst)
    }
    fn supports_question_bound_answers(&self) -> bool {
        true
    }
    fn answer_question_bound(
        &self,
        question_id: i64,
        run_handle: &str,
        answer: &str,
    ) -> anyhow::Result<()> {
        if self.no_live_sender.load(Ordering::SeqCst) {
            anyhow::bail!(
                "run `{run_handle}` 没有存活会话绑定(会话已结束或核心已重启):\
                 无法证明接收者仍是 question {question_id} 的原问题,拒绝投递(fail-closed)"
            );
        }
        let mut slot = self.bound.lock();
        if let Some(delivered) = self.ledger.lock().get(&question_id) {
            if delivered == answer {
                return Ok(());
            }
            anyhow::bail!("question {question_id} 已投递过不同答案:拒绝冲突重放");
        }
        let Some((slot_run, slot_question)) = slot.as_ref() else {
            anyhow::bail!(
                "run `{run_handle}` 当前没有等待中的问题:question {question_id} \
                 无法投递(fail-closed)"
            );
        };
        anyhow::ensure!(
            slot_run == run_handle,
            "待答槽属于 run `{slot_run}`,不是 `{run_handle}`"
        );
        anyhow::ensure!(
            *slot_question == question_id,
            "run `{run_handle}` 正在等待 question {slot_question}(另一次提问),\
             不是 {question_id}:拒绝把旧答案投给新问题(fail-closed)"
        );
        self.injected.lock().push((question_id, answer.to_string()));
        self.ledger.lock().insert(question_id, answer.to_string());
        *slot = None;
        Ok(())
    }
}

// ---------- 测试脚手架 ----------

struct NopDirectory;

impl ExecutionDirectoryProvider for NopDirectory {
    fn id(&self) -> &str {
        "nop"
    }
    fn isolates(&self) -> bool {
        false
    }
    fn acquire(&self, ctx: &LeaseContext) -> anyhow::Result<ExecutionLease> {
        Ok(ExecutionLease {
            id: format!("lease-{}", ctx.step_key),
            path: PathBuf::from("."),
            isolated: false,
            provider: "nop".into(),
            metadata: serde_json::json!({}),
        })
    }
    fn merge(&self, _leases: &[ExecutionLease]) -> anyhow::Result<MergeOutcome> {
        Ok(MergeOutcome::Merged)
    }
    fn release(&self, _lease: &ExecutionLease) -> anyhow::Result<()> {
        Ok(())
    }
}

fn one_step() -> PipelineDraft {
    PipelineDraft {
        steps: vec![StepDraft {
            key: "work".into(),
            title: "work".into(),
            instructions: "do it".into(),
            agent_profile: "test".into(),
            session_policy: SessionPolicy::Fresh,
            deps: vec![],
        }],
    }
}

fn build_orchestrator(
    store: &Arc<Store>,
    dir: &std::path::Path,
    host: Arc<LedgerHost>,
) -> Arc<Orchestrator> {
    Orchestrator::start_with_routing(
        store.clone(),
        dir.to_path_buf(),
        Config::default(),
        host,
        Arc::new(RwLock::new(ProfileCatalog::default())),
        GlobalLimiter::new(4),
        "pipe".into(),
        Arc::new(NopDirectory),
        WorkflowKernel {
            catalog: CatalogStore::memory().unwrap(),
            pins: None,
            instance_resolver: None,
        },
        DirectoryRouting {
            current_pin: None,
            resolver: None,
        },
    )
    .unwrap()
}

struct Rig {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    db_path: PathBuf,
    store: Arc<Store>,
    host: Arc<LedgerHost>,
    orch: Arc<Orchestrator>,
    task_id: i64,
    step_id: i64,
    run: RunView,
    question: StepQuestionView,
}

fn rig(file_backed: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("workflow-v1.db");
    let store = if file_backed {
        Store::open(&db_path).unwrap()
    } else {
        Store::memory().unwrap()
    };
    let host = Arc::new(LedgerHost::default());
    let orch = build_orchestrator(&store, dir.path(), host.clone());
    let task = store.create_task("t", "g").unwrap();
    let revision = store.create_draft_revision(task.id, &one_step()).unwrap();
    store.activate_revision(task.id).unwrap();
    let step = store.task_steps(task.id).unwrap()[0].clone();
    let session = store.create_session(None, "mock", "test", "s").unwrap();
    let run = store
        .create_run(task.id, step.id, revision.id, Some(session.id))
        .unwrap();
    store
        .set_step_status(step.id, StepStatus::NeedsInput)
        .unwrap();
    store
        .set_task_status(task.id, TaskStatus::NeedsYou)
        .unwrap();
    let question = store
        .ask_question(task.id, Some(step.id), Some(run.id), "continue?")
        .unwrap();
    Rig {
        dir,
        db_path,
        store,
        host,
        orch,
        task_id: task.id,
        step_id: step.id,
        run,
        question,
    }
}

/// 执行 L-CMD accept 并取回唯一的投递 action(不含答案明文)。
fn accept(rig: &Rig, answer: &str) -> RunAction {
    let result = rig
        .store
        .with_tx(|tx| {
            Store::apply_run_mutation_tx(
                tx,
                RunMutation::Respond {
                    question_id: rig.question.id,
                    answer: answer.to_string(),
                },
            )
        })
        .unwrap();
    let RunMutationOutput::Responded(_) = result.output else {
        panic!("accept 必须返回 Responded");
    };
    let [action] = result.actions.as_slice() else {
        panic!(
            "run 绑定问题的 accept 必须恰产出一个 action: {:?}",
            result.actions
        );
    };
    action.clone()
}

// ---------- 契约测试 ----------

/// 快乐路径:accept → 宿主确认 → 单事务收口;question/Step/Task 只在
/// 投递确认后推进;收口后明文清除、action 重放不再注入。
#[test]
fn two_phase_accept_deliver_confirm_advances_state_only_after_delivery() {
    let rig = rig(false);
    let run_revision_before = rig.run.revision;
    let action = accept(&rig, "yes");

    // accept 后:未最终回答,Step/Task 不动。
    assert_eq!(
        rig.store.question(rig.question.id).unwrap().unwrap().status,
        "open"
    );
    assert_eq!(
        rig.store
            .step_view(rig.step_id)
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "needs-input"
    );
    assert_eq!(
        rig.store
            .task_view(rig.task_id)
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "needs-you"
    );
    assert_eq!(
        rig.store.run_view(rig.run.id).unwrap().unwrap().revision,
        run_revision_before + 1,
        "accept 必须推进 Run revision，供 Kernel 原子承载 durable action"
    );

    rig.host.bind_slot(&rig.run.public_handle, rig.question.id);
    rig.orch.execute_durable_run_action(&action).unwrap();

    let events = rig.orch.events_rx.try_iter().collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| matches!(
            event,
            SchedulerEvent::QuestionAnswered { question_id, .. }
                if *question_id == rig.question.id
        )),
        "投递确认必须发布不含回答明文的事实事件: {events:?}"
    );
    assert!(
        !format!("{events:?}").contains("yes"),
        "SchedulerEvent/Debug 不得包含回答明文: {events:?}"
    );

    assert_eq!(
        rig.host.injected_answers(rig.question.id),
        vec!["yes".to_string()]
    );
    let question = rig.store.question(rig.question.id).unwrap().unwrap();
    assert_eq!(question.status, "answered");
    assert_eq!(question.answer.as_deref(), Some("yes"));
    assert_eq!(
        rig.store
            .step_view(rig.step_id)
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "running"
    );
    assert_eq!(
        rig.store
            .task_view(rig.task_id)
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "running"
    );
    // 收口后:私有表 delivered、明文清除;重放(投递后 ack 已完成)幂等。
    let delivery = rig
        .store
        .answer_delivery_of_question(rig.question.id)
        .unwrap()
        .unwrap();
    assert_eq!(delivery.status, "delivered");
    assert_eq!(delivery.answer, "");
    rig.orch.execute_durable_run_action(&action).unwrap();
    assert_eq!(
        rig.host.injected_answers(rig.question.id),
        vec!["yes".to_string()],
        "收口后的 action 重放不得二次注入"
    );
}

#[test]
fn question_view_debug_redacts_answer_plaintext() {
    let rig = rig(false);
    let mut answered = rig.question.clone();
    answered.answer = Some("secret-answer-never-log".into());
    let debug = format!("{answered:?}");
    assert!(debug.contains("<redacted>"), "{debug}");
    assert!(!debug.contains("secret-answer-never-log"), "{debug}");
}

/// commit 后、投递前崩溃:pending 状态跨重启存活;outbox 式重放先因
/// 无 live sender fail-closed,宿主恢复后同一 action 补投成功。
#[test]
fn crash_between_commit_and_delivery_is_replayable_across_restart() {
    let rig = rig(true);
    let action = accept(&rig, "yes");
    // 模拟投递前进程崩溃:丢弃 store/宿主/调度器,只留数据库文件。
    drop(rig.orch);
    drop(rig.host);
    drop(rig.store);

    let store = Store::open(&rig.db_path).unwrap();
    let host = Arc::new(LedgerHost::default());
    host.no_live_sender.store(true, Ordering::SeqCst); // 重启后无存活会话
    let dir = tempfile::tempdir().unwrap();
    let orch = build_orchestrator(&store, dir.path(), host.clone());

    // pending 投递与未回答的问题都完好保留。
    let delivery = store
        .answer_delivery_of_question(rig.question.id)
        .unwrap()
        .unwrap();
    assert_eq!(delivery.status, "pending");
    assert_eq!(delivery.answer, "yes");
    assert_eq!(
        store.question(rig.question.id).unwrap().unwrap().status,
        "open"
    );
    assert_eq!(
        store
            .step_view(rig.step_id)
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "needs-input"
    );

    // 无 live sender:fail-closed,不假成功,状态保留。
    let err = orch.execute_durable_run_action(&action).unwrap_err();
    let chain = format!("{err:#}");
    assert!(chain.contains("没有存活会话绑定"), "{chain}");
    assert!(
        store
            .answer_delivery_of_question(rig.question.id)
            .unwrap()
            .unwrap()
            .status
            == "pending"
    );
    assert!(store.question(rig.question.id).unwrap().unwrap().status == "open");

    // 会话恢复(重连 + 待答槽重新绑定到同一 question):同一 action 补投。
    host.no_live_sender.store(false, Ordering::SeqCst);
    host.bind_slot(&rig.run.public_handle, rig.question.id);
    orch.execute_durable_run_action(&action).unwrap();
    assert_eq!(
        host.injected_answers(rig.question.id),
        vec!["yes".to_string()]
    );
    assert_eq!(
        store.question(rig.question.id).unwrap().unwrap().status,
        "answered"
    );
    assert_eq!(
        store
            .step_view(rig.step_id)
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "running"
    );
}

/// 宿主已注入但收口事务未提交(ack 前崩溃的进程内形态):
/// 同 action 重放按账本/私有表幂等,不产生第二次输入,也不重复推进。
#[test]
fn replay_after_delivery_without_ack_does_not_double_deliver() {
    let rig = rig(false);
    let action = accept(&rig, "yes");
    rig.host.bind_slot(&rig.run.public_handle, rig.question.id);
    rig.orch.execute_durable_run_action(&action).unwrap();

    // 模拟「已投递、confirm 未落库」:手工把私有表拨回 pending,
    // 再重放 —— 宿主账本保证不再注入,confirm 收敛到 delivered。
    rig.store
        .with_conn(|c| {
            c.execute(
                "UPDATE question_answer_deliveries SET status='pending', answer=?2, delivered_at=NULL
                 WHERE question_id=?1",
                rusqlite::params![rig.question.id, "yes"],
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();
    rig.orch.execute_durable_run_action(&action).unwrap();
    assert_eq!(
        rig.host.injected_answers(rig.question.id),
        vec!["yes".to_string()],
        "ack 前重放不得产生第二次输入"
    );
    let delivery = rig
        .store
        .answer_delivery_of_question(rig.question.id)
        .unwrap()
        .unwrap();
    assert_eq!(delivery.status, "delivered");
}

/// 陈旧/错位 action(nonce 或 run 绑定不匹配)一律拒绝,
/// 且拒绝错误不携带答案明文。
#[test]
fn stale_or_mismatched_action_is_rejected_without_leaking_answer() {
    let rig = rig(false);
    let action = accept(&rig, "secret-answer-42");
    rig.host.bind_slot(&rig.run.public_handle, rig.question.id);

    let RunAction::AnswerRuntime {
        question_id,
        run_id,
        run_handle,
        ..
    } = &action
    else {
        panic!("必须是 AnswerRuntime");
    };
    for stale in [
        RunAction::AnswerRuntime {
            question_id: *question_id,
            run_id: *run_id,
            run_handle: run_handle.clone(),
            nonce: format!("{run_handle}:stale-nonce"),
        },
        RunAction::AnswerRuntime {
            question_id: *question_id,
            run_id: *run_id,
            run_handle: format!("{run_handle}-other"),
            nonce: action_nonce(&action),
        },
    ] {
        let err = rig.orch.execute_durable_run_action(&stale).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("绑定不匹配"), "{chain}");
        assert!(!chain.contains("secret-answer-42"), "{chain}");
    }
    // 状态与待答槽不受影响,正确 action 仍可投递。
    assert!(rig.host.injected_answers(rig.question.id).is_empty());
    assert_eq!(
        rig.store.question(rig.question.id).unwrap().unwrap().status,
        "open"
    );
    rig.orch.execute_durable_run_action(&action).unwrap();
    assert_eq!(
        rig.host.injected_answers(rig.question.id),
        vec!["secret-answer-42".to_string()]
    );
}

fn action_nonce(action: &RunAction) -> String {
    let RunAction::AnswerRuntime { nonce, .. } = action else {
        panic!("必须是 AnswerRuntime");
    };
    nonce.clone()
}

/// q1 的 action 在运行时已等待 q2 时必须被拒绝:q2 通道不被污染,
/// q1 的 pending 状态保留(fail-closed 可恢复)。
#[test]
fn q1_action_is_rejected_when_runtime_waits_on_q2() {
    let rig = rig(false);
    let q1 = accept(&rig, "answer-q1");
    // 运行时已推进到下一题:待答槽换绑 q2(另一次提问)。
    rig.host.bind_slot(&rig.run.public_handle, 4242);

    let err = rig.orch.execute_durable_run_action(&q1).unwrap_err();
    let chain = format!("{err:#}");
    assert!(chain.contains("拒绝把旧答案投给新问题"), "{chain}");
    assert!(rig.host.injected_answers(rig.question.id).is_empty());
    assert_eq!(
        rig.store.question(rig.question.id).unwrap().unwrap().status,
        "open"
    );
    assert_eq!(
        rig.store
            .answer_delivery_of_question(rig.question.id)
            .unwrap()
            .unwrap()
            .status,
        "pending",
        "q1 的待投递状态必须保留(可恢复)"
    );
}

/// 并发重复投递(outbox 重放 + 命令重试同时到达):恰好一次注入,
/// 全部调用方 Ok;confirm 的 CAS 保证状态只推进一次。
#[test]
fn concurrent_duplicate_actions_deliver_exactly_once() {
    let rig = rig(false);
    let action = accept(&rig, "yes");
    rig.host.bind_slot(&rig.run.public_handle, rig.question.id);

    let mut joins = Vec::new();
    for _ in 0..8 {
        let orch = rig.orch.clone();
        let action = action.clone();
        joins.push(std::thread::spawn(move || {
            orch.execute_durable_run_action(&action)
                .expect("并发重放必须全部 Ok(幂等)");
        }));
    }
    for join in joins {
        join.join().expect("线程不得 panic");
    }
    assert_eq!(
        rig.host.injected_answers(rig.question.id),
        vec!["yes".to_string()],
        "并发重放只允许一次输入"
    );
    let question = rig.store.question(rig.question.id).unwrap().unwrap();
    assert_eq!(question.status, "answered");
    assert_eq!(question.answer.as_deref(), Some("yes"));
    assert_eq!(
        rig.store
            .answer_delivery_of_question(rig.question.id)
            .unwrap()
            .unwrap()
            .status,
        "delivered"
    );
}

/// 直连缝隙 `answer_question`:宿主失败时上抛且不假成功;
/// 同答案重试幂等补投;Debug 链不泄露答案明文。
#[test]
fn direct_seam_fails_closed_then_retries_idempotently() {
    let rig = rig(false);
    rig.host.no_live_sender.store(true, Ordering::SeqCst);
    let err =
        run_lifecycle::answer_question(&rig.orch, rig.question.id, "secret-answer-7").unwrap_err();
    let chain = format!("{err:#}");
    assert!(chain.contains("没有存活会话绑定"), "{chain}");
    assert!(!chain.contains("secret-answer-7"), "{chain}");
    // 状态保留:question open、pending 投递完好。
    assert_eq!(
        rig.store.question(rig.question.id).unwrap().unwrap().status,
        "open"
    );
    assert_eq!(
        rig.store
            .answer_delivery_of_question(rig.question.id)
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );

    // 宿主恢复后同答案重试:幂等补投并收口。
    rig.host.no_live_sender.store(false, Ordering::SeqCst);
    rig.host.bind_slot(&rig.run.public_handle, rig.question.id);
    run_lifecycle::answer_question(&rig.orch, rig.question.id, "secret-answer-7").unwrap();
    assert_eq!(
        rig.host.injected_answers(rig.question.id),
        vec!["secret-answer-7".to_string()]
    );
    assert_eq!(
        rig.store.question(rig.question.id).unwrap().unwrap().status,
        "answered"
    );
}

/// 明文边界:投递记录的 Debug、冲突错误链都不得包含答案。
#[test]
fn delivery_debug_and_conflict_errors_stay_redacted() {
    let rig = rig(false);
    accept(&rig, "secret-answer-99");

    let debug = format!(
        "{:?}",
        rig.store
            .answer_delivery_of_question(rig.question.id)
            .unwrap()
            .unwrap()
    );
    assert!(!debug.contains("secret-answer-99"), "{debug}");
    assert!(debug.contains("<redacted>"), "{debug}");

    let conflict = rig
        .store
        .with_tx(|tx| {
            Store::apply_run_mutation_tx(
                tx,
                RunMutation::Respond {
                    question_id: rig.question.id,
                    answer: "different".into(),
                },
            )
        })
        .unwrap_err();
    let chain = format!("{conflict:#}");
    assert!(chain.contains("冲突"), "{chain}");
    assert!(!chain.contains("secret-answer-99"), "{chain}");
}
