//! Agent 工作流端到端验收测试(UI Task 5;设计 §15):
//! 双 Claude 实例并行互不覆盖、全局 CLI 配置零写入、
//! 插件贡献视图汇总、unsafe-parallel 用户开关。

use crate::plugin_contribution_view::{
    contribution_summary, unsafe_parallel_allowed, PluginContributionSummary,
};

fn summary_fixture() -> Vec<PluginContributionSummary> {
    vec![
        PluginContributionSummary {
            full_id: "monkeyfence.claude".into(),
            name: "Claude (内置)".into(),
            version: "0.1.0".into(),
            content_hash: "hash-a".into(),
            enabled: true,
            authorized_at: Some("2026-08-28T00:00:00Z".into()),
            contribution_counts: vec![
                ("agent_types".into(), 1),
                ("execution_directory_providers".into(), 0),
            ],
            requested_permissions: vec!["net".into(), "hooks".into()],
            compatible: true,
            active_pins: 1,
        },
        PluginContributionSummary {
            full_id: "monkeyfence.git".into(),
            name: "Git worktree".into(),
            version: "0.1.0".into(),
            content_hash: "hash-b".into(),
            enabled: false,
            authorized_at: None,
            contribution_counts: vec![("execution_directory_providers".into(), 1)],
            requested_permissions: vec!["vcs".into()],
            compatible: true,
            active_pins: 0,
        },
    ]
}

#[test]
fn contribution_summary_lists_types_permissions_and_versions() {
    let rows = summary_fixture();
    let text = contribution_summary(&rows);
    assert!(text.contains("monkeyfence.claude"));
    assert!(text.contains("agent_types: 1"));
    assert!(text.contains("execution_directory_providers: 1"));
    assert!(text.contains("vcs"));
    assert!(text.contains("已禁用"), "禁用状态必须可见");
    // 固定版本与内容哈希可见(设计 §11.5)
    assert!(text.contains("0.1.0"));
    assert!(text.contains("hash-a"));
}

#[test]
fn unsafe_parallel_defaults_off_and_user_can_opt_in() {
    // 默认关闭:目录不能隔离时禁止并行(编译器拒绝)
    assert!(!unsafe_parallel_allowed(false, false));
    // 用户显式开启风险开关:允许(自行承担冲突)
    assert!(unsafe_parallel_allowed(false, true));
    // worktree 可隔离:无需开关
    assert!(unsafe_parallel_allowed(true, false));
}

#[test]
fn two_claude_instances_compile_without_global_config_writes() {
    // 编译路径:两个 Claude 实例的 run-temp 互不相同,
    // 且都不指向 ~/.claude(真实全局配置零写入)。
    use mf_agent::agent_instance::AgentInstanceDraft;
    use mf_agent::catalog_store::CatalogStore;
    use mf_agent::{InstanceScope, RunMode};
    use std::collections::HashSet;

    let catalog = CatalogStore::memory().unwrap();
    let mk = |name: &str| AgentInstanceDraft {
        name: name.into(),
        agent_type: "claude".into(),
        scope: InstanceScope::User,
        project_key: None,
        enabled: true,
        run_mode: RunMode::OneShot,
        executable: "claude".into(),
        argv: vec![],
        env: vec![],
        config: serde_json::json!({}),
        execution_contract: serde_json::json!({ "completion": "process-exit" }),
        sealed_secret_ids: vec![],
    };
    let a = catalog.create_agent_instance(mk("implementation")).unwrap();
    let b = catalog.create_agent_instance(mk("review")).unwrap();
    assert_ne!(a.id, b.id, "同类型多实例必须彼此独立");

    let adapter = mf_plugins::builtin::adapter_for("claude-code").unwrap();
    let home = dirs::home_dir().unwrap();
    let mut seen_dirs: HashSet<std::path::PathBuf> = HashSet::new();
    for id in [a.id, b.id] {
        let snapshot = catalog.snapshot_agent_instance(&id, None).unwrap();
        let run_temp = std::env::temp_dir()
            .join("monkeyfence-e2e")
            .join(format!("{id:?}"));
        let ctx = mf_agent::LaunchContext::new(run_temp.clone(), std::path::PathBuf::from("."));
        let plan = adapter.compile_launch(&snapshot, &ctx).unwrap();
        let config_dir = std::path::PathBuf::from(
            plan.env
                .iter()
                .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
                .map(|(_, v)| v.clone())
                .unwrap(),
        );
        assert!(config_dir.starts_with(&run_temp), "必须在 run-temp 下");
        assert_ne!(config_dir, home.join(".claude"), "绝不指向真实全局配置");
        seen_dirs.insert(config_dir);
    }
    assert_eq!(seen_dirs.len(), 2, "两个实例的隔离目录互不重叠");
}

#[test]
fn registry_summaries_carry_real_hash_counts_and_pins() {
    // 真实注册表(内置合成插件):计数非 0 占位、兼容性为计算值
    let host = mf_plugins::PluginHost::load_at_with_catalog(
        std::env::temp_dir().join("mf-pcv-test"),
        mf_agent::CatalogStore::memory().unwrap(),
        &mf_agent::Config::default(),
        &[],
    );
    let rows = crate::plugin_contribution_view::summaries_from_registry(&host);
    let claude = rows
        .iter()
        .find(|r| r.full_id == "monkeyfence.claude")
        .expect("内置 claude");
    assert!(
        claude
            .contribution_counts
            .iter()
            .any(|(k, c)| k == "agent_types" && *c == 1),
        "agent_types 计数必须真实: {:?}",
        claude.contribution_counts
    );
    assert!(claude.compatible);
    assert_eq!(claude.active_pins, 0);
    // 内置目录提供器插件贡献 execution_directories
    let dirs = rows
        .iter()
        .find(|r| r.full_id == "monkeyfence.directories")
        .expect("目录提供器合成插件");
    assert!(
        dirs.contribution_counts
            .iter()
            .any(|(k, c)| k == "execution_directories" && *c == 2),
        "project-dir + worktree 两个贡献: {:?}",
        dirs.contribution_counts
    );
}

// ---------- T0c:fake Agent CLI 行为冻结基线(Issue #14) ----------
//
// 三条链的外部行为契约(不测内部结构):
// - Preview 链 = 离散 CLI 会话(launch_ad_hoc):当前 Preview Session 的
//   实际形态 —— 不属于工作流运行、不产生 Settlement/Handoff;
// - Node 链 = 工作流节点 Agent Session(launch_workflow);
// - mfctl 链 = PipeServer Settlement/Handoff(见 pipe_server.rs 与
//   crates/mfctl/tests/mfctl_pipe_contract.rs)。
// Non-goals:不把 Screen/256 KiB tail 或已知脱敏缺陷固化为正确协议。

use std::sync::Arc;
use std::time::{Duration, Instant};

/// 可控伪 Agent CLI(`tests/fixtures/fake-agent`,mf 包附加 bin target):
/// 启动时把环境(argv/cwd/MF_* 指纹)写入 `<record>/launch.json`,
/// 之后逐行读取 stdin 控制命令,原始字节全量记录到 `<record>/input.hex`,
/// stdout 输出记录到 `<record>/output.hex`。
/// MF_RUN_TOKEN 只记录 SHA-256 指纹与长度 —— 记录文件、测试输出、
/// panic 信息都不携带真实 token。
pub(crate) mod fake_agent {
    use super::*;
    use serde_json::Value;

    /// 构建并定位独立 fixture crate 的 fake-agent。输出目录按
    /// Cargo.toml + main.rs 内容哈希分片,新版本绝不覆盖 Windows 上
    /// 可能仍被旧测试进程锁定的 exe。fixture crate 不属于 workspace,
    /// 普通 build/install/release 不会产出它。
    pub(crate) fn exe() -> std::path::PathBuf {
        static EXE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        EXE.get_or_init(|| {
            let current = std::env::current_exe().expect("current_exe");
            let mut profile_dir = current.parent().expect("parent").to_path_buf();
            if profile_dir.file_name().and_then(|n| n.to_str()) == Some("deps") {
                profile_dir.pop();
            }
            let workspace_target = profile_dir.parent().expect("target root");
            let fixture_dir =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agent");
            let mut source =
                std::fs::read(fixture_dir.join("Cargo.toml")).expect("fixture Cargo.toml");
            source.extend_from_slice(
                &std::fs::read(fixture_dir.join("main.rs")).expect("fixture main"),
            );
            let digest = sha256_hex(&source);
            let fixture_target = workspace_target
                .join("test-fixtures")
                .join(format!("fake-agent-{}", &digest[..16]));
            let name = if cfg!(windows) {
                "fake-agent.exe"
            } else {
                "fake-agent"
            };
            let candidate = fixture_target.join("debug").join(name);
            if !candidate.is_file() {
                let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
                let mut cmd = std::process::Command::new(cargo);
                cmd.arg("build")
                    .arg("--manifest-path")
                    .arg(fixture_dir.join("Cargo.toml"))
                    .arg("--target-dir")
                    .arg(&fixture_target);
                let output = cmd
                    .output()
                    .expect("启动 cargo 构建 fake-agent 失败(CARGO 未设置?)");
                assert!(
                    output.status.success(),
                    "构建独立 fake-agent fixture 失败: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                candidate.is_file(),
                "fake-agent bin 不存在且构建未产出: {}",
                candidate.display()
            );
            candidate
        })
        .clone()
    }

    pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(bytes))
    }

    #[cfg(windows)]
    pub(crate) fn process_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        unsafe { CloseHandle(handle) };
        true
    }

    #[cfg(not(windows))]
    pub(crate) fn process_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    pub(crate) fn b64(bytes: &[u8]) -> String {
        // 测试内自足的 base64(只覆盖命令负载,避免新增依赖)
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            out.push(TABLE[(b[0] >> 2) as usize] as char);
            out.push(TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[(b[2] & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    pub(crate) fn read_launch(record: &std::path::Path) -> Value {
        let text = std::fs::read_to_string(record.join("launch.json"))
            .expect("fake-agent 必须在启动时写出 launch.json");
        serde_json::from_str(&text).expect("launch.json 必须是合法 JSON")
    }

    /// 读 input.hex(全量原始字节记录)并解码为字节。
    pub(crate) fn recorded_input(record: &std::path::Path) -> Vec<u8> {
        hex_decode(&std::fs::read_to_string(record.join("input.hex")).unwrap_or_default())
            .unwrap_or_default()
    }

    fn hex_decode(text: &str) -> Result<Vec<u8>, String> {
        let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = cleaned.as_bytes();
        if bytes.len() % 2 != 0 {
            return Err("hex 长度必须是偶数".into());
        }
        (0..bytes.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect()
    }

    /// 等待条件为真(轮询;用于 fake-agent 异步写记录文件)。
    pub(crate) fn wait_for_record(
        record: &std::path::Path,
        timeout: Duration,
        mut cond: impl FnMut(&Value) -> bool,
    ) -> Value {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Ok(text) = std::fs::read_to_string(record.join("launch.json")) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if cond(&value) {
                        return value;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        panic!("等待 launch.json 超时({timeout:?}): {}", record.display());
    }

    /// 等待 input.hex / output.hex 记录出现指定字节序列。
    pub(crate) fn wait_recorded_bytes(
        record: &std::path::Path,
        file: &str,
        needle: &[u8],
        timeout: Duration,
    ) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Ok(text) = std::fs::read_to_string(record.join(file)) {
                if let Ok(bytes) = hex_decode(&text) {
                    if contains_subsequence(&bytes, needle) {
                        return;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let seen = std::fs::read_to_string(record.join(file)).unwrap_or_default();
        panic!(
            "等待 {file} 出现 {} 字节超时;已记录(hex): {}",
            needle.len(),
            truncate(&seen, 4000)
        );
    }

    /// 字节子序列匹配:PTY 规程可能在行尾插入 \r/\n,但不破坏
    /// MonkeyFence 透传的原始字节内容本身。
    pub(crate) fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn truncate(text: &str, max: usize) -> &str {
        match text.char_indices().nth(max) {
            Some((idx, _)) => &text[..idx],
            None => text,
        }
    }
}

/// 契约:fake-agent 以 `--record <dir>` 启动即写 launch.json;
/// MF_RUN_TOKEN 只以指纹出现,原文绝不落入记录。
#[test]
fn fake_agent_bin_records_launch_with_token_fingerprint_only() {
    let record = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(fake_agent::exe())
        .arg("--record")
        .arg(record.path())
        .arg("尾随参数-prompt-42")
        .env("MF_RUN_TOKEN", "mft-canonical-red")
        .env("MF_PIPE", r"\\.\pipe\monkeyfence-mfctl-424242")
        .env("MFCTL_HINT", "hint-中文-42")
        .env("FAKE_AGENT_SESSION", "session-tag")
        .output()
        .expect("启动 fake-agent 失败");
    assert!(out.status.success(), "fake-agent 应正常退出");
    let launch = fake_agent::read_launch(record.path());
    let env = launch.get("env").expect("launch.json 必须记录 env");
    assert_eq!(
        env.get("MF_RUN_TOKEN_SHA256")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        fake_agent::sha256_hex(b"mft-canonical-red"),
        "token 必须以 SHA-256 指纹记录"
    );
    assert_eq!(
        env.get("MF_RUN_TOKEN_LEN").and_then(|v| v.as_i64()),
        Some(17),
        "token 长度用于比对真实令牌"
    );
    assert_eq!(
        env.get("MF_PIPE").and_then(|v| v.as_str()),
        Some(r"\\.\pipe\monkeyfence-mfctl-424242")
    );
    assert_eq!(
        env.get("MFCTL_HINT").and_then(|v| v.as_str()),
        Some("hint-中文-42"),
        "MFCTL_HINT 原样注入"
    );
    assert_eq!(
        env.get("FAKE_AGENT_SESSION").and_then(|v| v.as_str()),
        Some("session-tag")
    );
    // launch.json 全文不得包含 token 原文(指纹之外的泄露路径)
    let raw = std::fs::read_to_string(record.path().join("launch.json")).unwrap();
    assert!(
        !raw.contains("mft-canonical-red"),
        "launch.json 泄露 token 原文"
    );
    // argv 记录 --record 之后的尾随参数(prompt 注入形态)
    let argv: Vec<&str> = launch
        .get("argv")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(argv.last().copied(), Some("尾随参数-prompt-42"));
}

// ---------- Preview 链(离散 CLI 会话):当前 Preview Session 形态 ----------

use crate::runtime_host::{RuntimeHostImpl, SessionRegistry};
use mf_agent::runtime::{AdHocLaunchSpec, RuntimeEvent, RuntimeHost as _};

/// 构造离散 CLI 会话(Preview 链)的启动规格:可执行文件 = fake-agent,
/// cwd/注册路由键 = workdir,进程注册键 = display_id。
fn fake_ad_hoc_spec(
    events: crossbeam_channel::Sender<(i64, RuntimeEvent)>,
    exe: &std::path::Path,
    record: &std::path::Path,
    workdir: &std::path::Path,
    display_id: i64,
) -> AdHocLaunchSpec {
    use mf_agent::InputInjection;
    static NEXT_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    AdHocLaunchSpec {
        task_id: 1,
        session_id: display_id + 100, // ad-hoc 行号(事件 tag),与 display 分离
        display_session_id: display_id,
        display_session_handle: format!(
            "session-preview-{display_id}-{}",
            NEXT_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ),
        title: "预览会话".into(),
        run_mode: mf_agent::RunMode::Interactive,
        plan: mf_agent::LaunchPlan {
            run_temp: record.join("run-temp"),
            executable: exe.to_path_buf(),
            argv: vec![
                "--record".into(),
                record.to_string_lossy().into_owned(),
                "预览-prompt-尾参".into(),
            ],
            env: vec![("FAKE_AGENT_SESSION".into(), format!("display-{display_id}"))],
            secret_env: vec![],
            cwd: None, // None → 进程 cwd = spec.workdir
            temp_files: vec![],
            input: InputInjection::Argv(String::new()),
            completion: mf_agent::CompletionDetector::ProcessExit,
            uses_shell: false,
        },
        run_temp: record.join("run-temp"),
        workdir: workdir.to_path_buf(),
        events,
    }
}

fn wait_alive(registry: &SessionRegistry, session_handle: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if registry.session_alive(session_handle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    false
}

/// panic 安全的会话清理:测试失败(unwind)也终止 fake-agent 子进程,
/// 避免孤儿进程锁住构建产物导致后续测试的 fixture 重建失败。
struct SessionGuard<'a> {
    registry: &'a SessionRegistry,
    session_handle: String,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let pid = self.registry.session_pid(&self.session_handle);
        self.registry.kill_session(&self.session_handle);
        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(5);
            while fake_agent::process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// 契约:Preview/离散会话以 (workdir, display_id) 路由拉起真实 CLI;
/// argv 原样传递、cwd = workdir;不注入结算令牌(离散会话无 Settlement)。
#[test]
fn preview_ad_hoc_session_launches_fake_agent_without_settlement_token() {
    let record = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let registry = SessionRegistry::new(mf_agent::Config::default());
    let host = RuntimeHostImpl::new(registry.clone());
    let (events, _rx) = crossbeam_channel::bounded(16);
    let spec = fake_ad_hoc_spec(
        events,
        &fake_agent::exe(),
        record.path(),
        workdir.path(),
        800,
    );
    let session_handle = spec.display_session_handle.clone();
    host.launch_ad_hoc(spec).expect("离散会话启动失败");

    let _guard = SessionGuard {
        registry: &registry,
        session_handle: session_handle.clone(),
    };
    let launch = fake_agent::wait_for_record(record.path(), Duration::from_secs(15), |_| true);
    // argv 原样(--record 与尾随参数均在,顺序保留)
    let argv: Vec<String> = launch["argv"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert_eq!(
        argv,
        vec![
            "--record".to_string(),
            record.path().to_string_lossy().into_owned(),
            "预览-prompt-尾参".to_string()
        ],
        "argv 必须原样传递"
    );
    // cwd = workdir(项目目录;注册表路由键与进程 cwd 同源)
    let cwd = std::path::PathBuf::from(launch["cwd"].as_str().unwrap());
    assert_eq!(cwd, workdir.path(), "进程 cwd 必须是 workdir");
    // 离散会话不参与结算:无 MF_RUN_TOKEN 注入
    assert_eq!(
        launch["env"]["MF_RUN_TOKEN_LEN"].as_i64(),
        Some(0),
        "Preview/离散会话不得注入结算令牌"
    );
    // 会话以 display id 注册存活
    assert!(
        wait_alive(&registry, &session_handle, Duration::from_secs(10)),
        "会话必须以 display handle 注册存活"
    );
    assert!(!registry.session_alive("unrelated-session-handle"));
}

/// 契约:Preview 会话的原始输入/输出不被 MonkeyFence 解释或改写 ——
/// `/xxx` slash command、Unicode、ANSI/TUI 转义序列原样到达 CLI
/// (input.hex 子序列证明),CLI 写出的 Unicode/ANSI 原样进入屏幕投影。
#[test]
fn preview_session_passes_raw_bytes_without_interpretation() {
    let record = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let registry = SessionRegistry::new(mf_agent::Config::default());
    let host = RuntimeHostImpl::new(registry.clone());
    let (events, _rx) = crossbeam_channel::bounded(16);
    let spec = fake_ad_hoc_spec(
        events,
        &fake_agent::exe(),
        record.path(),
        workdir.path(),
        801,
    );
    let session_handle = spec.display_session_handle.clone();
    host.launch_ad_hoc(spec).unwrap();
    let _guard = SessionGuard {
        registry: &registry,
        session_handle: session_handle.clone(),
    };
    assert!(
        wait_alive(&registry, &session_handle, Duration::from_secs(10)),
        "前置:会话应存活"
    );

    // 1) 原始输入:slash command / Unicode 原样到达 CLI
    //    (MonkeyFence send_prompt_raw 是字节透传;PTY 行规程会把行尾 \r
    //    规范为 \r\n —— 子序列断言不受影响)
    registry
        .send_prompt_raw(&session_handle, b"/model gpt-5\r")
        .expect("写入 PTY 失败");
    registry
        .send_prompt_raw(&session_handle, "/skills 你好-技能-Ω\r".as_bytes())
        .expect("写入 PTY 失败");
    fake_agent::wait_recorded_bytes(
        record.path(),
        "input.hex",
        b"/model gpt-5",
        Duration::from_secs(10),
    );
    fake_agent::wait_recorded_bytes(
        record.path(),
        "input.hex",
        "/skills 你好-技能-Ω".as_bytes(),
        Duration::from_secs(10),
    );

    // 2) 原始输出:CLI 写出的 Unicode + ANSI 颜色原样进入屏幕投影
    let mut payload = b"tui-out\x1b[31m".to_vec();
    payload.extend_from_slice("输出-你好-Φ→红字".as_bytes());
    payload.extend_from_slice(b"-end");
    // 后缀把 StreamingRedactor 的跨块 carry 推过 payload 尾部;
    // 断言本身不依赖 carry 长度。
    let mut emitted = payload.clone();
    emitted.extend(std::iter::repeat_n(b'.', 256));
    let command = format!("!out {}\r", fake_agent::b64(&emitted));
    registry
        .send_prompt_raw(&session_handle, command.as_bytes())
        .expect("写入 PTY 失败");
    fake_agent::wait_recorded_bytes(
        record.path(),
        "output.hex",
        &emitted,
        Duration::from_secs(10),
    );
    // 生产 reader 管线在 Screen 解释之前保留完整字节;
    // 此处不断言现有 tail 容量,只断言小负载不被改写。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut raw_saw = false;
    while Instant::now() < deadline {
        if let Some(bytes) = registry.pty_output_bytes(&session_handle) {
            if fake_agent::contains_subsequence(&bytes, &payload) {
                raw_saw = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(raw_saw, "CLI 输出必须逐字节穿过生产 PTY reader");
    // Screen 如何解释 ANSI 属于本 ticket 的明确 non-goal;上面只冻结
    // reader 进入 Screen 之前的原始字节边界。
}

/// 契约:像真实 TUI 一样开启 VT/raw 输入后,ESC、NUL、Unicode
/// 与 slash command 逐字节到达 CLI;MonkeyFence 不解释键盘协议。
#[test]
fn preview_tui_input_sequences_reach_cli_unchanged() {
    let record = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let registry = SessionRegistry::new(mf_agent::Config::default());
    let host = RuntimeHostImpl::new(registry.clone());
    let (events, _rx) = crossbeam_channel::bounded(16);
    let spec = fake_ad_hoc_spec(
        events,
        &fake_agent::exe(),
        record.path(),
        workdir.path(),
        802,
    );
    let session_handle = spec.display_session_handle.clone();
    host.launch_ad_hoc(spec).unwrap();
    let _guard = SessionGuard {
        registry: &registry,
        session_handle: session_handle.clone(),
    };
    assert!(
        wait_alive(&registry, &session_handle, Duration::from_secs(10)),
        "前置:会话应存活"
    );
    fake_agent::wait_for_record(record.path(), Duration::from_secs(10), |_| true);
    // 使用真实键盘 TUI 序列(上箭头/Ctrl+左),而不是属于输出端
    // 的清屏/光标命令;ConPTY 会解析后者而不会把它们交给 CLI。
    let mut raw = b"\x1b[A\x1b[1;5D\x00/model ".to_vec();
    raw.extend_from_slice("终端-Ω".as_bytes());
    raw.push(b'\r');
    registry
        .send_prompt_raw(&session_handle, &raw)
        .expect("写入 PTY 失败");
    let expected = &raw[..raw.len() - 1];
    fake_agent::wait_recorded_bytes(
        record.path(),
        "input.hex",
        expected,
        Duration::from_secs(10),
    );
    let recorded = fake_agent::recorded_input(record.path());
    assert!(
        fake_agent::contains_subsequence(&recorded, expected),
        "ESC/NUL/Unicode/slash 输入必须保持原始字节序列"
    );
}

/// 契约:Preview 会话进程以指定码退出 → 事件流携带真实退出码
/// (tag = ad-hoc 行号),注册表条目摘除。
#[test]
fn preview_session_exit_reports_code_and_detaches() {
    let record = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let registry = SessionRegistry::new(mf_agent::Config::default());
    let host = RuntimeHostImpl::new(registry.clone());
    let (events, rx) = crossbeam_channel::bounded(16);
    let spec = fake_ad_hoc_spec(
        events,
        &fake_agent::exe(),
        record.path(),
        workdir.path(),
        803,
    );
    let session_handle = spec.display_session_handle.clone();
    host.launch_ad_hoc(spec).unwrap();
    let _guard = SessionGuard {
        registry: &registry,
        session_handle: session_handle.clone(),
    };
    assert!(
        wait_alive(&registry, &session_handle, Duration::from_secs(10)),
        "前置:会话应存活"
    );
    registry
        .send_prompt_raw(&session_handle, b"!exit 7\r")
        .expect("写入 PTY 失败");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut exit_event = None;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok((tag, RuntimeEvent::AdHocExited { exit_code, .. })) => {
                assert_eq!(tag, 903, "事件 tag 是 ad-hoc 行号(session_id)");
                exit_event = exit_code;
                break;
            }
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert_eq!(exit_event, Some(7), "退出事件必须携带真实退出码");
    let removed = (0..150).any(|_| {
        std::thread::sleep(Duration::from_millis(20));
        !registry.session_alive(&session_handle)
    });
    assert!(removed, "退出后注册表条目必须摘除");
}

/// 契约:终止只影响目标 Agent Session —— 同项目其他会话继续服务,
/// 跨项目的同编号会话(各自数据库行号会碰撞)也不受影响。
#[test]
fn preview_termination_isolates_target_session_only() {
    let registry = SessionRegistry::new(mf_agent::Config::default());
    let host = RuntimeHostImpl::new(registry.clone());
    let project_dir = tempfile::tempdir().unwrap();

    // 同项目两个会话 + 另一项目的同编号会话
    let mut records = Vec::new();
    let mut launch = |display: i64, workdir: &std::path::Path| {
        let record = tempfile::tempdir().unwrap();
        let (events, _rx) = crossbeam_channel::bounded(16);
        let spec = fake_ad_hoc_spec(events, &fake_agent::exe(), record.path(), workdir, display);
        let handle = spec.display_session_handle.clone();
        host.launch_ad_hoc(spec).unwrap();
        records.push((record, handle));
    };
    launch(810, project_dir.path());
    launch(811, project_dir.path());
    let other_project = tempfile::tempdir().unwrap();
    launch(810, other_project.path()); // 跨项目同 display id
    for (_, handle) in &records {
        assert!(
            wait_alive(&registry, handle, Duration::from_secs(10)),
            "前置:会话 {handle} 应存活"
        );
    }
    let pids: Vec<u32> = records
        .iter()
        .map(|(record, _)| {
            fake_agent::wait_for_record(record.path(), Duration::from_secs(10), |_| true)["pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .expect("fake-agent 必须记录 OS PID")
        })
        .collect();
    assert!(pids.iter().all(|pid| fake_agent::process_alive(*pid)));
    let _guard_main = SessionGuard {
        registry: &registry,
        session_handle: records[1].1.clone(),
    };
    let _guard_other = SessionGuard {
        registry: &registry,
        session_handle: records[2].1.clone(),
    };

    // 终止第一个 handle：其余会话（即便行号同为 810）都必须继续。
    registry.kill_session(&records[0].1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while registry.session_alive(&records[0].1) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!registry.session_alive(&records[0].1), "目标会话必须终止");
    let target_exited = (0..200).any(|_| {
        if !fake_agent::process_alive(pids[0]) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
        false
    });
    assert!(target_exited, "目标 fake-agent OS 进程必须真正退出");
    assert!(
        registry.session_alive(&records[1].1),
        "同项目其他会话必须继续存活"
    );
    assert!(
        registry.session_alive(&records[2].1),
        "跨项目同编号会话必须不受影响(注册表按 opaque handle 路由)"
    );
    assert!(
        fake_agent::process_alive(pids[1]) && fake_agent::process_alive(pids[2]),
        "非目标 fake-agent OS 进程必须继续存活"
    );

    // 存活会话仍可响应输入(终止未破坏共享通道)
    let probe = "隔离探针-仍然存活".as_bytes();
    let command = format!("!out {}\r", fake_agent::b64(probe));
    registry
        .send_prompt_raw(&records[1].1, command.as_bytes())
        .expect("写入 PTY 失败");
    let probe_record = records[1].0.path();
    fake_agent::wait_recorded_bytes(probe_record, "output.hex", probe, Duration::from_secs(10));
}

// ---------- Node 链:工作流节点 Agent Session(Orchestrator 全链) ----------

use mf_agent::orchestrator::{GlobalLimiter, Orchestrator, ProfileCatalog, WorkflowKernel};
use mf_agent::workflow::{ProjectWorkflowDraft, WorkflowNodeDraft, WorkflowTemplateVersion};
use mf_agent::AgentInstanceDraft;
use parking_lot::RwLock;

/// Node 链夹具:真实 Store + Orchestrator + RuntimeHostImpl(带插件启动器)
/// + fake-agent 插件/实例;项目工作流单节点,confirm_and_run 真实调度。
struct NodeChain {
    orch: Arc<Orchestrator>,
    registry: Arc<SessionRegistry>,
    record: tempfile::TempDir,
    task_id: i64,
    pipe_name: String,
    _plugins_root: tempfile::TempDir,
    _project: tempfile::TempDir,
}

impl NodeChain {
    fn spawn(pipe_name: &str) -> NodeChain {
        let exe = fake_agent::exe();
        let record = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = project.path().to_path_buf();
        let catalog: Arc<mf_agent::CatalogStore> = mf_agent::CatalogStore::memory().unwrap();

        // 插件:贡献 fakeagent Agent Type(generic-command 适配器)
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("monkeyfence-plugin.toml"),
            r#"[manifest]
version = 2
publisher = "mf-e2e"
id = "fakeagent"
name = "Fake Agent Fixture"
version_str = "0.1.0"
description = "T0c node chain fixture"

[capabilities]

[[agent_types]]
id = "fake"
name = "Fake Agent"
adapter = "generic-command"
command = "fake-agent"
modes = ["oneshot", "interactive"]
"#,
        )
        .unwrap();
        let plugins_root = tempfile::tempdir().unwrap();
        let plugins = mf_plugins::PluginRegistry::load_at_with_catalog(
            plugins_root.path().to_path_buf(),
            catalog.clone(),
            &mf_agent::Config::default(),
            &[],
        );
        plugins
            .install_package(
                src.path(),
                mf_plugins::install::InstallSource::Local {
                    path: src.path().display().to_string(),
                },
            )
            .unwrap();
        plugins.enable("mf-e2e.fakeagent", true).unwrap();

        // 实例:executable = fake-agent,argv 注入 --record(契约断言原样传递)
        let instance = catalog
            .create_agent_instance(AgentInstanceDraft {
                name: "fake-worker".into(),
                agent_type: "mf-e2e.fakeagent.fake".into(),
                scope: mf_agent::InstanceScope::User,
                project_key: None,
                enabled: true,
                run_mode: mf_agent::RunMode::OneShot,
                executable: exe.to_string_lossy().into_owned(),
                argv: vec![
                    "--record".into(),
                    record.path().to_string_lossy().into_owned(),
                ],
                env: vec![],
                config: serde_json::json!({}),
                execution_contract: serde_json::json!({
                    "input": "argv",
                    "completion": "manual"
                }),
                sealed_secret_ids: vec![],
            })
            .unwrap();

        let registry = SessionRegistry::new(mf_agent::Config::default());
        let host = crate::runtime_host::RuntimeHostImpl::with_launcher(
            registry.clone(),
            crate::runtime_host::WorkflowLauncher {
                plugins: plugins.clone(),
                catalog: catalog.clone(),
                secret_master_key: None,
            },
        );
        let store: Arc<mf_agent::Store> =
            mf_agent::Store::open(&root.join("workflow-v1.db")).unwrap();
        let pipe_name = pipe_name.to_string();
        let orch = Orchestrator::start_with(
            store,
            root.clone(),
            mf_agent::Config::default(),
            host,
            Arc::new(RwLock::new(ProfileCatalog::default())),
            GlobalLimiter::new(4),
            pipe_name.clone(),
            Arc::new(mf_agent::execution_directory::ProjectDirectoryProvider),
            WorkflowKernel {
                catalog,
                pins: None,
                instance_resolver: None,
            },
        )
        .unwrap();

        // 项目工作流 → 投影模板版本 → assign → confirm_and_run(真实调度)
        let record_draft = orch
            .store
            .save_project_workflow(&ProjectWorkflowDraft {
                key: "wf-fake".into(),
                name: "fake 工作流".into(),
                nodes: vec![WorkflowNodeDraft {
                    key: "n1".into(),
                    title: "节点 n1".into(),
                    instructions: "执行 fake-agent 步骤".into(),
                    agent_instance_id: instance.id.clone(),
                    deps: vec![],
                }],
                allow_unsafe_parallel: false,
            })
            .unwrap();
        let version = WorkflowTemplateVersion {
            version_id: 0,
            template_key: format!("project-workflow/{}", record_draft.key),
            version: 1,
            nodes: record_draft.nodes.clone(),
            created_at: String::new(),
        };
        let task = orch
            .create_task("节点链任务", "目标-冻结节点会话行为")
            .unwrap();
        let pins = crate::adapter_launch::workflow_plugin_index(&plugins);
        orch.assign_workflow(task.id, &version, &pins, false)
            .unwrap();
        orch.confirm_and_run(task.id).unwrap();
        NodeChain {
            orch,
            registry,
            record,
            task_id: task.id,
            pipe_name,
            _plugins_root: plugins_root,
            _project: project,
        }
    }

    fn wait_run(&self) -> mf_agent::model::RunView {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(runs) = self.orch.store.list_runs_of_task(self.task_id) {
                if let Some(run) = runs.into_iter().max_by_key(|r| r.id) {
                    return run;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("节点 run 未在时限内启动");
    }
}

impl Drop for NodeChain {
    fn drop(&mut self) {
        let mut pids = Vec::new();
        if let Ok(runs) = self.orch.store.list_runs_of_task(self.task_id) {
            for run in runs {
                if let Some(sid) = run.session_id {
                    if let Ok(Some(session)) = self.orch.store.session_view(sid) {
                        if let Some(pid) = self.registry.session_pid(&session.public_handle) {
                            pids.push(pid);
                        }
                        self.registry.kill_session(&session.public_handle);
                    }
                }
            }
        }
        self.orch.stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&self.orch) > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        for pid in pids {
            let deadline = Instant::now() + Duration::from_secs(5);
            while fake_agent::process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// 契约:Node 链注入的能力令牌与管道名原样进入 Agent CLI 环境;
/// argv 原样传递(prompt 为尾参);令牌与 run 记录一一对应。
#[test]
fn node_session_injects_capability_token_and_pipe_into_cli_env() {
    let chain = NodeChain::spawn(r"\\.\pipe\monkeyfence-mfctl-node-a");
    let run = chain.wait_run();
    let token = run.capability_token.clone();
    assert!(!token.is_empty(), "run 必须持有一次性能力令牌");

    // fake-agent 记录的环境:token 指纹与 run 令牌一致、MF_PIPE 一致
    let launch = fake_agent::wait_for_record(
        chain.record.path(),
        Duration::from_secs(15),
        |v: &serde_json::Value| v["env"]["MF_RUN_TOKEN_LEN"].as_i64() == Some(token.len() as i64),
    );
    assert_eq!(
        launch["env"]["MF_RUN_TOKEN_SHA256"].as_str().unwrap(),
        fake_agent::sha256_hex(token.as_bytes()),
        "CLI 收到的 MF_RUN_TOKEN 必须与 run 的 capability_token 一致"
    );
    assert_eq!(
        launch["env"]["MF_PIPE"].as_str().unwrap(),
        chain.pipe_name,
        "MF_PIPE 必须注入 Orchestrator 的管道名"
    );
    // 进程实际接收 --record + prompt 尾参;fake-agent 记录层会
    // 脱敏 prompt 中的令牌,因此只断言非机密形状与目标文本。
    let argv: Vec<String> = launch["argv"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert_eq!(argv.first().map(String::as_str), Some("--record"));
    let prompt = argv.last().unwrap();
    assert!(
        prompt.contains("目标-冻结节点会话行为"),
        "prompt(goal)必须作为尾参传递(以长度前缀避免落盘 token): {} 字节",
        prompt.len()
    );
    assert!(prompt.contains("MF_RUN_TOKEN 环境变量"));
    assert!(!prompt.contains("mfctl --token"));
    assert!(
        !prompt.contains("[MF_RUN_TOKEN]"),
        "prompt 本身不得再携带需由记录层脱敏的能力令牌"
    );
    let launch_raw = std::fs::read_to_string(chain.record.path().join("launch.json")).unwrap();
    assert!(
        !launch_raw.contains(&token),
        "launch.json 不得保存真实 capability token"
    );
    // 原始输入直达节点会话
    let session_id = run.session_id.expect("节点 run 必须绑定会话");
    let session_handle = chain
        .orch
        .store
        .session_view(session_id)
        .unwrap()
        .unwrap()
        .public_handle;
    assert!(chain.registry.session_alive(&session_handle));
    chain
        .registry
        .send_prompt_raw(&session_handle, b"/model node-42\r")
        .expect("写入 PTY 失败");
    fake_agent::wait_recorded_bytes(
        chain.record.path(),
        "input.hex",
        b"/model node-42",
        Duration::from_secs(10),
    );
}

/// 契约:Agent CLI 退出(即使退出码 0)后,Workflow Run 仍等待显式
/// Settlement —— 进入 awaiting-outcome 并保持,不自动判定成功;
/// 显式结算后才收敛。
#[test]
fn node_cli_exit_awaits_explicit_settlement() {
    let chain = NodeChain::spawn(r"\\.\pipe\monkeyfence-mfctl-node-b");
    let run = chain.wait_run();
    let session_id = run.session_id.expect("节点 run 必须绑定会话");
    let session_handle = chain
        .orch
        .store
        .session_view(session_id)
        .unwrap()
        .unwrap()
        .public_handle;
    assert!(
        wait_alive(&chain.registry, &session_handle, Duration::from_secs(15)),
        "前置:节点会话应注册存活"
    );

    // CLI 以退出码 0 结束
    chain
        .registry
        .send_prompt_raw(&session_handle, b"!exit 0\r")
        .expect("写入 PTY 失败");
    let awaiting = Instant::now() + Duration::from_secs(15);
    while Instant::now() < awaiting {
        if let Ok(Some(r)) = chain.orch.store.run_view(run.id) {
            if r.status == mf_agent::RunStatus::AwaitingOutcome {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        chain
            .orch
            .store
            .run_view(run.id)
            .unwrap()
            .is_some_and(|r| r.status == mf_agent::RunStatus::AwaitingOutcome),
        "CLI 退出后 run 必须进入 awaiting-outcome"
    );
    // 等待窗口内保持待结算(进程退出 ≠ 成功结算)
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        chain
            .orch
            .store
            .run_view(run.id)
            .unwrap()
            .is_some_and(|r| r.status == mf_agent::RunStatus::AwaitingOutcome),
        "无显式 Settlement 时 run 必须保持 awaiting-outcome"
    );
    assert_eq!(
        chain
            .orch
            .store
            .task_view(chain.task_id)
            .unwrap()
            .unwrap()
            .status,
        mf_agent::TaskStatus::NeedsYou,
        "Task 应进入 needs-you 等待人工决策"
    );

    // 显式结算 → 收敛
    chain
        .orch
        .settle_by_token(
            &run.capability_token,
            mf_agent::Settlement::Complete {
                summary: "节点链完成".into(),
                output: Default::default(),
            },
        )
        .unwrap();
    let done = Instant::now() + Duration::from_secs(15);
    while Instant::now() < done {
        if let Ok(Some(t)) = chain.orch.store.task_view(chain.task_id) {
            if t.status == mf_agent::TaskStatus::Succeeded {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("显式结算后 Task 必须收敛为 succeeded");
}

/// 契约:capability token 只对签发它的 run/项目有效 ——
/// 跨项目令牌不可结算(不同项目数据库各自签发,互不相认)。
#[test]
fn node_capability_token_is_scoped_to_its_own_project() {
    let chain_a = NodeChain::spawn(r"\\.\pipe\monkeyfence-mfctl-node-c1");
    let chain_b = NodeChain::spawn(r"\\.\pipe\monkeyfence-mfctl-node-c2");
    let run_a = chain_a.wait_run();
    let run_b = chain_b.wait_run();

    // 两个项目的令牌互不相同(指纹不同;不打印原值)
    assert_ne!(
        fake_agent::sha256_hex(run_a.capability_token.as_bytes()),
        fake_agent::sha256_hex(run_b.capability_token.as_bytes()),
        "跨项目 run 必须持不同令牌"
    );
    // A 的令牌在 B 上结算 → 拒绝
    let rejected = chain_b.orch.settle_by_token(
        &run_a.capability_token,
        mf_agent::Settlement::Complete {
            summary: "越权结算".into(),
            output: Default::default(),
        },
    );
    assert!(rejected.is_err(), "跨项目 token 结算必须被拒绝");
    assert!(
        chain_b
            .orch
            .store
            .run_view(run_b.id)
            .unwrap()
            .is_some_and(|r| r.outcome.is_none()),
        "被拒绝的结算不得影响 B 的 run"
    );
    // B 自己的令牌结算 → 成功
    chain_b
        .orch
        .settle_by_token(
            &run_b.capability_token,
            mf_agent::Settlement::Complete {
                summary: "B 项目完成".into(),
                output: Default::default(),
            },
        )
        .unwrap();
    let settled = Instant::now() + Duration::from_secs(15);
    while Instant::now() < settled {
        if let Ok(Some(r)) = chain_b.orch.store.run_view(run_b.id) {
            if r.outcome.is_some() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        chain_b
            .orch
            .store
            .run_view(run_b.id)
            .unwrap()
            .is_some_and(|r| r.outcome.is_some()),
        "本项目的令牌结算必须生效"
    );
    let session_a = chain_a
        .orch
        .store
        .session_view(run_a.session_id.expect("A run 绑定会话"))
        .unwrap()
        .unwrap();
    chain_a.registry.kill_session(&session_a.public_handle);
}

// ---------- 工作流优先主路径端到端(ADR 0004 / Task 8) ----------

fn e2e_wait(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

fn dir_fingerprint(path: &std::path::Path) -> String {
    use sha2::Digest;
    let mut acc = String::new();
    fn walk(prefix: &str, dir: &std::path::Path, acc: &mut String) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map(|it| it.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if entry.path().is_dir() {
                walk(&rel, &entry.path(), acc);
            } else {
                let bytes = std::fs::read(entry.path()).unwrap_or_default();
                let digest = sha2::Sha256::digest(&bytes);
                acc.push_str(&format!("{rel}:{}:{:x}", bytes.len(), digest));
            }
        }
    }
    walk("", path, &mut acc);
    acc
}

/// 检测到的默认 CLI 测试插件(adapter claude-code + 命令 hostname:
/// 无论参数如何都会退出 → manual 完成语义下进入 awaiting-outcome):
/// 注册进临时根插件宿主后注入 AppCtx —— 全链真实、不触用户 ~/.monkeyfence。
fn install_e2e_cli_plugin(
    catalog: &Arc<mf_agent::CatalogStore>,
) -> Arc<mf_plugins::PluginRegistry> {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(
        src.path().join("monkeyfence-plugin.toml"),
        r#"[manifest]
version = 2
publisher = "mf-e2e"
id = "hostcli"
name = "Hostname CLI E2E"
version_str = "0.1.0"
description = "workflow-first e2e plugin"

[capabilities]

[[agent_types]]
id = "hostname"
name = "Hostname Agent"
adapter = "claude-code"
command = "hostname"
modes = ["oneshot", "interactive"]
"#,
    )
    .unwrap();
    let host = mf_plugins::PluginRegistry::load_at_with_catalog(
        tempfile::tempdir().unwrap().path().to_path_buf(),
        catalog.clone(),
        &mf_agent::Config::default(),
        &[],
    );
    host.install_package(
        src.path(),
        mf_plugins::install::InstallSource::Local {
            path: src.path().display().to_string(),
        },
    )
    .unwrap();
    host.enable("mf-e2e.hostcli", true).unwrap();
    host
}

/// 主场景:打开项目(不建 Task)→ 新建项目工作流 →
/// 默认 CLI 节点 + 保存实例节点 + 依赖 → 画布请求运行 → Composer 输入目标 →
/// 自动创建 Task/Revision 并启动第一个节点 → 第二个节点 awaiting-outcome →
/// 徽标 1 → 直达第二个节点 → 人工结算收敛 → 徽标清零 →
/// 工作流跨重启保留 → 默认 CLI 零写入外部配置。
#[gpui::test]
fn project_workflow_first_run_loop_e2e(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    let catalog = mf_agent::CatalogStore::memory().unwrap();
    // 插件注册表 → 临时根(内置 + 检测到的默认 CLI 测试插件);
    // 先注入注册表再打开项目(RuntimeHost 在 open_project 时接线)
    let ctx = crate::app_ctx::AppCtx::with_parts_and_plugins_for_tests(
        mf_agent::Config::default(),
        catalog.clone(),
        install_e2e_cli_plugin(&catalog),
    );

    // 1) 打开项目,不创建任何 Task
    let project = tempfile::tempdir().unwrap();
    let service = mf_kernel::project_registry::ServiceStore::open(
        &project.path().join("service-first-run.db"),
    )
    .unwrap();
    let (runtime, client) = mf_kernel::kernel::InProcessKernelRuntime::for_test(
        service,
        mf_kernel::command::ServiceIdempotencyKey::for_test(vec![0x73; 32]).unwrap(),
        mf_kernel::handles::ClientId::parse("workflow-first-run").unwrap(),
        mf_kernel::handles::Principal::parse("workflow-first-run-user").unwrap(),
    )
    .unwrap();
    ctx.install_kernel_tracer_for_tests(runtime, client);
    let orch = ctx.open_project(project.path().to_path_buf()).unwrap();
    assert!(orch.store.list_tasks(false).unwrap().is_empty());

    // 2-4) 画布:新建项目工作流,添加保存实例节点 + 默认 CLI 节点 + 依赖
    let instance = catalog
        .create_agent_instance(mf_agent::AgentInstanceDraft {
            name: "e2e-worker".into(),
            agent_type: "claude".into(),
            scope: mf_agent::InstanceScope::User,
            project_key: None,
            enabled: true,
            run_mode: mf_agent::RunMode::OneShot,
            executable: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
            argv: vec!["/C".into(), "exit".into(), "0".into()],
            env: vec![],
            config: serde_json::json!({}),
            execution_contract: serde_json::json!({
                "input": "argv",
                "completion": "process-exit"
            }),
            sealed_secret_ids: vec![],
        })
        .unwrap();
    let canvas = cx.new(|cx| crate::workflow_canvas::WorkflowCanvas::new(ctx.clone(), cx));
    cx.update_entity(&canvas, |c, cx| {
        c.set_project(Some(project.path().to_path_buf()), cx);
        c.new_workflow(cx);
        assert_eq!(
            c.current_key.as_deref(),
            Some("wf-1"),
            "稳定 key,不使用 task id"
        );
        // 默认 CLI 节点(库条目 → 正确引用)
        let default_cli_ref = c
            .library
            .iter()
            .find_map(|e| e.node_reference())
            .filter(|r| r.starts_with("default-cli:"))
            .expect("检测到的默认 CLI 必须出现在画布库");
        assert_eq!(default_cli_ref, "default-cli:mf-e2e.hostcli.hostname");
        c.editor.drag_from_library(&default_cli_ref);
        // 保存实例节点
        c.editor.drag_from_library(&instance.id);
        // 依赖:默认 CLI 节点依赖保存实例节点
        let keys: Vec<String> = c.editor.nodes().iter().map(|n| n.key.clone()).collect();
        assert_eq!(keys.len(), 2);
        let (first, second) = (keys[0].clone(), keys[1].clone());
        assert_eq!(
            c.editor.nodes()[0].instance_id,
            default_cli_ref,
            "第一个节点是默认 CLI"
        );
        c.editor.add_dependency(&first, &second).expect("依赖建立");
        c.save_after_edit();
        assert!(c.save_error.is_none(), "依赖与节点已自动保存");
    });
    let record = orch
        .store
        .load_project_workflow("wf-1")
        .unwrap()
        .expect("项目工作流已持久化");
    assert_eq!(record.nodes.len(), 2);
    assert_eq!(record.nodes[0].deps, vec![record.nodes[1].key.clone()]);

    // 外部配置哨兵(默认 CLI 只读外部配置)
    let sentinel = tempfile::tempdir().unwrap();
    std::fs::write(sentinel.path().join("settings.json"), "{\"keep\":true}\n").unwrap();
    let sentinel_before = dir_fingerprint(sentinel.path());

    // 5-6) 画布请求运行(意图)→ Composer 输入目标 → 自动创建 Task/Revision
    let ws = cx.new(|cx| crate::agent_workspace::AgentWorkspace::new(ctx.clone(), cx));
    cx.update_entity(&ws, |aw, cx| {
        aw.open_run_composer(project.path().to_path_buf(), "wf-1".into(), cx);
        let composer = aw.run_composer.clone().unwrap();
        composer.update(cx, |c, cx| {
            c.state.set_goal("发布前检查\n把报告写到 report.md");
            cx.notify();
        });
        aw.submit_run_composer(cx);
        assert_eq!(
            aw.active_tab(),
            crate::workspace::AgentTab::Runs,
            "运行后进入 Runs"
        );
    });
    assert!(
        e2e_wait(Duration::from_secs(20), || orch
            .store
            .list_tasks(false)
            .map(|tasks| tasks.len() == 1)
            .unwrap_or(false)),
        "Accepted Operation 必须在后台创建唯一 Workflow Run"
    );
    let tasks = orch.store.list_tasks(false).unwrap();
    assert_eq!(tasks.len(), 1, "自动创建且仅创建一个 Task");
    let task_id = tasks[0].id;
    assert_eq!(tasks[0].title, "发布前检查");
    assert!(tasks[0].active_revision.is_some(), "Revision 已冻结激活");

    // 6) 第一个节点(保存实例,无依赖)真实启动
    assert!(
        e2e_wait(Duration::from_secs(20), || orch
            .store
            .list_runs_of_task(task_id)
            .map(|runs| runs.len() == 1)
            .unwrap_or(false)),
        "第一个节点必须启动"
    );
    // 人工确认第一节点(显式结算;进程退出不自动等同成功)
    let instance_run = orch.store.list_runs_of_task(task_id).unwrap()[0].clone();
    assert!(
        e2e_wait(Duration::from_secs(15), || orch
            .store
            .run_view(instance_run.id)
            .map(|r| r.is_some_and(|r| r.status == mf_agent::RunStatus::AwaitingOutcome))
            .unwrap_or(false)),
        "第一个节点进程退出后进入待结算,实际 {:?}",
        orch.store
            .run_view(instance_run.id)
            .map(|r| r.map(|r| r.status))
    );
    orch.settle_by_token(
        &instance_run.capability_token,
        mf_agent::Settlement::Complete {
            summary: "实例节点完成".into(),
            output: Default::default(),
        },
    )
    .unwrap();

    // 7) 第二个节点(默认 CLI,manual 完成语义)启动 → awaiting-outcome
    let default_cli_step_of = || {
        orch.store
            .task_steps(task_id)
            .unwrap()
            .into_iter()
            .find(|s| s.agent_profile == "mf-e2e.hostcli.hostname")
    };
    assert!(
        e2e_wait(Duration::from_secs(30), || default_cli_step_of()
            .map(|s| s.status == mf_agent::StepStatus::AwaitingOutcome)
            .unwrap_or(false)),
        "第二个节点(默认 CLI)必须进入 awaiting-outcome,实际 {:?}",
        orch.store.task_steps(task_id).map(|s| s
            .iter()
            .map(|x| (x.step_key.clone(), x.status))
            .collect::<Vec<_>>())
    );

    // 8) 徽标显示 1：只读真实 Overview Hub，不在测试中手算 Attention。
    let attention_of = || {
        ctx.overview()
            .current()
            .attention_runs
            .iter()
            .find(|attention| {
                attention.project_root == project.path() && attention.task_id == task_id
            })
            .cloned()
    };
    let has_attention = e2e_wait(Duration::from_secs(10), || attention_of().is_some());
    assert!(has_attention, "第二个节点 awaiting-outcome 必须产生徽标");
    let attention = attention_of().unwrap();
    assert_eq!(attention.task_id, task_id);
    let awaiting_step = default_cli_step_of().unwrap();

    // 9) 点击徽标直达第二个节点(open_attention_run)
    cx.update_entity(&ws, |aw, cx| {
        aw.open_attention_run(&attention, cx);
        assert_eq!(aw.active_tab(), crate::workspace::AgentTab::Runs);
        let focused = aw.runs_page.read(cx).monitor.read(cx).focused_step();
        assert_eq!(focused, Some(awaiting_step.id), "直达优先处理节点");
    });

    // 12) 默认 CLI 零写入:外部哨兵不变 + run-temp 无隔离配置目录
    assert_eq!(
        dir_fingerprint(sentinel.path()),
        sentinel_before,
        "默认 CLI 外部配置目录必须保持原样"
    );
    let default_cli_run = orch
        .store
        .list_runs_of_task(task_id)
        .unwrap()
        .into_iter()
        .max_by_key(|r| r.id)
        .unwrap();
    let run_temp = std::env::temp_dir()
        .join("monkeyfence")
        .join("steps")
        .join(format!("{}-{}", std::process::id(), default_cli_run.id));
    assert!(
        !run_temp.join("claude").exists(),
        "external_config 快照不得物化隔离配置目录: {}",
        run_temp.display()
    );

    // 10) 人工确认(显式结算)→ 运行收敛 → 徽标清零
    orch.settle_by_token(
        &default_cli_run.capability_token,
        mf_agent::Settlement::Complete {
            summary: "默认 CLI 节点完成".into(),
            output: Default::default(),
        },
    )
    .unwrap();
    assert!(
        e2e_wait(Duration::from_secs(30), || orch
            .store
            .task_view(task_id)
            .map(|t| t
                .map(|t| t.status == mf_agent::TaskStatus::Succeeded)
                .unwrap_or(false))
            .unwrap_or(false)),
        "人工确认后运行必须收敛,实际 {:?}",
        orch.store.task_view(task_id).map(|t| t.map(|t| t.status))
    );
    assert!(
        e2e_wait(Duration::from_secs(10), || attention_of().is_none()),
        "唯一直接原因处理后 Hub 徽标必须清零"
    );

    // 清理真实进程并完全关闭第一套 AppCtx/Orchestrator。
    for r in orch.store.list_runs_of_task(task_id).unwrap() {
        if let Some(sid) = r.session_id {
            if let Some(session) = orch.store.session_view(sid).unwrap() {
                ctx.registry().kill_session(&session.public_handle);
            }
        }
    }
    let restart_config = ctx.config_snapshot().clone();
    let restart_plugins = ctx.plugins().clone();
    orch.stop();
    ctx.close_project(&project.path().to_path_buf());

    // 11) 真重启：新建 AppCtx/Orchestrator，重新打开同一项目数据库。
    let restarted = crate::app_ctx::AppCtx::with_parts_and_plugins_for_tests(
        restart_config,
        catalog.clone(),
        restart_plugins,
    );
    let restarted_orch = restarted
        .open_project(project.path().to_path_buf())
        .unwrap();
    let workflow_kept = restarted_orch
        .store
        .load_project_workflow("wf-1")
        .unwrap()
        .expect("项目工作流跨重启保留");
    assert_eq!(workflow_kept.nodes.len(), 2);
    assert!(
        e2e_wait(Duration::from_secs(10), || restarted
            .overview()
            .current()
            .attention_runs
            .iter()
            .all(|attention| attention.task_id != task_id)),
        "收敛后重启不复活徽标"
    );
    restarted_orch.stop();
    restarted.close_project(&project.path().to_path_buf());
}
