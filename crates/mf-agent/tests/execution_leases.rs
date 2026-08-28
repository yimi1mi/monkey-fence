//! Execution Directory Provider 接缝(Orchestration Task 4):
//! lease 生命周期 = 派发前 acquire+落库 → 终态结算/取消后 release,
//! 未知状态(awaiting-outcome / interrupted)保持持有;
//! 自动重试保留文件修改 → 不释放,新会话在同一租约目录重跑。

use crossbeam_channel::Sender;
use mf_agent::execution_directory::{
    ExecutionDirectoryProvider, ExecutionLease, LeaseContext, MergeOutcome,
};
use mf_agent::model::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog};
use mf_agent::pipeline::{PipelineDraft, SessionPolicy, StepDraft};
use mf_agent::runtime::{LaunchSpec, RuntimeEvent, RuntimeHost};
use mf_agent::store::Store;
use mf_agent::{AdHocLaunchSpec, RetryMode};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------- Recording Provider ----------

#[derive(Default)]
struct RecordingProvider {
    acquired: Mutex<Vec<String>>,
    released: Mutex<Vec<String>>,
    merged: Mutex<Vec<Vec<String>>>,
    isolated: bool,
    attempts: AtomicU32,
}

impl RecordingProvider {
    fn shared(isolated: bool) -> Arc<RecordingProvider> {
        Arc::new(RecordingProvider {
            isolated,
            ..Default::default()
        })
    }

    fn released_ids(&self) -> Vec<String> {
        self.released.lock().clone()
    }

    fn acquired_ids(&self) -> Vec<String> {
        self.acquired.lock().clone()
    }
}

impl ExecutionDirectoryProvider for RecordingProvider {
    fn id(&self) -> &str {
        "recording"
    }

    fn acquire(&self, ctx: &LeaseContext) -> anyhow::Result<ExecutionLease> {
        let n = self.attempts.fetch_add(1, Ordering::SeqCst);
        let id = format!("lease-{}-{}-{}", ctx.task_id, ctx.step_key, n + 1);
        self.acquired.lock().push(id.clone());
        Ok(ExecutionLease {
            id,
            path: ctx.project_root.clone(),
            isolated: self.isolated,
            provider: "recording".into(),
            metadata: serde_json::json!({ "attempt": n + 1 }),
        })
    }

    fn merge(&self, leases: &[ExecutionLease]) -> anyhow::Result<MergeOutcome> {
        self.merged
            .lock()
            .push(leases.iter().map(|l| l.id.clone()).collect());
        Ok(MergeOutcome::Merged)
    }

    fn release(&self, lease: &ExecutionLease) -> anyhow::Result<()> {
        self.released.lock().push(lease.id.clone());
        Ok(())
    }
}

// ---------- Mock Host ----------

#[derive(Default)]
struct MockHost {
    launches: Mutex<Vec<LaunchSpec>>,
    senders: Mutex<HashMap<String, Sender<(i64, RuntimeEvent)>>>,
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

// ---------- Fixture ----------

struct Fixture {
    orch: Arc<Orchestrator>,
    host: Arc<MockHost>,
    provider: Arc<RecordingProvider>,
}

impl Fixture {
    fn with(provider: Arc<RecordingProvider>) -> Fixture {
        let store = Store::memory().unwrap();
        let host = Arc::new(MockHost::default());
        let orch = Orchestrator::start(
            store,
            PathBuf::from("."),
            mf_agent::Config::default(),
            host.clone(),
            mock_profiles(),
            GlobalLimiter::new(4),
            "test-pipe".into(),
            provider.clone(),
        )
        .unwrap();
        Fixture {
            orch,
            host,
            provider,
        }
    }

    fn single_step() -> Fixture {
        Self::with(RecordingProvider::shared(false))
    }

    fn run_task(&self) -> i64 {
        let task = self.orch.create_task("t", "g").unwrap();
        let draft = PipelineDraft {
            steps: vec![step("only")],
        };
        self.orch.save_pipeline(task.id, &draft).unwrap();
        self.orch.confirm_and_run(task.id).unwrap();
        self.wait_running();
        task.id
    }

    fn steps(&self) -> Vec<StepView> {
        let task = self.orch.tasks().unwrap().remove(0);
        self.orch.store.task_steps(task.id).unwrap()
    }

    fn wait_running(&self) {
        let ok = wait_until(5_000, || {
            self.steps()
                .iter()
                .all(|s| matches!(s.status, StepStatus::Running))
        });
        assert!(ok, "等待步骤进入 running 超时");
    }

    fn token_of_only_step(&self) -> String {
        let launches = self.host.launches.lock();
        launches.last().unwrap().capability_token.clone()
    }

    fn complete_one_run(&self) {
        let token = self.token_of_only_step();
        self.orch
            .settle_by_token(
                &token,
                Settlement::Complete {
                    summary: "完成".into(),
                },
            )
            .unwrap();
        let ok = wait_until(5_000, || !self.provider.released_ids().is_empty());
        assert!(ok, "终态结算后应释放租约");
    }

    fn fail_one_run(&self) {
        let token = self.token_of_only_step();
        self.orch
            .settle_by_token(
                &token,
                Settlement::Fail {
                    reason: "失败".into(),
                },
            )
            .unwrap();
    }
}

fn step(key: &str) -> StepDraft {
    StepDraft {
        key: key.into(),
        title: key.into(),
        instructions: String::new(),
        agent_profile: "mock".into(),
        session_policy: SessionPolicy::Fresh,
        deps: vec![],
    }
}

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

fn wait_until(timeout_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(timeout_ms) {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

// ---------- 生命周期 ----------

#[test]
fn lease_is_released_after_terminal_run() {
    let fixture = Fixture::single_step();
    fixture.run_task();
    fixture.complete_one_run();
    assert_eq!(
        fixture.provider.released_ids(),
        fixture.provider.acquired_ids()
    );
    fixture.orch.stop();
}

#[test]
fn failed_run_releases_lease_too() {
    let fixture = Fixture::single_step();
    fixture.run_task();
    fixture.fail_one_run();
    let ok = wait_until(5_000, || !fixture.provider.released_ids().is_empty());
    assert!(ok, "失败终态也应释放租约");
    assert_eq!(fixture.provider.released_ids().len(), 1);
    fixture.orch.stop();
}

#[test]
fn awaiting_outcome_keeps_lease_held() {
    let fixture = Fixture::single_step();
    fixture.run_task();
    // 进程退出但未结算 → awaiting-outcome(未知状态):租约必须保持
    let run_id = fixture.host.launches.lock().last().unwrap().run_id;
    fixture
        .orch
        .push_runtime_event(run_id, RuntimeEvent::Exited { code: Some(0) });
    let ok = wait_until(5_000, || {
        fixture
            .steps()
            .first()
            .map(|s| s.status == StepStatus::AwaitingOutcome)
            .unwrap_or(false)
    });
    assert!(ok, "退出未结算应进入 awaiting-outcome");
    assert!(
        fixture.provider.released_ids().is_empty(),
        "未知状态必须保持租约持有"
    );
    fixture.orch.stop();
}

#[test]
fn cancelled_run_releases_lease() {
    let fixture = Fixture::single_step();
    let task_id = fixture.run_task();
    fixture.orch.cancel_task(task_id).unwrap();
    let ok = wait_until(5_000, || !fixture.provider.released_ids().is_empty());
    assert!(ok, "取消运行应释放租约");
    fixture.orch.stop();
}

#[test]
fn auto_retry_preserves_lease_for_next_attempt() {
    let fixture = Fixture::single_step();
    let task_id = fixture.run_task();
    let step_id = fixture.steps()[0].id;
    fixture.orch.store.set_step_auto_retry(step_id, 1).unwrap();

    fixture.fail_one_run();
    // 自动重试:新会话重跑,租约不释放(保留文件修改)
    let ok = wait_until(5_000, || {
        fixture.steps()[0].attempts >= 2 && matches!(fixture.steps()[0].status, StepStatus::Running)
    });
    assert!(ok, "自动重试应再次派发");
    assert!(
        fixture.provider.released_ids().is_empty(),
        "自动重试必须保持租约"
    );

    // 第二次成功 → 释放一次(同一租约)
    let token = fixture.token_of_only_step();
    fixture
        .orch
        .settle_by_token(
            &token,
            Settlement::Complete {
                summary: "重试成功".into(),
            },
        )
        .unwrap();
    let ok = wait_until(5_000, || !fixture.provider.released_ids().is_empty());
    assert!(ok);
    assert_eq!(
        fixture.provider.acquired_ids().len(),
        1,
        "自动重试复用同一租约"
    );
    assert_eq!(
        fixture.provider.released_ids(),
        fixture.provider.acquired_ids()
    );
    let _ = task_id;
    fixture.orch.stop();
}

#[test]
fn lease_persisted_and_launch_runs_in_lease_directory() {
    let fixture = Fixture::single_step();
    fixture.run_task();
    // 派发前已落库
    let task = fixture.orch.tasks().unwrap().remove(0);
    let leases = fixture.orch.store.list_execution_leases(task.id).unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].status, "held");
    assert_eq!(leases[0].provider, "recording");

    // LaunchSpec 的工作目录取自租约路径
    let launches = fixture.host.launches.lock();
    assert_eq!(launches.last().unwrap().workdir, PathBuf::from("."));
    drop(launches);
    fixture.complete_one_run();
    let leases = fixture.orch.store.list_execution_leases(task.id).unwrap();
    assert_eq!(leases[0].status, "released");
    fixture.orch.stop();
}

#[test]
fn project_directory_provider_defaults_to_project_root() {
    let provider = mf_agent::execution_directory::ProjectDirectoryProvider::default();
    assert_eq!(provider.id(), "project-dir");
    let lease = provider
        .acquire(&LeaseContext {
            task_id: 1,
            step_id: 2,
            attempt: 1,
            project_root: PathBuf::from("C:/proj"),
            step_key: "build".into(),
        })
        .unwrap();
    assert_eq!(lease.path, PathBuf::from("C:/proj"));
    assert!(!lease.isolated, "项目目录不隔离,并行需风险开关");
    assert!(matches!(
        provider.merge(&[lease.clone()]).unwrap(),
        MergeOutcome::NotRequired
    ));
    provider.release(&lease).unwrap();
}
