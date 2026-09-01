//! T0a 基线 golden fixtures(Issue #12):冻结 Project v6 / Catalog v1 /
//! session.json 的确定性快照,作为后续 v6→v7、catalog v1→v2、
//! session.json→project_registry 迁移「迁移前后等价」的对照基线。
//!
//! - fixtures 由 `common/baseline.rs` 的生成 harness 产出
//!   (`MF_REGEN_BASELINE=1 cargo test -p mf-agent --test baseline_storage
//!   regenerate_baseline_fixtures -- --ignored --exact --nocapture`);
//! - 生成过程走当前生产 Store/CatalogStore 写路径,再对时间戳/随机 ID
//!   做确定性规范化(值由列名决定,与运行时间无关);
//! - fixture 必须能被当前代码原样打开读取(迁移起点不可破坏);
//! - `manifest.json` 记录 generator version 与每个文件的 SHA-256;
//! - 不含任何真实用户数据/Secret/MF_RUN_TOKEN。

#[path = "common/baseline.rs"]
mod baseline;

use baseline::{fixtures_dir, GENERATOR_VERSION};
use mf_agent::catalog_store::CatalogStore;
use mf_agent::schema::{CATALOG_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION};
use mf_agent::store::Store;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// 把 fixture 复制到临时目录再打开:一方面避免 WAL 伴随文件
/// (`-wal`/`-shm`)弄脏 `tests/fixtures/baseline/`,另一方面保证
/// 测试绝不写 fixture 本体。
fn copy_to_temp(file: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let src = fixtures_dir().join(file);
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join(file);
    fs::copy(&src, &dst).unwrap();
    (tmp, dst)
}

fn expected_dump(name: &str) -> String {
    let path = fixtures_dir().join("expected").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取 expected/{name} 失败: {e}"))
}

fn canonical(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap() + "\n"
}

/// Project v6 fixture 能被当前 Store 原样打开,且核心实体齐全:
/// Task / Pipeline Revision / Step / Agent Run(含 Settlement 终态)/
/// Handoff / 项目工作流。
#[test]
fn project_v6_fixture_opens_with_current_code() {
    let (_tmp, db) = copy_to_temp("project-v6.db");
    assert_eq!(
        baseline::raw_schema_version(&db).unwrap(),
        baseline::FROZEN_PROJECT_SCHEMA_VERSION
    );
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), PROJECT_SCHEMA_VERSION);

    let tasks = store.list_tasks(true).unwrap();
    assert_eq!(tasks.len(), 2, "fixture 含两个任务: {tasks:?}");

    let running = tasks.iter().find(|t| t.title == "基线任务A").unwrap();
    let revisions = store.revision_statuses(running.id).unwrap();
    assert_eq!(revisions.len(), 1, "任务A 恰好一个 Revision");
    let steps = store.task_steps(running.id).unwrap();
    assert_eq!(steps.len(), 2, "任务A 两个 Step(串行依赖)");
    assert!(
        steps.iter().any(|s| s.status.as_str() == "succeeded"),
        "已结算 Step 终态保留: {steps:?}"
    );

    let runs = store.list_runs_of_task(running.id).unwrap();
    assert_eq!(runs.len(), 1, "任务A 一个 Agent Run");
    let run = &runs[0];
    assert_eq!(run.status.as_str(), "succeeded");
    assert_eq!(run.outcome.as_deref(), Some("complete"), "Settlement 终态");
    assert!(
        run.capability_token.starts_with("mft_fixture_"),
        "能力令牌已规范化为确定性值: {}",
        run.capability_token
    );

    let handoffs = store.list_handoff_rows(running.id).unwrap();
    assert_eq!(handoffs.len(), 1, "任务A 一条 Handoff(成功结算同事务落库)");
    assert_eq!(handoffs[0].handoff.status, "complete");
    assert_eq!(handoffs[0].step_id, Some(run.step_id));
    assert_eq!(handoffs[0].run_id, Some(run.id));

    let workflows = store.list_project_workflows().unwrap();
    assert_eq!(workflows.len(), 1, "fixture 含一个项目工作流");
    assert_eq!(workflows[0].key, "baseline-flow");
    assert_eq!(workflows[0].nodes.len(), 2);
}

/// Catalog v1 fixture 能被当前 CatalogStore 原样打开:Agent Instance
/// (用户/项目作用域)、不可变版本行、插件 pin、工作流模板。
#[test]
fn catalog_v1_fixture_opens_with_current_code() {
    let (_tmp, db) = copy_to_temp("catalog-v1.db");
    assert_eq!(
        baseline::raw_schema_version(&db).unwrap(),
        baseline::FROZEN_CATALOG_SCHEMA_VERSION
    );
    let catalog = CatalogStore::open(&db).unwrap();
    assert_eq!(catalog.schema_version().unwrap(), CATALOG_SCHEMA_VERSION);

    let instances = catalog.list_agent_instances(None).unwrap();
    assert_eq!(instances.len(), 1, "用户作用域实例: {instances:?}");
    assert_eq!(instances[0].id, "fixture-inst-user");

    let scoped = catalog
        .list_agent_instances(Some("fixture-project"))
        .unwrap();
    assert_eq!(scoped.len(), 2, "用户 + 项目作用域实例都可见");

    for inst in &scoped {
        let versions = catalog.agent_instance_versions(&inst.id).unwrap();
        assert!(!versions.is_empty(), "实例 {} 有版本行", inst.id);
        assert!(
            versions[0].sealed_secret_ids.is_empty(),
            "fixture 不携带 Secret 引用"
        );
    }

    let pins = catalog.list_plugin_pins().unwrap();
    assert_eq!(pins.len(), 2, "两条插件 pin");
    assert!(pins.iter().all(|p| p.content_hash.len() == 64));

    let templates = catalog.list_templates(false).unwrap();
    assert_eq!(templates.len(), 1, "一个全局模板");
    assert_eq!(templates[0].key, "tpl-baseline");
}

/// Project Revision 快照中的 Agent Type / Directory Provider pin
/// 与 Catalog 按同一 revision run_key 保留的 pin 完整对应。
#[test]
fn plugin_pin_chain_links_revision_snapshot_to_catalog() {
    let (_project_tmp, project_db) = copy_to_temp("project-v6.db");
    let store = Store::open(&project_db).unwrap();
    let task = store
        .list_tasks(true)
        .unwrap()
        .into_iter()
        .find(|task| task.title == "基线任务A")
        .unwrap();
    let revision = store.active_revision(task.id).unwrap().unwrap();
    let snapshot = store
        .revision_snapshot(revision.id)
        .unwrap()
        .expect("基线 Revision 必须携带冻结快照");
    let mut frozen: Vec<_> = snapshot
        .nodes
        .iter()
        .filter_map(|node| node.plugin.as_ref())
        .chain(snapshot.directory_provider.as_ref())
        .map(|pin| {
            (
                pin.full_id.clone(),
                pin.version.clone(),
                pin.content_hash.clone(),
            )
        })
        .collect();
    frozen.sort();
    frozen.dedup();

    let (_catalog_tmp, catalog_db) = copy_to_temp("catalog-v1.db");
    let catalog = CatalogStore::open(&catalog_db).unwrap();
    let run_key = baseline::fixture_pin_run_key(task.id, revision.id);
    let mut persisted: Vec<_> = catalog
        .list_plugin_pins()
        .unwrap()
        .into_iter()
        .filter(|pin| pin.run_key == run_key)
        .map(|pin| (pin.full_id, pin.version, pin.content_hash))
        .collect();
    persisted.sort();
    assert_eq!(persisted, frozen, "Catalog pin 必须精确保护该 Revision");
}

/// session.json fixture:老/新字段共存的项目列表形态,含一个
/// 不存在的项目(迁移后应保留为 missing,不得删除)。
#[test]
fn session_fixture_has_project_list_shape() {
    let raw = fs::read_to_string(fixtures_dir().join("session.json")).unwrap();
    let v: Value = serde_json::from_str(&raw).unwrap();
    let projects = v["projects"].as_array().expect("projects 数组");
    assert!(projects.len() >= 3, "至少三个项目路径");
    assert!(v["foreground"].is_string(), "foreground 保留");
    let states = v["project_states"].as_array().expect("project_states");
    assert!(states.iter().any(|s| s["selected_task_id"].is_u64()));
    // 有一个项目目录不存在(保留为 missing 的语义来源)
    assert!(
        projects
            .iter()
            .any(|p| p.as_str() == Some(baseline::SESSION_MISSING_PROJECT)),
        "fixture 必须包含固定不存在路径 {}: {projects:?}",
        baseline::SESSION_MISSING_PROJECT
    );
    assert!(
        !Path::new(baseline::SESSION_MISSING_PROJECT).exists(),
        "缺失项目路径不得真实存在"
    );
}

/// 规范化 dump 与提交的 expected JSON 逐字节一致(golden)。
#[test]
fn project_dump_matches_expected_golden() {
    let (_tmp, db) = copy_to_temp("project-v6.db");
    let store = Store::open(&db).unwrap();
    let dump = baseline::dump_project(&store).unwrap();
    assert_eq!(canonical(&dump), expected_dump("project-v6.dump.json"));
}

#[test]
fn catalog_dump_matches_expected_golden() {
    let (_tmp, db) = copy_to_temp("catalog-v1.db");
    let catalog = CatalogStore::open(&db).unwrap();
    let dump = baseline::dump_catalog(&catalog).unwrap();
    assert_eq!(canonical(&dump), expected_dump("catalog-v1.dump.json"));
}

#[test]
fn session_dump_matches_expected_golden() {
    let raw = fs::read_to_string(fixtures_dir().join("session.json")).unwrap();
    let dump = baseline::dump_session(&raw).unwrap();
    assert_eq!(canonical(&dump), expected_dump("session.dump.json"));
}

/// 重复生成不受时间戳、随机值、排序影响:两轮的 DB、
/// session、expected dump 与 manifest 均逐字节一致,且与提交的基线一致。
#[test]
fn regeneration_is_deterministic() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let baseline1 = dir1.path().join("baseline");
    let baseline2 = dir2.path().join("baseline");
    let workflow_goldens: Vec<_> = fs::read_dir(fixtures_dir().join("expected"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name.starts_with("workflow-") && name.ends_with(".json"))
                .then(|| (name, fs::read(entry.path()).unwrap()))
        })
        .collect();
    for dir in [&baseline1, &baseline2] {
        baseline::write_baseline_with_workflow_goldens(dir, &workflow_goldens).unwrap();
    }
    // 再覆盖一次已有目录,显式覆盖 Windows 不支持 rename 覆盖
    // 已有文件的重生成路径。
    baseline::write_baseline_with_workflow_goldens(&baseline1, &workflow_goldens).unwrap();
    for name in baseline::fixture_file_paths(&fixtures_dir()).unwrap() {
        let a = fs::read(baseline1.join(&name)).unwrap();
        let b = fs::read(baseline2.join(&name)).unwrap();
        assert_eq!(a, b, "{} 两轮生成不一致", name.display());
        assert_eq!(
            a,
            fs::read(fixtures_dir().join(&name)).unwrap(),
            "{} 与提交的基线不一致",
            name.display()
        );
    }
    for parent in [dir1.path(), dir2.path()] {
        let leftovers: Vec<_> = fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".mf-baseline-"))
            .collect();
        assert!(leftovers.is_empty(), "基线安装遗留临时目录: {leftovers:?}");
    }
}

/// manifest 记录 generator version 与每个 fixture 文件的 SHA-256;
/// 当前工作区的 fixture 必须与 manifest 完全一致(完整性校验)。
#[test]
fn manifest_matches_fixture_files() {
    let manifest = baseline::read_manifest(&fixtures_dir()).unwrap();
    assert_eq!(
        manifest["generator_version"].as_str().unwrap(),
        GENERATOR_VERSION
    );
    let files = manifest["files"].as_array().expect("files 数组");
    assert!(
        files.len() >= 6,
        "至少覆盖 db/session/expected/manifest 外全部文件"
    );
    let listed: BTreeSet<_> = files
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_string())
        .collect();
    let actual: BTreeSet<_> = baseline::fixture_file_paths(&fixtures_dir())
        .unwrap()
        .into_iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .filter(|path| path != "manifest.json")
        .collect();
    assert_eq!(listed, actual, "manifest 不得遗漏或多列 fixture 文件");
    for entry in files {
        let rel = entry["path"].as_str().unwrap();
        let path = fixtures_dir().join(rel);
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("读取 {rel} 失败: {e}"));
        assert_eq!(
            entry["bytes"].as_u64().unwrap(),
            bytes.len() as u64,
            "{rel} 大小与 manifest 不符"
        );
        assert_eq!(
            entry["sha256"].as_str().unwrap(),
            baseline::sha256_hex(&bytes),
            "{rel} SHA-256 与 manifest 不符"
        );
    }
}

/// fixtures 不得包含任何 Secret 痕迹:sealed_secrets 表为空,
/// 文件字节中不出现运行令牌/常见凭据关键字。
#[test]
fn fixtures_contain_no_secrets_or_tokens() {
    for file in ["project-v6.db", "catalog-v1.db", "session.json"] {
        let bytes = fs::read(fixtures_dir().join(file)).unwrap();
        let text = String::from_utf8_lossy(&bytes).to_lowercase();
        for needle in ["mf_run_token", "api_key", "password=", "bearer "] {
            assert!(!text.contains(needle), "{file} 含禁用关键字 {needle}");
        }
    }
    let (_tmp, db) = copy_to_temp("catalog-v1.db");
    let catalog = CatalogStore::open(&db).unwrap();
    let count: i64 = catalog
        .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM sealed_secrets", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(count, 0, "sealed_secrets 必须为空");
}

/// fixture 生成入口(默认 ignore)。更新基线时只运行此测试,
/// 避免与其他读取 fixture 的测试并发:
/// `MF_REGEN_BASELINE=1 cargo test -p mf-agent --test baseline_storage
/// regenerate_baseline_fixtures -- --ignored --exact --nocapture`
#[test]
#[ignore = "会替换提交的基线 fixtures"]
fn regenerate_baseline_fixtures() {
    assert!(
        std::env::var("MF_REGEN_BASELINE").as_deref() == Ok("1"),
        "必须显式设置 MF_REGEN_BASELINE=1"
    );
    let dir = fixtures_dir();
    assert!(
        Path::new(&dir).join("manifest.json").is_file()
            || std::env::var_os("MF_REGEN_ALLOW_INIT").is_some(),
        "首次生成需显式 MF_REGEN_ALLOW_INIT=1"
    );
    baseline::write_baseline(&dir).unwrap();
    eprintln!("已重写 {}", dir.display());
}
