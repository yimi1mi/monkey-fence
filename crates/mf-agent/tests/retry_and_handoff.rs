//! 重试感知 Run Coordinator 与 Handoff 解锁(Orchestration Task 3):
//! 重试模式(续会话/新会话)、有限自动重试、下游仅在
//! "成功结算 + Handoff 落库"后解锁、重试耗尽保持阻塞。

use crossbeam_channel::Sender;
use mf_agent::model::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::pipeline::{PipelineDraft, SessionPolicy, StepDraft};
use mf_agent::runtime::{LaunchSpec, RuntimeEvent, RuntimeHost};
use mf_agent::store::Store;
use mf_agent::{AdHocLaunchSpec, RetryMode, RunMode};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------- 测试宿主 ----------

#[derive(Default)]
struct MockHost {
    launches: Mutex<Vec<LaunchSpec>>,
    senders: Mutex<HashMap<String, Sender<(i64, RuntimeEvent)>>>,
}

impl MockHost {
    fn token_of(&self, step_key: &str, store: &Store, revision_id: i64) -> Option<String> {
        // 通过最近一次该 step 的 launch 找令牌
        let launches = self.launches.lock();
        launches
            .iter()
            .rev()
            .find(|s| {
                store
                    .step_view(s.step_id)
                    .ok()
                    .flatten()
                    .map(|v| v.step_key == step_key && v.revision_id == revision_id)
                    .unwrap_or(false)
            })
            .map(|s| s.capability_token.clone())
    }
}

impl RuntimeHost for MockHost {
    fn launch(&self, spec: LaunchSpec, events: Sender<(i64, RuntimeEvent)>) {
        let run_id = spec.run_id;
        self.senders
            .lock()
            .insert(spec.capability_token.clone(), events.clone());
        self.launches.lock().push(spec);
        let _ = events.send((run_id, RuntimeEvent::Launched));
    }
    fn launch_ad_hoc(&self, _spec: AdHocLaunchSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn send_prompt(&self, _p: &str, _r: i64, _s: i64, _t: &str) {}
    fn stop_run(&self, _p: &str, _r: i64) {}
    fn kill_session(&self, _p: &str, _s: i64) {}
    fn kill_ad_hoc(&self, _p: &str, _s: i64) {}
    fn answer_question(&self, _p: &str, _r: i64, _a: &str) {}
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    cond()
}

// ---------- Fixture ----------

fn mock_profiles() -> Arc<RwLock<ProfileCatalog>> {
    let mut index = mf_agent::pipeline::ProfileIndex::default();
    index.entries.insert(
        "mock".into(),
        mf_agent::pipeline::ProfileAvailability {
            installed: true,
            enabled: true,
            detected: true,
        },
    );
    let mut specs = HashMap::new();
    specs.insert(
        "mock".to_string(),
        mf_agent::runtime::AgentProfileSpec {
            id: "mock".into(),
            display_name: "Mock".into(),
            runtime: mf_agent::runtime::RuntimeKind::Pty,
            command: "cmd.exe".into(),
            args: vec![],
            env: vec![],
            permission_args: vec![],
            provider: None,
            icon: None,
            homepage: None,
            hook: None,
        },
    );
    Arc::new(RwLock::new(ProfileCatalog { index, specs }))
}

struct Fixture {
    orch: Arc<Orchestrator>,
    host: Arc<MockHost>,
    revision_id: i64,
}

impl Fixture {
    /// build/docs 并行,package 汇合;全部自动派发运行。
    fn parallel() -> Fixture {
        let store = Store::memory().unwrap();
        let host = Arc::new(MockHost::default());
        let orch = Orchestrator::start(
            store,
            PathBuf::from("."),
            mf_agent::Config::default(),
            host.clone(),
            mock_profiles(),
            GlobalLimiter::new(8),
            "test-pipe".into(),
            Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider::default()),
        )
        .unwrap();
        let task = orch.create_task("t", "goal").unwrap();
        let draft = PipelineDraft {
            steps: vec![
                step("build", &[]),
                step("docs", &[]),
                step("package", &["build", "docs"]),
            ],
        };
        orch.save_pipeline(task.id, &draft).unwrap();
        orch.confirm_and_run(task.id).unwrap();
        let revision_id = orch.store.active_revision(task.id).unwrap().unwrap().id;
        let fx = Fixture {
            orch,
            host,
            revision_id,
        };
        fx.wait_running(&["build", "docs"]);
        fx
    }

    fn steps(&self) -> Vec<StepView> {
        self.orch.store.revision_steps(self.revision_id).unwrap()
    }

    fn step_id(&self, key: &str) -> i64 {
        self.steps().iter().find(|s| s.step_key == key).unwrap().id
    }

    fn status(&self, key: &str) -> StepStatus {
        self.steps()
            .iter()
            .find(|s| s.step_key == key)
            .unwrap()
            .status
    }

    fn attempts(&self, key: &str) -> i32 {
        self.steps()
            .iter()
            .find(|s| s.step_key == key)
            .unwrap()
            .attempts
    }

    fn wait_running(&self, keys: &[&str]) {
        let ok = wait_until(Duration::from_secs(5), || {
            keys.iter()
                .all(|k| matches!(self.status(k), StepStatus::Running))
        });
        assert!(ok, "等待 {:?} 进入 running 超时", keys);
    }

    fn wait_status(&self, key: &str, expected: StepStatus) {
        let ok = wait_until(Duration::from_secs(5), || self.status(key) == expected);
        assert!(ok, "等待 {key} → {expected:?} 超时");
    }

    fn settle(&self, key: &str, settlement: Settlement) {
        let token = self
            .host
            .token_of(key, &self.orch.store, self.revision_id)
            .unwrap_or_else(|| panic!("找不到 {key} 的运行令牌"));
        self.orch
            .settle_by_token(&token, settlement)
            .expect("结算失败");
    }

    fn fail(&self, key: &str) {
        self.settle(
            key,
            Settlement::Fail {
                reason: "测试失败".into(),
            },
        );
    }

    fn complete(&self, key: &str, summary: &str) {
        self.settle(
            key,
            Settlement::Complete {
                summary: summary.into(),
            },
        );
    }

    fn retry_and_complete(&self, key: &str) {
        self.orch
            .retry_step(self.step_id(key), RetryMode::FreshSession)
            .unwrap();
        self.wait_running(&[key]);
        self.complete(key, "重试成功");
    }

    fn task_id(&self) -> i64 {
        self.orch.store.revision_steps(self.revision_id).unwrap()[0].task_id
    }
}

fn step(key: &str, deps: &[&str]) -> StepDraft {
    StepDraft {
        key: key.into(),
        title: key.into(),
        instructions: String::new(),
        agent_profile: "mock".into(),
        session_policy: SessionPolicy::Fresh,
        deps: deps.iter().map(|s| s.to_string()).collect(),
    }
}

// ---------- 计划核心用例 ----------

#[test]
fn retry_success_unblocks_descendant_but_other_branch_keeps_running() {
    let fx = Fixture::parallel();
    // docs 分支先成功,保持终结;build 失败阻塞 package
    fx.complete("docs", "文档完成");
    fx.fail("build");
    assert_eq!(fx.status("package"), StepStatus::Blocked);
    assert_eq!(fx.status("docs"), StepStatus::Succeeded);

    fx.retry_and_complete("build");
    assert_eq!(fx.status("package"), StepStatus::Ready);
    // 另一分支未被重试波及
    assert_eq!(fx.status("docs"), StepStatus::Succeeded);
    assert_eq!(fx.attempts("build"), 2);
    fx.orch.stop();
}

#[test]
fn downstream_unlocks_only_after_settlement_and_handoff() {
    let fx = Fixture::parallel();
    fx.fail("docs");
    fx.complete("build", "构建完成");
    // docs 失败:package 仍阻塞
    assert_eq!(fx.status("package"), StepStatus::Blocked);

    fx.retry_and_complete("docs");
    fx.wait_status("package", StepStatus::Ready);
    // 成功结算伴随 Handoff 落库(原子同事务)
    let handoffs = fx.orch.store.list_handoffs(fx.task_id()).unwrap();
    assert!(
        handoffs.iter().any(|(_, h)| h.summary.contains("重试成功")),
        "应存在结算 Handoff: {handoffs:?}"
    );
    // 幂等结算不产生重复 Handoff
    let before = handoffs.len();
    let token = fx
        .host
        .token_of("docs", &fx.orch.store, fx.revision_id)
        .unwrap();
    let outcome = fx
        .orch
        .settle_by_token(
            &token,
            Settlement::Complete {
                summary: "重试成功".into(),
            },
        )
        .unwrap();
    assert_eq!(outcome, SettleOutcome::AlreadyApplied);
    assert_eq!(
        fx.orch.store.list_handoffs(fx.task_id()).unwrap().len(),
        before
    );
    fx.orch.stop();
}

// ---------- 重试模式 ----------

#[test]
fn continue_session_requires_live_session() {
    let fx = Fixture::parallel();
    fx.fail("build");

    // 会话已死:ContinueSession 必须显式拒绝,建议 FreshSession
    let session_id = fx
        .orch
        .store
        .list_runs_of_step(fx.step_id("build"))
        .unwrap()
        .last()
        .unwrap()
        .session_id
        .unwrap();
    fx.orch
        .store
        .update_session(session_id, Some(SessionStatus::Dead), None, None)
        .unwrap();
    let err = fx
        .orch
        .retry_step(fx.step_id("build"), RetryMode::ContinueSession)
        .unwrap_err();
    assert!(
        err.to_string().contains("存活") || err.to_string().contains("FreshSession"),
        "{err}"
    );

    // FreshSession 对死会话也可用
    fx.orch
        .retry_step(fx.step_id("build"), RetryMode::FreshSession)
        .unwrap();
    fx.orch.stop();
}

#[test]
fn continue_session_attaches_the_same_live_session() {
    let fx = Fixture::parallel();
    fx.fail("build");
    // 会话仍存活(Host 未上报死亡):继续会话合法,重派发必须附加同一会话
    let first = fx
        .host
        .launches
        .lock()
        .iter()
        .find(|s| s.step_id == fx.step_id("build"))
        .cloned()
        .unwrap();
    fx.orch
        .retry_step(fx.step_id("build"), RetryMode::ContinueSession)
        .unwrap();
    let attached = wait_until(Duration::from_secs(5), || {
        fx.host
            .launches
            .lock()
            .iter()
            .any(|s| s.step_id == fx.step_id("build") && s.attach_existing_session)
    });
    assert!(attached, "重派发应附加既有会话");
    let second = fx
        .host
        .launches
        .lock()
        .iter()
        .filter(|s| s.step_id == fx.step_id("build"))
        .map(|s| s.session_id)
        .collect::<Vec<_>>();
    assert!(second.iter().all(|id| *id == first.session_id));
    fx.orch.stop();
}

#[test]
fn exhausted_retries_leave_failed_and_descendants_blocked() {
    let fx = Fixture::parallel();
    fx.complete("docs", "完成");
    fx.fail("build");
    fx.orch
        .retry_step(fx.step_id("build"), RetryMode::FreshSession)
        .unwrap();
    fx.wait_running(&["build"]);
    fx.fail("build");
    // 重试耗尽:build 保持 failed,package 保持 blocked,任务 needs-you
    fx.wait_status("build", StepStatus::Failed);
    assert_eq!(fx.status("package"), StepStatus::Blocked);
    let task = fx.orch.store.task_view(fx.task_id()).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::NeedsYou);
    fx.orch.stop();
}

// ---------- 自动重试 ----------

#[test]
fn automatic_retry_creates_fresh_session_until_limit() {
    let fx = Fixture::parallel();
    fx.orch
        .store
        .set_step_auto_retry(fx.step_id("build"), 1)
        .unwrap();

    // 第一次失败:自动重试(attempts=1 ≤ limit=1)→ 新会话再跑
    fx.fail("build");
    let ok = wait_until(Duration::from_secs(5), || fx.attempts("build") >= 2);
    assert!(ok, "自动重试应创建第二次尝试");
    fx.wait_running(&["build"]);

    // 第二次失败:耗尽 → failed + 阻塞下游,不进入自动循环
    fx.fail("build");
    fx.wait_status("build", StepStatus::Failed);
    assert_eq!(fx.attempts("build"), 2);
    assert_eq!(fx.status("package"), StepStatus::Blocked);
    fx.orch.stop();
}

#[test]
fn automatic_retry_success_unblocks_downstream() {
    let fx = Fixture::parallel();
    fx.orch
        .store
        .set_step_auto_retry(fx.step_id("build"), 2)
        .unwrap();
    fx.complete("docs", "完成");

    fx.fail("build"); // 自动重试 #1
    fx.wait_running(&["build"]);
    fx.complete("build", "自动重试成功"); // 成功 → 下游解锁

    fx.wait_status("package", StepStatus::Ready);
    fx.orch.stop();
}

// ---------- 跳过与取消(回归保护)----------

#[test]
fn skip_unblocks_descendants_and_converges() {
    let fx = Fixture::parallel();
    fx.fail("build");
    fx.orch.skip_step(fx.step_id("build"), true).unwrap();
    fx.wait_status("package", StepStatus::Blocked); // docs 还在跑
    fx.complete("docs", "完成");
    fx.wait_status("package", StepStatus::Ready);
    fx.orch.stop();
}
