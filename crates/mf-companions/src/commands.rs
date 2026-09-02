//! launcher/tray/picker companion 骨架与安全退出(T6c,Issue #49;spec §11)。
//!
//! companions 是**不拥有 Core** 的薄进程:launcher 的 `start/open/status/
//! stop`、tray 摘要与显式 Exit、picker 返回 Core 后端 opaque project
//! handle。浏览器/tray 关闭**不停止 Core**(只有显式安全退出经
//! ShutdownAssessment 确认);start 幂等(Core 已存在 → 只转发 open);
//! 无自启动;tray 崩溃不影响 Core。

use std::time::Duration;

use mf_kernel::shutdown::{ShutdownAssessment, ShutdownIntent};

/// companion 命令结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanionCommand {
    Start,
    Open { project: Option<String> },
    Status,
    Stop { force: bool },
}

/// 命令处置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanionOutcome {
    /// Core 已在运行:start 幂等(不重启;open 转发)。
    AlreadyRunning { pid: u32, port: Option<u16> },
    /// Core 未运行:start 拉起(影子阶段由宿主进程承载)。
    Started { pid: u32 },
    /// open 转发给存活 Core(败者/客户端不自行打开第二实例)。
    OpenForwarded,
    /// status 摘要。
    Status {
        running: bool,
        pid: Option<u32>,
        port: Option<u16>,
    },
    /// stop:安全退出评估。无阻塞 → 确认退出;有阻塞 → 需要用户确认
    /// (freeze/drain/forced-kill 三级;§11.4)。
    StopAssessment(StopDecision),
    /// Core 未运行时的 stop/status 幂等 no-op。
    NotRunning,
}

/// 安全退出决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDecision {
    /// 无阻塞项:立即安全退出。
    Safe,
    /// 有阻塞项(活动 Run/Session/Installation/未发布事件):展示
    /// 确认;用户确认后 freeze→drain;超时 → forced kill(grace 后)。
    NeedsConfirmation { blockers: Vec<String> },
    /// forced:跳过 drain,forced_kill_grace 后强杀(§11.4)。
    ForcedKill { grace_ms: u64 },
}

/// Core 存活观察(注入;生产读 discovery 文件 + liveness)。
pub trait CorePresence: Send + Sync {
    fn running(&self) -> Option<(u32, Option<u16>)>;
}

/// companion 命令处置器(无状态;全部判定可测)。
pub struct CompanionDispatcher;

impl CompanionDispatcher {
    pub fn dispatch(
        command: CompanionCommand,
        presence: &dyn CorePresence,
        assessment: &dyn Fn() -> ShutdownAssessment,
    ) -> CompanionOutcome {
        let running = presence.running();
        match command {
            CompanionCommand::Start => match running {
                // 幂等:不重启、不杀,只报告
                Some((pid, port)) => CompanionOutcome::AlreadyRunning { pid, port },
                None => {
                    // 影子阶段:由宿主启动(此处判定层只表达意图)
                    CompanionOutcome::Started { pid: 0 }
                }
            },
            CompanionCommand::Open { .. } => match running {
                Some(_) => CompanionOutcome::OpenForwarded,
                None => CompanionOutcome::NotRunning,
            },
            CompanionCommand::Status => match running {
                Some((pid, port)) => CompanionOutcome::Status {
                    running: true,
                    pid: Some(pid),
                    port,
                },
                None => CompanionOutcome::Status {
                    running: false,
                    pid: None,
                    port: None,
                },
            },
            CompanionCommand::Stop { force } => match running {
                None => CompanionOutcome::NotRunning,
                Some(_) => {
                    if force {
                        return CompanionOutcome::StopAssessment(StopDecision::ForcedKill {
                            grace_ms: 10_000,
                        });
                    }
                    let assessment = assessment();
                    if assessment.safe_to_proceed && assessment.blockers.is_empty() {
                        CompanionOutcome::StopAssessment(StopDecision::Safe)
                    } else {
                        CompanionOutcome::StopAssessment(StopDecision::NeedsConfirmation {
                            blockers: assessment.blockers.clone(),
                        })
                    }
                }
            },
        }
    }
}

/// tray 状态摘要(Root 红标/活动对象;崩溃不影响 Core——tray 只是观察者)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TraySummary {
    pub core_running: bool,
    pub active_runs: usize,
    pub active_sessions: usize,
    pub active_installations: usize,
    pub root_mode: bool,
    pub needs_you: usize,
}

impl TraySummary {
    /// Root 红标条件(§10.1 UI 持续红色提示)。
    pub fn root_indicator(&self) -> bool {
        self.root_mode
    }

    /// 活动对象清单(stop 确认与 tray 摘要共用)。
    pub fn active_objects(&self) -> Vec<String> {
        let mut items = Vec::new();
        if self.active_runs > 0 {
            items.push(format!("{} 个运行中的 Workflow Run", self.active_runs));
        }
        if self.active_sessions > 0 {
            items.push(format!("{} 个存活 Agent Session", self.active_sessions));
        }
        if self.active_installations > 0 {
            items.push(format!("{} 个进行中安装", self.active_installations));
        }
        if self.needs_you > 0 {
            items.push(format!("{} 个 Needs You", self.needs_you));
        }
        items
    }
}

/// picker 决策:只返回 Core 后端 opaque project handle(不触文件系统/
/// 不做第二套项目列表;§11.3)。
pub fn picker_project_handle(projects: &[(String, String)], query: &str) -> Vec<String> {
    let query = query.trim().to_lowercase();
    projects
        .iter()
        .filter(|(handle, name)| {
            query.is_empty()
                || name.to_lowercase().contains(&query)
                || handle.to_lowercase().contains(&query)
        })
        .map(|(handle, _)| handle.clone())
        .collect()
}

/// 静默升级不触发退出:companions 的 stop 只来自显式用户命令
/// (`CompanionCommand::Stop`);升级/Bridge 场景不产生该命令,内核的
/// `ShutdownIntent::Assess` 只是评估(不改状态)。
pub fn stop_requires_explicit_user_intent(command: Option<&CompanionCommand>) -> bool {
    matches!(command, Some(CompanionCommand::Stop { .. }))
}

/// 供 tray/stop 展示的冻结/强杀超时(§11.4 常量)。
pub const DRAIN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);
pub const FORCED_KILL_GRACE: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;
    use mf_kernel::shutdown::ShutdownAssessment;

    struct StaticPresence(Option<(u32, Option<u16>)>);
    impl CorePresence for StaticPresence {
        fn running(&self) -> Option<(u32, Option<u16>)> {
            self.0
        }
    }

    fn empty_assessment() -> ShutdownAssessment {
        ShutdownAssessment {
            safe_to_proceed: true,
            ..Default::default()
        }
    }

    fn blocked_assessment() -> ShutdownAssessment {
        ShutdownAssessment {
            safe_to_proceed: false,
            blockers: vec!["2 个存活 Agent Session".into()],
            pending_outbox_events: 3,
            ..Default::default()
        }
    }

    #[test]
    fn start_is_idempotent_when_core_running() {
        let presence = StaticPresence(Some((4242, Some(51234))));
        match CompanionDispatcher::dispatch(CompanionCommand::Start, &presence, &empty_assessment) {
            CompanionOutcome::AlreadyRunning { pid, port } => {
                assert_eq!((pid, port), (4242, Some(51234)));
            }
            other => panic!("Core 已存在时 start 幂等:{other:?}"),
        }
    }

    #[test]
    fn browser_or_tray_close_never_stops_core() {
        // 关闭事件不是 stop 命令:唯一 stop 路径是显式命令
        let stop = CompanionCommand::Stop { force: false };
        assert!(stop_requires_explicit_user_intent(Some(&stop)));
        // 升级/关闭/其他一切路径不触发
        assert!(!stop_requires_explicit_user_intent(None));
        assert!(!stop_requires_explicit_user_intent(Some(
            &CompanionCommand::Status
        )));
        let _ = ShutdownIntent::Assess; // 内核意图仅评估,不退出
    }

    #[test]
    fn stop_needs_confirmation_when_active_objects() {
        let presence = StaticPresence(Some((4242, None)));
        match CompanionDispatcher::dispatch(
            CompanionCommand::Stop { force: false },
            &presence,
            &blocked_assessment,
        ) {
            CompanionOutcome::StopAssessment(StopDecision::NeedsConfirmation { blockers }) => {
                assert_eq!(blockers, vec!["2 个存活 Agent Session".to_string()]);
            }
            other => panic!("活动对象需确认:{other:?}"),
        }
        // 无阻塞 → Safe
        match CompanionDispatcher::dispatch(
            CompanionCommand::Stop { force: false },
            &presence,
            &empty_assessment,
        ) {
            CompanionOutcome::StopAssessment(StopDecision::Safe) => {}
            other => panic!("无阻塞直接安全退出:{other:?}"),
        }
        // force → ForcedKill(带 grace)
        match CompanionDispatcher::dispatch(
            CompanionCommand::Stop { force: true },
            &presence,
            &blocked_assessment,
        ) {
            CompanionOutcome::StopAssessment(StopDecision::ForcedKill { grace_ms }) => {
                assert_eq!(grace_ms, 10_000);
            }
            other => panic!("{other:?}"),
        }
        // Core 未运行 → no-op(不删 discovery/owner state)
        let absent = StaticPresence(None);
        assert_eq!(
            CompanionDispatcher::dispatch(
                CompanionCommand::Stop { force: false },
                &absent,
                &empty_assessment
            ),
            CompanionOutcome::NotRunning
        );
    }

    #[test]
    fn tray_summary_and_root_indicator() {
        let summary = TraySummary {
            core_running: true,
            active_runs: 2,
            active_sessions: 3,
            active_installations: 1,
            root_mode: true,
            needs_you: 1,
        };
        assert!(summary.root_indicator(), "Root 红标");
        let items = summary.active_objects();
        assert_eq!(items.len(), 4, "活动对象清单:{items:?}");
        assert!(!TraySummary::default().root_indicator());
    }

    #[test]
    fn picker_returns_only_opaque_handles() {
        let projects = vec![
            ("proj_aaa".to_string(), "工作台".to_string()),
            ("proj_bbb".to_string(), "MonkeyFence".to_string()),
        ];
        assert_eq!(
            picker_project_handle(&projects, ""),
            vec!["proj_aaa", "proj_bbb"]
        );
        assert_eq!(picker_project_handle(&projects, "monkey"), vec!["proj_bbb"]);
        assert_eq!(picker_project_handle(&projects, "proj_a"), vec!["proj_aaa"]);
        assert!(picker_project_handle(&projects, "不存在").is_empty());
    }
}
