//! mf-elevated broker bin(T4d,Issue #44)。
//!
//! 发行形态的 Broker 是最小化独立提权进程(Windows requireAdministrator
//! manifest / macOS helper / polkit)。**本仓库阶段它以 fake seam 驱动**:
//! 消费 `RootExecutionGate`/`BrokerGate` 的判定与 host 生命周期,不执行
//! 真实提权(Non-goals);真实 UAC/ServiceManagement 接线随打包里程碑。

fn main() {
    println!("mf-elevated broker (fake seam; real elevation at packaging milestone)");
}
