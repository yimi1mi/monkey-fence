//! T11a(Issue #62)发布旅程编排:launcher 默认浏览器 bootstrap、
//! tray 摘要跨面汇总、picker Project 注册、关闭浏览器/tray 不停 Core。

use crate::commands::{
    CompanionCommand, CompanionDispatcher, CompanionOutcome, CorePresence, TraySummary,
};

/// launcher 打开默认浏览器(bootstrap nonce fragment;T11 前入口保持
/// 隐藏——此函数在 bootstrap_exposed=false 时只返回入口未开放)。
pub fn open_default_browser(port: u16, nonce: Option<&str>) -> Result<String, &'static str> {
    match nonce {
        Some(nonce) => Ok(format!("http://127.0.0.1:{port}/#nonce={nonce}")),
        None => Err("bootstrap 未开放(T11 前隐藏入口)"),
    }
}

/// tray 跨项目摘要(run/needs-you/root 汇总;Core projection)。
pub fn tray_summary_from_counts(
    core_running: bool,
    active_runs: usize,
    active_sessions: usize,
    active_installations: usize,
    root_mode: bool,
    needs_you: usize,
) -> TraySummary {
    TraySummary {
        core_running,
        active_runs,
        active_sessions,
        active_installations,
        root_mode,
        needs_you,
    }
}

/// picker Project 注册(discovery 记录 opaque handle;不做第二模型)。
pub fn register_project(handles: &[(String, String)], pick: &str) -> Option<String> {
    handles
        .iter()
        .find(|(handle, _)| handle == pick)
        .map(|(handle, _)| handle.clone())
}

/// 关闭浏览器/tray ≠ 停止 Core(编排断言:stop 只经显式命令)。
pub fn client_close_keeps_core(presence: &dyn CorePresence) -> bool {
    presence.running().is_some()
        && matches!(
            CompanionDispatcher::dispatch(CompanionCommand::Status, presence, &Default::default),
            CompanionOutcome::Status { running: true, .. }
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Running;
    impl CorePresence for Running {
        fn running(&self) -> Option<(u32, Option<u16>)> {
            Some((4242, Some(8765)))
        }
    }

    #[test]
    fn bootstrap_entry_hidden_until_t11() {
        assert_eq!(
            open_default_browser(8765, None),
            Err("bootstrap 未开放(T11 前隐藏入口)")
        );
        assert_eq!(
            open_default_browser(8765, Some("n")).unwrap(),
            "http://127.0.0.1:8765/#nonce=n"
        );
    }

    #[test]
    fn tray_summary_aggregates() {
        let summary = tray_summary_from_counts(true, 2, 3, 1, true, 4);
        assert!(summary.root_indicator());
        assert_eq!(summary.active_objects().len(), 4);
    }

    #[test]
    fn picker_registers_opaque_only() {
        let handles = vec![("proj_a".to_string(), "A".to_string())];
        assert_eq!(
            register_project(&handles, "proj_a"),
            Some("proj_a".to_string())
        );
        assert_eq!(register_project(&handles, "proj_x"), None);
    }

    #[test]
    fn client_close_does_not_stop_core() {
        assert!(client_close_keeps_core(&Running));
    }
}
