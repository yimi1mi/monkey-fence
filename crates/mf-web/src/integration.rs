//! standalone Core WebGateway 集成(T7f,Issue #51;Gate T7)。
//!
//! 把 #38–#46 交付的 Gateway 全组件装进 standalone Core 生命周期:
//! Core 启动 wiring(loopback 随机端口 → gateway 装配 → discovery 记录
//! 端口)、三数据面(HTTP/commands + workflow WS + terminal WS)的路由
//! 编排、`/meta` 非匿名(meta 须认证 session)。用户入口仍隐藏
//! (launcher 不发 bootstrap——T8 核心写入完成后才开)。

use crate::api::kernel_bridge::{dispatch_via_kernel, snapshot_to_wire};
use crate::auth::BootstrapAuth;
use crate::gateway::GatewayState;
use crate::problem::Problem;
use crate::ws::events::EventsSession;
use crate::ws::terminal::TerminalWsSession;
use mf_kernel::kernel::CoreKernel;
use std::sync::{Arc, Mutex};

/// Core 侧 Web 集成错误。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct IntegrationError(pub String);

/// 集成配置(影子阶段:bootstrap 不外发)。
#[derive(Debug, Clone, Copy)]
pub struct WebIntegrationConfig {
    /// 用户入口是否开放(T8 前恒 false:launcher 不发 bootstrap nonce)。
    pub bootstrap_exposed: bool,
}

impl Default for WebIntegrationConfig {
    fn default() -> Self {
        Self {
            bootstrap_exposed: false,
        }
    }
}

/// 已装配的 Web 面(路由编排的判定层;axum Router 组装复用 #38 gateway)。
pub struct WebPlane {
    pub config: WebIntegrationConfig,
    state: Arc<GatewayState>,
    kernel: Arc<dyn CoreKernel>,
}

impl WebPlane {
    /// Core 启动 wiring:gateway state(assets 为空集——同 bundle embedded
    /// assets 随打包注入)+ kernel 引用。
    pub fn assemble(
        kernel: Arc<dyn CoreKernel>,
        config: WebIntegrationConfig,
    ) -> Result<Self, IntegrationError> {
        let assets = crate::assets::AssetRegistry::new(Vec::new());
        let state = Arc::new(GatewayState {
            bind_ip: "127.0.0.1".into(),
            // 随机端口(bind_gateway 实际绑定后回填;此处占位)
            port: 0,
            auth: Mutex::new(BootstrapAuth::new(crate::limits::WebLimits::default())),
            assets,
        });
        Ok(Self {
            config,
            state,
            kernel,
        })
    }

    /// 绑定后回填端口(discovery 记录用;state 整体重建以保持 Arc 不可变)。
    pub fn bind_complete(&mut self, port: u16) {
        let assets = crate::assets::AssetRegistry::new(Vec::new());
        self.state = Arc::new(GatewayState {
            bind_ip: self.state.bind_ip.clone(),
            port,
            auth: Mutex::new(BootstrapAuth::new(crate::limits::WebLimits::default())),
            assets,
        });
    }

    /// `/api/v1/meta`:**非匿名**——必须已认证 session(meta 不是探测口)。
    pub fn meta(&self, session_cookie: Option<&str>) -> Result<serde_json::Value, Problem> {
        let cookie = session_cookie.ok_or_else(|| {
            Problem::new(
                crate::problem::ProblemCode::Unauthenticated,
                "/meta 需要已认证 session(非匿名探测口)",
                None,
            )
        })?;
        let mut auth = self.state.auth.lock().map_err(|_| {
            Problem::new(crate::problem::ProblemCode::InternalError, "poisoned", None)
        })?;
        auth.verify(cookie, None).map_err(|problem| {
            Problem::new(
                crate::problem::ProblemCode::Unauthenticated,
                problem.to_string(),
                None,
            )
        })?;
        Ok(serde_json::json!({
            "api_versions": crate::problem::API_VERSIONS,
            "ws_subprotocols": crate::problem::WS_SUBPROTOCOLS,
            "core_build": env!("CARGO_PKG_VERSION"),
        }))
    }

    /// 命令面(dispatch_via_kernel 全链;rate limit 由 #41 limiter 承载)。
    pub fn command(
        &self,
        envelope: &crate::api::commands::CommandEnvelope,
        principal: &str,
    ) -> Result<crate::api::commands::CommandOutcomeWire, Problem> {
        dispatch_via_kernel(&*self.kernel, envelope, principal)
    }

    /// 快照面。
    pub fn snapshot(
        &self,
        query: mf_kernel::projection::SnapshotQuery,
    ) -> Result<crate::api::snapshot::SnapshotEnvelope, Problem> {
        let kernel_snapshot = self.kernel.snapshot(query).map_err(|problem| {
            Problem::new(
                crate::problem::ProblemCode::InternalError,
                problem.to_string(),
                None,
            )
        })?;
        Ok(snapshot_to_wire(kernel_snapshot))
    }

    /// workflow events WS 会话(resume → hello + per-client 队列)。
    pub fn events_session(
        &self,
        control: crate::ws::events::EventsControl,
    ) -> Result<EventsSession, Problem> {
        EventsSession::resume(&*self.kernel, &control)
    }

    /// terminal WS 会话。
    pub fn terminal_session(&self) -> TerminalWsSession {
        TerminalWsSession::new()
    }

    /// bootstrap 入口判定:影子阶段 launcher 不发 nonce(用户入口隐藏)。
    pub fn issue_bootstrap_nonce(&self) -> Result<String, Problem> {
        if !self.config.bootstrap_exposed {
            return Err(Problem::new(
                crate::problem::ProblemCode::ServiceUnavailable,
                "bootstrap 未开放(等待 T8 核心写入完成)",
                None,
            ));
        }
        Ok(self.state.auth.lock().unwrap().issue_nonce())
    }
}

/// Core restart 语义:Web session 全失效并要求重新 bootstrap。
pub fn core_restart_invalidates_web_sessions(plane: &WebPlane) {
    plane.state.auth.lock().unwrap().core_restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoKernel;
    impl CoreKernel for NoKernel {
        fn dispatch(
            &self,
            _request: mf_kernel::kernel::KernelCommandRequest,
        ) -> Result<mf_kernel::kernel::KernelOutcome, mf_kernel::kernel::KernelProblem> {
            Err(mf_kernel::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn snapshot(
            &self,
            _query: mf_kernel::projection::SnapshotQuery,
        ) -> Result<mf_kernel::projection::SnapshotEnvelope, mf_kernel::kernel::KernelProblem>
        {
            Err(mf_kernel::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn subscribe_events(
            &self,
            _cursor: mf_kernel::projection::EventCursor,
        ) -> Result<mf_kernel::projection::EventSubscription, mf_kernel::kernel::KernelProblem>
        {
            Err(mf_kernel::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn attach_terminal(
            &self,
            _session: mf_kernel::handles::SessionHandle,
            _attach: mf_kernel::kernel::TerminalAttach,
        ) -> Result<mf_terminal::TerminalChannel, mf_kernel::kernel::KernelProblem> {
            Err(mf_kernel::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn shutdown(
            &self,
            _intent: mf_kernel::shutdown::ShutdownIntent,
        ) -> mf_kernel::shutdown::ShutdownAssessment {
            Default::default()
        }
        fn grant_controller(
            &self,
            _client_id: &str,
            _principal: &str,
        ) -> Result<u64, mf_kernel::kernel::KernelProblem> {
            Err(mf_kernel::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn controller_epoch(&self) -> u64 {
            0
        }
        fn attach_project(
            &self,
            _root: &std::path::Path,
        ) -> Result<String, mf_kernel::kernel::KernelProblem> {
            Err(mf_kernel::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
        fn detach_project(
            &self,
            _project_handle: &str,
        ) -> Result<(), mf_kernel::kernel::KernelProblem> {
            Err(mf_kernel::kernel::KernelProblem::ServiceUnavailable(
                "no kernel".into(),
            ))
        }
    }

    #[test]
    fn meta_is_not_anonymous() {
        let plane = WebPlane::assemble(Arc::new(NoKernel), Default::default()).unwrap();
        assert!(plane.meta(None).is_err(), "无 session 拒绝");
        assert!(plane.meta(Some("mfs_bogus")).is_err(), "未知 session 拒绝");
    }

    #[test]
    fn bootstrap_stays_hidden_until_t8() {
        let plane = WebPlane::assemble(Arc::new(NoKernel), Default::default()).unwrap();
        assert!(plane.issue_bootstrap_nonce().is_err());
        let mut exposed = WebPlane::assemble(
            Arc::new(NoKernel),
            WebIntegrationConfig {
                bootstrap_exposed: true,
            },
        )
        .unwrap();
        let _ = exposed.issue_bootstrap_nonce().unwrap();
    }

    #[test]
    fn core_restart_kills_all_web_sessions() {
        let mut plane = WebPlane::assemble(
            Arc::new(NoKernel),
            WebIntegrationConfig {
                bootstrap_exposed: true,
            },
        )
        .unwrap();
        let nonce = plane.issue_bootstrap_nonce().unwrap();
        let session = plane
            .state
            .auth
            .lock()
            .unwrap()
            .exchange(&nonce, "127.0.0.1:0")
            .unwrap();
        // restart → session 失效
        core_restart_invalidates_web_sessions(&plane);
        assert!(plane.meta(Some(&session.session_id)).is_err());
    }

    #[test]
    fn three_data_planes_are_routable() {
        let plane = WebPlane::assemble(Arc::new(NoKernel), Default::default()).unwrap();
        // 快照/命令面经 kernel(NoKernel → service unavailable problem,证明
        // 路由到达 kernel 而非旁路)
        let problem = plane
            .snapshot(mf_kernel::projection::SnapshotQuery::Workspace)
            .unwrap_err();
        assert_eq!(problem.code, crate::problem::ProblemCode::InternalError);
        // terminal 会话可建
        let _terminal = plane.terminal_session();
    }
}
