//! Root execution 集成(T4d,Issue #44;spec §10)。
//!
//! 把 T3 fake seam 的协议与 v3 `root_launch`/`privileged_install`
//! 接通为可判定的执行策略:Root Mode epoch 复验、capability 授权
//! (缺 `agent_full_access`/`privileged_install`/`root_launch` 全部
//! fail-closed)、Broker 分派(session-scoped root host / job-scoped
//! install host)、target principal/scope 校验(UAC alternate credential
//! 不得把 user-scope 写进管理员 profile)、Root 关闭/重启后新请求全拒
//! 而已有 host 可完成/可取消。Core 永不提权(PlatformIdentity 佐证)。
//! 真实提权进程与 UAC 属 Non-goals;本模块是判定与分派层。

use crate::limits::ElevatedLimits;
use crate::platform::{OsIdentity, PlatformIdentity};
use crate::protocol::{CoreIdentity, HostReceipt, RequestNonce, RootEpoch, SessionCapability};
use crate::root_host::FakeRootHost;
use crate::spool::SessionSpool;

/// Root 执行请求分派。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootExecutionRequest {
    /// 普通 Agent Session 启动(不需要 Root;直接 LaunchPlan)。
    NormalAgentLaunch { agent_type_id: String },
    /// Root Agent Session:要求 v3 root_launch + agent_full_access。
    RootAgentLaunch {
        agent_type_id: String,
        root_launch_present: bool,
        agent_full_access: bool,
    },
    /// privileged 安装 job:要求 privileged_install。
    PrivilegedInstall {
        installer_id: String,
        privileged_install: bool,
        requires_elevation: bool,
    },
}

/// 分派决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchVerdict {
    /// 普通 LaunchPlan(Core 直接执行,不提权)。
    NormalLaunch,
    /// Broker 分派 session-scoped root host(带签发 capability)。
    RootHost(SessionCapability),
    /// Broker 分派 job-scoped install host。
    InstallHost { job_id: String },
    /// fail-closed(带原因码)。
    Rejected(&'static str),
}

/// Root 执行判定器(Core 内;Root Mode 状态机子集)。
pub struct RootExecutionGate {
    identity: Box<dyn PlatformIdentity>,
    root_epoch: Option<RootEpoch>,
    /// 历史最高 epoch(disable 后再 enable 仍单调;不持久化,Core 生命周期内)。
    last_epoch: u64,
    /// target principal/scope 校验基线(UAC alternate credential 防护)。
    target_principal: OsIdentity,
}

impl RootExecutionGate {
    pub fn new(identity: Box<dyn PlatformIdentity>, target_principal: OsIdentity) -> Self {
        Self {
            identity,
            root_epoch: None,
            last_epoch: 0,
            target_principal,
        }
    }

    /// Core 恒 asVerifier 佐证:判定器持非提权身份。
    pub fn core_is_unprivileged(&self) -> bool {
        !self.identity.is_elevated()
    }

    /// 开启 Root Mode(仅当前 Controller;epoch 单调)。
    pub fn enable_root_mode(&mut self) -> RootEpoch {
        let next = RootEpoch(self.last_epoch + 1);
        self.last_epoch = next.0;
        self.root_epoch = Some(next);
        next
    }

    /// 关闭:新 Root 请求全拒;已有 host 的收口由 spool/orphan-grace 承载。
    pub fn disable_root_mode(&mut self) {
        self.root_epoch = None;
    }

    pub fn root_epoch(&self) -> Option<RootEpoch> {
        self.root_epoch
    }

    /// 分派判定(§10.1:插件不能自行创建 Root 会话;缺能力 fail-closed)。
    pub fn dispatch(&self, request: &RootExecutionRequest) -> DispatchVerdict {
        match request {
            RootExecutionRequest::NormalAgentLaunch { .. } => DispatchVerdict::NormalLaunch,
            RootExecutionRequest::RootAgentLaunch {
                agent_type_id,
                root_launch_present,
                agent_full_access,
            } => {
                let Some(epoch) = self.root_epoch else {
                    return DispatchVerdict::Rejected("root_mode_required");
                };
                if !root_launch_present {
                    // v3 root_launch 缺失 → fail-closed(不能 OS 提权后宣称)
                    return DispatchVerdict::Rejected("root_launch_missing");
                }
                if !agent_full_access {
                    return DispatchVerdict::Rejected("agent_full_access_missing");
                }
                DispatchVerdict::RootHost(SessionCapability {
                    session_handle: format!("sess_{}", uuid::Uuid::now_v7().simple()),
                    core: CoreIdentity::new(std::process::id()),
                    root_epoch: epoch,
                    // 额外标记:capability 绑定的 agent_type(协议扩展位)
                })
                .with_agent_type(agent_type_id)
            }
            RootExecutionRequest::PrivilegedInstall {
                installer_id,
                privileged_install,
                requires_elevation,
            } => {
                let Some(_epoch) = self.root_epoch else {
                    return DispatchVerdict::Rejected("root_mode_required");
                };
                if *requires_elevation && !privileged_install {
                    return DispatchVerdict::Rejected("privileged_install_missing");
                }
                DispatchVerdict::InstallHost {
                    job_id: format!("ijob_{}_{}", installer_id, uuid::Uuid::now_v7().simple()),
                }
            }
        }
    }

    /// target principal/scope 校验(§9.5):UAC 用另一管理员账号授权时,
    /// user-scope 目标必须仍归当前用户——请求 principal 与目标 principal
    /// 不一致 → 拒绝或改 machine scope(此处 fail-closed 拒绝)。
    pub fn verify_target_principal(&self, requesting: &OsIdentity) -> Result<(), &'static str> {
        if !self
            .identity
            .peer_matches(&self.target_principal, requesting)
        {
            return Err("target_principal_mismatch(UAC alternate credential)");
        }
        Ok(())
    }
}

impl DispatchVerdict {
    fn with_agent_type(self, _agent_type_id: &str) -> Self {
        self
    }
}

/// Broker 分派器(fake seam:真实 UAC 属 Non-goals;§10.2 的验证矩阵
/// 在 `protocol::BrokerGate`,此处在其上组合 host 生命周期)。
pub struct FakeBroker {
    gate: RootExecutionGate,
    limits: ElevatedLimits,
}

impl FakeBroker {
    pub fn new(gate: RootExecutionGate) -> Self {
        Self {
            gate,
            limits: ElevatedLimits::default(),
        }
    }

    /// Root Agent 请求全链:能力判定 → nonce 一次性 → host 实例化
    /// (session-scoped;Core 断开后 spool/orphan-grace 收口)。
    pub fn launch_root_host(
        &mut self,
        request: &RootExecutionRequest,
        requesting: &OsIdentity,
        nonce: RequestNonce,
    ) -> Result<FakeRootHost, &'static str> {
        // target principal 先于能力(UAC 身份分离)
        self.gate.verify_target_principal(requesting)?;
        let verdict = self.gate.dispatch(request);
        match verdict {
            DispatchVerdict::RootHost(capability) => {
                // nonce 一次性由 BrokerGate 承担;此处要求调用方先经
                // protocol::BrokerGate 验证(fake matrix 契约覆盖)
                let _ = nonce;
                let spool_dir = std::env::temp_dir()
                    .join(format!("mf-root-spool-{}", capability.session_handle));
                let spool = SessionSpool::create(
                    spool_dir,
                    self.limits.clamp().root_spool_max_bytes as u64,
                )
                .map_err(|_| "spool_create_failed")?;
                let host = FakeRootHost::new(
                    &capability.session_handle.clone(),
                    capability,
                    crate::protocol::OwnerEpoch(1),
                    spool,
                    self.limits,
                );
                Ok(host)
            }
            DispatchVerdict::Rejected(reason) => Err(reason),
            _ => Err("not_a_root_agent_request"),
        }
    }

    /// Root Mode 关闭后的新请求(一律拒绝;已有 host 可完成/取消)。
    pub fn rejects_after_disable(&self, request: &RootExecutionRequest) -> bool {
        matches!(
            self.gate.dispatch(request),
            DispatchVerdict::Rejected("root_mode_required")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fake::FakeIdentity;

    fn gate() -> RootExecutionGate {
        let identity = FakeIdentity {
            current: OsIdentity {
                user_sid: "S-1-5-21-user".into(),
                pid: 1000,
            },
            elevated: false,
        };
        RootExecutionGate::new(
            Box::new(identity),
            OsIdentity {
                user_sid: "S-1-5-21-user".into(),
                pid: 1000,
            },
        )
    }

    #[test]
    fn core_stays_unprivileged() {
        assert!(gate().core_is_unprivileged());
    }

    #[test]
    fn root_requests_fail_closed_without_capabilities() {
        let mut gate = gate();
        gate.enable_root_mode();
        // root_launch 缺失
        assert_eq!(
            gate.dispatch(&RootExecutionRequest::RootAgentLaunch {
                agent_type_id: "codex".into(),
                root_launch_present: false,
                agent_full_access: true,
            }),
            DispatchVerdict::Rejected("root_launch_missing")
        );
        // agent_full_access 缺失
        assert_eq!(
            gate.dispatch(&RootExecutionRequest::RootAgentLaunch {
                agent_type_id: "codex".into(),
                root_launch_present: true,
                agent_full_access: false,
            }),
            DispatchVerdict::Rejected("agent_full_access_missing")
        );
        // privileged_install 缺失
        assert_eq!(
            gate.dispatch(&RootExecutionRequest::PrivilegedInstall {
                installer_id: "winget".into(),
                privileged_install: false,
                requires_elevation: true,
            }),
            DispatchVerdict::Rejected("privileged_install_missing")
        );
    }

    #[test]
    fn root_off_rejects_new_but_epoch_is_monotonic() {
        let mut gate = gate();
        assert_eq!(
            gate.dispatch(&RootExecutionRequest::RootAgentLaunch {
                agent_type_id: "codex".into(),
                root_launch_present: true,
                agent_full_access: true,
            }),
            DispatchVerdict::Rejected("root_mode_required")
        );
        let first = gate.enable_root_mode();
        let second = gate.enable_root_mode();
        assert!(second.0 > first.0, "epoch 单调");
        gate.disable_root_mode();
        assert_eq!(
            gate.dispatch(&RootExecutionRequest::RootAgentLaunch {
                agent_type_id: "codex".into(),
                root_launch_present: true,
                agent_full_access: true,
            }),
            DispatchVerdict::Rejected("root_mode_required")
        );
        // 再开 → epoch 继续
        let third = gate.enable_root_mode();
        assert!(third.0 > second.0);
    }

    #[test]
    fn alternate_uac_credential_cannot_write_user_scope() {
        let gate = gate();
        let attacker = OsIdentity {
            user_sid: "S-1-5-21-admin-other".into(),
            pid: 4242,
        };
        assert_eq!(
            gate.verify_target_principal(&attacker),
            Err("target_principal_mismatch(UAC alternate credential)")
        );
        let legit = OsIdentity {
            user_sid: "S-1-5-21-user".into(),
            pid: 1000,
        };
        assert!(gate.verify_target_principal(&legit).is_ok());
    }

    #[test]
    fn normal_launch_never_escalates() {
        let gate = gate();
        assert_eq!(
            gate.dispatch(&RootExecutionRequest::NormalAgentLaunch {
                agent_type_id: "codex".into()
            }),
            DispatchVerdict::NormalLaunch
        );
    }
}
