//! T4 运行中改图:暂停 → 图补丁(新 Revision + 继承) → 恢复。
//! 验收:A 成功+B 未启动,暂停后插入 C(A→C→B),恢复只跑 C/B,
//! 二者仍引用 A 的原始结果;未暂停打补丁被拒。

mod common;

use common::*;
use mf_agent::model::*;
use mf_agent::workflow::{WorkflowNodeDraft, WorkflowTemplateDraft};
use mf_agent::workflow_compiler::{CompileInput, WorkflowCompiler};
use std::time::Duration;

fn compile_nodes(
    fx: &Fixture,
    nodes: Vec<WorkflowNodeDraft>,
    template_key: &str,
    version: i64,
) -> mf_agent::workflow::WorkflowSnapshot {
    let template = mf_agent::workflow::WorkflowTemplateVersion {
        version_id: 0,
        template_key: template_key.into(),
        version: version + 1,
        nodes,
        created_at: String::new(),
    };
    WorkflowCompiler::new()
        .compile(CompileInput {
            template: &template,
            directory_provider_isolates: true,
            allow_unsafe_shared_directory: false,
            agent_type_plugins: &plugin_index(),
            resolve_instance: &|reference| {
                fx.catalog
                    .snapshot_agent_instance(reference, None)
                    .map_err(|e| anyhow::anyhow!("{e:#}"))
            },
            directory_provider: Some(plugin_pin("scripted", "hash-scripted")),
        })
        .expect("补丁图必须编译通过")
}

#[test]
fn t4_pause_patch_resume_runs_only_new_downstream_and_inherits_handoffs() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "patch",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node(
                "b",
                &["a"],
                "读取 ${nodes.a.output.report_path}",
                &fx.instance_id,
            ),
        ],
    );
    let task = fx.orch.create_task("改图", "A→C→B").unwrap();
    fx.assign_and_run(task.id, &version);

    // A 派发并结算(输出 report_path)
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 1
    }));
    let token_a = token_of_node(&fx.orch, task.id, "a");
    fx.orch
        .settle_by_token(
            &token_a,
            Settlement::Complete {
                summary: "A 完成".into(),
                output: serde_json::json!({"report_path": "reports/r1.md"}),
            },
        )
        .unwrap();

    // 暂停:B 不再派发
    let paused = fx.orch.pause_task(task.id).unwrap();
    assert!(paused.paused);
    std::thread::sleep(Duration::from_millis(700));
    {
        let guard = fx.host.workflow.lock();
        assert!(
            guard.iter().all(|(spec, _)| spec.node_key != "b"),
            "暂停后 B 不得派发"
        );
    }

    // 图补丁:A(冻结原样) + 新 C(deps=[a]) + B(deps=[c])
    let patch_nodes = vec![
        node("a", &[], "做 A", &fx.instance_id),
        node("c", &["a"], "做 C", &fx.instance_id),
        node(
            "b",
            &["c"],
            "读取 ${nodes.a.output.report_path}",
            &fx.instance_id,
        ),
    ];
    let snapshot = compile_nodes(&fx, patch_nodes.clone(), "patch", version.version);
    let digest = mf_agent::workflow::workflow_content_digest(&patch_nodes, false);
    let (view, _rev) = fx
        .orch
        .store
        .with_tx(|tx| mf_agent::Store::create_patched_revision_tx(tx, task.id, &snapshot, &digest))
        .unwrap();
    assert!(view.revision > version.version, "补丁必须产生新 Revision");

    // 继承:A 保持成功(attempts/result 保留);handoff 重映射到新 step 行
    let all_steps = fx.orch.store.task_steps(task.id).unwrap();
    let steps: Vec<_> = all_steps
        .into_iter()
        .filter(|s| s.revision_id == _rev)
        .collect();
    let a = steps.iter().find(|s| s.step_key == "a").unwrap();
    assert_eq!(a.status, StepStatus::Succeeded, "成功节点不重新执行");
    assert_eq!(a.attempts, 1, "attempts 继承");
    assert!(a.result.is_some());
    let b = steps.iter().find(|s| s.step_key == "b").unwrap();
    assert_eq!(b.status, StepStatus::Pending);
    assert!(steps.iter().any(|s| s.step_key == "c"));

    // 恢复:C 派发(不是 B);结算 C 后 B 派发并引用 A 的原始结果
    fx.orch.resume_task(task.id).unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "c")
    }));
    let token_c = token_of_node(&fx.orch, task.id, "c");
    fx.orch
        .settle_by_token(
            &token_c,
            Settlement::Complete {
                summary: "C 完成".into(),
                output: Default::default(),
            },
        )
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host
            .workflow
            .lock()
            .iter()
            .any(|(spec, _)| spec.node_key == "b")
    }));
    {
        let guard = fx.host.workflow.lock();
        let (spec_b, _) = guard.iter().find(|(spec, _)| spec.node_key == "b").unwrap();
        assert!(
            spec_b.prompt.contains("reports/r1.md"),
            "B 仍引用 A 的原始 Handoff: {}",
            spec_b.prompt
        );
        assert!(!spec_b.prompt.contains("${nodes."));
    }
    // A 只执行过一次(继承不重跑)
    let run_count_a = fx
        .orch
        .store
        .list_runs_of_task(task.id)
        .unwrap()
        .into_iter()
        .filter(|_| true)
        .count();
    let a_specs = {
        let guard = fx.host.workflow.lock();
        guard
            .iter()
            .filter(|(spec, _)| spec.node_key == "a")
            .count()
    };
    assert_eq!(a_specs, 1, "A 不重复执行");
    let _ = run_count_a;
    fx.orch.stop();
}

#[test]
fn t4_patch_rejects_modifying_started_node() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "guard",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &["a"], "做 B", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("守卫", "已启动节点不可改").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 1
    }));
    fx.orch.pause_task(task.id).unwrap();

    // 修改已启动的 A(attempts=1)的定义 → 编译缝隙拒绝(store 层不校验,
    // 这里验证补丁后的快照与 attempts 守卫的组合语义:改定义后创建仍会
    // 继承 A 的终态,正确路径的守卫在 port prepare;此处断言 create 侧
    // 继承仍然发生,真正的拒绝断言见 port 级(e2e/kernel)。
    let patch_nodes = vec![
        node("a", &[], "改过的 A", &fx.instance_id),
        node("b", &["a"], "做 B", &fx.instance_id),
    ];
    let snapshot = compile_nodes(&fx, patch_nodes.clone(), "guard", version.version);
    let digest = mf_agent::workflow::workflow_content_digest(&patch_nodes, false);
    let active_before = fx.orch.store.active_revision(task.id).unwrap().unwrap().id;
    let result = fx
        .orch
        .store
        .with_tx(|tx| mf_agent::Store::create_patched_revision_tx(tx, task.id, &snapshot, &digest));
    // S3 收紧:修改已启动节点定义的补丁在事务内也被拒绝(此前只在
    // port prepare 守卫;提交点必须复验,见第二轮交接)
    assert!(
        result.is_err(),
        "S3:事务必须拒绝修改已启动节点的定义:{result:?}"
    );
    let active_after = fx.orch.store.active_revision(task.id).unwrap().unwrap().id;
    assert_eq!(active_before, active_after, "拒绝后活动 Revision 不变");
    fx.orch.stop();
}

#[test]
fn t4_pause_prevents_dispatch_run_creation_transactionally() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(tmp.path());
    fx.pins.resolve_ok(true);
    let version = fx.template(
        "txn",
        vec![
            node("a", &[], "做 A", &fx.instance_id),
            node("b", &["a"], "做 B", &fx.instance_id),
        ],
    );
    let task = fx.orch.create_task("事务", "暂停前置").unwrap();
    fx.assign_and_run(task.id, &version);
    assert!(wait_until(Duration::from_secs(5), || {
        fx.host.workflow.lock().len() >= 1
    }));
    // 暂停后立刻并发派发 B:dispatch_run_consuming 必须拒绝创建 run
    fx.orch.pause_task(task.id).unwrap();
    let steps = fx.orch.store.task_steps(task.id).unwrap();
    let b = steps.iter().find(|s| s.step_key == "b").unwrap();
    let session: i64 = fx
        .orch
        .store
        .with_conn(|c| -> anyhow::Result<i64> {
            use rusqlite::OptionalExtension;
            let row: Option<i64> = c
                .query_row("SELECT id FROM agent_sessions LIMIT 1", [], |r| r.get(0))
                .optional()?;
            Ok(row.unwrap_or(1))
        })
        .unwrap_or(1);
    let result = fx
        .orch
        .store
        .dispatch_run_consuming(task.id, b.id, b.revision_id, session, None);
    assert!(result.is_err(), "暂停后创建 Agent Run 必须被事务拒绝");
    let b_after = fx
        .orch
        .store
        .task_steps(task.id)
        .unwrap()
        .into_iter()
        .find(|s| s.step_key == "b")
        .unwrap();
    assert_eq!(b_after.attempts, 0, "未消费 attempt");
    fx.orch.stop();
}

// 复用 common 的模板保存辅助(template() 已在 Fixture 上)
#[allow(dead_code)]
fn _template_helper(fx: &Fixture) {
    let _ = fx
        .catalog
        .save_template(&WorkflowTemplateDraft {
            key: "x".into(),
            name: "x".into(),
            task_local: false,
            nodes: vec![],
        })
        .unwrap();
}
