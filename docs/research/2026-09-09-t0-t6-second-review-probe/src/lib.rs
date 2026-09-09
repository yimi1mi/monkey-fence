#![allow(dead_code, unused_imports)]
#[path="../../../../crates/mf-agent/tests/common/mod.rs"]
mod common;
#[path="../../../../crates/mf-agent/tests/common/run_lifecycle.rs"]
mod lifecycle;
use common::*;
use mf_agent::{Store, Settlement, RetryMode};
use mf_agent::workflow::*;
use mf_agent::node_input::*;
use mf_kernel::handles::*;
use mf_kernel::kernel::{WorkflowRunCommand,WorkflowRunExpected};
use mf_kernel::run_lifecycle::{RunLifecyclePort,RunPreparation};
use std::sync::Arc;
use std::time::Duration;

struct StrictResolver(Arc<mf_agent::CatalogStore>);
impl mf_agent::orchestrator::WorkflowInstanceResolver for StrictResolver {
 fn resolve(&self, reference: &str)->anyhow::Result<mf_agent::AgentInstanceSnapshot> {
  self.0.snapshot_agent_instance(reference,None)
 }
}
fn production_port(fx:&Fixture)->mf_web::execution_ports::OrchestratorRunLifecyclePort {
 let directory:Arc<dyn mf_agent::execution_directory::ExecutionDirectoryProvider>=fx.directory.clone();
 let start=mf_web::execution_ports::OrchestratorWorkflowStartPort::new(fx.orch.clone(),plugin_index(),Arc::new(StrictResolver(fx.catalog.clone())),&directory,Some(plugin_pin("scripted","hash-scripted")));
 mf_web::execution_ports::OrchestratorRunLifecyclePort{orchestrator:fx.orch.clone(),start:Some(Arc::new(start))}
}
fn patch_prepare(fx:&Fixture,task_id:i64,nodes:Vec<WorkflowNodeDraft>)->Result<RunPreparation,mf_kernel::kernel::KernelProblem> {
 let task=fx.orch.store.task_view(task_id).unwrap().unwrap();
 let rev=fx.orch.store.active_revision(task_id).unwrap().unwrap();
 production_port(fx).prepare(&CommandId::new(),&WorkflowRunCommand::ApplyGraphPatch{
 project:ProjectStoreHandle::generate(),workflow_run:WorkflowRunHandle::parse(task.public_handle).unwrap(),base_revision:rev.public_handle,nodes,expected:WorkflowRunExpected::only_run(task.revision as u64)})
}
fn start_ab(review:bool,bound:bool)->(tempfile::TempDir,Fixture,i64) {
 let tmp=tempfile::tempdir().unwrap(); let fx=fixture(tmp.path()); fx.pins.resolve_ok(true);
 let mut b=node("b",&["a"],if bound{"read ${inputs.report}"}else{"do B"},&fx.instance_id);
 b.require_input_review=review;
 if bound {b.input_bindings=vec![InputBinding{name:"report".into(),source_node_key:"a".into(),field_path:"output.path".into(),required:true,default_value:None}];}
 let version=fx.template("review-probe",vec![node("a",&[],"do A",&fx.instance_id),b]);
 let task=fx.orch.create_task("probe","probe").unwrap(); fx.assign_and_run(task.id,&version);
 assert!(wait_until(Duration::from_secs(5),||fx.host.workflow.lock().len()==1));
 (tmp,fx,task.id)
}
fn complete_a(fx:&Fixture,task:i64) {
 fx.orch.settle_by_token(&token_of_node(&fx.orch,task,"a"),Settlement::Complete{summary:"A done".into(),output:serde_json::json!({"path":"UPSTREAM_VALID.md"})}).unwrap();
}
fn b_id(fx:&Fixture,task:i64)->i64 {fx.orch.store.task_steps(task).unwrap().into_iter().find(|s|s.step_key=="b").unwrap().id}
fn await_review(fx:&Fixture,task:i64,step:i64)->mf_agent::store::NodeInputRecord {
 assert!(wait_until(Duration::from_secs(5),||fx.orch.store.latest_node_input_of_step(task,step).unwrap().map(|r|r.review_state=="awaiting_review").unwrap_or(false)));
 fx.orch.store.latest_node_input_of_step(task,step).unwrap().unwrap()
}
#[test]
fn binding_only_override_must_reach_the_sent_prompt() {
 let (_tmp,fx,task)=start_ab(true,true); complete_a(&fx,task); let b=b_id(&fx,task); let rec=await_review(&fx,task,b);
 let mut overrides=InputOverrides::default(); overrides.binding_values.insert("report".into(),"USER_FIXED.md".into());
 let compiled=apply_overrides(rec.compiled,&overrides); fx.orch.stop();
 assert!(full_prompt(&compiled).contains("USER_FIXED.md"),"binding value changed but prompt still contains original input: {}",compiled.business_prompt);
}
#[test]
fn review_retry_must_keep_upstream_handoff() {
 let (_tmp,fx,task)=start_ab(true,true); complete_a(&fx,task); let b=b_id(&fx,task); let rec=await_review(&fx,task,b);
 fx.orch.store.confirm_node_input(rec.id,b,rec.input_revision).unwrap();
 assert!(wait_until(Duration::from_secs(5),||fx.orch.store.latest_node_input_of_step(task,b).unwrap().map(|r|r.status=="dispatched").unwrap_or(false)));
 fx.orch.settle_by_token(&token_of_node(&fx.orch,task,"b"),Settlement::Fail{reason:"retry me".into()}).unwrap();
 lifecycle::retry_step(&fx.orch,b,RetryMode::FreshSession).unwrap();
 let next=await_review(&fx,task,b); fx.orch.stop();
 assert!(next.compiled.missing_required.is_empty(),"valid upstream output became missing on retry: {:?}",next.compiled.bindings);
}
#[test]
fn graph_panel_roundtrip_must_resolve_saved_agent_instances() {
 let (_tmp,fx,task)=start_ab(false,false); fx.orch.pause_task(task).unwrap();
 let steps=fx.orch.store.task_steps(task).unwrap();
 let nodes=steps.iter().map(|s|WorkflowNodeDraft{key:s.step_key.clone(),title:s.title.clone(),instructions:s.instructions.clone(),agent_instance_id:fx.instance_id.clone(),deps:s.deps.iter().map(|id|steps.iter().find(|p|p.id==*id).unwrap().step_key.clone()).collect(),..Default::default()}).collect();
 let result=patch_prepare(&fx,task,nodes); fx.orch.stop();
 assert!(result.is_ok(),"run-panel DTO failed with production catalog resolver: {result:?}");
}
#[test]
fn graph_patch_must_reject_removing_a_started_node() {
 let (_tmp,fx,task)=start_ab(false,false); fx.orch.pause_task(task).unwrap();
 let result=patch_prepare(&fx,task,vec![node("b",&[],"do B",&fx.instance_id)]); fx.orch.stop();
 assert!(result.is_err(),"production prepare accepted deleting running A");
}
#[test]
fn saving_input_must_advance_a_confirmation_revision() {
 // R4 修复采用独立 input_revision 轴(按交接说明迁移):保存覆盖推进
 // 该轴,用户看过的旧版本确认必须被拒绝(比推进 run/step revision 更强的行为断言)
 let (_tmp,fx,task)=start_ab(true,true); complete_a(&fx,task); let b=b_id(&fx,task); let rec=await_review(&fx,task,b); fx.orch.pause_task(task).unwrap();
 let mut overrides=InputOverrides::default();overrides.business_prompt=Some("different business input".into());
 fx.orch.store.with_tx(|tx|Store::apply_run_mutation_tx(tx,mf_agent::RunMutation::SaveInputOverrides{step_id:b,expected_input_revision:rec.input_revision,overrides})).unwrap();
 let after=fx.orch.store.latest_node_input_of_step(task,b).unwrap().unwrap(); fx.orch.stop();
 assert!(after.input_revision>rec.input_revision,"saving input must advance the confirmation revision axis");
 let stale=fx.orch.store.confirm_node_input(after.id,b,rec.input_revision);
 assert!(stale.is_err(),"stale confirmation (user saw rev {}) must conflict",rec.input_revision);
}
#[test]
fn patch_must_retain_input_history_for_inherited_step() {
 let (_tmp,fx,task)=start_ab(false,false); fx.orch.pause_task(task).unwrap();complete_a(&fx,task);
 let old_a=fx.orch.store.task_steps(task).unwrap().into_iter().find(|s|s.step_key=="a").unwrap();
 assert!(fx.orch.store.latest_node_input_of_step(task,old_a.id).unwrap().is_some());
 let nodes=vec![node("a",&[],"do A",&fx.instance_id),node("b",&["a"],"do B",&fx.instance_id)];
 let prepared=patch_prepare(&fx,task,nodes).unwrap();
 if let RunPreparation::GraphPatch{pipeline_json,digest}=prepared {
  let snapshot:WorkflowSnapshot=serde_json::from_str(&pipeline_json).unwrap();
  fx.orch.store.with_tx(|tx|Store::create_patched_revision_tx(tx,task,&snapshot,&digest)).unwrap();
 }
 let new_a=fx.orch.store.task_steps(task).unwrap().into_iter().find(|s|s.step_key=="a").unwrap();
 // R6 迁移(按交接说明):历史按 node_key 经权威接口可见;原 step 归属不变
 let by_key=fx.orch.store.latest_node_input_of_key(task,"a").unwrap();
 let by_new_step=fx.orch.store.latest_node_input_of_step(task,new_a.id).unwrap();fx.orch.stop();
 assert!(by_key.is_some(),"inherited A has no visible input record after patch (old step {}, new step {})",old_a.id,new_a.id);
 assert!(by_new_step.is_none(),"history stays attributed to the old step row");
 assert_eq!(by_key.unwrap().step_id,old_a.id);
}




#[test]
fn existing_v12_database_must_upgrade_input_revision_automatically() {
 let tmp=tempfile::tempdir().unwrap();let path=tmp.path().join("old-v12.db");
 {
  let old=Store::open(&path).unwrap();
  old.with_conn(|c|{c.execute_batch("ALTER TABLE node_inputs DROP COLUMN input_revision")?;Ok(())}).unwrap();
  // 构造真实 v12 库:drop 列后把 user_version 回拨到 12
  old.with_conn(|c|{c.pragma_update(None,"user_version",12)?;Ok(())}).unwrap();
  assert_eq!(old.schema_version().unwrap(),12);
 }
 let reopened=Store::open(&path).unwrap();
 let result=reopened.latest_node_input_of_key(1,"a");
 assert!(result.is_ok(),"previously created v12 is not upgraded: {result:?}");
}
#[test]
fn graph_commit_must_recheck_newly_started_node_definition() {
 let (_tmp,fx,task)=start_ab(false,false);fx.orch.pause_task(task).unwrap();complete_a(&fx,task);
 let prepared=patch_prepare(&fx,task,vec![node("a",&[],"do A",&fx.instance_id),node("b",&["a"],"NEW B INSTRUCTIONS",&fx.instance_id)]).unwrap();
 fx.orch.resume_task(task).unwrap();
 assert!(wait_until(Duration::from_secs(5),||fx.host.workflow.lock().iter().any(|(s,_)|s.node_key=="b")));
 fx.orch.pause_task(task).unwrap();
 let RunPreparation::GraphPatch{pipeline_json,digest}=prepared else {panic!("wrong preparation")};
 let snapshot:WorkflowSnapshot=serde_json::from_str(&pipeline_json).unwrap();
 let result=fx.orch.store.with_tx(|tx|Store::create_patched_revision_tx(tx,task,&snapshot,&digest));
 fx.orch.stop();
 assert!(result.is_err(),"B started with old instructions after prepare, but transaction accepted changing them");
}
fn actual_projection_after_stop(fx:&Fixture, task:i64, tmp:&std::path::Path)->serde_json::Value {
 use mf_kernel::kernel::InProcessKernelRuntime;
 use mf_kernel::project_registry::ServiceStore;
 use mf_kernel::command::ServiceIdempotencyKey;
 let projectroot=tmp.join("projection-project");std::fs::create_dir_all(projectroot.join(".mf-agent")).unwrap();
 let target=projectroot.join(".mf-agent/workflow-v1.db");
 fx.orch.store.with_conn(|c|{c.backup(rusqlite::DatabaseName::Main,&target,None)?;Ok(())}).unwrap();
 let service=ServiceStore::open(&tmp.join("projection-service.db")).unwrap();
 let (runtime,client)=InProcessKernelRuntime::for_test(service,ServiceIdempotencyKey::for_test(vec![0x39;32]).unwrap(),ClientId::parse("review-client").unwrap(),Principal::parse("review-user").unwrap()).unwrap();
 let project=runtime.open_project(&projectroot).unwrap();
 let run=fx.orch.store.task_view(task).unwrap().unwrap();
 let snapshot=client.workflow_run_snapshot(project.handle(),&WorkflowRunHandle::parse(run.public_handle).unwrap()).unwrap();
 match snapshot.data { mf_kernel::projection::SnapshotData::WorkflowRun(data) => serde_json::to_value(data).unwrap(), _ => panic!("wrong snapshot kind") }
}
#[test]
fn patched_unstarted_node_must_not_show_obsolete_input_as_current() {
 let (tmp,fx,task)=start_ab(true,false);complete_a(&fx,task);let b=b_id(&fx,task);let rec=await_review(&fx,task,b);
 fx.orch.pause_task(task).unwrap();
 let mut changed=node("b",&["a"],"NEW B INSTRUCTIONS",&fx.instance_id);changed.require_input_review=true;
 let prepared=patch_prepare(&fx,task,vec![node("a",&[],"do A",&fx.instance_id),changed]).unwrap();
 let RunPreparation::GraphPatch{pipeline_json,digest}=prepared else{panic!("wrong preparation")};
 let snapshot:WorkflowSnapshot=serde_json::from_str(&pipeline_json).unwrap();
 fx.orch.store.with_tx(|tx|Store::create_patched_revision_tx(tx,task,&snapshot,&digest)).unwrap();fx.orch.stop();
 let projection=actual_projection_after_stop(&fx,task,tmp.path());
 let bview=projection["steps"].as_array().unwrap().iter().find(|s|s["key"]=="b").unwrap();
 let input=&bview["input"];
 assert!(input.is_null() || input["template"]=="NEW B INSTRUCTIONS","current B instructions={} but current input template={}, old input id={}",bview["instructions"],input["template"],rec.id);
}
#[test]
fn cancelled_gate_must_not_remain_in_needs_you() {
 let(tmp,fx,task)=start_ab(true,false);complete_a(&fx,task);let b=b_id(&fx,task);await_review(&fx,task,b);
 fx.orch.cancel_task(task).unwrap();fx.orch.stop();
 let projection=actual_projection_after_stop(&fx,task,tmp.path());
 assert_eq!(projection["status"],"cancelled");
 assert_eq!(projection["needs_you"],false,"cancelled workflow still advertises input-review: {}",projection["needs_you_reasons"]);
}
