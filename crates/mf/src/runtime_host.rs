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
pub fn session_key(project: &str, id: i64) -> String {
    format!("{}#{}", project.replace('#', "_"), id)
}

/// 离散 CLI 会话键:ad_hoc_sessions 行号与 agent_sessions 行号是两套
/// 自增序列,同项目下会撞号,必须用独立命名空间隔离路由。
pub fn ad_hoc_session_key(project: &str, id: i64) -> String {
    format!("{}#ad#{}", project.replace('#', "_"), id)
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

    /// 离散 CLI 会话快照(独立命名空间,见 `ad_hoc_session_key`)。
    pub fn snapshot_ad_hoc(&self, project: &str, session_id: i64) -> Option<SessionSnapshot> {
        let key = ad_hoc_session_key(project, session_id);
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

    /// 向离散 CLI 会话写入提示(独立命名空间)。
    pub fn send_prompt_ad_hoc(&self, project: &str, session_id: i64, text: &str) -> Result<()> {
        let key = ad_hoc_session_key(project, session_id);
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

    /// 终止离散 CLI 会话(独立命名空间)。
    pub fn kill_ad_hoc(&self, project: &str, session_id: i64) {
        let key = ad_hoc_session_key(project, session_id);
        Self::kill_at(&self.sessions, &key);
    }

    fn kill_at(sessions: &Mutex<HashMap<String, SessionInner>>, key: &str) {
        if let Some(s) = sessions.lock().remove(key) {
            match &s {
                SessionInner::Pty(p) => {
                    p.alive.store(false, Ordering::SeqCst);
                    p.writer.lock().take();
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

    /// 离散 CLI 会话注册(独立命名空间,不与 agent_sessions 撞号)。
    fn register_ad_hoc(&self, project: &str, session_id: i64, inner: SessionInner) {
        self.sessions
            .lock()
            .insert(ad_hoc_session_key(project, session_id), inner);
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

fn launch_pty(registry: &SessionRegistry, spec: &LaunchSpec, events: Sender<(i64, RuntimeEvent)>) {
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

    let child = match pair.slave.spawn_command(cmd) {
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
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::SpawnError(format!("克隆 PTY reader 失败: {e}")),
            ));
            return;
        }
    };
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

    let session = Arc::new(PtySession {
        session_id: spec.session_id,
        master: Mutex::new(Some(pair.master)),
        writer: Mutex::new(Some(writer)),
        child: Mutex::new(Some(child)),
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

    let _ = events.send((spec.run_id, RuntimeEvent::Launched));
    let _ = events.send((
        spec.run_id,
        RuntimeEvent::AgentState(mf_agent::AgentState::Working),
    ));

    // reader 线程:喂终端模拟器 + 输出缓冲 + Output 事件(节流)
    let run_id = spec.run_id;
    let events_out = events.clone();
    std::thread::Builder::new()
        .name(format!("pty-reader-{}", session.session_id))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            let mut last_output_event =
                std::time::Instant::now() - std::time::Duration::from_secs(10);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        {
                            let mut screen = session.screen.lock();
                            screen.feed(&buf[..n]);
                            if !screen.title.is_empty() {
                                *session.title.lock() = screen.title.clone();
                            }
                        }
                        {
                            let mut tail = session.output_tail.lock();
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
            session.alive.store(false, Ordering::SeqCst);
            let code = session
                .child
                .lock()
                .as_mut()
                .and_then(|c| c.wait().ok())
                .map(|s| s.exit_code() as i32);
            session.writer.lock().take();
            session.master.lock().take();
            let _ = events_out.send((run_id, RuntimeEvent::Exited { code }));
            let _ = events_out.send((run_id, RuntimeEvent::AgentState(mf_agent::AgentState::Dead)));
        })
        .ok();
}

// ---------------- Ad-hoc CLI 会话 ----------------

/// 离散 CLI 会话启动(设计 §4.7 / §10):挂在 Task 下,
/// 无 Step / Agent Run / 结算事件流;注册表仍以 (project, session_id)
/// 路由,UI 终端视图、send_prompt、kill 与普通会话一致。
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
            anyhow::bail!("临时文件路径不得逃许运行临时目录: {}", file.path.display());
        }
        let target = run_temp.join(&file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建临时文件目录失败: {}", parent.display()))?;
        }
        std::fs::write(&target, &file.contents)
            .with_context(|| format!("写入临时文件失败: {}", target.display()))?;
    }
    Ok(())
}

fn launch_ad_hoc_pty(registry: &SessionRegistry, spec: &AdHocLaunchSpec) -> Result<()> {
    let project = spec.workdir.to_string_lossy().to_string();
    registry.kill_ad_hoc(&project, spec.session_id); // 同键旧会话清理

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

    let mut cmd = CommandBuilder::new(&spec.plan.executable);
    cmd.args(&spec.plan.argv);
    for (k, v) in &spec.plan.env {
        cmd.env(k, v);
    }
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
    let mut writer = pair
        .master
        .take_writer()
        .context("离散会话 take_writer 失败")?;
    let child = pair.slave.spawn_command(cmd).with_context(|| {
        format!(
            "离散会话启动 `{}` 失败(CLI 未安装或配置无效)",
            spec.plan.executable.display()
        )
    })?;
    drop(pair.slave);
    if let InputInjection::Stdin(bytes) = &spec.plan.input {
        writer
            .write_all(bytes)
            .context("向离散会话 stdin 写入初始提示失败")?;
        writer.flush().context("刷新离散会话 stdin 失败")?;
    }

    let session = Arc::new(PtySession {
        session_id: spec.session_id,
        master: Mutex::new(Some(pair.master)),
        writer: Mutex::new(Some(writer)),
        child: Mutex::new(Some(child)),
        screen: Mutex::new(Screen::new(TERM_ROWS, TERM_COLS)),
        title: Mutex::new(spec.title.clone()),
        output_tail: Mutex::new(Vec::new()),
        alive: AtomicBool::new(true),
    });
    registry.register_ad_hoc(
        &project,
        spec.session_id,
        SessionInner::Pty(session.clone()),
    );

    let reader_project = project.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(format!("ad-hoc-pty-reader-{}", spec.session_id))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut screen = session.screen.lock();
                        screen.feed(&buf[..n]);
                        if !screen.title.is_empty() {
                            *session.title.lock() = screen.title.clone();
                        }
                        drop(screen);
                        let mut tail = session.output_tail.lock();
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > OUT_BUFFER_CAP {
                            let drop = tail.len() - OUT_BUFFER_CAP;
                            tail.drain(..drop);
                        }
                    }
                }
            }
            session.alive.store(false, Ordering::SeqCst);
            let _ = session.child.lock().as_mut().and_then(|c| c.wait().ok());
            session.writer.lock().take();
            session.master.lock().take();
        })
    {
        registry.kill_ad_hoc(&reader_project, spec.session_id);
        return Err(error).context("启动离散会话 reader 线程失败");
    }
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
}

impl RuntimeHostImpl {
    pub fn new(registry: Arc<SessionRegistry>) -> Arc<RuntimeHostImpl> {
        Arc::new(RuntimeHostImpl { registry })
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

    fn launch_ad_hoc(&self, spec: AdHocLaunchSpec) -> Result<()> {
        launch_ad_hoc_pty(&self.registry, &spec)
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
    fn ad_hoc_sessions_use_separate_key_namespace() {
        // ad_hoc_sessions 与 agent_sessions 是两套自增行号,
        // 同项目下同号必须路由到不同注册表条目。
        assert_ne!(session_key("proj", 7), ad_hoc_session_key("proj", 7));
        assert_eq!(ad_hoc_session_key("pro#j", 7), "pro_j#ad#7");
    }

    #[test]
    fn ad_hoc_snapshot_missing_is_none() {
        let registry = SessionRegistry::new(mf_agent::Config::default());
        assert!(registry.snapshot_ad_hoc("proj", 1).is_none());
        assert!(registry.send_prompt_ad_hoc("proj", 1, "hi").is_err());
        registry.kill_ad_hoc("proj", 1); // 不存在时是 no-op
    }
}
