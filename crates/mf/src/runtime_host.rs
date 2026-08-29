//! Agent Runtime 宿主实现:Session Registry 拥有 PTY 子进程、终端状态与输出缓冲,
//! UI 只持有 Session Handle —— 切换项目、关闭卡片或离开工作区都不会终止 Agent。
//!
//! 三种 Adapter(ADR 0002):
//! - PtyAgentRuntime:本地 CLI(ConPTY 命令/参数/环境/工作目录)
//! - HttpAgentRuntime:OpenAI 兼容 / Anthropic / mock(工具循环 + 结构化结算)
//! - PluginWorkerRuntime:第三方可执行插件(NDJSON worker)

use crate::term::Screen;

const TERM_ROWS: usize = 26;
const TERM_COLS: usize = 120;
use anyhow::{anyhow, Context as _, Result};
use crossbeam_channel::Sender;
use mf_agent::provider::{complete, AssistantBlock, ChatMessage, ToolDef};
use mf_agent::runtime::{
    AdHocLaunchSpec, AgentProfileSpec, LaunchSpec, RuntimeEvent, RuntimeHost, RuntimeKind,
};
use mf_agent::Settlement;
use mf_agent::{InputInjection, TempFileSpec};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const OUT_BUFFER_CAP: usize = 256 * 1024;

/// 供 UI 恢复/渲染的会话快照(不持有进程资源)。
pub struct SessionSnapshot {
    pub session_id: i64,
    pub kind: RuntimeKind,
    pub alive: bool,
    pub title: String,
    pub screen_rows: Vec<String>,
    pub cursor: (usize, usize),
    pub transcript: Vec<(String, String)>, // (role, text)
}

struct PtySession {
    session_id: i64,
    master: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    child: Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>,
    /// 独立 kill 句柄(离散会话:child 由 waiter 线程持有等待,
    /// kill 经此克隆执行,避免跨线程争抢 Child)。
    killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send>>>,
    screen: Mutex<Screen>,
    title: Mutex<String>,
    output_tail: Mutex<Vec<u8>>,
    alive: AtomicBool,
}

struct HttpSession {
    session_id: i64,
    transcript: Mutex<Vec<(String, String)>>,
    alive: AtomicBool,
    /// 等待用户回答的 ask_human 通道。
    answer_tx: Mutex<Option<Sender<String>>>,
    /// 终止信号。
    cancel: AtomicBool,
}

enum SessionInner {
    Pty(Arc<PtySession>),
    Http(Arc<HttpSession>),
}

const TRANSCRIPT_CAP: usize = 500;

fn push_capped(list: &mut Vec<(String, String)>, item: (String, String)) {
    if list.len() >= TRANSCRIPT_CAP {
        list.remove(0);
    }
    list.push(item);
}

/// 全局唯一会话键:run/session id 是各项目数据库的行号,跨项目会碰撞,
/// 注册表一律以 `{project}#{id}` 定位。
/// 离散 CLI 会话的进程也注册在展示会话行(agent_sessions)键下 ——
/// Registry/PtySession/kill/detach/snapshot 一律使用 display session ID;
/// ad_hoc_sessions 行号只作为 `AdHocExited` 事件 tag,不参与进程路由。
pub fn session_key(project: &str, id: i64) -> String {
    format!("{}#{}", project.replace('#', "_"), id)
}

pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, SessionInner>>,
    run_sessions: Mutex<HashMap<String, String>>,
    config: Mutex<mf_agent::Config>,
}

impl SessionRegistry {
    pub fn new(config: mf_agent::Config) -> Arc<SessionRegistry> {
        Arc::new(SessionRegistry {
            sessions: Mutex::new(HashMap::new()),
            run_sessions: Mutex::new(HashMap::new()),
            config: Mutex::new(config),
        })
    }

    pub fn update_config(&self, config: mf_agent::Config) {
        *self.config.lock() = config;
    }

    fn get_inner(&self, key: &str) -> Option<SessionInner> {
        self.sessions.lock().get(key).cloned()
    }

    /// UI 终端视图重新挂载时恢复当前屏幕。
    pub fn snapshot(&self, project: &str, session_id: i64) -> Option<SessionSnapshot> {
        let key = session_key(project, session_id);
        self.snapshot_at(&key, session_id)
    }

    fn snapshot_at(&self, key: &str, session_id: i64) -> Option<SessionSnapshot> {
        match self.get_inner(key)? {
            SessionInner::Pty(p) => {
                let screen = p.screen.lock();
                let mut rows = Vec::with_capacity(TERM_ROWS);
                for y in 0..TERM_ROWS {
                    rows.push(screen.line_text(y));
                }
                let cursor = screen.cursor();
                Some(SessionSnapshot {
                    session_id,
                    kind: RuntimeKind::Pty,
                    alive: p.alive.load(Ordering::SeqCst),
                    title: p.title.lock().clone(),
                    screen_rows: rows,
                    cursor,
                    transcript: Vec::new(),
                })
            }
            SessionInner::Http(h) => Some(SessionSnapshot {
                session_id,
                kind: RuntimeKind::Http,
                alive: h.alive.load(Ordering::SeqCst),
                title: String::new(),
                screen_rows: Vec::new(),
                cursor: (0, 0),
                transcript: h.transcript.lock().clone(),
            }),
        }
    }

    /// 终端输出尾部(卡片"最后回复"用)。
    pub fn pty_tail(&self, project: &str, session_id: i64, lines: usize) -> Vec<String> {
        let key = session_key(project, session_id);
        let sessions = self.sessions.lock();
        match sessions.get(&key) {
            Some(SessionInner::Pty(p)) => {
                let screen = p.screen.lock();
                screen.tail_lines(lines)
            }
            _ => Vec::new(),
        }
    }

    pub fn send_prompt(&self, project: &str, session_id: i64, text: &str) -> Result<()> {
        let key = session_key(project, session_id);
        self.send_prompt_at(&key, session_id, text)
    }

    /// 锁外做阻塞 I/O(ConPTY 输入缓冲满时 write 会阻塞,不能拿着注册表锁)。
    fn send_prompt_at(&self, key: &str, session_id: i64, text: &str) -> Result<()> {
        let sess = self.get_inner(key);
        match sess {
            Some(SessionInner::Pty(p)) => {
                let mut writer = p.writer.lock();
                let w = writer.as_mut().ok_or_else(|| anyhow!("会话已关闭"))?;
                w.write_all(text.as_bytes())
                    .and_then(|_| w.write_all(b"\r"))
                    .and_then(|_| w.flush())
                    .context("写入 PTY 失败")?;
                Ok(())
            }
            Some(SessionInner::Http(h)) => {
                push_capped(&mut h.transcript.lock(), ("user".into(), text.to_string()));
                Ok(())
            }
            None => Err(anyhow!("会话 {session_id} 不存在")),
        }
    }

    /// 终端键盘直通:原始字节写入 PTY(不追加回车)。
    pub fn send_prompt_raw(&self, project: &str, session_id: i64, bytes: &[u8]) -> Result<()> {
        let key = session_key(project, session_id);
        let sess = {
            let sessions = self.sessions.lock();
            sessions.get(&key).cloned()
        };
        match sess {
            Some(SessionInner::Pty(p)) => {
                let mut writer = p.writer.lock();
                let w = writer
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("会话已关闭"))?;
                w.write_all(bytes)
                    .and_then(|_| w.flush())
                    .context("写入 PTY 失败")?;
                Ok(())
            }
            _ => Err(anyhow!("会话不是 PTY")),
        }
    }

    pub fn kill_session(&self, project: &str, session_id: i64) {
        let key = session_key(project, session_id);
        Self::kill_at(&self.sessions, &key);
    }

    fn kill_at(sessions: &Mutex<HashMap<String, SessionInner>>, key: &str) {
        if let Some(s) = sessions.lock().remove(key) {
            match &s {
                SessionInner::Pty(p) => {
                    p.alive.store(false, Ordering::SeqCst);
                    p.writer.lock().take();
                    if let Some(mut killer) = p.killer.lock().take() {
                        let _ = killer.kill();
                    }
                    if let Some(mut child) = p.child.lock().take() {
                        let _ = child.kill();
                    }
                    p.master.lock().take();
                }
                SessionInner::Http(h) => {
                    h.cancel.store(true, Ordering::SeqCst);
                    h.alive.store(false, Ordering::SeqCst);
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn alive_session_count(&self) -> usize {
        self.sessions
            .lock()
            .values()
            .filter(|s| match s {
                SessionInner::Pty(p) => p.alive.load(Ordering::SeqCst),
                SessionInner::Http(h) => h.alive.load(Ordering::SeqCst),
            })
            .count()
    }

    pub fn session_alive(&self, project: &str, session_id: i64) -> bool {
        self.sessions
            .lock()
            .get(&session_key(project, session_id))
            .map(|s| match s {
                SessionInner::Pty(p) => p.alive.load(Ordering::SeqCst),
                SessionInner::Http(h) => h.alive.load(Ordering::SeqCst),
            })
            .unwrap_or(false)
    }

    fn register(&self, project: &str, session_id: i64, inner: SessionInner) {
        self.sessions
            .lock()
            .insert(session_key(project, session_id), inner);
    }

    fn bind_run(&self, project: &str, run_id: i64, session_id: i64) {
        self.run_sessions.lock().insert(
            session_key(project, run_id),
            session_key(project, session_id),
        );
    }

    pub fn session_of_run(&self, project: &str, run_id: i64) -> Option<i64> {
        let key = self
            .run_sessions
            .lock()
            .get(&session_key(project, run_id))?
            .clone();
        key.rsplit('#').next()?.parse().ok()
    }

    fn http_answer(&self, project: &str, session_id: i64, answer: &str) {
        let key = session_key(project, session_id);
        let sessions = self.sessions.lock();
        if let Some(SessionInner::Http(h)) = sessions.get(&key) {
            if let Some(tx) = h.answer_tx.lock().take() {
                let _ = tx.send(answer.to_string());
            }
        }
    }
}

impl Clone for SessionInner {
    fn clone(&self) -> Self {
        match self {
            SessionInner::Pty(p) => SessionInner::Pty(p.clone()),
            SessionInner::Http(h) => SessionInner::Http(h.clone()),
        }
    }
}

// ---------------- PTY Adapter ----------------

fn launch_pty(
    registry: &Arc<SessionRegistry>,
    spec: &LaunchSpec,
    events: Sender<(i64, RuntimeEvent)>,
) {
    let project = spec.workdir.to_string_lossy().to_string();
    if spec.attach_existing_session && registry.session_alive(&project, spec.session_id) {
        // 复用存活会话:直接发送提示
        let _ = registry.send_prompt(&project, spec.session_id, &spec.prompt);
        let _ = events.send((
            spec.run_id,
            RuntimeEvent::AgentState(mf_agent::AgentState::Working),
        ));
        return;
    }
    registry.kill_session(&project, spec.session_id); // 同键旧会话清理

    let pty_system: Box<dyn portable_pty::PtySystem> = Box::new(NativePtySystem::default());
    let pair = match pty_system.openpty(PtySize {
        rows: TERM_ROWS as u16,
        cols: TERM_COLS as u16,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::SpawnError(format!("openpty 失败: {e}")),
            ));
            return;
        }
    };

    let mut cmd = CommandBuilder::new(&spec.profile.command);
    // yolo 模式追加权限参数;manual 模式不附加(用户在终端里手动批准)
    let yolo = {
        let cfg = registry.config.lock();
        cfg.agents.permission_mode != "manual"
    };
    if yolo {
        cmd.args(&spec.profile.permission_args);
    }
    cmd.args(&spec.profile.args);
    // prompt 作为尾随参数(内置 CLI 均支持位置参数提示)
    if !spec.prompt.trim().is_empty() {
        cmd.arg(&spec.prompt);
    }
    for (k, v) in &spec.profile.env {
        cmd.env(k, v);
    }
    // 能力令牌 + 管道名注入环境,mfctl 自动读取
    cmd.env("MF_RUN_TOKEN", &spec.capability_token);
    cmd.env("MF_PIPE", &spec.pipe_name);
    if let Some(hint) = &spec.mfctl_hint {
        cmd.env("MFCTL_HINT", hint);
    }
    cmd.cwd(&spec.workdir);

    // reader/writer 在 spawn 之前克隆:spawn 成功后的任何失败都必须
    // kill + wait 子进程,绝不留孤儿 CLI
    let reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::SpawnError(format!("克隆 PTY reader 失败: {e}")),
            ));
            return;
        }
    };
    let mut reader = reader;
    let writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::SpawnError(format!("take_writer 失败: {e}")),
            ));
            return;
        }
    };
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::SpawnError(format!(
                    "启动 `{}` 失败: {e}(CLI 未安装?仅检测 PATH,不自动安装)",
                    spec.profile.command
                )),
            ));
            return;
        }
    };
    drop(pair.slave);
    let killer = child.clone_killer();

    let session = Arc::new(PtySession {
        session_id: spec.session_id,
        master: Mutex::new(Some(pair.master)),
        writer: Mutex::new(Some(writer)),
        // child 由 reader 线程持有并 wait;kill 经 killer 克隆执行
        child: Mutex::new(None),
        killer: Mutex::new(Some(killer)),
        screen: Mutex::new(Screen::new(TERM_ROWS, TERM_COLS)),
        title: Mutex::new(spec.profile.display_name.clone()),
        output_tail: Mutex::new(Vec::new()),
        alive: AtomicBool::new(true),
    });
    registry.register(
        &project,
        spec.session_id,
        SessionInner::Pty(session.clone()),
    );
    registry.bind_run(&project, spec.run_id, spec.session_id);

    // reader 线程:喂终端模拟器 + 输出缓冲 + Output 事件(节流);
    // 拥有 child 并负责 wait。线程启动失败 → 收回 child kill + wait,
    // 注册表条目摘除,上报 SpawnError(调度方按失败结算),不留孤儿。
    let child_slot = Arc::new(Mutex::new(Some(child)));
    let slot_for_thread = child_slot.clone();
    let run_id = spec.run_id;
    let events_out = events.clone();
    let reader_registry = registry.clone();
    let reader_project = project.clone();
    let reader_session = session.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("pty-reader-{}", session.session_id))
        .spawn(move || {
            let mut child = slot_for_thread.lock().take();
            let mut buf = [0u8; 8192];
            let mut last_output_event =
                std::time::Instant::now() - std::time::Duration::from_secs(10);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        {
                            let mut screen = reader_session.screen.lock();
                            screen.feed(&buf[..n]);
                            if !screen.title.is_empty() {
                                *reader_session.title.lock() = screen.title.clone();
                            }
                        }
                        {
                            let mut tail = reader_session.output_tail.lock();
                            tail.extend_from_slice(&buf[..n]);
                            if tail.len() > OUT_BUFFER_CAP {
                                let drop = tail.len() - OUT_BUFFER_CAP;
                                tail.drain(..drop);
                            }
                        }
                        if last_output_event.elapsed() >= std::time::Duration::from_millis(600) {
                            last_output_event = std::time::Instant::now();
                            let _ = events_out.send((run_id, RuntimeEvent::Output));
                        }
                    }
                }
            }
            reader_session.alive.store(false, Ordering::SeqCst);
            let code = child
                .as_mut()
                .and_then(|c| c.wait().ok())
                .map(|s| s.exit_code() as i32);
            reader_session.writer.lock().take();
            reader_session.master.lock().take();
            let _ = events_out.send((run_id, RuntimeEvent::Exited { code }));
            let _ = events_out.send((run_id, RuntimeEvent::AgentState(mf_agent::AgentState::Dead)));
            // 进程已结束并 wait:摘除注册表条目(不 kill)
            reader_registry.kill_session(&reader_project, reader_session.session_id);
        });
    if let Err(error) = spawned {
        // reader 线程未启动:无人读取/等待 PTY → 收回 child kill + wait
        if let Some(mut child) = child_slot.lock().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut killer) = session.killer.lock().take() {
            let _ = killer.kill();
        }
        session.writer.lock().take();
        session.master.lock().take();
        session.alive.store(false, Ordering::SeqCst);
        registry.kill_session(&project, spec.session_id);
        let _ = events.send((
            spec.run_id,
            RuntimeEvent::SpawnError(format!("启动 PTY reader 线程失败: {error}")),
        ));
        return;
    }
    // reader 线程已完全接管生命周期 → 才上报 Launched
    let _ = events.send((spec.run_id, RuntimeEvent::Launched));
    let _ = events.send((
        spec.run_id,
        RuntimeEvent::AgentState(mf_agent::AgentState::Working),
    ));
}

// ---------------- Ad-hoc CLI 会话 ----------------

/// 离散 CLI 会话启动(设计 §4.7 / §10):挂在 Task 下,
/// 无 Step / Agent Run / 结算事件流;注册表仍以 (project, session_id)
/// 路由,UI 终端视图、send_prompt、kill 与普通会话一致。

/// 拒绝符号链接/接合点/junction 逃逸:从 run_temp 到 target 的
/// 每一级都必须是真实目录(create_dir_all 可能静默穿过已存在的链接)。
fn ensure_no_link_escape(run_temp: &Path, target: &Path) -> Result<()> {
    let relative = target
        .strip_prefix(run_temp)
        .map_err(|_| anyhow::anyhow!("目标不在运行临时目录内: {}", target.display()))?;
    let mut current = run_temp.to_path_buf();
    let mut check = |path: &Path| -> Result<()> {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            // Windows 上 junction/reparse point 同样以 is_symlink 呈现
            if meta.file_type().is_symlink() {
                anyhow::bail!(
                    "运行临时目录内不允许符号链接/接合点(路径逃逸): {}",
                    path.display()
                );
            }
        }
        Ok(())
    };
    check(&current)?;
    for component in relative.components() {
        current.push(component);
        check(&current)?;
    }
    Ok(())
}

fn materialize_temp_files(run_temp: &Path, files: &[TempFileSpec]) -> Result<()> {
    std::fs::create_dir_all(run_temp)
        .with_context(|| format!("创建运行临时目录失败: {}", run_temp.display()))?;
    for file in files {
        if file.path.as_os_str().is_empty()
            || file.path.is_absolute()
            || file.path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir
                        | std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            anyhow::bail!("临时文件路径不得逃逸运行临时目录: {}", file.path.display());
        }
        let target = run_temp.join(&file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建临时文件目录失败: {}", parent.display()))?;
        }
        ensure_no_link_escape(run_temp, &target)
            .with_context(|| format!("临时文件路径可疑: {}", file.path.display()))?;
        std::fs::write(&target, &file.contents)
            .with_context(|| format!("写入临时文件失败: {}", target.display()))?;
    }
    Ok(())
}

/// spawn 成功后的看护:线程接管生命周期前任何初始化失败都会在
/// drop 时 kill(经 killer 克隆)并摘除注册表条目;child 的 reap
/// 由 waiter 线程完成(kill 后 wait 返回),不留孤儿 CLI。
/// 注册表条目位于展示会话键下 —— kill/detach 都用 display ID。
struct AdHocReapGuard<'a> {
    registry: &'a SessionRegistry,
    project: &'a str,
    /// 进程注册键(展示会话行;与 ad_hoc_sessions 行号不同)。
    display_id: i64,
    session: Option<Arc<PtySession>>,
}

impl Drop for AdHocReapGuard<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            self.registry.kill_session(self.project, self.display_id);
            if let Some(mut killer) = session.killer.lock().take() {
                let _ = killer.kill();
            }
            session.writer.lock().take();
            session.master.lock().take();
            session.alive.store(false, Ordering::SeqCst);
        }
    }
}

/// 离散 CLI 会话启动。输出在进入 screen/output_tail 前经
/// `StreamingRedactor` 跨块脱敏;进程退出时向 Orchestrator 上报
/// `AdHocExited`(tag 为 session_id)并从注册表摘除会话。
fn launch_ad_hoc_pty(registry: &Arc<SessionRegistry>, spec: &AdHocLaunchSpec) -> Result<()> {
    let project = spec.workdir.to_string_lossy().to_string();
    // 进程注册键 = 展示会话行:Agents 卡片与终端交互复用既有通道
    let display_id = spec.display_session_id;
    registry.kill_session(&project, display_id); // 同键旧会话清理

    if spec.plan.uses_shell {
        anyhow::bail!("离散 CLI Runtime 尚不支持 Shell 启动计划");
    }
    if spec.plan.executable.as_os_str().is_empty() {
        anyhow::bail!("离散 CLI 的可执行文件不能为空");
    }
    materialize_temp_files(&spec.run_temp, &spec.plan.temp_files)?;

    let pty_system: Box<dyn portable_pty::PtySystem> = Box::new(NativePtySystem::default());
    let pair = pty_system
        .openpty(PtySize {
            rows: TERM_ROWS as u16,
            cols: TERM_COLS as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("离散会话 openpty 失败")?;

    let mut argv = spec.plan.argv.clone();
    apply_prompt_file_argv(&mut argv, &spec.plan.input)?;
    let mut cmd = CommandBuilder::new(&spec.plan.executable);
    cmd.args(&argv);
    for (k, v) in &spec.plan.env {
        cmd.env(k, v);
    }
    // Secret 值只进入 spawn 调用的环境块;不写日志、不进任何持久化
    let mut redactor = {
        let secret_values = spec
            .plan
            .secret_env
            .iter()
            .map(|(_, lease)| lease.as_slice().to_vec())
            .collect();
        mf_agent::secrets::StreamingRedactor::new(secret_values)
    };
    for (key, lease) in &spec.plan.secret_env {
        let value = std::str::from_utf8(lease.as_slice()).with_context(|| {
            format!(
                "Secret `{}` 不是有效 UTF-8,无法注入环境变量 {key}",
                lease.id()
            )
        })?;
        cmd.env(key, value);
    }
    cmd.cwd(spec.plan.cwd.as_deref().unwrap_or(&spec.workdir));

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("离散会话克隆 PTY reader 失败")?;
    let writer = pair
        .master
        .take_writer()
        .context("离散会话 take_writer 失败")?;
    let mut child = pair.slave.spawn_command(cmd).with_context(|| {
        format!(
            "离散会话启动 `{}` 失败(CLI 未安装或配置无效)",
            spec.plan.executable.display()
        )
    })?;
    drop(pair.slave);
    let killer = child.clone_killer();

    let session = Arc::new(PtySession {
        session_id: spec.session_id,
        master: Mutex::new(Some(pair.master)),
        writer: Mutex::new(Some(writer)),
        // child 由 waiter 线程持有并 wait;kill 经 killer 克隆执行
        child: Mutex::new(None),
        killer: Mutex::new(Some(killer)),
        screen: Mutex::new(Screen::new(TERM_ROWS, TERM_COLS)),
        title: Mutex::new(spec.title.clone()),
        output_tail: Mutex::new(Vec::new()),
        alive: AtomicBool::new(true),
    });
    registry.register(&project, display_id, SessionInner::Pty(session.clone()));
    // 看护 armed:下方任何失败路径(初始 stdin 写入、线程启动)
    // 都会 kill 并摘除注册表条目
    let mut reap = AdHocReapGuard {
        registry,
        project: &project,
        display_id,
        session: Some(session.clone()),
    };

    if let InputInjection::Stdin(bytes) = &spec.plan.input {
        let mut stdin = session.writer.lock();
        let handle = stdin.as_mut().ok_or_else(|| anyhow!("离散会话已关闭"))?;
        handle
            .write_all(bytes)
            .and_then(|_| handle.flush())
            .context("向离散会话 stdin 写入初始提示失败")?;
    }

    // ConPTY 在 master 存续期间不会给 reader EOF:waiter 拥有 child,
    // wait 返回后记录退出码并关闭 master,解除 reader 阻塞。
    let exit_code_slot = Arc::new(Mutex::new(None::<i32>));
    let waiter_session = session.clone();
    let waiter_slot = exit_code_slot.clone();
    let waiter = std::thread::Builder::new()
        .name(format!("ad-hoc-pty-waiter-{}", display_id))
        .spawn(move || {
            let code = child.wait().ok().map(|status| status.exit_code() as i32);
            *waiter_slot.lock() = code;
            // 先写 slot 再关 master:reader 被解除阻塞时退出码已就绪
            waiter_session.master.lock().take();
        })
        .context("启动离散会话 waiter 线程失败")?;

    let completion = spec.plan.completion.clone();
    let events = spec.events.clone();
    let reader_project = project.clone();
    let reader_session = session.clone();
    let reader_registry = registry.clone();
    let reader_display_id = display_id;
    let exit_code_slot_reader = exit_code_slot.clone();
    let build = std::thread::Builder::new()
        .name(format!("ad-hoc-pty-reader-{}", display_id))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // 脱敏在进入 screen/output_tail 之前(跨块部分匹配)
                        let clean = redactor.redact_chunk(&buf[..n]);
                        if !clean.is_empty() {
                            let mut screen = reader_session.screen.lock();
                            screen.feed(&clean);
                            if !screen.title.is_empty() {
                                *reader_session.title.lock() = screen.title.clone();
                            }
                            drop(screen);
                            let mut tail = reader_session.output_tail.lock();
                            tail.extend_from_slice(&clean);
                            if tail.len() > OUT_BUFFER_CAP {
                                let drop = tail.len() - OUT_BUFFER_CAP;
                                tail.drain(..drop);
                            }
                        }
                    }
                }
            }
            let rest = redactor.finish();
            if !rest.is_empty() {
                let mut screen = reader_session.screen.lock();
                screen.feed(&rest);
                let mut tail = reader_session.output_tail.lock();
                tail.extend_from_slice(&rest);
                if tail.len() > OUT_BUFFER_CAP {
                    let drop = tail.len() - OUT_BUFFER_CAP;
                    tail.drain(..drop);
                }
            }
            reader_session.alive.store(false, Ordering::SeqCst);
            let exit_code = *exit_code_slot_reader.lock();
            reader_session.writer.lock().take();
            reader_session.master.lock().take();
            // 完成检测素材:marker 扫描(脱敏后的缓冲足够,标记不得是 Secret)
            let tail_bytes = reader_session.output_tail.lock().clone();
            let tail_text = String::from_utf8_lossy(&tail_bytes).into_owned();
            let marker_seen = match &completion {
                mf_agent::CompletionDetector::StdoutMarker(marker) => {
                    tail_text.contains(marker.as_str())
                }
                _ => false,
            };
            let result_file_present = match &completion {
                mf_agent::CompletionDetector::ResultFile(path) => path.is_file(),
                _ => false,
            };
            let _ = events.send((
                reader_session.session_id,
                RuntimeEvent::AdHocExited {
                    session_id: reader_session.session_id,
                    exit_code,
                    marker_seen,
                    result_file_present,
                },
            ));
            // 退出后从注册表摘除(不 kill:进程已自然结束并 wait)。
            // 注册键是展示会话行 —— 必须用 display ID 摘除,
            // ad_hoc 行号(事件 tag)与它分属两套自增序列,互不相等。
            reader_registry.kill_session(&reader_project, reader_display_id);
        });
    if let Err(error) = build {
        // guard 仍 armed → drop 时 kill(killer)+摘除注册表;waiter 负责 reap
        drop(reap);
        return Err(error).context("启动离散会话 reader 线程失败");
    }
    // reader/waiter 线程接管生命周期:解除看护
    reap.session = None;
    Ok(())
}

// ---------------- 工作流 Step(LaunchPlan)----------------

/// 把 PromptFile 输入注入应用到 argv:文件已由 Runtime Host 在可信
/// run-temp 下物化,CLI 收到的是可信绝对路径。
/// Argv 模式由适配器在编译期并入 argv;Stdin 模式由调用方在 spawn 前写入。
fn apply_prompt_file_argv(argv: &mut Vec<String>, input: &InputInjection) -> Result<()> {
    if let InputInjection::PromptFile(path) = input {
        anyhow::ensure!(
            path.is_absolute(),
            "PromptFile 注入必须是可信绝对路径: {}",
            path.display()
        );
        argv.push(path.to_string_lossy().into_owned());
    }
    Ok(())
}

/// 工作流 Step 启动:冻结 Agent Instance 经真实 Adapter 编译出的
/// LaunchPlan 直启 PTY。事件以 run_id 回流(Launched/Output/Exited/Dead);
/// 进程注册在 (project, session_id) 键下,reader 线程完全接管后才上报 Launched。
fn launch_workflow_pty(
    registry: &Arc<SessionRegistry>,
    spec: &mf_agent::runtime::WorkflowLaunchSpec,
    plan: &mf_agent::LaunchPlan,
    events: Sender<(i64, RuntimeEvent)>,
) -> Result<()> {
    let project = spec.workdir.to_string_lossy().to_string();
    // 复用存活会话:直接发送提示,不再拉起进程
    if spec.attach_existing_session && registry.session_alive(&project, spec.session_id) {
        registry.send_prompt(&project, spec.session_id, &spec.prompt)?;
        let _ = events.send((
            spec.run_id,
            RuntimeEvent::AgentState(mf_agent::AgentState::Working),
        ));
        return Ok(());
    }
    registry.kill_session(&project, spec.session_id); // 同键旧会话清理

    if plan.uses_shell {
        anyhow::bail!("工作流 Runtime 尚不支持 Shell 启动计划");
    }
    if plan.executable.as_os_str().is_empty() {
        anyhow::bail!("工作流节点的可执行文件不能为空");
    }
    if plan.run_temp != spec.run_temp {
        anyhow::bail!("Agent Adapter 试图改写可信 run-temp,已拒绝启动");
    }
    materialize_temp_files(&spec.run_temp, &plan.temp_files)?;

    let pty_system: Box<dyn portable_pty::PtySystem> = Box::new(NativePtySystem::default());
    let pair = pty_system
        .openpty(PtySize {
            rows: TERM_ROWS as u16,
            cols: TERM_COLS as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("工作流节点 openpty 失败")?;

    let mut argv = plan.argv.clone();
    apply_prompt_file_argv(&mut argv, &plan.input)?;
    let mut cmd = CommandBuilder::new(&plan.executable);
    cmd.args(&argv);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    // Secret 值只进入 spawn 调用的环境块;不写日志、不进任何持久化
    let mut redactor = {
        let secret_values = plan
            .secret_env
            .iter()
            .map(|(_, lease)| lease.as_slice().to_vec())
            .collect();
        mf_agent::secrets::StreamingRedactor::new(secret_values)
    };
    for (key, lease) in &plan.secret_env {
        let value = std::str::from_utf8(lease.as_slice()).with_context(|| {
            format!(
                "Secret `{}` 不是有效 UTF-8,无法注入环境变量 {key}",
                lease.id()
            )
        })?;
        cmd.env(key, value);
    }
    // 能力令牌 + 管道名注入环境(mfctl 结算纪律在提示文本中)
    cmd.env("MF_RUN_TOKEN", &spec.capability_token);
    cmd.env("MF_PIPE", &spec.pipe_name);
    if let Some(hint) = &spec.mfctl_hint {
        cmd.env("MFCTL_HINT", hint);
    }
    cmd.cwd(plan.cwd.as_deref().unwrap_or(&spec.workdir));

    // reader/writer 在 spawn 前克隆:spawn 后任何失败都 kill + wait
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("工作流节点克隆 PTY reader 失败")?;
    let writer = pair
        .master
        .take_writer()
        .context("工作流节点 take_writer 失败")?;
    let mut child = pair.slave.spawn_command(cmd).with_context(|| {
        format!(
            "工作流节点启动 `{}` 失败(CLI 未安装或配置无效)",
            plan.executable.display()
        )
    })?;
    drop(pair.slave);
    let killer = child.clone_killer();

    let session = Arc::new(PtySession {
        session_id: spec.session_id,
        master: Mutex::new(Some(pair.master)),
        writer: Mutex::new(Some(writer)),
        // child 由 waiter 线程持有等待;kill 经 killer 克隆执行
        child: Mutex::new(None),
        killer: Mutex::new(Some(killer)),
        screen: Mutex::new(Screen::new(TERM_ROWS, TERM_COLS)),
        title: Mutex::new(spec.step_title.clone()),
        output_tail: Mutex::new(Vec::new()),
        alive: AtomicBool::new(true),
    });
    registry.register(
        &project,
        spec.session_id,
        SessionInner::Pty(session.clone()),
    );
    registry.bind_run(&project, spec.run_id, spec.session_id);
    let mut reap = AdHocReapGuard {
        registry,
        project: &project,
        display_id: spec.session_id,
        session: Some(session.clone()),
    };

    if let InputInjection::Stdin(bytes) = &plan.input {
        let mut stdin = session.writer.lock();
        let handle = stdin.as_mut().ok_or_else(|| anyhow!("工作流会话已关闭"))?;
        handle
            .write_all(bytes)
            .and_then(|_| handle.flush())
            .context("向工作流节点 stdin 写入提示失败")?;
    }

    // ConPTY:waiter 拥有 child,wait 返回后关闭 master 解除 reader 阻塞
    let exit_code_slot = Arc::new(Mutex::new(None::<i32>));
    let waiter_session = session.clone();
    let waiter_slot = exit_code_slot.clone();
    std::thread::Builder::new()
        .name(format!("wf-pty-waiter-{}", spec.session_id))
        .spawn(move || {
            let code = child.wait().ok().map(|status| status.exit_code() as i32);
            *waiter_slot.lock() = code;
            waiter_session.master.lock().take();
        })
        .context("启动工作流 waiter 线程失败")?;

    let run_id = spec.run_id;
    let reader_session = session.clone();
    let reader_registry = registry.clone();
    let reader_project = project.clone();
    let reader_session_id = spec.session_id;
    let exit_code_slot_reader = exit_code_slot.clone();
    let events_out = events.clone();
    let build = std::thread::Builder::new()
        .name(format!("wf-pty-reader-{}", spec.session_id))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // 脱敏在进入 screen/output_tail 之前(跨块部分匹配)
                        let clean = redactor.redact_chunk(&buf[..n]);
                        if !clean.is_empty() {
                            let mut screen = reader_session.screen.lock();
                            screen.feed(&clean);
                            if !screen.title.is_empty() {
                                *reader_session.title.lock() = screen.title.clone();
                            }
                            drop(screen);
                            let mut tail = reader_session.output_tail.lock();
                            tail.extend_from_slice(&clean);
                            if tail.len() > OUT_BUFFER_CAP {
                                let drop = tail.len() - OUT_BUFFER_CAP;
                                tail.drain(..drop);
                            }
                        }
                        let _ = events_out.send((run_id, RuntimeEvent::Output));
                    }
                }
            }
            let rest = redactor.finish();
            if !rest.is_empty() {
                let mut screen = reader_session.screen.lock();
                screen.feed(&rest);
                let mut tail = reader_session.output_tail.lock();
                tail.extend_from_slice(&rest);
                if tail.len() > OUT_BUFFER_CAP {
                    let drop = tail.len() - OUT_BUFFER_CAP;
                    tail.drain(..drop);
                }
            }
            reader_session.alive.store(false, Ordering::SeqCst);
            let exit_code = *exit_code_slot_reader.lock();
            reader_session.writer.lock().take();
            reader_session.master.lock().take();
            let _ = events_out.send((run_id, RuntimeEvent::Exited { code: exit_code }));
            let _ = events_out.send((run_id, RuntimeEvent::AgentState(mf_agent::AgentState::Dead)));
            // 进程已结束并 wait:摘除注册表条目(不 kill)
            reader_registry.kill_session(&reader_project, reader_session_id);
        });
    if let Err(error) = build {
        // guard 仍 armed → drop 时 kill(killer)+摘除注册表;waiter 负责 reap
        drop(reap);
        return Err(error).context("启动工作流 reader 线程失败");
    }
    // reader/waiter 线程完全接管生命周期:解除看护并上报 Launched
    reap.session = None;
    let _ = events.send((spec.run_id, RuntimeEvent::Launched));
    let _ = events.send((
        spec.run_id,
        RuntimeEvent::AgentState(mf_agent::AgentState::Working),
    ));
    Ok(())
}

// ---------------- HTTP Adapter ----------------

fn http_tool_defs() -> Vec<ToolDef> {
    let obj = |props: serde_json::Value, required: &[&str]| {
        let required: Vec<String> = required.iter().map(|s| s.to_string()).collect();
        serde_json::json!({ "type": "object", "properties": props, "required": required })
    };
    vec![
        ToolDef {
            name: "fs_read",
            description: "读取工作区内文件(256KB 上限)",
            parameters: obj(
                serde_json::json!({ "path": { "type": "string", "description": "相对工作区路径" } }),
                &["path"],
            ),
        },
        ToolDef {
            name: "fs_write",
            description: "写入工作区内文件(整文件覆盖)",
            parameters: obj(
                serde_json::json!({
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                }),
                &["path", "content"],
            ),
        },
        ToolDef {
            name: "fs_list",
            description: "列出目录",
            parameters: obj(
                serde_json::json!({ "path": { "type": "string", "description": "默认为工作区根" } }),
                &[],
            ),
        },
        ToolDef {
            name: "complete_step",
            description: "显式结算:本步骤成功完成,summary 为一句话总结",
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "summary": { "type": "string" } },
                "required": ["summary"]
            }),
        },
        ToolDef {
            name: "fail_step",
            description: "显式结算:本步骤失败,reason 说明原因",
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "reason": { "type": "string" } },
                "required": ["reason"]
            }),
        },
        ToolDef {
            name: "ask_human",
            description: "向用户提问并阻塞等待回答",
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "question": { "type": "string" } },
                "required": ["question"]
            }),
        },
    ]
}

fn sandbox_path(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut norm = PathBuf::new();
    for comp in Path::new(rel).components() {
        match comp {
            std::path::Component::ParentDir => anyhow::bail!("路径越界(仅允许工作区内): {rel}"),
            std::path::Component::CurDir => {}
            c => norm.push(c.as_os_str()),
        }
    }
    let full = if norm.is_absolute() {
        norm
    } else {
        root.join(norm)
    };
    // 词法前缀校验(不要求文件存在)
    let full_txt = full.to_string_lossy().replace('/', "\\");
    let root_txt = root.to_string_lossy().replace('/', "\\");
    let root_txt = root_txt.trim_end_matches('\\').to_string();
    if !(full_txt == root_txt || full_txt.starts_with(&format!("{root_txt}\\"))) {
        anyhow::bail!("路径越界(仅允许工作区内): {rel}");
    }
    Ok(full)
}

enum HttpOutcome {
    Settled(Settlement),
    Pending,
}

fn launch_http(registry: &SessionRegistry, spec: &LaunchSpec, events: Sender<(i64, RuntimeEvent)>) {
    let project = spec.workdir.to_string_lossy().to_string();
    // HTTP 会话复用:同键存活则续用 transcript(会话连续性)
    if spec.attach_existing_session && registry.session_alive(&project, spec.session_id) {
        let key = session_key(&project, spec.session_id);
        let existing = {
            let sessions = registry.sessions.lock();
            sessions.get(&key).cloned()
        };
        if let Some(SessionInner::Http(h)) = existing {
            push_capped(
                &mut h.transcript.lock(),
                ("user".into(), spec.prompt.clone()),
            );
            let _ = events.send((spec.run_id, RuntimeEvent::Launched));
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::AgentState(mf_agent::AgentState::Working),
            ));
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::Transcript {
                    role: "user".into(),
                    text: spec.prompt.clone(),
                },
            ));
            run_http_turn_async(registry, spec, h, events);
            return;
        }
    }
    let session = Arc::new(HttpSession {
        session_id: spec.session_id,
        transcript: Mutex::new(vec![("user".into(), spec.prompt.clone())]),
        alive: AtomicBool::new(true),
        answer_tx: Mutex::new(None),
        cancel: AtomicBool::new(false),
    });
    registry.register(
        &project,
        spec.session_id,
        SessionInner::Http(session.clone()),
    );
    registry.bind_run(&project, spec.run_id, spec.session_id);
    let _ = events.send((spec.run_id, RuntimeEvent::Launched));
    let _ = events.send((
        spec.run_id,
        RuntimeEvent::AgentState(mf_agent::AgentState::Working),
    ));
    let _ = events.send((
        spec.run_id,
        RuntimeEvent::Transcript {
            role: "user".into(),
            text: spec.prompt.clone(),
        },
    ));

    run_http_turn_async(registry, spec, session, events);
}

/// 在独立线程上执行一次 HTTP 轮次(新建与复用会话共用)。
fn run_http_turn_async(
    registry: &SessionRegistry,
    spec: &LaunchSpec,
    session: Arc<HttpSession>,
    events: Sender<(i64, RuntimeEvent)>,
) {
    let run_id = spec.run_id;
    let provider = spec
        .profile
        .provider
        .clone()
        .unwrap_or_else(|| mf_agent::ProviderConfig {
            kind: mf_agent::ProviderKind::Mock,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        });
    let workdir = spec.workdir.clone();
    let events2 = events.clone();
    let instructions = spec.prompt.clone();
    let max_iterations = registry.config.lock().engine.max_iterations;
    let title = spec.step_title.clone();
    let cancel = session.clone();
    std::thread::Builder::new()
        .name(format!("http-agent-{run_id}"))
        .spawn(move || {
            let outcome = run_http_turn(
                &provider,
                &instructions,
                &workdir,
                &title,
                max_iterations,
                &session,
                &events2,
                run_id,
                cancel,
            );
            match outcome {
                HttpOutcome::Settled(s) => {
                    let _ = events2.send((run_id, RuntimeEvent::Settled(s)));
                }
                HttpOutcome::Pending => {
                    // 轮次结束但未结算 → Orchestrator 进入 awaiting-outcome
                    session.alive.store(false, Ordering::SeqCst);
                    let _ = events2.send((run_id, RuntimeEvent::Exited { code: None }));
                }
            }
        })
        .ok();
}

#[allow(clippy::too_many_arguments)]
fn run_http_turn(
    provider: &mf_agent::ProviderConfig,
    instructions: &str,
    workdir: &Path,
    title: &str,
    max_iterations: usize,
    session: &Arc<HttpSession>,
    events: &Sender<(i64, RuntimeEvent)>,
    run_id: i64,
    cancel: Arc<HttpSession>,
) -> HttpOutcome {
    // mock:脚本化执行(写一个产物文件 + 结构化结算),让 --agent-smoke 无网络可用
    if provider.kind == mf_agent::ProviderKind::Mock {
        std::thread::sleep(std::time::Duration::from_millis(300));
        push_capped(
            &mut session.transcript.lock(),
            ("assistant".into(), format!("(mock)执行步骤「{title}」…")),
        );
        let _ = events.send((
            run_id,
            RuntimeEvent::Transcript {
                role: "assistant".into(),
                text: format!("(mock)执行步骤「{title}」…"),
            },
        ));
        let artifact = workdir
            .join(".mf-agent")
            .join(format!("step-run-{run_id}.md"));
        let _ = std::fs::create_dir_all(artifact.parent().unwrap());
        let _ = std::fs::write(
            &artifact,
            format!("# {title}\n\nmock Agent Run #{run_id} 产物。\n"),
        );
        if instructions.contains("MOCK_FAIL") {
            push_capped(
                &mut session.transcript.lock(),
                ("assistant".into(), "(mock)按指示结算失败".into()),
            );
            let _ = events.send((
                run_id,
                RuntimeEvent::Transcript {
                    role: "assistant".into(),
                    text: "(mock)按指示结算失败".into(),
                },
            ));
            return HttpOutcome::Settled(Settlement::Fail {
                reason: "mock 指定失败 (MOCK_FAIL)".into(),
            });
        }
        let summary = format!("mock 完成「{title}」,产物 {}", artifact.display());
        push_capped(
            &mut session.transcript.lock(),
            ("assistant".into(), summary.clone()),
        );
        let _ = events.send((
            run_id,
            RuntimeEvent::Transcript {
                role: "assistant".into(),
                text: summary.clone(),
            },
        ));
        return HttpOutcome::Settled(Settlement::Complete { summary });
    }

    // 真实 provider:工具循环(API Agent 通过 complete_step/fail_step 工具显式结算)
    let mut messages = vec![
        ChatMessage::system(
            "你是 MonkeyFence 流水线中的执行者。遵守:先读后写;小步前进;\
             完成后必须调用 complete_step(summary) 或 fail_step(reason) 显式结算;\
             需要用户决策时调用 ask_human(question)。不要执行任何版本控制提交/推送。"
                .to_string(),
        ),
        ChatMessage::user(instructions.to_string()),
    ];
    let tools = http_tool_defs();
    for _ in 0..max_iterations.max(4) {
        if cancel.cancel.load(Ordering::SeqCst) {
            return HttpOutcome::Pending;
        }
        let blocks = match complete(provider, &messages, &tools) {
            Ok(b) => b,
            Err(e) => {
                let _ = events.send((
                    run_id,
                    RuntimeEvent::Transcript {
                        role: "error".into(),
                        text: format!("provider 错误: {e:#}"),
                    },
                ));
                return HttpOutcome::Pending;
            }
        };
        let mut has_tool = false;
        for block in &blocks {
            match block {
                AssistantBlock::Text(t) => {
                    if !t.trim().is_empty() {
                        session
                            .transcript
                            .lock()
                            .push(("assistant".into(), t.clone()));
                        let _ = events.send((
                            run_id,
                            RuntimeEvent::Transcript {
                                role: "assistant".into(),
                                text: t.clone(),
                            },
                        ));
                    }
                }
                AssistantBlock::ToolUse(call) => {
                    has_tool = true;
                    let _ = events.send((
                        run_id,
                        RuntimeEvent::Transcript {
                            role: "tool".into(),
                            text: format!(
                                "🔧 {} {}",
                                call.name,
                                &call.arguments[..call.arguments.len().min(120)]
                            ),
                        },
                    ));
                }
            }
        }
        if !has_tool {
            // 纯文本回复:未显式结算 → Pending(Orchestrator 转 awaiting-outcome)
            return HttpOutcome::Pending;
        }
        // 执行工具
        messages.push(ChatMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: blocks
                .iter()
                .filter_map(|b| match b {
                    AssistantBlock::ToolUse(c) => Some(c.clone()),
                    _ => None,
                })
                .collect(),
            tool_call_id: None,
        });
        for block in &blocks {
            let AssistantBlock::ToolUse(call) = block else {
                continue;
            };
            let params: serde_json::Value =
                serde_json::from_str(&call.arguments).unwrap_or_default();
            let arg_str = |k: &str| {
                params
                    .get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            match call.name.as_str() {
                "fs_read" => {
                    let result = sandbox_path(workdir, &arg_str("path"))
                        .and_then(|p| std::fs::read_to_string(p).context("读取失败"))
                        .unwrap_or_else(|e| format!("错误: {e:#}"));
                    messages.push(ChatMessage::tool_result(call.id.clone(), result));
                }
                "fs_write" => {
                    let result = sandbox_path(workdir, &arg_str("path")).and_then(|p| {
                        if let Some(parent) = p.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&p, arg_str("content")).context("写入失败")
                    });
                    let result = match result {
                        Ok(()) => format!("已写入 {}", arg_str("path")),
                        Err(e) => format!("错误: {e:#}"),
                    };
                    messages.push(ChatMessage::tool_result(call.id.clone(), result));
                }
                "fs_list" => {
                    let result = sandbox_path(workdir, &arg_str("path"))
                        .and_then(|p| {
                            let mut names: Vec<String> = std::fs::read_dir(&p)
                                .context("读取目录失败")?
                                .filter_map(|e| e.ok())
                                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                                .map(|e| e.file_name().to_string_lossy().to_string())
                                .collect();
                            names.sort();
                            Ok(names.join("\n"))
                        })
                        .unwrap_or_else(|e| format!("错误: {e:#}"));
                    messages.push(ChatMessage::tool_result(call.id.clone(), result));
                }
                "complete_step" => {
                    messages.push(ChatMessage::tool_result(
                        call.id.clone(),
                        "已结算:成功".into(),
                    ));
                    return HttpOutcome::Settled(Settlement::Complete {
                        summary: arg_str("summary"),
                    });
                }
                "fail_step" => {
                    messages.push(ChatMessage::tool_result(
                        call.id.clone(),
                        "已结算:失败".into(),
                    ));
                    return HttpOutcome::Settled(Settlement::Fail {
                        reason: arg_str("reason"),
                    });
                }
                "ask_human" => {
                    let question = arg_str("question");
                    let _ = events.send((run_id, RuntimeEvent::Question(question)));
                    let (tx, rx) = crossbeam_channel::bounded::<String>(1);
                    *session.answer_tx.lock() = Some(tx);
                    match rx.recv_timeout(std::time::Duration::from_secs(6 * 3600)) {
                        Ok(answer) => {
                            messages.push(ChatMessage::tool_result(
                                call.id.clone(),
                                format!("用户回答: {answer}"),
                            ));
                        }
                        Err(_) => {
                            messages.push(ChatMessage::tool_result(
                                call.id.clone(),
                                "用户未在时限内回答".into(),
                            ));
                        }
                    }
                }
                other => {
                    messages.push(ChatMessage::tool_result(
                        call.id.clone(),
                        format!("未知工具: {other}"),
                    ));
                }
            }
        }
    }
    HttpOutcome::Pending
}

// ---------------- Plugin Worker Adapter ----------------

fn launch_plugin_worker(
    registry: &SessionRegistry,
    spec: &LaunchSpec,
    events: Sender<(i64, RuntimeEvent)>,
) {
    let _ = registry;
    let _ = events.send((
        spec.run_id,
        RuntimeEvent::SpawnError(
            "plugin-worker Runtime 首版未接入调度:第三方插件 worker 请通过插件管理页授权后使用"
                .into(),
        ),
    ));
}

// ---------------- RuntimeHost 实现 ----------------

pub struct RuntimeHostImpl {
    pub registry: Arc<SessionRegistry>,
    /// 工作流 Step 派发所需(插件宿主 + 目录库);
    /// None = 未接线(AppCtx 之外的场景,如测试)。
    launcher: Option<WorkflowLauncher>,
}

/// 工作流派发的编译依赖(Adapter 解析 + Secret 解封)。
pub struct WorkflowLauncher {
    pub plugins: Arc<mf_plugins::PluginRegistry>,
    pub catalog: Arc<mf_agent::CatalogStore>,
    /// Secret 主密钥覆盖(None = OS keyring;测试注入)。
    pub secret_master_key: Option<[u8; 32]>,
}

impl RuntimeHostImpl {
    pub fn new(registry: Arc<SessionRegistry>) -> Arc<RuntimeHostImpl> {
        Arc::new(RuntimeHostImpl {
            registry,
            launcher: None,
        })
    }

    /// 接线插件宿主与目录库(生产路径:工作流 Step 从冻结实例编译 LaunchPlan)。
    pub fn with_launcher(
        registry: Arc<SessionRegistry>,
        launcher: WorkflowLauncher,
    ) -> Arc<RuntimeHostImpl> {
        Arc::new(RuntimeHostImpl {
            registry,
            launcher: Some(launcher),
        })
    }
}

impl RuntimeHost for RuntimeHostImpl {
    fn launch(&self, spec: LaunchSpec, events: Sender<(i64, RuntimeEvent)>) {
        match spec.profile.runtime {
            RuntimeKind::Pty => launch_pty(&self.registry, &spec, events),
            RuntimeKind::Http => launch_http(&self.registry, &spec, events),
            RuntimeKind::PluginWorker => launch_plugin_worker(&self.registry, &spec, events),
        }
    }

    fn launch_workflow(
        &self,
        spec: mf_agent::runtime::WorkflowLaunchSpec,
        events: Sender<(i64, RuntimeEvent)>,
    ) -> Result<()> {
        let Some(launcher) = &self.launcher else {
            anyhow::bail!("工作流派发未接线插件宿主(RuntimeHostImpl::with_launcher)");
        };
        // 真实生产链:冻结 Agent Instance → Agent Adapter → LaunchPlan → PTY
        let run_token = format!("step:{}:{}", spec.run_id, spec.node_key);
        let plan = crate::adapter_launch::compile_instance_launch(
            &launcher.plugins,
            &launcher.catalog,
            &spec.instance,
            spec.run_temp.clone(),
            spec.workdir.clone(),
            Some(spec.prompt.clone()),
            &run_token,
            false,
            launcher.secret_master_key,
        )?;
        launch_workflow_pty(&self.registry, &spec, &plan, events)
    }

    fn launch_ad_hoc(&self, spec: AdHocLaunchSpec) -> Result<()> {
        launch_ad_hoc_pty(&self.registry, &spec)
    }

    fn kill_ad_hoc(&self, project: &str, display_session_id: i64) {
        // 进程注册在展示会话键下;ad_hoc 行号只作事件 tag
        self.registry.kill_session(project, display_session_id);
    }

    fn send_prompt(&self, project: &str, _run_id: i64, session_id: i64, text: &str) {
        let _ = self.registry.send_prompt(project, session_id, text);
    }

    fn stop_run(&self, _project: &str, _run_id: i64) {
        // run 级停止由 Orchestrator 结算/取消语义处理;这里保持会话存活(可复用)
    }

    fn kill_session(&self, project: &str, session_id: i64) {
        self.registry.kill_session(project, session_id);
    }

    fn answer_question(&self, project: &str, run_id: i64, answer: &str) {
        if let Some(session_id) = self.registry.session_of_run(project, run_id) {
            self.registry.http_answer(project, session_id, answer);
        }
    }

    fn is_session_alive(&self, project: &str, session_id: i64) -> bool {
        // 重启恢复探测:注册表内仍然存活的会话(含离散 CLI 的展示会话)
        // 保持原状态,不得推断为中断
        self.registry.session_alive(project, session_id)
    }
}

/// 保持唤醒(Windows SetThreadExecutionState)。
pub struct KeepAwake {
    active: AtomicBool,
}

#[cfg(test)]
mod ad_hoc_launch_tests {
    use super::*;
    use mf_agent::TempFileSpec;

    #[test]
    fn materializes_temp_files_only_inside_run_root() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("codex").join("config.toml");
        materialize_temp_files(
            root.path(),
            &[TempFileSpec {
                path: PathBuf::from("codex").join("config.toml"),
                contents: b"model = \"gpt-5\"\n".to_vec(),
            }],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(nested).unwrap(),
            "model = \"gpt-5\"\n"
        );

        let escaped = PathBuf::from("..").join("escape.txt");
        let error = materialize_temp_files(
            root.path(),
            &[TempFileSpec {
                path: escaped,
                contents: b"no".to_vec(),
            }],
        )
        .unwrap_err();
        assert!(error.to_string().contains("运行临时目录"));
    }

    #[test]
    fn junction_inside_run_root_is_rejected() {
        // Windows junction(mklink /J)不需要特权;非 Windows 或创建失败时跳过
        if !cfg!(windows) {
            return;
        }
        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .expect("启动 cmd 失败");
        if !output.status.success() {
            eprintln!("跳过:当前环境无法创建 junction");
            return;
        }
        let error = materialize_temp_files(
            root.path(),
            &[TempFileSpec {
                path: PathBuf::from("link").join("evil.txt"),
                contents: b"no".to_vec(),
            }],
        )
        .unwrap_err();
        let chain = format!("{error:#}");
        assert!(
            chain.contains("符号链接") || chain.contains("逃逸"),
            "{chain}"
        );
        assert!(!outside.path().join("evil.txt").exists());
    }

    // ---------- 真实进程的离散启动 ----------

    fn ad_hoc_spec(
        registry_events: crossbeam_channel::Sender<(i64, RuntimeEvent)>,
        executable: &str,
        argv: &[&str],
        secret: Option<(&str, &str)>,
    ) -> AdHocLaunchSpec {
        let run_temp = std::env::temp_dir().join("mf-ad-hoc-test");
        let mut plan = mf_agent::LaunchPlan {
            run_temp: run_temp.clone(),
            executable: PathBuf::from(executable),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            secret_env: vec![],
            cwd: Some(run_temp.clone()),
            temp_files: vec![],
            input: mf_agent::InputInjection::Argv(String::new()),
            completion: mf_agent::CompletionDetector::ProcessExit,
            uses_shell: false,
        };
        if let Some((name, value)) = secret {
            plan.secret_env.push((
                name.to_string(),
                Arc::new(mf_agent::secrets::SecretLease::new(
                    "sec-test",
                    value.as_bytes().to_vec(),
                )),
            ));
        }
        AdHocLaunchSpec {
            task_id: 1,
            // 两套自增序列刻意取不等值:注册/快照/kill 必须走 display(800),
            // 事件 tag 用 ad-hoc 行号(700)
            session_id: 700,
            display_session_id: 800,
            title: "测试离散会话".into(),
            run_mode: mf_agent::RunMode::OneShot,
            plan,
            run_temp: run_temp.clone(),
            // project 路由键固定用 ".":测试快照/摘除用同一键;
            // 真实 cwd 由 plan.cwd 提供
            workdir: PathBuf::from("."),
            events: registry_events,
        }
    }

    #[test]
    fn missing_executable_errors_without_registry_entry() {
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let (events, _rx) = crossbeam_channel::bounded(16);
        let spec = ad_hoc_spec(events, "definitely-not-a-real-cli-xyz", &[], None);
        let error = launch_ad_hoc_pty(&registry, &spec).unwrap_err();
        assert!(error.to_string().contains("启动"), "{error}");
        // 失败路径不得留下注册表条目(无孤儿会话句柄)
        assert!(
            registry.snapshot(".", 800).is_none(),
            "失败路径不得留下注册表条目(display 键)"
        );
        assert!(
            registry.snapshot(".", 700).is_none(),
            "ad-hoc 行号不是进程注册键"
        );
    }

    #[test]
    fn real_process_exit_reports_code_and_detaches() {
        let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let (events, rx) = crossbeam_channel::bounded(16);
        // 输出内容后非零退出:同时覆盖"有输出"与"非零码"两条路径
        let spec = ad_hoc_spec(events, &cmd, &["/C", "echo bye&&exit 2"], None);
        launch_ad_hoc_pty(&registry, &spec).unwrap();
        assert!(
            registry.snapshot(".", 800).is_some(),
            "启动后应能以 display ID 快照"
        );
        assert!(
            registry.snapshot(".", 700).is_none(),
            "ad-hoc 行号不是进程注册键"
        );

        let (tag, event) = rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("等待退出事件超时");
        assert_eq!(tag, 700, "事件 tag 是 ad-hoc 行号");
        match event {
            RuntimeEvent::AdHocExited {
                session_id,
                exit_code,
                ..
            } => {
                assert_eq!(session_id, 700);
                assert_eq!(exit_code, Some(2));
            }
            other => panic!("应为 AdHocExited,得到 {other:?}"),
        }
        // 退出后会话从注册表摘除
        let still_present = (0..150).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            registry.snapshot(".", 800).is_some()
        });
        assert!(!still_present, "会话应在退出后从注册表移除(display 键)");
    }

    #[test]
    fn silent_exit_still_reports_exit_event() {
        // 无任何输出、直接非零退出:不得让会话永远停在 reader 循环
        let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let (events, rx) = crossbeam_channel::bounded(16);
        let spec = ad_hoc_spec(events, &cmd, &["/C", "exit", "3"], None);
        launch_ad_hoc_pty(&registry, &spec).unwrap();
        let (tag, event) = rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("静默退出的进程也必须上报退出事件");
        assert_eq!(tag, 700, "事件 tag 是 ad-hoc 行号");
        assert!(matches!(event, RuntimeEvent::AdHocExited { .. }));
    }

    #[test]
    fn secret_echo_is_redacted_before_screen_and_tail() {
        // 真实 CLI 回显 Secret 后保持存活(pause):screen 不得包含明文
        let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let secret = "sk-live-secret-4242";
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let (events, _rx) = crossbeam_channel::bounded(16);
        let spec = ad_hoc_spec(
            events,
            &cmd,
            &["/C", &format!("echo {secret} & pause")],
            Some(("MY_TOKEN", secret)),
        );
        launch_ad_hoc_pty(&registry, &spec).unwrap();

        // 会话存活期间轮询:脱敏后的回显到达即断言并退出
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_output = false;
        while std::time::Instant::now() < deadline {
            if let Some(snapshot) = registry.snapshot(".", 800) {
                let text = snapshot.screen_rows.concat();
                if text.contains("***") {
                    saw_output = true;
                    assert!(!text.contains(secret), "screen 泄露 Secret: {text:?}");
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        assert!(saw_output, "应在会话存活期间观察到脱敏后的回显");
        assert!(
            registry.snapshot(".", 800).is_some(),
            "pause 会话应保持存活(display 键)"
        );
        registry.kill_session(".", 800);
    }
}

impl KeepAwake {
    pub fn new() -> KeepAwake {
        KeepAwake {
            active: AtomicBool::new(false),
        }
    }
    pub fn set_working(&self, working: bool) {
        if working == self.active.load(Ordering::SeqCst) {
            return;
        }
        self.active.store(working, Ordering::SeqCst);
        #[cfg(windows)]
        unsafe {
            const ES_CONTINUOUS: u32 = 0x80000000;
            const ES_SYSTEM_REQUIRED: u32 = 0x00000001;
            const ES_DISPLAY_REQUIRED: u32 = 0x00000002;
            let flags = if working {
                ES_CONTINUOUS | ES_SYSTEM_REQUIRED
            } else {
                ES_CONTINUOUS
            };
            windows_sys::Win32::System::Power::SetThreadExecutionState(flags);
        }
    }
}

/// 供测试与 smoke:HTTP mock 冒烟(无网络)。
#[allow(dead_code)]
pub fn provider_kind_of(spec: &AgentProfileSpec) -> String {
    match (&spec.runtime, &spec.provider) {
        (RuntimeKind::Http, Some(p)) => format!("{:?}", p.kind).to_lowercase(),
        _ => spec.runtime.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ad_hoc_and_display_ids_are_distinct_namespaces() {
        // ad_hoc_sessions 与 agent_sessions 是两套自增行号:
        // 进程路由只认 display session ID;ad-hoc 行号仅作事件 tag。
        // 注册表只有一条命名空间:普通会话键。
        let registry = SessionRegistry::new(mf_agent::Config::default());
        assert!(registry.snapshot("proj", 7).is_none());
        assert!(registry.send_prompt("proj", 7, "hi").is_err());
        registry.kill_session("proj", 1); // 不存在时是 no-op
    }
}
