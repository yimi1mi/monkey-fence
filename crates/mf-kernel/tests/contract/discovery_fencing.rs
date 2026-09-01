//! T1e 契约(Issue #20):stale discovery fencing(§11.1)。
//!
//! 覆盖:stale 判定 = 3×heartbeat 且以心跳可解析为前提;接管矩阵
//! (pid 死亡或心跳过期,二者其一;活 pid + 新鲜心跳拒绝);陈旧 owner
//! 永久 fencing——不能更新 discovery、不能释放新 owner 的锁;discovery
//! 缺失/损坏时的保守语义;owner 独占的 port/heartbeat 更新。
//! 全部使用 tempfile 与确定性 fake 缝隙,不触碰真实 `~/.monkeyfence`。

use crate::support::OwnerFixture;
use mf_kernel::singleton::{
    discovery_is_stale, read_core_lock, read_discovery, CoreOwnerLock, DiscoveryRecord,
    OwnerLockError,
};

/// 构造带指定 heartbeat 的 discovery 记录(其余字段取稳定值)。
fn discovery_at(heartbeat_at: &str) -> DiscoveryRecord {
    DiscoveryRecord {
        instance_id: "018f0000-0000-7000-8000-000000000001".to_string(),
        port: 47822,
        pid: 4242,
        build: "test".to_string(),
        heartbeat_at: heartbeat_at.to_string(),
    }
}

fn base_time() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

/// stale 边界:age ≥ 3×heartbeat 即 stale(§11.1 派生),阈值随
/// heartbeat 配置变化;heartbeat 不可解析按 stale(不能证明新鲜)。
#[test]
fn stale_boundary_is_three_heartbeats() {
    let t0 = base_time();
    let record = discovery_at("2026-09-01T00:00:00.000Z");

    // 默认 heartbeat 5s → stale_after 15s。
    assert!(!discovery_is_stale(&record, t0, 15_000));
    assert!(!discovery_is_stale(
        &record,
        t0 + chrono::Duration::milliseconds(14_999),
        15_000
    ));
    assert!(discovery_is_stale(
        &record,
        t0 + chrono::Duration::milliseconds(15_000),
        15_000
    ));
    assert!(discovery_is_stale(
        &record,
        t0 + chrono::Duration::milliseconds(15_001),
        15_000
    ));

    // 阈值随配置:min/max 范围内 heartbeat → 3s/90s。
    assert!(discovery_is_stale(
        &record,
        t0 + chrono::Duration::milliseconds(3_000),
        3_000
    ));
    assert!(!discovery_is_stale(
        &record,
        t0 + chrono::Duration::milliseconds(89_999),
        90_000
    ));

    // 未来心跳(时钟回拨)不算 stale;无法解析按 stale。
    assert!(!discovery_is_stale(
        &record,
        t0 - chrono::Duration::milliseconds(1),
        15_000
    ));
    let broken = discovery_at("not-a-timestamp");
    assert!(discovery_is_stale(&broken, t0, 15_000));
}

/// 接管矩阵(§11.1:「旧 pid 不存在 **或** heartbeat 过期」+ mutex 可取):
/// 仅 pid 存活 && 心跳新鲜 拒绝(owner_active);其余三种组合全部接管
/// 且 epoch = 前任 + 1;拒绝路径不动任何文件。
#[test]
fn takeover_requires_pid_dead_or_heartbeat_stale() {
    for (alive, advance_ms, expect_takeover) in [
        (true, 0, false),      // 存活 + 新鲜 → 拒绝
        (true, 15_000, true),  // 存活 + 过期 → 接管
        (false, 0, true),      // 死亡 + 新鲜 → 接管
        (false, 15_000, true), // 死亡 + 过期 → 接管
    ] {
        let fx = OwnerFixture::new("matrix");
        let pid = std::process::id();
        let first = CoreOwnerLock::acquire(fx.setup()).unwrap();
        assert_eq!(first.owner_epoch(), 1);
        // 模拟持有者死亡后 OS 回收互斥:挑战者可取互斥,fencing 成为
        // 唯一闸门。
        fx.mutex_handle().simulate_os_reclaim();
        drop(first);
        if alive {
            fx.liveness.mark_alive(pid);
        }
        fx.clock.advance_ms(advance_ms);

        let before_lock = std::fs::read(&fx.paths.lock_path).unwrap();
        let before_discovery = std::fs::read(&fx.paths.discovery_path).unwrap();
        match CoreOwnerLock::acquire(fx.setup()) {
            Ok(second) => {
                assert!(expect_takeover, "本组合必须拒绝接管");
                assert_eq!(second.owner_epoch(), 2, "接管 epoch 单调 +1");
                fx.mutex_handle().simulate_os_reclaim();
                drop(second);
            }
            Err(error) => {
                assert!(!expect_takeover, "本组合必须允许接管:{error}");
                assert!(
                    matches!(error, OwnerLockError::OwnerActive { pid: p } if p == pid),
                    "拒绝原因必须是 owner_active,实际:{error}"
                );
                assert_eq!(
                    std::fs::read(&fx.paths.lock_path).unwrap(),
                    before_lock,
                    "拒绝路径不改写 lock"
                );
                assert_eq!(
                    std::fs::read(&fx.paths.discovery_path).unwrap(),
                    before_discovery,
                    "拒绝路径不改写 discovery"
                );
            }
        }
    }
}

/// 陈旧 discovery 永远不能复活旧 owner:被新 owner 接管后,旧 owner 的
/// heartbeat/set_discovery_port/release 全部拒绝且不动新 owner 的文件;
/// fencing 是永久的(后续调用立即失败);新 owner 不受影响。
#[test]
fn stale_owner_can_never_revive_through_discovery_or_release() {
    let fx = OwnerFixture::new("fence");
    let pid = std::process::id();

    // 旧 owner A(epoch 1)「卡死」:互斥被 OS 回收,但对象仍存活。
    let stale_owner = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(stale_owner.owner_epoch(), 1);
    let stale_proof = stale_owner
        .shutdown_flow()
        .freeze()
        .unwrap()
        .drain()
        .unwrap()
        .stores_closed()
        .unwrap();
    fx.mutex_handle().simulate_os_reclaim();

    // 新 owner B 接管(pid 存活但心跳过期)。
    fx.liveness.mark_alive(pid);
    fx.clock.advance_ms(15_000);
    let current = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(current.owner_epoch(), 2);
    let b_lock_before = std::fs::read(&fx.paths.lock_path).unwrap();
    let b_discovery_before = std::fs::read(&fx.paths.discovery_path).unwrap();

    // A 的全部写路径被 fencing 拒绝,文件字节不变。
    let refused = stale_owner.heartbeat().unwrap_err();
    assert!(
        matches!(refused, OwnerLockError::Superseded { .. }),
        "陈旧 owner 不能更新 discovery:{refused}"
    );
    assert!(stale_owner.is_superseded());
    assert!(
        matches!(
            stale_owner.set_discovery_port(9),
            Err(OwnerLockError::Superseded { .. })
        ),
        "fencing 是永久的:后续调用立即失败"
    );
    assert!(
        matches!(
            stale_owner.release(stale_proof),
            Err(OwnerLockError::Superseded { .. })
        ),
        "陈旧 owner 不能释放新 owner 的锁"
    );
    assert_eq!(
        std::fs::read(&fx.paths.lock_path).unwrap(),
        b_lock_before,
        "lock 仍是新 owner 的"
    );
    assert_eq!(
        std::fs::read(&fx.paths.discovery_path).unwrap(),
        b_discovery_before,
        "discovery 仍是新 owner 的"
    );

    // 新 owner B 不受影响:心跳与干净释放正常。
    current.heartbeat().unwrap();
    let proof = current
        .shutdown_flow()
        .freeze()
        .unwrap()
        .drain()
        .unwrap()
        .stores_closed()
        .unwrap();
    current.release(proof).unwrap();
    let record = read_core_lock(&fx.paths.lock_path).unwrap().unwrap();
    assert_eq!(record.owner_epoch, 2);
    assert!(record.released);
}

/// discovery 缺失/损坏时的保守语义:接管判定视作「心跳不可证明新鲜」
/// (互斥是仲裁者);但活动 owner 面对损坏的 discovery 必须 fail-closed,
/// 不盲目覆盖未知内容。
#[test]
fn missing_or_corrupt_discovery_semantics() {
    // 1) 缺失 + pid 存活:允许接管。
    let fx = OwnerFixture::new("missing");
    let pid = std::process::id();
    let first = CoreOwnerLock::acquire(fx.setup()).unwrap();
    fx.mutex_handle().simulate_os_reclaim();
    drop(first);
    fx.liveness.mark_alive(pid);
    std::fs::remove_file(&fx.paths.discovery_path).unwrap();
    let second = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(second.owner_epoch(), 2);
    fx.mutex_handle().simulate_os_reclaim();
    drop(second);

    // 2) 损坏 + pid 存活:同样允许接管(证明不了新鲜);接管者重写 discovery。
    let fx = OwnerFixture::new("corrupt-disc");
    let first = CoreOwnerLock::acquire(fx.setup()).unwrap();
    fx.mutex_handle().simulate_os_reclaim();
    drop(first);
    fx.liveness.mark_alive(pid);
    std::fs::write(&fx.paths.discovery_path, b"garbage").unwrap();
    let second = CoreOwnerLock::acquire(fx.setup()).unwrap();
    assert_eq!(second.owner_epoch(), 2);
    assert!(
        read_discovery(&fx.paths.discovery_path).unwrap().is_some(),
        "新 owner 重写 discovery"
    );

    // 3) 活动 owner 遇损坏 discovery(接管后再次损坏):heartbeat
    //    fail-closed,不盲目覆盖未知内容。
    std::fs::write(&fx.paths.discovery_path, b"garbage").unwrap();
    let error = second.heartbeat().unwrap_err();
    assert!(
        matches!(error, OwnerLockError::DiscoveryCorrupt { .. }),
        "活动 owner 不得盲目覆盖损坏的 discovery:{error}"
    );
    assert_eq!(
        std::fs::read(&fx.paths.discovery_path).unwrap(),
        b"garbage",
        "fail-closed 不改写文件"
    );
}

/// owner 独占的 discovery 维护:port 修正与 heartbeat 刷新都要求当前
/// 归属;心跳单调推进且不动其它字段。
#[test]
fn owner_updates_port_and_heartbeat_only_while_owning() {
    let fx = OwnerFixture::new("hb");
    let owner = CoreOwnerLock::acquire(fx.setup()).unwrap();
    let before = read_discovery(&fx.paths.discovery_path).unwrap().unwrap();

    owner.set_discovery_port(47822).unwrap();
    let after_port = read_discovery(&fx.paths.discovery_path).unwrap().unwrap();
    assert_eq!(after_port.port, 47822);
    assert_eq!(after_port.instance_id, before.instance_id);
    assert_eq!(after_port.pid, before.pid);

    std::fs::remove_file(&fx.paths.discovery_path).unwrap();
    owner.heartbeat().unwrap();
    let rebuilt = read_discovery(&fx.paths.discovery_path).unwrap().unwrap();
    assert_eq!(rebuilt.port, 47822, "重建 discovery 不得回退已绑定 port");

    fx.clock.advance_ms(2_000);
    owner.heartbeat().unwrap();
    let after_beat = read_discovery(&fx.paths.discovery_path).unwrap().unwrap();
    assert_eq!(after_beat.port, 47822, "heartbeat 不回退其它字段");
    let old = chrono::DateTime::parse_from_rfc3339(&after_port.heartbeat_at).unwrap();
    let new = chrono::DateTime::parse_from_rfc3339(&after_beat.heartbeat_at).unwrap();
    assert!(new > old, "heartbeat 时间戳前进");

    // 释放后 discovery 移除:旧 owner 无法再通过任何入口维护它。
    let proof = owner
        .shutdown_flow()
        .freeze()
        .unwrap()
        .drain()
        .unwrap()
        .stores_closed()
        .unwrap();
    owner.release(proof).unwrap();
    assert!(read_discovery(&fx.paths.discovery_path).unwrap().is_none());
}
