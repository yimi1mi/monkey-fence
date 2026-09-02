//! T4c 契约(Issue #43):plan TOCTOU、download 硬化、receipt pin、
//! 三类 executor(fake env)、状态必经 post-probe、取消、repair-needed。

use mf_installer::discovery::InstallationKind;
use mf_installer::executor::{
    execute_plan, validate_archive_entry, validate_download_url, validate_structured_argv,
    DownloadPolicy, ExecuteOutcome, ExecutorEnv, JobPhase,
};
use mf_installer::plan::{InstallPreview, PlanFreezer, PlanProblem};
use mf_installer::receipt::{
    installation_kind_of, is_receipt_owned, verify_pin, ExecutableIdentityPin, InstallationReceipt,
    RollbackMethod,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn preview(kind: &str) -> InstallPreview {
    InstallPreview {
        agent_type_id: "codex".into(),
        installer_id: "npm-global".into(),
        kind: kind.into(),
        exact_package: "npm".into(),
        exact_version: "1.2.3".into(),
        argv: vec!["install".into(), "-g".into(), "pkg@1.2.3".into()],
        download: None,
        catalog_revision: 7,
    }
}

#[test]
fn plan_freeze_redeem_and_toctou() {
    let mut freezer = PlanFreezer::new(600);
    // catalog revision 漂移 → TOCTOU 拒绝
    assert_eq!(
        freezer.freeze(preview("package-manager"), 8),
        Err(PlanProblem::CatalogDrift { preview: 7, now: 8 })
    );
    let ticket = freezer.freeze(preview("package-manager"), 7).unwrap();
    // 提交只认三元组:错误 digest 拒绝
    let bad_digest = PlanTicketShim {
        plan_handle: ticket.plan_handle.clone(),
        recipe_digest: "deadbeef".into(),
        catalog_revision: 7,
    };
    let _ = bad_digest;
    // 正确票据 → redeem 成功
    let plan = freezer.redeem(&ticket).unwrap().clone();
    assert_eq!(plan.catalog_revision, 7);
    // TTL 过期(ttl=0 被 clamp?PlanFreezer::new(0) → Duration 0 → 立即过期)
    let mut short = PlanFreezer::new(0);
    let ticket = short.freeze(preview("package-manager"), 7).unwrap();
    assert!(matches!(
        short.redeem(&ticket),
        Err(PlanProblem::Expired { .. })
    ));
    // consume 后一次性
    let mut once = PlanFreezer::new(600);
    let ticket = once.freeze(preview("package-manager"), 7).unwrap();
    once.redeem(&ticket).unwrap();
    once.consume(&ticket.plan_handle);
    assert_eq!(once.redeem(&ticket), Err(PlanProblem::UnknownHandle));
}

/// ticket shim(直接构造,测试 digest 不匹配路径)。
#[derive(Debug, Clone)]
struct PlanTicketShim {
    plan_handle: String,
    recipe_digest: String,
    catalog_revision: u64,
}

#[test]
fn plan_ticket_digest_mismatch_rejected() {
    let mut freezer = PlanFreezer::new(600);
    let ticket = freezer.freeze(preview("package-manager"), 7).unwrap();
    let forged = mf_installer::plan::PlanTicket {
        plan_handle: ticket.plan_handle.clone(),
        recipe_digest: "0".repeat(64),
        catalog_revision: 7,
    };
    assert!(matches!(
        freezer.redeem(&forged),
        Err(PlanProblem::DigestMismatch { .. })
    ));
}

struct FakeEnv {
    exit_code: i32,
    output: Vec<u8>,
    downloaded: Vec<u8>,
    download_should_fail: bool,
    published: Vec<(PathBuf, Vec<u8>)>,
    probe_version: Result<String, String>,
    cleaned: usize,
}

impl FakeEnv {
    fn ok() -> Self {
        Self {
            exit_code: 0,
            output: Vec::new(),
            downloaded: Vec::new(),
            download_should_fail: false,
            published: Vec::new(),
            probe_version: Ok("1.2.3".into()),
            cleaned: 0,
        }
    }
}

impl ExecutorEnv for FakeEnv {
    fn run_structured(
        &mut self,
        _program: &str,
        _argv: &[String],
        _cwd: Option<&Path>,
    ) -> Result<(i32, Vec<u8>), String> {
        Ok((self.exit_code, self.output.clone()))
    }

    fn download(&mut self, _url: &str, _frozen_host: &str) -> Result<Vec<u8>, String> {
        if self.download_should_fail {
            Err("network unreachable".into())
        } else {
            Ok(self.downloaded.clone())
        }
    }

    fn publish(&mut self, target: &Path, bytes: &[u8]) -> Result<(), String> {
        self.published.push((target.to_path_buf(), bytes.to_vec()));
        Ok(())
    }

    fn probe_version(
        &mut self,
        _executable: &Path,
        _version_argv: &[String],
    ) -> Result<String, String> {
        self.probe_version.clone()
    }

    fn cleanup_staging(&mut self, _staging: &Path) {
        self.cleaned += 1;
    }

    fn file_sha256(&mut self, _path: &Path) -> Result<String, String> {
        Ok("a".repeat(64))
    }
}

fn no_cancel() -> bool {
    false
}

#[test]
fn package_manager_requires_post_probe_for_installed() {
    let mut freezer = PlanFreezer::new(600);
    let ticket = freezer.freeze(preview("package-manager"), 7).unwrap();
    let plan = freezer.redeem(&ticket).unwrap().clone();
    let staging = PathBuf::from("/tmp/staging");
    let target = PathBuf::from("/managed/codex.exe");

    // probe 失败 → Failed(退出码 0 不算安装成功)
    let mut env = FakeEnv::ok();
    env.probe_version = Err("探测超时".into());
    match execute_plan(
        &mut env,
        &plan,
        &staging,
        &target,
        &["--version".to_string()],
        &DownloadPolicy::default(),
        &no_cancel,
    ) {
        ExecuteOutcome::Failed { phase, reason } => {
            assert_eq!(phase, JobPhase::Verifying);
            assert!(reason.contains("post-install probe"));
        }
        other => panic!("probe 失败必须 Failed:{other:?}"),
    }
    // probe 成功 → Installed(receipt 含 actual_version)
    let mut env = FakeEnv::ok();
    match execute_plan(
        &mut env,
        &plan,
        &staging,
        &target,
        &["--version".to_string()],
        &DownloadPolicy::default(),
        &no_cancel,
    ) {
        ExecuteOutcome::Installed(receipt) => {
            assert_eq!(receipt.actual_version, "1.2.3");
            assert_eq!(receipt.recipe_digest, plan.recipe_digest);
            assert_eq!(receipt.scope, "user");
        }
        other => panic!("应安装成功:{other:?}"),
    }
}

#[test]
fn package_manager_partial_state_is_repair_needed() {
    let mut freezer = PlanFreezer::new(600);
    let ticket = freezer.freeze(preview("package-manager"), 7).unwrap();
    let plan = freezer.redeem(&ticket).unwrap().clone();
    let mut env = FakeEnv::ok();
    env.exit_code = 1;
    env.output = b"npm ERR! code EEXIST\nnpm ERR! dest already exists".to_vec();
    match execute_plan(
        &mut env,
        &plan,
        &PathBuf::from("/s"),
        &PathBuf::from("/t"),
        &["--version".to_string()],
        &DownloadPolicy::default(),
        &no_cancel,
    ) {
        ExecuteOutcome::RepairNeeded { reason } => {
            assert!(reason.contains("EEXIST"), "部分状态展示诊断:{reason}");
        }
        other => panic!("EEXIST 应 repair-needed:{other:?}"),
    }
}

#[test]
fn verified_download_hardens_url_size_and_digest() {
    // URL 硬化
    assert!(validate_download_url(
        "https://registry.example.com/pkg.tgz",
        "registry.example.com",
        0
    )
    .is_ok());
    assert!(
        validate_download_url(
            "http://registry.example.com/pkg.tgz",
            "registry.example.com",
            0
        )
        .is_err(),
        "仅 HTTPS"
    );
    assert!(
        validate_download_url("https://evil.com/pkg.tgz", "registry.example.com", 0).is_err(),
        "域漂移"
    );
    assert!(
        validate_download_url("https://registry.example.com/p", "registry.example.com", 6).is_err(),
        "redirect 超上限"
    );
    // digest 失败
    let mut freezer = PlanFreezer::new(600);
    let mut p = preview("verified-download");
    p.download = Some(mf_installer::plan::FrozenDownload {
        url: "https://dl.example.com/pkg.tgz".into(),
        sha256: "0".repeat(64),
        frozen_host: "dl.example.com".into(),
    });
    let ticket = freezer.freeze(p, 7).unwrap();
    let plan = freezer.redeem(&ticket).unwrap().clone();
    let mut env = FakeEnv::ok();
    env.downloaded = b"payload".to_vec();
    match execute_plan(
        &mut env,
        &plan,
        &PathBuf::from("/s"),
        &PathBuf::from("/t/x.exe"),
        &["--version".to_string()],
        &DownloadPolicy::default(),
        &no_cancel,
    ) {
        ExecuteOutcome::Failed { phase, reason } => {
            assert_eq!(phase, JobPhase::Verifying);
            assert!(reason.contains("校验失败"), "{reason}");
        }
        other => panic!("digest 不符必须失败:{other:?}"),
    }
    assert_eq!(env.cleaned, 1, "失败清理 staging");
    // 大小上限
    let mut env = FakeEnv::ok();
    env.downloaded = vec![0u8; 16 * 1024 * 1024 + 1];
    let small_policy = DownloadPolicy {
        max_bytes: 16 * 1024 * 1024,
        ..DownloadPolicy::default()
    };
    match execute_plan(
        &mut env,
        &plan,
        &PathBuf::from("/s"),
        &PathBuf::from("/t/x.exe"),
        &["--version".to_string()],
        &small_policy,
        &no_cancel,
    ) {
        ExecuteOutcome::Failed { reason, .. } => assert!(reason.contains("超限")),
        other => panic!("超限必须失败:{other:?}"),
    }
    // 成功路径:digest 匹配 → publish + probe → installed
    let payload = b"exact-bytes".to_vec();
    let mut p2 = preview("verified-download");
    p2.download = Some(mf_installer::plan::FrozenDownload {
        url: "https://dl.example.com/pkg.tgz".into(),
        sha256: format!("{:x}", Sha256::digest(&payload)),
        frozen_host: "dl.example.com".into(),
    });
    let ticket = freezer.freeze(p2, 7).unwrap();
    let plan = freezer.redeem(&ticket).unwrap().clone();
    let mut env = FakeEnv::ok();
    env.downloaded = payload;
    match execute_plan(
        &mut env,
        &plan,
        &PathBuf::from("/s"),
        &PathBuf::from("/t/x.exe"),
        &["--version".to_string()],
        &DownloadPolicy::default(),
        &no_cancel,
    ) {
        ExecuteOutcome::Installed(receipt) => {
            assert_eq!(receipt.executable_sha256, "a".repeat(64));
            assert_eq!(env.published.len(), 1, "原子发布一次");
        }
        other => panic!("应安装成功:{other:?}"),
    }
}

#[test]
fn archive_traversal_and_symlink_entries_rejected() {
    assert!(validate_archive_entry("normal/file.txt", false).is_ok());
    for bad in ["../escape", "/abs/path", "a/../../b", "C:/Windows/x"] {
        assert!(validate_archive_entry(bad, false).is_err(), "拒绝:{bad}");
    }
    assert!(
        validate_archive_entry("fine.txt", true).is_err(),
        "symlink 条目拒绝"
    );
}

#[test]
fn shell_injection_shapes_rejected() {
    assert!(validate_structured_argv("npm", &["install".into(), "-g".into()]).is_ok());
    assert!(
        validate_structured_argv("npm install", &[]).is_err(),
        "程序名含空白"
    );
    assert!(
        validate_structured_argv("npm", &["install".into(), "&& rm -rf /".into()]).is_err(),
        "argv 注入形态拒绝"
    );
}

#[test]
fn cancel_is_cooperative_and_cleans_staging() {
    let mut freezer = PlanFreezer::new(600);
    let ticket = freezer.freeze(preview("package-manager"), 7).unwrap();
    let plan = freezer.redeem(&ticket).unwrap().clone();
    let mut env = FakeEnv::ok();
    let yes_cancel = || true;
    match execute_plan(
        &mut env,
        &plan,
        &PathBuf::from("/s"),
        &PathBuf::from("/t"),
        &["--version".to_string()],
        &DownloadPolicy::default(),
        &yes_cancel,
    ) {
        ExecuteOutcome::Cancelled => {}
        other => panic!("取消旗标应取消:{other:?}"),
    }
    assert_eq!(env.cleaned, 1, "取消清理 staging");
}

#[test]
fn custom_command_fails_closed_on_nonzero() {
    let mut freezer = PlanFreezer::new(600);
    let ticket = freezer.freeze(preview("custom-command"), 7).unwrap();
    let plan = freezer.redeem(&ticket).unwrap().clone();
    let mut env = FakeEnv::ok();
    env.exit_code = 3;
    match execute_plan(
        &mut env,
        &plan,
        &PathBuf::from("/s"),
        &PathBuf::from("/t"),
        &["--version".to_string()],
        &DownloadPolicy::default(),
        &no_cancel,
    ) {
        ExecuteOutcome::Failed { phase, reason } => {
            assert_eq!(phase, JobPhase::Executing);
            assert!(reason.contains("3"));
        }
        other => panic!("非零退出失败:{other:?}"),
    }
}

#[test]
fn receipt_ownership_and_pin_verification() {
    let receipt = InstallationReceipt {
        receipt_id: "irec_1".into(),
        plugin_full_id: "builtin.codex".into(),
        agent_type_id: "codex".into(),
        installer_id: "npm-global".into(),
        recipe_digest: "d".repeat(64),
        requested_version: "1.2.3".into(),
        actual_version: "1.2.3".into(),
        scope: "user".into(),
        canonical_executable: PathBuf::from("/managed/codex/bin/codex.exe"),
        executable_sha256: "a".repeat(64),
        rollback: RollbackMethod::RemoveOwnedRoot {
            owned_root: PathBuf::from("/managed/codex"),
        },
        installed_at: "2026-09-02".into(),
    };
    // update/uninstall 只碰 receipt-owned
    assert!(is_receipt_owned(
        &receipt,
        Path::new("/managed/codex/bin/codex.exe")
    ));
    assert!(
        !is_receipt_owned(&receipt, Path::new("/usr/local/bin/codex")),
        "外部安装不可动"
    );
    assert!(
        !is_receipt_owned(&receipt, Path::new("/managed")),
        "owned root 上级不可整体删除"
    );
    // kind 呈现
    assert_eq!(
        installation_kind_of(Some(&receipt)),
        InstallationKind::Managed
    );
    assert_eq!(installation_kind_of(None), InstallationKind::External);
    // pin:路径/身份替换 → 拒绝 + Needs You
    let pin = ExecutableIdentityPin {
        installation_receipt_id: receipt.receipt_id.clone(),
        canonical_executable: receipt.canonical_executable.clone(),
        executable_sha256: receipt.executable_sha256.clone(),
        actual_cli_version: receipt.actual_version.clone(),
    };
    verify_pin(
        &pin,
        Path::new("/managed/codex/bin/codex.exe"),
        &"a".repeat(64),
    )
    .unwrap();
    assert!(matches!(
        verify_pin(&pin, Path::new("/other/path.exe"), &"a".repeat(64)),
        Err(mf_installer::receipt::PinProblem::ExecutableReplaced { .. })
    ));
    assert!(matches!(
        verify_pin(
            &pin,
            Path::new("/managed/codex/bin/codex.exe"),
            &"b".repeat(64)
        ),
        Err(mf_installer::receipt::PinProblem::IdentityDrift { .. })
    ));
}

#[test]
fn job_phase_ordering_is_monotonic() {
    assert!(JobPhase::Queued < JobPhase::Resolving);
    assert!(JobPhase::Resolving < JobPhase::Downloading);
    assert!(JobPhase::Downloading < JobPhase::Verifying);
    assert!(JobPhase::Verifying < JobPhase::Installed);
}
