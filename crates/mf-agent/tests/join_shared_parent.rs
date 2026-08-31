//! 共享父节点的多 join 调度(C1)与 join 暂缓持久化/重启重建(C2):
//! 回归图 A+B→J1、J1→C、A+C→J2 —— 每个结算顺序都必须收敛,
//! join 批按子节点分组判定(禁止把共享父参与的多个 join 父集合
//! union),暂缓不得全局阻塞全部 Ready;「成功父节点等待兄弟」的
//! deferral/batch membership 以 Store 为行为源,重启后从 held
//! execution_leases 重建 held_leases/step_leases。

mod common;

use common::*;
use mf_agent::model::*;
use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, WorkflowKernel};
use mf_agent::store::Store;
use mf_agent::workflow::{WorkflowNodeDraft, WorkflowTemplateDraft};
use mf_agent::Settlement;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// 回归图:A+B→J1,J1→C,A+C→J2(A 是共享父节点)。
/// 任意 A/B 完成顺序都必须收敛:J1 的批只由 {A,B} 构成(不得 union
/// 进 J2 的未完成组),J1 必须先运行,J2 只等 C;仅阻塞对应 join,
/// 全部租约最终释放。
#[test]
fn shared_parent_joins_settle_independently_without_deadlock() {
    for order in [vec!["a", "b"], vec!["b", "a"]] {
        let tmp = tempfile::tempdir().unwrap();
        let fx = fixture(tmp.path());
        fx.pins.resolve_ok(true);
        let version = fx.template(
            "shared-join",
            vec![
                node("a", &[], "做 A", &fx.instance_id),
                node("b", &[], "做 B", &fx.instance_id),
                node("j1", &["a", "b"], "汇合 A+B", &fx.instance_id),
                node("c", &["j1"], "做 C", &fx.instance_id),
                node("j2", &["a", "c"], "汇合 A+C", &fx.instance_id),
            ],
        );
        let task = fx.orch.create_task("共享父 join", "g").unwrap();
        fx.assign_and_run(task.id, &version);
        assert!(
            wait_until(Duration::from_secs(5), || fx.host.workflow.lock().len()
                == 2),
            "只有 A、B 两个根节点先派发(顺序 {order:?})"
        );

        // 按指定顺序结算 A、B:J1 组 {A,B} 完整 → 恰好一次整批判定
        for key in &order {
            fx.orch
                .settle_by_token(
                    &token_of_node(&fx.orch, task.id, key),
                    Settlement::complete("ok"),
                )
                .unwrap();
        }
        assert_eq!(
            fx.directory.merges.load(Ordering::SeqCst),
            1,
            "A、B 都终态后 J1 组恰好一次汇合(顺序 {order:?})"
        );
        assert_eq!(
            fx.directory.merge_batches.lock()[0],
            2,
            "J1 的批只由 {{A,B}} 构成,不得 union 进 J2 的父集合(顺序 {order:?},实际 {:?})",
            fx.directory.merge_batches.lock()
        );
        assert!(
            wait_until(Duration::from_secs(5), || fx
                .host
                .workflow
                .lock()
                .iter()
                .any(|(spec, _)| spec.node_key == "j1")),
            "J1 必须在 A、B 终态后派发:不得被 J2 的未完成组({{A,C}} 等 C)全局阻塞(顺序 {order:?})"
        );
        assert_eq!(
            fx.directory.released.lock().len(),
            2,
            "J1 批汇合后 A、B 租约都释放(顺序 {order:?})"
        );

        // J1 结算(不再参与 join)→ C 派发
        fx.orch
            .settle_by_token(
                &token_of_node(&fx.orch, task.id, "j1"),
                Settlement::complete("J1 完成"),
            )
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || fx
                .host
                .workflow
                .lock()
                .iter()
                .any(|(spec, _)| spec.node_key == "c")),
            "C 必须在 J1 之后派发(顺序 {order:?})"
        );

        // C 结算:J2 组 {A,C} 完整 → 汇合(A 已随 J1 批释放,批内只剩 C)
        fx.orch
            .settle_by_token(
                &token_of_node(&fx.orch, task.id, "c"),
                Settlement::complete("C 完成"),
            )
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || fx
                .host
                .workflow
                .lock()
                .iter()
                .any(|(spec, _)| spec.node_key == "j2")),
            "J2 必须在 C 终态后派发(顺序 {order:?})"
        );

        // J2 结算 → 收敛成功,零租约泄漏
        fx.orch
            .settle_by_token(
                &token_of_node(&fx.orch, task.id, "j2"),
                Settlement::complete("J2 完成"),
            )
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || fx
                .orch
                .store
                .task_view(task.id)
                .unwrap()
                .unwrap()
                .status
                == TaskStatus::Succeeded),
            "任务必须收敛(顺序 {order:?},实际 {:?})",
            fx.orch.store.task_view(task.id).unwrap().map(|t| t.status)
        );
        let held = fx
            .orch
            .store
            .list_execution_leases(task.id)
            .unwrap()
            .into_iter()
            .filter(|l| l.status == "held")
            .count();
        assert_eq!(held, 0, "全部租约必须释放,不得泄漏(顺序 {order:?})");
        fx.orch.stop();
    }
}

/// 共享父场景下,无关分支不得被 join 暂缓全局阻塞(C1 的
/// has_incomplete_join_deferral 全局门控回归):X→Y 是独立串行分支,
/// A 的租约因 J2({A,C} 等 C)暂缓时,Y 必须在 X 成功后照常派发。
#[test]
fn join_deferral_does_not_block_unrelated_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "unrelated",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &[], "做 B", &fx.instance_id),
            node("j1", &["a", "b"], "汇合 A+B", &fx.instance_id),
            node("c", &["j1"], "做 C", &fx.instance_id),
            node("j2", &["a", "c"], "汇合 A+C", &fx.instance_id),
            node("x", &[], "做 X", &fx.instance_id),
            node("y", &["x"], "做 Y", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("无关分支", "g").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || {
        let binding = fx.host.workflow.lock();
        let launched: Vec<&str> = binding.iter().map(|(s, _)| s.node_key.as_str()).collect();
        launched.contains(&"a") && launched.contains(&"b")
    }));

    // B 先结算 → A 结算:J1 批汇合,但 A 的 J2 组({A,C})仍不完整;
    // 此刻 X(无关根节点)占用释放出的槽位运行,成功后其下游 Y
    // 必须照常派发 —— join 暂缓只阻塞对应 join 的下游
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "b"),
            Settlement::complete("B 完成"),
        )
        .unwrap();
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "a"),
            Settlement::complete("A 完成"),
        )
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || fx
            .host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "x")),
        "无关根节点 X 应在槽位释放后派发"
    );
    fx.orch
        .settle_by_token(
            &token_of_node(&fx.orch, task.id, "x"),
            Settlement::complete("X 完成"),
        )
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || fx
            .host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "y")),
        "join 暂缓只阻塞对应 join 的下游,不得全局阻塞无关分支 Y"
    );
    fx.orch.stop();
}

/// 真实 worktree:A 成功结算、租约因 join({A,B} 等 B)暂缓时进程重启;
/// held 执行租约与 join deferral 从 Store(行为源)重建,B 后续成功
/// 结算后 {A,B} 作为完整批汇合释放,下游 J1 的基线同时看到 A 与 B。
#[test]
fn join_deferral_survives_restart_and_merges_full_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("project");
    std::fs::create_dir_all(&repo_root).unwrap();
    {
        let repo = git2::Repository::init(&repo_root).unwrap();
        let sig = git2::Signature::now("mf", "mf@test").unwrap();
        std::fs::write(repo_root.join("a.txt"), "base-a\n").unwrap();
        std::fs::write(repo_root.join("b.txt"), "base-b\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
    }
    let catalog = catalog_with_worker_instance();
    let pins = Arc::new(FakePins::default());
    pins.resolve_ok(true);
    let directory = Arc::new(
        mf_plugins::git_worktree_provider::GitWorktreeProvider::new(repo_root.clone()).unwrap(),
    );
    let db_path = repo_root.join(".mf-agent").join("workflow-v1.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let orch1 = Orchestrator::start_with_routing(
        Store::open(&db_path).unwrap(),
        repo_root.clone(),
        mf_agent::config::Config::default(),
        Arc::new(RecordingHost::default()),
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        directory.clone(),
        WorkflowKernel {
            catalog: catalog.clone(),
            pins: Some(pins.clone()),
            instance_resolver: None,
        },
        pinned_routing("builtin.core", "hash-worktree"),
    )
    .unwrap();
    let version = catalog
        .save_template(&WorkflowTemplateDraft {
            key: "join-restart".into(),
            name: "join 重启".into(),
            task_local: false,
            nodes: vec![
                WorkflowNodeDraft {
                    key: "a".into(),
                    title: "A".into(),
                    instructions: "做 A".into(),
                    agent_instance_id: fx_instance_id(&catalog),
                    deps: vec![],
                },
                WorkflowNodeDraft {
                    key: "b".into(),
                    title: "B".into(),
                    instructions: "做 B".into(),
                    agent_instance_id: fx_instance_id(&catalog),
                    deps: vec![],
                },
                WorkflowNodeDraft {
                    key: "j1".into(),
                    title: "J1".into(),
                    instructions: "汇合".into(),
                    agent_instance_id: fx_instance_id(&catalog),
                    deps: vec!["a".into(), "b".into()],
                },
            ],
        })
        .unwrap();
    let task = orch1.create_task("join 暂缓重启", "g").unwrap();
    orch1
        .assign_workflow(task.id, &version, &plugin_index(), false)
        .unwrap();
    orch1.confirm_and_run(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || orch1
        .store
        .list_execution_leases(task.id)
        .map(|ls| ls.iter().filter(|l| l.status == "held").count() == 2)
        .unwrap_or(false)));
    let lease_path = |key: &str| {
        orch1
            .store
            .list_execution_leases(task.id)
            .unwrap()
            .into_iter()
            .find(|l| {
                let meta: serde_json::Value =
                    serde_json::from_str(l.metadata_json.as_deref().unwrap_or("{}")).unwrap();
                meta["step_key"].as_str() == Some(key)
            })
            .map(|l| std::path::PathBuf::from(l.path))
            .unwrap()
    };
    std::fs::write(lease_path("a").join("a.txt"), "from-a\n").unwrap();
    std::fs::write(lease_path("b").join("b.txt"), "from-b\n").unwrap();

    // A 先成功结算:join 批 {A,B} 未完整 → A 的租约保持持有并持久化暂缓
    orch1
        .settle_by_token(
            &token_of_node(&orch1, task.id, "a"),
            Settlement::complete("A 完成"),
        )
        .unwrap();
    let held_a_still = orch1
        .store
        .list_execution_leases(task.id)
        .unwrap()
        .into_iter()
        .any(|l| {
            let meta: serde_json::Value =
                serde_json::from_str(l.metadata_json.as_deref().unwrap_or("{}")).unwrap();
            meta["step_key"].as_str() == Some("a") && l.status == "held"
        });
    assert!(held_a_still, "A 的租约必须保持持有等待 B");
    assert!(
        !orch1
            .store
            .list_join_deferrals(Some(task.id))
            .unwrap()
            .is_empty(),
        "join 暂缓必须在结算进程内持久化(Store 是行为源)"
    );
    orch1.stop();
    drop(orch1);

    // 重启(同 DB 同 provider):held 租约与 join deferral 从 Store 重建;
    // B 的 run 经恢复为 interrupted(宿主无法确认存活)
    let host2 = Arc::new(RecordingHost::default());
    let orch2 = Orchestrator::start_with_routing(
        Store::open(&db_path).unwrap(),
        repo_root.clone(),
        mf_agent::config::Config::default(),
        host2.clone(),
        empty_profiles(),
        GlobalLimiter::new(4),
        "pipe".into(),
        directory.clone(),
        WorkflowKernel {
            catalog: catalog.clone(),
            pins: Some(pins.clone()),
            instance_resolver: None,
        },
        pinned_routing("builtin.core", "hash-worktree"),
    )
    .unwrap();
    let step_a = orch2
        .store
        .task_steps(task.id)
        .unwrap()
        .into_iter()
        .find(|s| s.step_key == "a")
        .unwrap();
    assert_eq!(
        step_a.status,
        StepStatus::Succeeded,
        "A 的成功状态跨重启保持"
    );
    let deferrals = orch2.store.list_join_deferrals(Some(task.id)).unwrap();
    assert!(
        !deferrals.is_empty(),
        "join 暂缓必须跨重启持久化(行为源是 Store)"
    );

    // B 后续成功结算(中断 run 支持人工结算):{A,B} 完整批一次汇合
    orch2
        .settle_by_token(
            &token_of_node(&orch2, task.id, "b"),
            Settlement::complete("B 完成"),
        )
        .unwrap();
    let norm = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap().replace("\r\n", "\n");
    assert!(wait_until(Duration::from_secs(5), || host2
        .workflow
        .lock()
        .iter()
        .any(|(spec, _)| spec.node_key == "j1")));
    // 下游 J1 的基线必须同时看到 A 与 B 的修改
    assert!(
        wait_until(Duration::from_secs(5), || {
            orch2
                .store
                .list_execution_leases(task.id)
                .unwrap()
                .into_iter()
                .any(|l| {
                    let meta: serde_json::Value =
                        serde_json::from_str(l.metadata_json.as_deref().unwrap_or("{}")).unwrap();
                    meta["step_key"].as_str() == Some("j1") && l.status == "held"
                })
        }),
        "J1 派发后租约必须持有"
    );
    let j1_lease = orch2
        .store
        .list_execution_leases(task.id)
        .unwrap()
        .into_iter()
        .find(|l| {
            let meta: serde_json::Value =
                serde_json::from_str(l.metadata_json.as_deref().unwrap_or("{}")).unwrap();
            meta["step_key"].as_str() == Some("j1") && l.status == "held"
        })
        .map(|l| std::path::PathBuf::from(l.path))
        .unwrap();
    assert_eq!(
        norm(j1_lease.join("a.txt")),
        "from-a\n",
        "下游基线必须看到 A 的修改"
    );
    assert_eq!(
        norm(j1_lease.join("b.txt")),
        "from-b\n",
        "下游基线必须看到 B 的修改"
    );
    let still_held = orch2
        .store
        .list_execution_leases(task.id)
        .unwrap()
        .into_iter()
        .filter(|l| {
            let meta: serde_json::Value =
                serde_json::from_str(l.metadata_json.as_deref().unwrap_or("{}")).unwrap();
            matches!(meta["step_key"].as_str(), Some("a") | Some("b")) && l.status == "held"
        })
        .count();
    assert_eq!(still_held, 0, "完整批汇合后 A、B 租约必须释放");
    assert!(
        orch2
            .store
            .list_join_deferrals(Some(task.id))
            .unwrap()
            .is_empty(),
        "汇合完成后 join 暂缓行必须清除"
    );
    orch2.stop();
}
