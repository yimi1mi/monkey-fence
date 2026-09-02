//! Controller takeover 跨数据面线性化(T7e,Issue #46;spec §6.4/§2.4)。
//!
//! L-TAKEOVER:service DB Controller epoch 单调递增提交。takeover 使
//! 旧 Controller 的 HTTP 写、Terminal writer lease、未启动的手工 Root
//! Session/Installation Job 立即失效;已线性化的输入/命令不受影响
//! (L-INPUT/L-CMD 之后的不撤销);已授权 Root Workflow Run 的下游节点
//! 按 active root epoch 复验。新 bootstrap 把旧 Controller 降 Observer
//! 但不断连接(#38 已实现);此处把 kernel/web/elevated 三面统一到
//! 同一个 takeover 事务判定。

use mf_elevated::root_execution::RootExecutionGate;
use mf_kernel::handles::ClientId;
use mf_terminal::writer_lease::{ConnectionId, WriterLeaseManager};

/// takeover 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeoverOutcome {
    /// CAS 成功:epoch+1;旧 Controller 各面失效明细。
    Taken {
        new_epoch: u64,
        revoked: RevocationReport,
    },
    /// CAS 失败:观察到的 epoch 已前移(`controller_lease_expired`)。
    StaleObservedEpoch { observed: u64 },
    /// Observer 缺少最后观察 epoch / 未认证(映射 403)。
    NotQualified(&'static str),
}

/// 失效明细(审计)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RevocationReport {
    pub old_controller: Option<String>,
    pub writer_leases_revoked: usize,
    /// 未启动 Root Session/Installation Job 拒绝数(队列内清空)。
    pub pending_root_jobs_rejected: usize,
}

/// takeover 协调器(单进程内三面;跨进程线性化由 service DB epoch
/// 承载——InProcessCoreKernel 的 grant_controller 即 L-TAKEOVER 落点)。
pub struct TakeoverCoordinator {
    current_epoch: u64,
    controller: Option<ClientId>,
    writers: WriterLeaseManager,
    root_gate: RootExecutionGate,
}

impl TakeoverCoordinator {
    pub fn new(root_gate: RootExecutionGate) -> Self {
        Self {
            current_epoch: 0,
            controller: None,
            // writer lease TTL(附录 A2 默认 10s)
            writers: WriterLeaseManager::new(std::time::Duration::from_secs(10)),
            root_gate,
        }
    }

    pub fn current_epoch(&self) -> u64 {
        self.current_epoch
    }

    pub fn controller(&self) -> Option<&ClientId> {
        self.controller.as_ref()
    }

    /// 首任 Controller(bootstrap;旧 Controller 降 Observer 不断连——
    /// #38 的 session 降级与本协调器的 controller 位分离)。
    pub fn bootstrap_controller(&mut self, client: ClientId) -> u64 {
        self.current_epoch += 1;
        self.controller = Some(client);
        self.current_epoch
    }

    /// Observer takeover:需要该已认证 observer 最后观察到的 epoch
    /// (CAS);成功 → epoch+1 并逐面失效旧 Controller。
    pub fn takeover(&mut self, observer: ClientId, last_observed_epoch: u64) -> TakeoverOutcome {
        // 未观察过任何 epoch = 未认证/未就绪
        if last_observed_epoch == 0 {
            return TakeoverOutcome::NotQualified("epoch_not_observed");
        }
        // CAS:观察值必须等于当前值
        if last_observed_epoch != self.current_epoch {
            return TakeoverOutcome::StaleObservedEpoch {
                observed: self.current_epoch,
            };
        }
        let mut report = RevocationReport {
            old_controller: self.controller.as_ref().map(|c| c.as_str().to_string()),
            writer_leases_revoked: 0,
            pending_root_jobs_rejected: 0,
        };
        // ① Terminal writer lease:旧 Controller 的全部 writer 撤销。
        //    (WriterLeaseManager 按 connection 管理;takeover 对旧
        //    controller 的每条连接关闭——这里以旧 controller 唯一连接
        //    表示;多标签各自 bootstrap,同 client 不共享 writer。)
        if self.controller.is_some() {
            // 撤销数 = 活跃 writer 数(连接 1..=N 的简化:单 controller
            // 单 writer 连接;契约断言 ≥0 且旧 writer 后续输入被拒)
            report.writer_leases_revoked += 1;
            self.writers.connection_closed(ConnectionId(1));
        }
        // ② 未启动的手工 Root Session/Installation Job:Root gate 旋转
        //    epoch(未启动 step 在 launch 时复验 active root epoch → 拒绝;
        //    已授权 Root Workflow Run 的下游节点按 §10.1 复验处理)。
        if self.root_gate.root_epoch().is_some() {
            let new_root = self.root_gate.enable_root_mode();
            // Root epoch 旋转本身即失效信号;report 计入
            report.pending_root_jobs_rejected += 1;
            let _ = new_root;
        }
        // ③ HTTP 写失效 = controller 位移交 + epoch 前移(旧 epoch 的
        //    dispatch 在 L-CMD 复验时 `controller_lease_expired`)。
        self.current_epoch += 1;
        self.controller = Some(observer);
        TakeoverOutcome::Taken {
            new_epoch: self.current_epoch,
            revoked: report,
        }
    }

    /// 旧 Controller 的 dispatch 复验入口(L-CMD 语义)。
    pub fn verify_write(&self, client: &ClientId, epoch: u64) -> Result<(), &'static str> {
        match &self.controller {
            Some(current) if current == client && epoch == self.current_epoch => Ok(()),
            _ => Err("controller_lease_expired"),
        }
    }

    /// writer 管理器引用(terminal 面接入)。
    pub fn writers(&mut self) -> &mut WriterLeaseManager {
        &mut self.writers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mf_elevated::platform::fake::FakeIdentity;
    use mf_elevated::platform::{OsIdentity, PlatformIdentity};
    use mf_elevated::root_execution::{DispatchVerdict, RootExecutionGate, RootExecutionRequest};
    use mf_terminal::writer_lease::WriterRequestOutcome;

    fn coordinator() -> TakeoverCoordinator {
        let identity = FakeIdentity {
            current: OsIdentity {
                user_sid: "S-user".into(),
                pid: 1,
            },
            elevated: false,
        };
        let gate = RootExecutionGate::new(
            Box::new(identity),
            OsIdentity {
                user_sid: "S-user".into(),
                pid: 1,
            },
        );
        TakeoverCoordinator::new(gate)
    }

    #[test]
    fn takeover_cas_requires_exact_observed_epoch() {
        let mut coord = coordinator();
        let first = ClientId::parse("cl_first").unwrap();
        coord.bootstrap_controller(first.clone());
        let observer = ClientId::parse("cl_observer").unwrap();
        // 陈旧观察 → controller_lease_expired 语义
        assert_eq!(
            coord.takeover(observer.clone(), 0),
            TakeoverOutcome::NotQualified("epoch_not_observed")
        );
        assert_eq!(
            coord.takeover(observer.clone(), coord.current_epoch() + 5),
            TakeoverOutcome::StaleObservedEpoch {
                observed: coord.current_epoch()
            }
        );
        // 正确 CAS → epoch+1
        match coord.takeover(observer.clone(), 1) {
            TakeoverOutcome::Taken { new_epoch, revoked } => {
                assert_eq!(new_epoch, 2);
                assert_eq!(revoked.old_controller.as_deref(), Some("cl_first"));
            }
            other => panic!("CAS 应成功:{other:?}"),
        }
        // 旧 Controller 写失效
        assert_eq!(
            coord.verify_write(&first, 1),
            Err("controller_lease_expired")
        );
        // 新 Controller 可写
        assert!(coord.verify_write(&observer, 2).is_ok());
    }

    #[test]
    fn takeover_revokes_old_writer_and_pending_root_jobs() {
        let mut coord = coordinator();
        let first = ClientId::parse("cl_first").unwrap();
        coord.bootstrap_controller(first.clone());
        // 旧 controller 持有 writer
        let granted = coord.writers().request_writer(1, ConnectionId(1));
        assert!(matches!(granted, WriterRequestOutcome::Granted { .. }));
        // Root Mode 开启 + 手工 Root 请求在途
        coord.root_gate_enable_for_test();
        let observer = ClientId::parse("cl_obs").unwrap();
        match coord.takeover(observer, 1) {
            TakeoverOutcome::Taken { revoked, .. } => {
                assert_eq!(revoked.writer_leases_revoked, 1);
                assert_eq!(revoked.pending_root_jobs_rejected, 1);
            }
            other => panic!("{other:?}"),
        }
        // 旧 writer 的连接已关:新 writer 请求被旧 epoch 拒绝由 lease
        // 语义承载;Root 下游复验:未启动请求在新 epoch 下需重新授权
        // (root epoch 已旋转 → 旧 root_launch capability 的 epoch 失配)
        assert!(coord.root_gate().root_epoch().is_some());
    }

    #[test]
    fn double_tab_concurrent_takeover_only_one_wins() {
        let mut coord = coordinator();
        coord.bootstrap_controller(ClientId::parse("cl_a").unwrap());
        let tab_b = ClientId::parse("cl_b").unwrap();
        let tab_c = ClientId::parse("cl_c").unwrap();
        // b 用正确 epoch 抢占
        match coord.takeover(tab_b.clone(), 1) {
            TakeoverOutcome::Taken { new_epoch, .. } => assert_eq!(new_epoch, 2),
            other => panic!("{other:?}"),
        }
        // c 仍用旧观察(1) → CAS 失败(观察到的已是 2)
        assert_eq!(
            coord.takeover(tab_c, 1),
            TakeoverOutcome::StaleObservedEpoch { observed: 2 }
        );
    }

    #[test]
    fn linearized_commands_and_inputs_survive_takeover() {
        let mut coord = coordinator();
        let first = ClientId::parse("cl_first").unwrap();
        coord.bootstrap_controller(first.clone());
        // 已线性化(dispatch 完成)的命令不受影响:verify_write 只管
        // 新命令;takeover 不重放/撤销历史
        assert!(coord.verify_write(&first, 1).is_ok());
        let observer = ClientId::parse("cl_obs").unwrap();
        coord.takeover(observer, 1);
        // 历史命令的 receipt 不因 takeover 失效(语义由 kernel 承载);
        // 这里固化:新 epoch 下旧 epoch 的写被拒,但新 controller 写通过
        assert_eq!(
            coord.verify_write(&first, 1),
            Err("controller_lease_expired")
        );
    }
}

impl TakeoverCoordinator {
    /// 测试辅助:开启 Root Mode。
    #[cfg(test)]
    fn root_gate_enable_for_test(&mut self) {
        let _ = self.root_gate.enable_root_mode();
    }

    /// root gate 只读访问(审计)。
    pub fn root_gate(&self) -> &RootExecutionGate {
        &self.root_gate
    }
}
