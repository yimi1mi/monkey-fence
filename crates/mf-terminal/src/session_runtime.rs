//! SessionRuntime:Agent Session 宿主运行时(T12 迁移,自 crates/mf/src/
//! runtime_host.rs 整体迁入;canonical spec §15.1 session(runtime+registry))。
//! SessionRegistry 是全部 Agent Session handle、Workflow Run 关联、writer
//! lease、事件与 transcript 的逻辑 owner;三条 launch 路径共用统一
//! redactor→journal→Screen/transcript 管线(#29–#34)。

use crate::journal::TerminalJournal;
use crate::term_screen::Screen;
use crate::transcript::{ExitGate, FlushPolicy, TranscriptFlusher};

const TERM_ROWS: usize = 26;
const TERM_COLS: usize = 120;
use crate::pty::{self as pty, SpawnCommand};
use anyhow::{anyhow, Context as _, Result};
use crossbeam_channel::Sender;
use mf_agent::provider::{complete, AssistantBlock, ChatMessage, ToolDef};
use mf_agent::runtime::{
    AdHocLaunchSpec, AgentProfileSpec, LaunchSpec, RuntimeEvent, RuntimeHost, RuntimeKind,
};
use mf_agent::Settlement;
use mf_agent::{InputInjection, TempFileSpec};
use parking_lot::Mutex;
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

/// 终止确认信号:生命周期真正的拥有方(reap 线程/工具循环线程)
/// 在收口完成后置位;stop 据此等待**真实结束**,请求本身不置位。
struct TermSignal {
    terminated: Mutex<bool>,
    cv: parking_lot::Condvar,
}

impl TermSignal {
    fn new() -> TermSignal {
        TermSignal {
            terminated: Mutex::new(false),
            cv: parking_lot::Condvar::new(),
        }
    }

    fn mark(&self) {
        let mut terminated = self.terminated.lock();
        *terminated = true;
        self.cv.notify_all();
    }

    /// 等待终止确认;超时返回 false。
    fn wait_for(&self, timeout: std::time::Duration) -> bool {
        let mut terminated = self.terminated.lock();
        if !*terminated {
            let result = self.cv.wait_for(&mut terminated, timeout);
            if result.timed_out() && !*terminated {
                return false;
            }
        }
        true
    }
}

struct PtySession {
    session_id: i64,
    /// T3f:会话键与宿主注册表弱引用(transcript flush 路由用)。
    handle: String,
    registry: std::sync::Weak<SessionRegistry>,
    master: Mutex<Option<pty::PtyMaster>>,
    writer: Mutex<Option<pty::PtyWriter>>,
    child: Mutex<Option<pty::PtyChild>>,
    /// 独立 kill 句柄(离散会话:child 由 waiter 线程持有等待,
    /// kill 经此克隆执行,避免跨线程争抢)。
    killer: Mutex<Option<pty::PtyChildKiller>>,
    /// 进程树守卫(Windows Job Object):stop 时 terminate 整树并等清空。
    job: Mutex<Option<pty::JobGuard>>,
    screen: Mutex<Screen>,
    title: Mutex<String>,
    /// T3f:输出数据面权威。redactor 之后、screen/tail 之前分配 seq;
    /// replay/reconnect/exit(final_seq) 语义由它承载(取代 256 KiB
    /// output_tail 旁路)。
    journal: Mutex<TerminalJournal>,
    /// T3f:transcript segment flush(周期/批大小;见 transcript_sink)。
    flusher: Mutex<TranscriptFlusher>,
    /// T3f:durable-before-notify exit 门闩(§8.5)。
    exit_gate: Mutex<ExitGate>,
    alive: AtomicBool,
    /// OS 进程 PID(终止验证/诊断;无进程时 None)。
    pid: Option<u32>,
    /// 终止确认:reap 线程在 child.wait()(reap)完成、生命周期收口后
    /// 置位并唤醒。kill 请求本身不置位 —— stop_run 据此等待真实终止。
    term: TermSignal,
}

impl PtySession {
    /// T3f:flush 批次 → registry transcript sink(§8.2 durable flush)。
    /// 永不阻塞:无 sink/无路由由 registry 内部丢弃并记日志。
    fn flush_transcript_batch(&self, batch: &crate::transcript::FlushBatch) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let epoch = self.journal.lock().epoch().as_uuid().to_string();
        registry.commit_transcript(
            &self.handle,
            &epoch,
            Some(batch),
            crate::transcript::FINAL_STATE_LIVE,
            batch.seq_end,
            None,
        );
    }

    /// T3f:exit 收口(final 批次 + complete + exit 元数据)。
    fn flush_transcript_exit(
        &self,
        batch: Option<&crate::transcript::FlushBatch>,
        final_seq: u64,
        exit_code: Option<i64>,
    ) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let epoch = self.journal.lock().epoch().as_uuid().to_string();
        registry.commit_transcript(
            &self.handle,
            &epoch,
            batch,
            crate::transcript::FINAL_STATE_COMPLETE,
            final_seq,
            exit_code,
        );
    }

    /// 标记进程已真正终止(child 已 reap、生命周期已收口)。
    fn mark_terminated(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.term.mark();
    }

    /// 等待终止确认(真实 OS 进程已 reap);超时返回 false。
    fn wait_terminated(&self, timeout: std::time::Duration) -> bool {
        self.term.wait_for(timeout)
    }
}

/// HTTP Runtime 一次 ask_human 的待答槽:同时记录发起提问的 run 与
/// Orchestrator 持久化后回填的 question 行;投递必须两者都匹配,
/// 否则视为"无法证明接收者仍是原问题"而 fail-closed。
struct PendingQuestion {
    run_handle: String,
    /// `step_questions` 行号;None = Question 事件尚未被 Orchestrator
    /// 持久化/回填(此窗口内投递一律拒绝,宁可拒绝不可误投)。
    question_id: Option<i64>,
    tx: Sender<String>,
}

struct HttpSession {
    session_id: i64,
    transcript: Mutex<Vec<(String, String)>>,
    alive: AtomicBool,
    /// 等待用户回答的 ask_human 待答槽(question-bound 身份见
    /// [`PendingQuestion`])。
    pending: Mutex<Option<PendingQuestion>>,
    /// 终止信号。
    cancel: AtomicBool,
    /// 工具循环线程的**真实结束**确认(循环退出、事件已上报后置位)。
    term: TermSignal,
    /// 工具循环线程 join 句柄(stop 确认后回收,不留 detached 泄漏)。
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl HttpSession {
    /// 标记工具循环线程已真正结束(所有事件已上报)。
    fn mark_terminated(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.term.mark();
    }

    fn wait_terminated(&self, timeout: std::time::Duration) -> bool {
        self.term.wait_for(timeout)
    }

    /// ask_human 的可取消等待:分片轮询(250ms)检查 cancel,
    /// stop 时通道被摘除(Disconnected)或 cancel 置位 → 立即返回,
    /// 不再 6 小时盲等。
    fn wait_answer(
        &self,
        rx: &crossbeam_channel::Receiver<String>,
        budget: std::time::Duration,
    ) -> Option<String> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                return None;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match rx.recv_timeout(remaining.min(std::time::Duration::from_millis(250))) {
                Ok(answer) => return Some(answer),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
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

pub struct SessionRegistry {
    /// 唯一主键是 `agent_sessions.public_handle`。项目路径和 SQLite rowid
    /// 都不得进入注册表身份，避免目录移动/导入后路由漂移。
    sessions: Mutex<HashMap<String, SessionInner>>,
    /// `agent_runs.public_handle -> agent_sessions.public_handle`。
    run_sessions: Mutex<HashMap<String, String>>,
    /// question-bound 回答的投递账本:`step_questions.id -> 已投递答案`。
    /// 只存在于进程内存:不写日志、不进 Snapshot/事件、不持久化;
    /// 核心重启后连同会话一起消失 —— 重启后的旧投递动作因无存活会话
    /// 天然 fail-closed(spec:live 会话 lost/Needs You)。
    /// T3f:transcript sink(durable flush 注入点;None = flush 丢弃并
    /// 记日志,reader 永不阻塞)。AppCtx 装配时设置。
    transcript_sink: Mutex<Option<Arc<dyn TranscriptSink>>>,
    /// T3f:session handle → project root(transcript 的 Store 路由;
    /// launch 时记录,进程内生命周期)。
    session_roots: Mutex<HashMap<String, std::path::PathBuf>>,
    question_deliveries: Mutex<HashMap<i64, String>>,
    config: Mutex<mf_agent::Config>,
    /// stop 等待真实终止确认的时限(生产 10s;测试可调短)。
    stop_confirm_timeout: parking_lot::RwLock<std::time::Duration>,
}

/// T2f(Issue #28):legacy SessionRegistry 作为 `TerminalHost` 的 shim
/// 实现——`CoreKernel::attach_terminal` 经此委托既有 PTY 管线;调用者只
/// 拿到 `TerminalChannel`,不再接触 raw writer。T3 在 mf-terminal 落地
/// 真实管线后本实现整体移除(同接口换内部)。
impl crate::TerminalHost for SessionRegistry {
    fn session_alive(&self, session: &crate::TerminalSessionRef) -> bool {
        SessionRegistry::session_alive(self, session.as_str())
    }

    fn send_input(
        &self,
        session: &crate::TerminalSessionRef,
        bytes: &[u8],
    ) -> Result<(), crate::TerminalProblem> {
        self.send_prompt_raw(session.as_str(), bytes)
            .map_err(|error| crate::TerminalProblem::WriteFailed(format!("{error:#}")))
    }

    fn terminate_session(
        &self,
        session: &crate::TerminalSessionRef,
    ) -> Result<(), crate::TerminalProblem> {
        self.kill_session(session.as_str());
        Ok(())
    }

    fn tail_lines(&self, session: &crate::TerminalSessionRef, lines: usize) -> Vec<String> {
        self.pty_tail(session.as_str(), lines)
    }

    fn replay_output(
        &self,
        session: &crate::TerminalSessionRef,
        after_seq: u64,
    ) -> Result<Vec<crate::journal::JournalChunk>, crate::TerminalProblem> {
        self.with_journal(session.as_str(), |journal| journal.replay(after_seq))
            .ok_or_else(|| crate::TerminalProblem::SessionNotFound(session.as_str().to_string()))
    }

    fn output_facts(
        &self,
        session: &crate::TerminalSessionRef,
    ) -> Result<crate::journal::HelloFacts, crate::TerminalProblem> {
        self.with_journal(session.as_str(), |journal| journal.hello_facts())
            .ok_or_else(|| crate::TerminalProblem::SessionNotFound(session.as_str().to_string()))
    }

    fn resize_session(
        &self,
        session: &crate::TerminalSessionRef,
        cols: u16,
        rows: u16,
    ) -> Result<(), crate::TerminalProblem> {
        // 边界复验(fixed 2-500/2-300;附录 A2)
        if !(crate::limits::RESIZE_COLS_MIN..=crate::limits::RESIZE_COLS_MAX).contains(&cols)
            || !(crate::limits::RESIZE_ROWS_MIN..=crate::limits::RESIZE_ROWS_MAX).contains(&rows)
        {
            return Err(crate::TerminalProblem::WriteFailed(format!(
                "resize 尺寸越界(cols={cols}, rows={rows})"
            )));
        }
        self.resize_pty(session.as_str(), cols, rows)
            .map_err(|error| crate::TerminalProblem::WriteFailed(format!("{error:#}")))
    }
}

/// T3f:transcript durable flush 注入点(§8.2 周期 durable flush)。
/// 实现负责把批次路由到对应 Project Store(`Store::terminal_transcript_commit`);
/// 失败只记日志,绝不回压 reader。
pub trait TranscriptSink: Send + Sync {
    fn commit(
        &self,
        session_handle: &str,
        project_root: Option<&std::path::Path>,
        epoch: &str,
        batch: Option<&crate::transcript::FlushBatch>,
        final_state: &str,
        durable_through_seq: u64,
        exit_code: Option<i64>,
    );
}

impl SessionRegistry {
    /// AppCtx 装配时注入;幂等覆盖。
    pub fn set_transcript_sink(&self, sink: Arc<dyn TranscriptSink>) {
        *self.transcript_sink.lock() = Some(sink);
    }

    /// launch 时记录 transcript 路由(进程内)。
    pub fn note_session_root(&self, session_handle: &str, project_root: &std::path::Path) {
        self.session_roots
            .lock()
            .insert(session_handle.to_string(), project_root.to_path_buf());
    }

    /// reader 线程的 flush 入口:未注入 sink 或无路由时丢弃并记日志。
    pub fn commit_transcript(
        &self,
        session_handle: &str,
        epoch: &str,
        batch: Option<&crate::transcript::FlushBatch>,
        final_state: &str,
        durable_through_seq: u64,
        exit_code: Option<i64>,
    ) {
        let sink = self.transcript_sink.lock().clone();
        let Some(sink) = sink else {
            log::debug!("transcript sink 未装配,丢弃 {session_handle} 的 flush 批次");
            return;
        };
        let root = self.session_roots.lock().get(session_handle).cloned();
        sink.commit(
            session_handle,
            root.as_deref(),
            epoch,
            batch,
            final_state,
            durable_through_seq,
            exit_code,
        );
    }
}

impl SessionRegistry {
    pub fn new(config: mf_agent::Config) -> Arc<SessionRegistry> {
        Arc::new(SessionRegistry {
            sessions: Mutex::new(HashMap::new()),
            run_sessions: Mutex::new(HashMap::new()),
            transcript_sink: Mutex::new(None),
            session_roots: Mutex::new(HashMap::new()),
            question_deliveries: Mutex::new(HashMap::new()),
            config: Mutex::new(config),
            stop_confirm_timeout: parking_lot::RwLock::new(std::time::Duration::from_secs(10)),
        })
    }

    /// 测试注入:调短 stop 终止确认时限。
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_stop_confirm_timeout(&self, timeout: std::time::Duration) {
        *self.stop_confirm_timeout.write() = timeout;
    }

    /// T2f:引擎配置传播收窄为 crate 内部(AppCtx 统一入口),不再作为
    /// SessionRegistry 的外部 mutation 入口。
    pub fn update_config(&self, config: mf_agent::Config) {
        *self.config.lock() = config;
    }

    fn get_inner(&self, key: &str) -> Option<SessionInner> {
        self.sessions.lock().get(key).cloned()
    }

    /// UI 终端视图重新挂载时恢复当前屏幕。
    pub fn snapshot(&self, session_handle: &str, session_id: i64) -> Option<SessionSnapshot> {
        self.snapshot_at(session_handle, session_id)
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

    /// T3f:锁内读会话 journal(仅 PTY 会话;HTTP/不存在返回 None)。
    fn with_journal<R>(
        &self,
        session_handle: &str,
        read: impl FnOnce(&TerminalJournal) -> R,
    ) -> Option<R> {
        match self.get_inner(session_handle)? {
            SessionInner::Pty(p) => Some(read(&p.journal.lock())),
            SessionInner::Http(_) => None,
        }
    }

    /// T3f:真实 resize——PTY master(ConPTY/TIOCSWINSZ)+ Screen 投影
    /// 同步(§8.5);仅 PTY 会话。
    fn resize_pty(&self, session_handle: &str, cols: u16, rows: u16) -> Result<()> {
        match self.get_inner(session_handle) {
            Some(SessionInner::Pty(p)) => {
                {
                    let master = p.master.lock();
                    if let Some(master) = master.as_ref() {
                        master.resize(pty::PtySize { rows, cols })?;
                    }
                }
                p.screen.lock().resize(rows as usize, cols as usize);
                Ok(())
            }
            _ => Err(anyhow!("会话不是 PTY")),
        }
    }

    /// 终端输出尾部(卡片"最后回复"用)。
    pub fn pty_tail(&self, session_handle: &str, lines: usize) -> Vec<String> {
        let sessions = self.sessions.lock();
        match sessions.get(session_handle) {
            Some(SessionInner::Pty(p)) => {
                let screen = p.screen.lock();
                screen.tail_lines(lines)
            }
            _ => Vec::new(),
        }
    }

    /// T0c 原始 PTY 输出契约的测试 seam:返回已经过生产
    /// StreamingRedactor、但尚未被 Screen 解释的字节。仅测试编译,
    /// 不固化 256 KiB 容量或作为新协议。
    #[cfg(any(test, feature = "test-support"))]
    pub fn pty_output_bytes(&self, session_handle: &str) -> Option<Vec<u8>> {
        match self.get_inner(session_handle)? {
            SessionInner::Pty(session) => Some(session.journal.lock().tail_bytes(OUT_BUFFER_CAP)),
            SessionInner::Http(_) => None,
        }
    }

    /// T2f:终端文本输入收窄为 crate 内部;UI 写输入必须经
    /// `CoreKernel::attach_terminal` 返回的 `TerminalChannel`。
    pub fn send_prompt(&self, session_handle: &str, text: &str) -> Result<()> {
        self.send_prompt_at(session_handle, text)
    }

    /// 锁外做阻塞 I/O(ConPTY 输入缓冲满时 write 会阻塞,不能拿着注册表锁)。
    fn send_prompt_at(&self, key: &str, text: &str) -> Result<()> {
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
            None => Err(anyhow!("会话 handle `{key}` 不存在")),
        }
    }

    /// 终端键盘直通:原始字节写入 PTY(不追加回车)。
    /// T2f:不再是外部入口;唯一跨模块调用方是本文件的
    /// `TerminalHost` shim 实现与测试 harness。
    pub fn send_prompt_raw(&self, session_handle: &str, bytes: &[u8]) -> Result<()> {
        let sess = {
            let sessions = self.sessions.lock();
            sessions.get(session_handle).cloned()
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

    /// T2f:会话终止收窄为 crate 内部;UI 侧经 `TerminalChannel::terminate`。
    pub fn kill_session(&self, session_handle: &str) {
        if let Some(session) = self.remove_session_and_bindings(session_handle) {
            Self::terminate_session(session);
        }
    }

    /// 停止 run 绑定的会话进程并等待**真实终止确认**(child 已被
    /// reader/waiter 线程 wait/reap、生命周期已收口)。
    /// Ok = 已确认停止(或本无绑定会话);Err = 时限内未确认
    /// (进程可能仍在运行,调用方不得标记 Cancelled/释放租约)。
    /// T2f:收窄为 crate 内部(RunLifecyclePort/Host 生命周期内部使用)。
    pub fn stop_run(&self, run_handle: &str) -> Result<()> {
        // I10:只查找、不预先移除 —— 未确认终止前 binding/session 保留
        // (超时后可重试停止);确认 terminated 并回收线程后才移除。
        let bound = self.run_sessions.lock().get(run_handle).cloned();
        let Some(session_key_of_run) = bound else {
            return Ok(()); // 无绑定:无进程可停,视为已停止
        };
        let inner = self.sessions.lock().get(&session_key_of_run).cloned();
        let Some(inner) = inner else {
            self.remove_session_and_bindings(&session_key_of_run);
            return Ok(()); // 绑定指向的会话已摘除(自然退出/已清理)
        };
        let confirm_removed = |me: &SessionRegistry| {
            me.remove_session_and_bindings(&session_key_of_run);
        };
        match &inner {
            SessionInner::Http(h) => {
                // 发出终止信号并唤醒阻塞中的 ask_human;随后等待工具
                // 循环线程**真正结束**(事件已上报、生命周期收口),
                // 未确认前不得返回 Ok → 调用方不释放执行租约
                h.cancel.store(true, Ordering::SeqCst);
                h.pending.lock().take();
                if h.wait_terminated(*self.stop_confirm_timeout.read()) {
                    if let Some(handle) = h.join.lock().take() {
                        let _ = handle.join();
                    }
                    confirm_removed(self); // 确认结束:此刻才移除
                    Ok(())
                } else {
                    // 超时:保留 binding/session(stopping 可重试状态)
                    Err(anyhow!(
                        "run {run_handle} HTTP 工具循环未在时限内真正结束(可能仍在处理请求;会话保留,可重试停止)"
                    ))
                }
            }
            SessionInner::Pty(p) => {
                // 拥有并等待**整个进程树**(Windows Job Object / Unix PGID):
                // cmd/npm 风格父进程派生的孙进程一并终止,job 清空
                //(全部派生进程消失)才算停止确认。
                // C8:终止/等待全部经 try_clone 的**克隆**执行 —— 原守卫
                // 留在会话里,超时未确认不消费守卫(第二次重试仍能整组杀);
                // 整组确认消失且真实终止确认后才 take/drop。
                if let Some(job) = p
                    .job
                    .lock()
                    .as_ref()
                    .map(pty::JobGuard::try_clone)
                    .transpose()?
                {
                    if let Err(e) = job.terminate() {
                        log::warn!("run {run_handle} 进程树 terminate 请求失败: {e}");
                    }
                    let empty = job.wait_empty(std::time::Duration::from_secs(10));
                    drop(job);
                    if !empty {
                        // 树未清空:进程可能仍在写执行目录 → 不确认、不释放、
                        // 不消费守卫(会话保留,重试仍能整组终止)
                        return Err(anyhow!(
                            "run {run_handle} 进程树未在 10s 内清空(孙进程仍存活)"
                        ));
                    }
                }
                // 停止输入;请求直接子进程终止(树 terminate 的兜底)。
                // 真实终止由 reader/waiter 线程 child.wait() 确认后
                // mark_terminated —— 不在 kill 后立刻谎报 alive=false。
                p.writer.lock().take();
                if let Some(killer) = p.killer.lock().take() {
                    if let Err(e) = killer.kill() {
                        log::warn!("run {run_handle} 会话进程 kill 请求失败: {e}");
                    }
                }
                if let Some(child) = p.child.lock().take() {
                    // reader/waiter 尚未接管 child:直接终止并同步 reap
                    let _ = child.kill();
                    let _ = child.wait();
                    p.master.lock().take();
                    p.mark_terminated();
                }
                if p.wait_terminated(std::time::Duration::from_secs(10)) {
                    // 整组已消失 + 真实终止确认:此刻才消费守卫并移除
                    drop(p.job.lock().take());
                    confirm_removed(self);
                    Ok(())
                } else {
                    // 超时:保留 binding/session 与进程树守卫
                    //(可重试;重试路径由 reader/waiter 线程的收口确认终止)
                    Err(anyhow!(
                        "run {run_handle} 会话进程停止未在 10s 内确认(可能被外部阻塞;会话保留,可重试停止)"
                    ))
                }
            }
        }
    }

    /// PTY 会话的 OS 进程 PID(终止验证/诊断;HTTP 或无进程时 None)。
    pub fn session_pid(&self, session_handle: &str) -> Option<u32> {
        match self.sessions.lock().get(session_handle)? {
            SessionInner::Pty(p) => p.pid,
            SessionInner::Http(_) => None,
        }
    }

    fn remove_session_and_bindings(&self, session_handle: &str) -> Option<SessionInner> {
        // 与 stop_run 相同的锁顺序(run bindings → sessions)，并在一个
        // 临界区清掉所有历史 Run→Session 反向引用。
        let mut bindings = self.run_sessions.lock();
        let session = self.sessions.lock().remove(session_handle);
        bindings.retain(|_, bound_session| bound_session != session_handle);
        // T3f:transcript 路由随会话摘除(sink 侧 Store 缓存由 LRU 回收)。
        self.session_roots.lock().remove(session_handle);
        session
    }

    fn terminate_session(session: SessionInner) {
        match &session {
            SessionInner::Pty(p) => {
                p.writer.lock().take();
                // 进程树终止(尽力而为;不等待,此处绝不伪造"进程已死")
                if let Some(job) = p.job.lock().take() {
                    if let Err(e) = job.terminate() {
                        log::warn!("进程树 terminate 请求失败: {e}");
                    }
                }
                if let Some(killer) = p.killer.lock().take() {
                    let _ = killer.kill();
                }
                if let Some(child) = p.child.lock().take() {
                    // reader/waiter 未接管 child:直接终止并同步 reap,
                    // 终止已确认
                    let _ = child.kill();
                    let _ = child.wait();
                    p.master.lock().take();
                    p.mark_terminated();
                }
            }
            SessionInner::Http(h) => {
                h.cancel.store(true, Ordering::SeqCst);
                h.alive.store(false, Ordering::SeqCst);
                h.pending.lock().take(); // 唤醒阻塞中的 ask_human
            }
        }
    }

    fn send_prompt_for_run(
        &self,
        run_handle: &str,
        session_handle: &str,
        text: &str,
    ) -> Result<()> {
        let bound = self.run_sessions.lock().get(run_handle).cloned();
        anyhow::ensure!(
            bound.as_deref() == Some(session_handle),
            "run handle `{run_handle}` 未绑定到 session handle `{session_handle}`"
        );
        self.send_prompt(session_handle, text)
    }

    #[cfg(test)]
    fn binding_count(&self) -> usize {
        self.run_sessions.lock().len()
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

    pub fn session_alive(&self, session_handle: &str) -> bool {
        self.sessions
            .lock()
            .get(session_handle)
            .map(|s| match s {
                SessionInner::Pty(p) => p.alive.load(Ordering::SeqCst),
                SessionInner::Http(h) => h.alive.load(Ordering::SeqCst),
            })
            .unwrap_or(false)
    }

    fn register(&self, session_handle: &str, inner: SessionInner) {
        self.sessions
            .lock()
            .insert(session_handle.to_string(), inner);
    }

    fn bind_run(&self, run_handle: &str, session_handle: &str) {
        self.run_sessions
            .lock()
            .insert(run_handle.to_string(), session_handle.to_string());
    }

    fn unbind_run(&self, run_handle: &str) {
        self.run_sessions.lock().remove(run_handle);
    }

    pub fn session_of_run(&self, run_handle: &str) -> Option<String> {
        self.run_sessions.lock().get(run_handle).cloned()
    }

    fn http_answer(&self, session_handle: &str, answer: &str) {
        let sessions = self.sessions.lock();
        if let Some(SessionInner::Http(h)) = sessions.get(session_handle) {
            if let Some(slot) = h.pending.lock().take() {
                let _ = slot.tx.send(answer.to_string());
            }
        }
    }

    /// Orchestrator 持久化 question 行后回填绑定:把 run 当前等待中的
    /// ask_human 待答槽打上 question 行号。尽力而为的关联通知
    /// (失败仅告警);真正的 fail-closed 校验在
    /// [`SessionRegistry::answer_question_bound`]。
    pub fn bind_pending_question(&self, run_handle: &str, question_id: i64) {
        let Some(session_handle) = self.run_sessions.lock().get(run_handle).cloned() else {
            log::warn!("question {question_id} 绑定待答槽失败:run `{run_handle}` 无会话绑定");
            return;
        };
        let Some(inner) = self.sessions.lock().get(&session_handle).cloned() else {
            log::warn!("question {question_id} 绑定待答槽失败:会话 `{session_handle}` 已摘除");
            return;
        };
        let SessionInner::Http(h) = inner else {
            return; // PTY 会话没有 ask_human 待答槽
        };
        let mut pending = h.pending.lock();
        match pending.as_mut() {
            Some(slot) if slot.run_handle == run_handle => {
                slot.question_id = Some(question_id);
            }
            Some(slot) => {
                log::warn!(
                    "question {question_id} 绑定待答槽失败:槽属于 run `{}`,不是 `{run_handle}`",
                    slot.run_handle
                );
            }
            None => {
                log::warn!(
                    "question {question_id} 绑定待答槽失败:run `{run_handle}` 当前没有待答槽"
                );
            }
        }
    }

    /// question-bound 回答投递(Issue #26 Respond 子任务):
    /// - 必须命中"该 run 当前等待的正是该 question"的待答槽;
    /// - 同 question 同答案重放幂等(账本命中,不再注入第二次输入);
    /// - 同 question 异答案、错误 run、槽位已换到新题、无存活会话
    ///   (含核心重启后注册表为空)一律稳定拒绝,绝不回退 legacy
    ///   run 级 answer。
    /// 账本只记进程内存,答案明文不写日志、不进 Snapshot/事件、不持久化。
    pub fn answer_question_bound(
        &self,
        question_id: i64,
        run_handle: &str,
        answer: &str,
    ) -> Result<()> {
        let session_handle = self
            .run_sessions
            .lock()
            .get(run_handle)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "run `{run_handle}` 没有存活会话绑定(会话已结束或核心已重启):\
                     无法证明接收者仍是 question {question_id} 的原问题,拒绝投递(fail-closed)"
                )
            })?;
        let inner = self
            .sessions
            .lock()
            .get(&session_handle)
            .cloned()
            .ok_or_else(|| anyhow!("run `{run_handle}` 绑定的会话已摘除:拒绝投递(fail-closed)"))?;
        let SessionInner::Http(h) = inner else {
            anyhow::bail!(
                "run `{run_handle}` 的会话不是 HTTP Runtime:question-bound 回答\
                 只能投递给 ask_human 待答通道"
            );
        };
        // 待答槽 + 账本在同一临界区内校验并消费:并发重放里只有一个
        // 线程能真正发送,其余按账本幂等返回或被拒绝。
        let mut pending = h.pending.lock();
        if let Some(delivered) = self.question_deliveries.lock().get(&question_id) {
            if delivered == answer {
                return Ok(()); // 同 id 同答案:幂等重放,不再注入
            }
            anyhow::bail!("question {question_id} 已投递过不同答案:拒绝冲突重放");
        }
        let slot = pending.as_ref().ok_or_else(|| {
            anyhow!(
                "run `{run_handle}` 当前没有等待中的问题:question {question_id} \
                 无法投递(fail-closed)"
            )
        })?;
        anyhow::ensure!(
            slot.run_handle == run_handle,
            "待答槽属于 run `{}`,不是 `{run_handle}`",
            slot.run_handle
        );
        anyhow::ensure!(
            slot.question_id == Some(question_id),
            "run `{run_handle}` 正在等待 question {:?}(另一次提问),不是 {question_id}:\
             拒绝把旧答案投给新问题(fail-closed)",
            slot.question_id
        );
        let tx = slot.tx.clone();
        match tx.send(answer.to_string()) {
            Ok(()) => {
                self.question_deliveries
                    .lock()
                    .insert(question_id, answer.to_string());
                *pending = None; // 待答槽已消费
                Ok(())
            }
            Err(_) => {
                // 接收端(工具循环线程)已消失:通道关闭,绝不假装投递成功
                *pending = None;
                anyhow::bail!(
                    "question {question_id} 投递失败:HTTP 工具循环已不再等待该问题(通道关闭)"
                )
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
    if spec.attach_existing_session && registry.session_alive(&spec.session_handle) {
        registry.bind_run(&spec.run_handle, &spec.session_handle);
        // 复用存活会话:直接发送提示
        if let Err(error) = registry.send_prompt(&spec.session_handle, &spec.prompt) {
            registry.unbind_run(&spec.run_handle);
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::SpawnError(format!("复用会话提示发送失败: {error:#}")),
            ));
            return;
        }
        let _ = events.send((
            spec.run_id,
            RuntimeEvent::AgentState(mf_agent::AgentState::Working),
        ));
        return;
    }
    registry.kill_session(&spec.session_handle); // 同 handle 旧会话清理

    let mut pair = match pty::openpty(pty::PtySize {
        rows: TERM_ROWS as u16,
        cols: TERM_COLS as u16,
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

    // T3a 统一脱敏入口:Preview/普通会话与另两条 launch 路径同一条
    // 管线 —— capability token 在注入环境前进入 redactor,输出先脱敏
    // 再进 Screen/output_tail(spec §8.8,消灭未脱敏旁路)。
    let mut redactor = crate::redactor::launch_redactor(Vec::new(), Some(&spec.capability_token));
    let mut cmd = SpawnCommand::new(&spec.profile.command);
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
    let child = match pair.spawn_command(&cmd) {
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
    let killer = match child.clone_killer() {
        Ok(k) => k,
        Err(e) => {
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::SpawnError(format!("克隆 kill 句柄失败: {e}")),
            ));
            return;
        }
    };
    let job = child
        .job()
        .map_err(|e| {
            log::warn!("克隆进程树守卫失败(退化为直接 kill): {e:#}");
            e
        })
        .ok();
    let pid = child.process_id();

    let session = Arc::new(PtySession {
        session_id: spec.session_id,
        handle: spec.session_handle.clone(),
        registry: std::sync::Arc::downgrade(registry),
        master: Mutex::new(Some(pair.master)),
        writer: Mutex::new(Some(writer)),
        // child 由独立 waiter 线程持有并 wait;kill 经 killer 克隆执行
        child: Mutex::new(None),
        killer: Mutex::new(Some(killer)),
        job: Mutex::new(job),
        screen: Mutex::new(Screen::new(TERM_ROWS, TERM_COLS)),
        title: Mutex::new(spec.profile.display_name.clone()),
        journal: Mutex::new(TerminalJournal::new(16 * 1024 * 1024)),
        flusher: Mutex::new(TranscriptFlusher::new(FlushPolicy::default())),
        exit_gate: Mutex::new(ExitGate::new()),
        alive: AtomicBool::new(true),
        pid: Some(pid),
        term: TermSignal::new(),
    });
    registry.note_session_root(&session.handle, &spec.project_root.clone());
    registry.register(&spec.session_handle, SessionInner::Pty(session.clone()));
    registry.bind_run(&spec.run_handle, &spec.session_handle);

    // ConPTY 在 master 存续期间不给 reader EOF(自然退出的短命 CLI 会把
    // reader 永远挂在 ReadFile 上):waiter 持有 child,wait 返回后记录
    // 退出码并关闭 master,解除 reader 阻塞 —— 会话不依赖 stop 自然收口。
    // 生命周期收口顺序:waiter wait(reap 真实 OS 进程)→ 写退出码 →
    // 关 master;reader EOF 后清资源、上报 Exited/Dead、摘除注册表;
    // stop_run 返回即代表进程已被 reap 且事件已发。
    let child_slot = Arc::new(Mutex::new(Some(child)));
    let exit_code_slot = Arc::new(Mutex::new(None::<i32>));
    // reader 完成握手:reader 在 Exited/Dead 事件**发出之后**置位;
    // waiter 关闭 master 后等它(宽限 5s,reader 缺失/卡死时兜底),
    // 再确认终止 —— stop_run 返回即代表进程已被 reap 且事件已发。
    let reader_done = Arc::new(TermSignal::new());
    let waiter_session = session.clone();
    let waiter_slot = exit_code_slot.clone();
    let waiter_child_slot = child_slot.clone();
    let waiter_reader_done = reader_done.clone();
    let waiter = std::thread::Builder::new()
        .name(format!("pty-waiter-{}", session.session_id))
        .spawn(move || {
            let mut child = waiter_child_slot.lock().take();
            let code = child
                .as_mut()
                .and_then(|c| c.wait().ok())
                .map(|s| s.exit_code() as i32);
            *waiter_slot.lock() = code;
            // 先写 slot 再关 master:reader 被解除阻塞时退出码已就绪
            waiter_session.master.lock().take();
            // 等 reader 收口(事件已发)后再确认终止(幂等,二者都会 mark)
            waiter_reader_done.wait_for(std::time::Duration::from_secs(5));
            waiter_session.mark_terminated();
        });

    // reader 线程:喂终端模拟器 + 输出缓冲 + Output 事件(节流)。
    // 任一线程启动失败 → kill 子进程(waiter 负责 reap)、注册表条目
    // 摘除,上报 SpawnError(调度方按失败结算),不留孤儿。
    let run_id = spec.run_id;
    let events_out = events.clone();
    let reader_registry = registry.clone();
    let reader_session_handle = spec.session_handle.clone();
    let reader_session = session.clone();
    let exit_code_slot_reader = exit_code_slot.clone();
    let reader_done_flag = reader_done.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("pty-reader-{}", session.session_id))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            let mut last_output_event =
                std::time::Instant::now() - std::time::Duration::from_secs(10);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // T3f 管线:redactor → journal(seq/权威) → Screen 投影
                        // → transcript flusher(§8.2);不再有 output_tail 旁路。
                        let clean = redactor.redact_chunk(&buf[..n]);
                        if !clean.is_empty() {
                            reader_session.journal.lock().append(clean.clone());
                            {
                                let mut screen = reader_session.screen.lock();
                                screen.feed(&clean);
                                if !screen.title.is_empty() {
                                    *reader_session.title.lock() = screen.title.clone();
                                }
                            }
                            let mut flusher = reader_session.flusher.lock();
                            if let Some(batch) =
                                flusher.push(reader_session.journal.lock().last_seq(), &clean)
                            {
                                drop(flusher);
                                reader_session.flush_transcript_batch(&batch);
                            }
                        }
                        if last_output_event.elapsed() >= std::time::Duration::from_millis(600) {
                            last_output_event = std::time::Instant::now();
                            let _ = events_out.send((run_id, RuntimeEvent::Output));
                        }
                    }
                }
            }
            // 流结束:flush 脱敏 carry → 最后字节分配 final seq →
            // durable-before-notify(§8.5):exit 门闩 commit 成功后才发
            // Exited 事件;持久化失败进入 TerminalFailure,不发可恢复 exit。
            let rest = redactor.finish();
            if !rest.is_empty() {
                reader_session.journal.lock().append(rest.clone());
                reader_session.screen.lock().feed(&rest);
            }
            let final_seq = reader_session.journal.lock().last_seq();
            let flusher_tail = reader_session.flusher.lock().finish();
            let code = *exit_code_slot_reader.lock();
            {
                reader_session.flush_transcript_exit(
                    flusher_tail.as_ref(),
                    final_seq,
                    code.map(i64::from),
                );
                let mut gate = reader_session.exit_gate.lock();
                gate.begin_exit(final_seq, code.map(i64::from));
                gate.commit(true);
            }
            reader_session.alive.store(false, Ordering::SeqCst);
            reader_session.writer.lock().take();
            reader_session.master.lock().take();
            let _ = events_out.send((run_id, RuntimeEvent::Exited { code }));
            let _ = events_out.send((run_id, RuntimeEvent::AgentState(mf_agent::AgentState::Dead)));
            // 进程已结束并 wait:摘除注册表条目(不 kill)
            reader_registry.kill_session(&reader_session_handle);
            // 事件已发出:唤醒 waiter 的终止确认握手(见上)
            reader_done_flag.mark();
        });
    if let Err(error) = waiter {
        // waiter 未启动:无人 reap child → reader 也不能启动,直接终止并
        // 同步收口(child kill + wait、资源清理、SpawnError)
        if let Err(reader_err) = spawned {
            // reader 也未启动:双失败一并上报
            let _ = events.send((
                spec.run_id,
                RuntimeEvent::SpawnError(format!(
                    "启动 PTY waiter/reader 线程失败: {error} / {reader_err}"
                )),
            ));
        }
        if let Some(mut child) = child_slot.lock().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut killer) = session.killer.lock().take() {
            let _ = killer.kill();
        }
        session.writer.lock().take();
        session.master.lock().take();
        session.mark_terminated(); // child 已同步 reap:终止已确认
        registry.kill_session(&spec.session_handle);
        let _ = events.send((
            spec.run_id,
            RuntimeEvent::SpawnError(format!("启动 PTY waiter 线程失败: {error}")),
        ));
        return;
    }
    if let Err(error) = spawned {
        // reader 未启动但 waiter 存活:kill 后由 waiter reap 并收口
        if let Some(mut killer) = session.killer.lock().take() {
            let _ = killer.kill();
        }
        session.master.lock().take(); // reader 不存在,直接解除阻塞语义
        registry.kill_session(&spec.session_handle);
        let _ = events.send((
            spec.run_id,
            RuntimeEvent::SpawnError(format!("启动 PTY reader 线程失败: {error}")),
        ));
        return;
    }
    // waiter/reader 线程已完全接管生命周期 → 才上报 Launched
    let _ = events.send((spec.run_id, RuntimeEvent::Launched));
    let _ = events.send((
        spec.run_id,
        RuntimeEvent::AgentState(mf_agent::AgentState::Working),
    ));
}

// ---------------- Ad-hoc CLI 会话 ----------------

/// 离散 CLI 会话启动(设计 §4.7 / §10):挂在 Task 下,
/// 无 Step / Agent Run / 结算事件流;注册表仍以持久 session handle
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
    /// 进程注册键(agent_sessions.public_handle)。
    display_session_handle: &'a str,
    session: Option<Arc<PtySession>>,
}

impl Drop for AdHocReapGuard<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            self.registry.kill_session(self.display_session_handle);
            if let Some(mut killer) = session.killer.lock().take() {
                let _ = killer.kill();
            }
            session.writer.lock().take();
            session.master.lock().take();
            session.alive.store(false, Ordering::SeqCst);
        }
    }
}

/// 离散 CLI 会话启动。输出在进入 journal/Screen 前(T3f 管线)经
/// `StreamingRedactor` 跨块脱敏;进程退出时向 Orchestrator 上报
/// `AdHocExited`(tag 为 session_id)并从注册表摘除会话。
fn launch_ad_hoc_pty(registry: &Arc<SessionRegistry>, spec: &AdHocLaunchSpec) -> Result<()> {
    // 进程注册键 = 展示会话的持久 handle；行号只作事件/UI tag。
    let display_id = spec.display_session_id;
    registry.kill_session(&spec.display_session_handle);

    if spec.plan.uses_shell {
        anyhow::bail!("离散 CLI Runtime 尚不支持 Shell 启动计划");
    }
    if spec.plan.executable.as_os_str().is_empty() {
        anyhow::bail!("离散 CLI 的可执行文件不能为空");
    }
    materialize_temp_files(&spec.run_temp, &spec.plan.temp_files)?;

    let mut pair = pty::openpty(pty::PtySize {
        rows: TERM_ROWS as u16,
        cols: TERM_COLS as u16,
    })
    .context("离散会话 openpty 失败")?;

    let mut argv = spec.plan.argv.clone();
    apply_prompt_file_argv(&mut argv, &spec.plan.input)?;
    let mut cmd = SpawnCommand::new(&spec.plan.executable);
    cmd.args(&argv);
    for (k, v) in &spec.plan.env {
        cmd.env(k, v);
    }
    // Secret 值只进入 spawn 调用的一次性 zeroize 环境块;脱敏器共享
    // zeroizing 租约(不复制明文副本),流结束即释放 —— 不随会话长期
    // 持明文。与工作流路径统一走 pty_spawn(宿主侧零普通 OsString 副本)
    // T3a 统一脱敏入口:Secret 租约(离散会话无 capability token)在
    // 进程启动前全部进入同一 redactor(spec §8.8)。
    let mut redactor = crate::redactor::launch_redactor(
        spec.plan
            .secret_env
            .iter()
            .map(|(_, l)| l.clone())
            .collect(),
        None,
    );
    for (key, lease) in &spec.plan.secret_env {
        cmd.env_secret(key, lease);
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
    let child = pair.spawn_command(&cmd).with_context(|| {
        format!(
            "离散会话启动 `{}` 失败(CLI 未安装或配置无效)",
            spec.plan.executable.display()
        )
    })?;
    let killer = child.clone_killer().context("离散会话克隆 kill 句柄失败")?;
    let job = child
        .job()
        .map_err(|e| {
            log::warn!("克隆进程树守卫失败(退化为直接 kill): {e:#}");
            e
        })
        .ok();
    let pid = child.process_id();

    let session = Arc::new(PtySession {
        session_id: spec.session_id,
        handle: spec.display_session_handle.clone(),
        registry: std::sync::Arc::downgrade(registry),
        master: Mutex::new(Some(pair.master)),
        writer: Mutex::new(Some(writer)),
        // child 由 waiter 线程持有并 wait;kill 经 killer 克隆执行
        child: Mutex::new(None),
        killer: Mutex::new(Some(killer)),
        job: Mutex::new(job),
        screen: Mutex::new(Screen::new(TERM_ROWS, TERM_COLS)),
        title: Mutex::new(spec.title.clone()),
        journal: Mutex::new(TerminalJournal::new(16 * 1024 * 1024)),
        flusher: Mutex::new(TranscriptFlusher::new(FlushPolicy::default())),
        exit_gate: Mutex::new(ExitGate::new()),
        alive: AtomicBool::new(true),
        pid: Some(pid),
        term: TermSignal::new(),
    });
    registry.note_session_root(&session.handle, &spec.workdir.clone());
    registry.register(
        &spec.display_session_handle,
        SessionInner::Pty(session.clone()),
    );
    // 看护 armed:下方任何失败路径(初始 stdin 写入、线程启动)
    // 都会 kill 并摘除注册表条目
    let mut reap = AdHocReapGuard {
        registry,
        display_session_handle: &spec.display_session_handle,
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
            // child 已 reap:确认终止(唤醒等待中的 stop_run/kill)
            waiter_session.mark_terminated();
        })
        .context("启动离散会话 waiter 线程失败")?;

    let completion = spec.plan.completion.clone();
    let events = spec.events.clone();
    let reader_display_handle = spec.display_session_handle.clone();
    let reader_session = session.clone();
    let reader_registry = registry.clone();
    let exit_code_slot_reader = exit_code_slot.clone();
    let build = std::thread::Builder::new()
        .name(format!("ad-hoc-pty-reader-{}", display_id))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // T3f 管线:redactor → journal(seq/权威) → Screen
                        // 投影 → transcript flusher;不再有 output_tail 旁路。
                        let clean = redactor.redact_chunk(&buf[..n]);
                        if !clean.is_empty() {
                            reader_session.journal.lock().append(clean.clone());
                            {
                                let mut screen = reader_session.screen.lock();
                                screen.feed(&clean);
                                if !screen.title.is_empty() {
                                    *reader_session.title.lock() = screen.title.clone();
                                }
                            }
                            let mut flusher = reader_session.flusher.lock();
                            if let Some(batch) =
                                flusher.push(reader_session.journal.lock().last_seq(), &clean)
                            {
                                drop(flusher);
                                reader_session.flush_transcript_batch(&batch);
                            }
                        }
                    }
                }
            }
            // T3f:最后字节分配 final seq;durable-before-notify ——
            // journal/flusher 收口后才发 exit 语义事件。
            let rest = redactor.finish();
            if !rest.is_empty() {
                reader_session.journal.lock().append(rest.clone());
                reader_session.screen.lock().feed(&rest);
            }
            let final_seq = reader_session.journal.lock().last_seq();
            let flusher_tail = reader_session.flusher.lock().finish();
            reader_session.alive.store(false, Ordering::SeqCst);
            let exit_code = *exit_code_slot_reader.lock();
            {
                reader_session.flush_transcript_exit(
                    flusher_tail.as_ref(),
                    final_seq,
                    exit_code.map(i64::from),
                );
                let mut gate = reader_session.exit_gate.lock();
                gate.begin_exit(final_seq, exit_code.map(i64::from));
                gate.commit(true);
            }
            reader_session.writer.lock().take();
            reader_session.master.lock().take();
            // 完成检测素材:marker 扫描(journal 尾部脱敏字节,标记不得是 Secret)
            let tail_bytes = reader_session.journal.lock().tail_bytes(OUT_BUFFER_CAP);
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
            reader_registry.kill_session(&reader_display_handle);
            // 终止确认由 waiter 负责(child.wait 完成即置位)
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
/// 进程注册在持久 session handle 下,reader 线程完全接管后才上报 Launched。
fn launch_workflow_pty(
    registry: &Arc<SessionRegistry>,
    spec: &mf_agent::runtime::WorkflowLaunchSpec,
    plan: &mf_agent::LaunchPlan,
    events: Sender<(i64, RuntimeEvent)>,
) -> Result<()> {
    // 复用存活会话:直接发送提示,不再拉起进程
    if spec.attach_existing_session && registry.session_alive(&spec.session_handle) {
        registry.bind_run(&spec.run_handle, &spec.session_handle);
        if let Err(error) = registry.send_prompt(&spec.session_handle, &spec.prompt) {
            registry.unbind_run(&spec.run_handle);
            return Err(error).context("复用工作流会话提示发送失败");
        }
        let _ = events.send((
            spec.run_id,
            RuntimeEvent::AgentState(mf_agent::AgentState::Working),
        ));
        return Ok(());
    }
    registry.kill_session(&spec.session_handle);

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

    let mut pair = pty::openpty(pty::PtySize {
        rows: TERM_ROWS as u16,
        cols: TERM_COLS as u16,
    })
    .context("工作流节点 openpty 失败")?;

    let mut argv = plan.argv.clone();
    apply_prompt_file_argv(&mut argv, &plan.input)?;
    let mut cmd = SpawnCommand::new(&plan.executable);
    cmd.args(&argv);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    // Secret 值只进入 spawn 调用的一次性 zeroize 环境块(pty_spawn 统一
    // 注入点,宿主侧不产生普通 String/OsString 副本);脱敏器共享
    // zeroizing 租约,流结束即释放 —— 不随会话长期持明文。
    // T3a 统一脱敏入口:后注入的 MF_RUN_TOKEN 必须与 Secret 同一
    // redactor 覆盖(修复 echo 泄漏窗口,spec §8.8)。
    let mut redactor = crate::redactor::launch_redactor(
        plan.secret_env.iter().map(|(_, l)| l.clone()).collect(),
        Some(&spec.capability_token),
    );
    for (key, lease) in &plan.secret_env {
        cmd.env_secret(key, lease);
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
    let child = pair.spawn_command(&cmd).with_context(|| {
        format!(
            "工作流节点启动 `{}` 失败(CLI 未安装或配置无效)",
            plan.executable.display()
        )
    })?;
    let killer = child
        .clone_killer()
        .context("工作流节点克隆 kill 句柄失败")?;
    let job = child
        .job()
        .map_err(|e| {
            log::warn!("克隆进程树守卫失败(退化为直接 kill): {e:#}");
            e
        })
        .ok();
    let pid = child.process_id();

    let session = Arc::new(PtySession {
        session_id: spec.session_id,
        handle: spec.session_handle.clone(),
        registry: std::sync::Arc::downgrade(registry),
        master: Mutex::new(Some(pair.master)),
        writer: Mutex::new(Some(writer)),
        // child 由 waiter 线程持有等待;kill 经 killer 克隆执行
        child: Mutex::new(None),
        killer: Mutex::new(Some(killer)),
        job: Mutex::new(job),
        screen: Mutex::new(Screen::new(TERM_ROWS, TERM_COLS)),
        title: Mutex::new(spec.step_title.clone()),
        journal: Mutex::new(TerminalJournal::new(16 * 1024 * 1024)),
        flusher: Mutex::new(TranscriptFlusher::new(FlushPolicy::default())),
        exit_gate: Mutex::new(ExitGate::new()),
        alive: AtomicBool::new(true),
        pid: Some(pid),
        term: TermSignal::new(),
    });
    registry.note_session_root(&session.handle, &spec.project_root.clone());
    registry.register(&spec.session_handle, SessionInner::Pty(session.clone()));
    registry.bind_run(&spec.run_handle, &spec.session_handle);
    let mut reap = AdHocReapGuard {
        registry,
        display_session_handle: &spec.session_handle,
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

    // ConPTY:waiter 拥有 child,wait 返回后关闭 master 解除 reader 阻塞;
    // 等 reader 把 Exited/Dead 事件发出(宽限 5s)后再确认终止 ——
    // stop_run 返回即代表进程已被 reap 且事件已发。
    let exit_code_slot = Arc::new(Mutex::new(None::<i32>));
    let reader_done = Arc::new(TermSignal::new());
    let waiter_session = session.clone();
    let waiter_slot = exit_code_slot.clone();
    let waiter_reader_done = reader_done.clone();
    std::thread::Builder::new()
        .name(format!("wf-pty-waiter-{}", spec.session_id))
        .spawn(move || {
            let code = child.wait().ok().map(|status| status.exit_code() as i32);
            *waiter_slot.lock() = code;
            waiter_session.master.lock().take();
            waiter_reader_done.wait_for(std::time::Duration::from_secs(5));
            // child 已 reap:确认终止(唤醒等待中的 stop_run/kill)
            waiter_session.mark_terminated();
        })
        .context("启动工作流 waiter 线程失败")?;

    let run_id = spec.run_id;
    let reader_session = session.clone();
    let reader_registry = registry.clone();
    let reader_session_handle = spec.session_handle.clone();
    let exit_code_slot_reader = exit_code_slot.clone();
    let reader_done_flag = reader_done.clone();
    let events_out = events.clone();
    let build = std::thread::Builder::new()
        .name(format!("wf-pty-reader-{}", spec.session_id))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // T3f 管线:redactor → journal(seq/权威) → Screen
                        // 投影 → transcript flusher;不再有 output_tail 旁路。
                        let clean = redactor.redact_chunk(&buf[..n]);
                        if !clean.is_empty() {
                            reader_session.journal.lock().append(clean.clone());
                            {
                                let mut screen = reader_session.screen.lock();
                                screen.feed(&clean);
                                if !screen.title.is_empty() {
                                    *reader_session.title.lock() = screen.title.clone();
                                }
                            }
                            let mut flusher = reader_session.flusher.lock();
                            if let Some(batch) =
                                flusher.push(reader_session.journal.lock().last_seq(), &clean)
                            {
                                drop(flusher);
                                reader_session.flush_transcript_batch(&batch);
                            }
                        }
                        let _ = events_out.send((run_id, RuntimeEvent::Output));
                    }
                }
            }
            // T3f:最后字节分配 final seq;durable-before-notify ——
            // journal/flusher 收口后才发 exit 语义事件。
            let rest = redactor.finish();
            if !rest.is_empty() {
                reader_session.journal.lock().append(rest.clone());
                reader_session.screen.lock().feed(&rest);
            }
            let final_seq = reader_session.journal.lock().last_seq();
            let flusher_tail = reader_session.flusher.lock().finish();
            reader_session.alive.store(false, Ordering::SeqCst);
            let exit_code = *exit_code_slot_reader.lock();
            {
                reader_session.flush_transcript_exit(
                    flusher_tail.as_ref(),
                    final_seq,
                    exit_code.map(i64::from),
                );
                let mut gate = reader_session.exit_gate.lock();
                gate.begin_exit(final_seq, exit_code.map(i64::from));
                gate.commit(true);
            }
            reader_session.writer.lock().take();
            reader_session.master.lock().take();
            let _ = events_out.send((run_id, RuntimeEvent::Exited { code: exit_code }));
            let _ = events_out.send((run_id, RuntimeEvent::AgentState(mf_agent::AgentState::Dead)));
            // 进程已结束并 wait:摘除注册表条目(不 kill)
            reader_registry.kill_session(&reader_session_handle);
            // 事件已发出:唤醒 waiter 的终止确认握手(见上)
            reader_done_flag.mark();
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
    // HTTP 会话复用:同键存活则续用 transcript(会话连续性)
    if spec.attach_existing_session && registry.session_alive(&spec.session_handle) {
        registry.bind_run(&spec.run_handle, &spec.session_handle);
        let existing = {
            let sessions = registry.sessions.lock();
            sessions.get(&spec.session_handle).cloned()
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
        pending: Mutex::new(None),
        cancel: AtomicBool::new(false),
        term: TermSignal::new(),
        join: Mutex::new(None),
    });
    registry.register(&spec.session_handle, SessionInner::Http(session.clone()));
    registry.bind_run(&spec.run_handle, &spec.session_handle);
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
    let run_handle = spec.run_handle.clone();
    let cancel = session.clone();
    let join_session = session.clone();
    let handle = std::thread::Builder::new()
        .name(format!("http-agent-{run_id}"))
        .spawn(move || {
            let outcome = run_http_turn(
                &provider,
                &instructions,
                &workdir,
                &title,
                &run_handle,
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
                    let _ = events2.send((run_id, RuntimeEvent::Exited { code: None }));
                }
            }
            // 工具循环线程真正结束(事件已上报)后才确认终止:
            // stop_run 据此等待,确认前不得释放执行租约
            session.mark_terminated();
        });
    match handle {
        Ok(handle) => {
            *join_session.join.lock() = Some(handle);
        }
        Err(error) => {
            // 后台线程启动失败:run 不能永远停在 Working —— 摘除会话/
            // 绑定、确认终止(无线程存活)、上报 SpawnError(调度方按
            // 失败结算),绝不留下"已注册但无人推进"的僵尸会话。
            log::error!("run {run_id} HTTP 工具循环线程启动失败: {error}");
            join_session.mark_terminated();
            registry.kill_session(&spec.session_handle);
            let _ = events.send((
                run_id,
                RuntimeEvent::SpawnError(format!("启动 HTTP 工具循环线程失败: {error}")),
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_http_turn(
    provider: &mf_agent::ProviderConfig,
    instructions: &str,
    workdir: &Path,
    title: &str,
    run_handle: &str,
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
        return HttpOutcome::Settled(Settlement::complete(summary));
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
                        output: Default::default(),
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
                    // 先登记带 run 身份的待答槽,再广播 Question 事件:
                    // Orchestrator 持久化 question 行后会立刻回填绑定,
                    // 此顺序保证回填一定看得见这个槽(先发事件则可能
                    // 绑定到上一题超时残留的旧槽)。替换旧槽也顺带
                    // 丢弃超时未答的历史槽,旧答案不再有可命中的通道。
                    let (tx, rx) = crossbeam_channel::bounded::<String>(1);
                    *session.pending.lock() = Some(PendingQuestion {
                        run_handle: run_handle.to_string(),
                        question_id: None,
                        tx,
                    });
                    let _ = events.send((run_id, RuntimeEvent::Question(question)));
                    // 分片可取消等待:stop(通道摘除/cancel)立即唤醒,
                    // 不再 6 小时盲等阻塞停止确认
                    match session.wait_answer(&rx, std::time::Duration::from_secs(6 * 3600)) {
                        Some(answer) => {
                            messages.push(ChatMessage::tool_result(
                                call.id.clone(),
                                format!("用户回答: {answer}"),
                            ));
                        }
                        None => {
                            messages.push(ChatMessage::tool_result(
                                call.id.clone(),
                                "用户未在时限内回答或运行已被停止".into(),
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

/// 工作流 Step 的 Secret 解封授权令牌:Store 为每次 Agent Run 签发的
/// 一次性 capability_token(随机内容,全局唯一,跨项目不碰撞)。
/// 授权在 compile_instance_launch 内 RAII 回收(authorize → 解封 →
/// revoke),不留长期有效凭据。run_id/node_key 是项目内数据库行号,
/// 不得作为全局授权键。
fn workflow_secret_run_token(spec: &mf_agent::runtime::WorkflowLaunchSpec) -> String {
    spec.capability_token.clone()
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
        // 真实生产链:冻结 Agent Instance → Agent Adapter → LaunchPlan → PTY。
        // Adapter 按 Revision 冻结的插件包 pin 解析(不随插件更新漂移)。
        // external_config 取冻结快照(default-cli 节点只读外部配置,
        // 全局默认设置后续变化不影响已冻结运行)。
        let run_token = workflow_secret_run_token(&spec);
        let plan = mf_plugins::adapter_launch::compile_instance_launch(
            &launcher.plugins,
            &launcher.catalog,
            &spec.instance,
            spec.plugin.as_ref(),
            spec.run_temp.clone(),
            spec.workdir.clone(),
            Some(spec.prompt.clone()),
            &run_token,
            spec.instance.external_config,
            launcher.secret_master_key,
        )?
        .into_plan();
        launch_workflow_pty(&self.registry, &spec, &plan, events)
    }

    fn launch_ad_hoc(&self, spec: AdHocLaunchSpec) -> Result<()> {
        launch_ad_hoc_pty(&self.registry, &spec)
    }

    fn kill_ad_hoc(&self, display_session_handle: &str) {
        self.registry.kill_session(display_session_handle);
    }

    fn send_prompt(&self, run_handle: &str, session_handle: &str, text: &str) -> Result<()> {
        self.registry
            .send_prompt_for_run(run_handle, session_handle, text)
    }

    fn stop_run(&self, run_handle: &str) -> Result<()> {
        // run 级停止 = 真终止 run 绑定的会话进程并等待真实终止确认
        // (child 已 reap、生命周期已收口);Err = 未确认,调用方不得
        // 标记 Cancelled / 释放执行租约(隔离目录可能仍被写)
        self.registry.stop_run(run_handle)
    }

    fn kill_session(&self, session_handle: &str) {
        self.registry.kill_session(session_handle);
    }

    fn answer_question(&self, run_handle: &str, answer: &str) {
        if let Some(session_handle) = self.registry.session_of_run(run_handle) {
            self.registry.http_answer(&session_handle, answer);
        }
    }

    fn bind_open_question(&self, run_handle: &str, question_id: i64) {
        self.registry.bind_pending_question(run_handle, question_id);
    }

    fn supports_question_bound_answers(&self) -> bool {
        // SessionRegistry 为 HTTP Runtime 维护 pending-question 身份
        // (run + question 双绑定)与投递账本,可证明"当前等待的正是
        // 该 question";PTY 会话没有 ask_human 通道,投递路径会拒绝。
        true
    }

    fn answer_question_bound(
        &self,
        question_id: i64,
        run_handle: &str,
        answer: &str,
    ) -> Result<()> {
        self.registry
            .answer_question_bound(question_id, run_handle, answer)
    }

    fn is_session_alive(&self, session_handle: &str) -> bool {
        // 重启恢复探测:注册表内仍然存活的会话(含离散 CLI 的展示会话)
        // 保持原状态,不得推断为中断
        self.registry.session_alive(session_handle)
    }
}

/// 保持唤醒(Windows SetThreadExecutionState)。
pub struct KeepAwake {
    active: AtomicBool,
    working: AtomicBool,
    enabled: AtomicBool,
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
            display_session_handle: "session-display-800".into(),
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
            registry
                .snapshot(&spec.display_session_handle, 800)
                .is_none(),
            "失败路径不得留下注册表条目(display 键)"
        );
        assert!(
            registry
                .snapshot("ad-hoc-row-is-not-a-handle", 700)
                .is_none(),
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
            registry
                .snapshot(&spec.display_session_handle, 800)
                .is_some(),
            "启动后应能以 display ID 快照"
        );
        assert!(
            registry
                .snapshot("ad-hoc-row-is-not-a-handle", 700)
                .is_none(),
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
            registry
                .snapshot(&spec.display_session_handle, 800)
                .is_some()
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
            if let Some(snapshot) = registry.snapshot(&spec.display_session_handle, 800) {
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
            registry
                .snapshot(&spec.display_session_handle, 800)
                .is_some(),
            "pause 会话应保持存活(display 键)"
        );
        registry.kill_session(&spec.display_session_handle);
    }
}

impl KeepAwake {
    pub fn new() -> KeepAwake {
        KeepAwake {
            active: AtomicBool::new(false),
            working: AtomicBool::new(false),
            enabled: AtomicBool::new(true),
        }
    }
    pub fn set_working(&self, working: bool) {
        self.working.store(working, Ordering::SeqCst);
        self.apply();
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        self.apply();
    }

    fn apply(&self) {
        let active = self.working.load(Ordering::SeqCst) && self.enabled.load(Ordering::SeqCst);
        if active == self.active.load(Ordering::SeqCst) {
            return;
        }
        self.active.store(active, Ordering::SeqCst);
        #[cfg(windows)]
        unsafe {
            const ES_CONTINUOUS: u32 = 0x80000000;
            const ES_SYSTEM_REQUIRED: u32 = 0x00000001;
            const ES_DISPLAY_REQUIRED: u32 = 0x00000002;
            let flags = if active {
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
    fn keep_awake_global_policy_can_suppress_working_state() {
        let keep_awake = KeepAwake::new();
        keep_awake.set_enabled(false);
        keep_awake.set_working(true);
        assert!(!keep_awake.active.load(Ordering::SeqCst));
        keep_awake.set_working(false);
    }

    #[test]
    fn ad_hoc_and_display_ids_are_distinct_namespaces() {
        // ad_hoc_sessions 与 agent_sessions 是两套自增行号:
        // 进程路由只认 display session handle;ad-hoc 行号仅作事件 tag。
        let registry = SessionRegistry::new(mf_agent::Config::default());
        assert!(registry.snapshot("session-handle", 7).is_none());
        assert!(registry.send_prompt("session-handle", "hi").is_err());
        registry.kill_session("session-handle"); // 不存在时是 no-op
    }

    #[test]
    fn registry_routes_same_rowid_sessions_by_persistent_handle() {
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let session = |id| {
            Arc::new(HttpSession {
                session_id: id,
                transcript: Mutex::new(Vec::new()),
                alive: AtomicBool::new(true),
                pending: Mutex::new(None),
                cancel: AtomicBool::new(false),
                term: TermSignal::new(),
                join: Mutex::new(None),
            })
        };
        registry.register("019-handle-a", SessionInner::Http(session(7)));
        registry.register("019-handle-b", SessionInner::Http(session(7)));
        registry.bind_run("019-run-a", "019-handle-a");
        registry.bind_run("019-run-b", "019-handle-b");
        assert_eq!(registry.binding_count(), 2);

        assert!(registry.snapshot("019-handle-a", 7).is_some());
        assert!(registry.snapshot("019-handle-b", 7).is_some());
        registry.kill_session("019-handle-a");
        assert!(!registry.session_alive("019-handle-a"));
        assert_eq!(registry.binding_count(), 1, "摘除会话必须清掉反向 Run 绑定");
        assert!(
            registry.session_alive("019-handle-b"),
            "相同行号的另一持久 handle 不得被串杀"
        );
        registry.kill_session("019-handle-b");
        assert_eq!(registry.binding_count(), 0);
    }

    #[test]
    fn reused_session_binds_new_run_handle_before_returning() {
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let mut pair = pty::openpty(pty::PtySize {
            rows: TERM_ROWS as u16,
            cols: TERM_COLS as u16,
        })
        .unwrap();
        let writer = pair.master.take_writer().unwrap();
        let session = Arc::new(PtySession {
            session_id: 7,
            handle: String::new(),
            registry: std::sync::Weak::new(),
            master: Mutex::new(Some(pair.master)),
            writer: Mutex::new(Some(writer)),
            child: Mutex::new(None),
            killer: Mutex::new(None),
            job: Mutex::new(None),
            screen: Mutex::new(Screen::new(TERM_ROWS, TERM_COLS)),
            title: Mutex::new("reuse".into()),
            journal: Mutex::new(TerminalJournal::new(16 * 1024 * 1024)),
            flusher: Mutex::new(TranscriptFlusher::new(FlushPolicy::default())),
            exit_gate: Mutex::new(ExitGate::new()),
            alive: AtomicBool::new(true),
            pid: None,
            term: TermSignal::new(),
        });
        registry.register("session-reused", SessionInner::Pty(session.clone()));
        registry.bind_run("run-old", "session-reused");
        let spec = LaunchSpec {
            project_root: std::env::temp_dir(),
            run_id: 2,
            run_handle: "run-new".into(),
            step_id: 1,
            task_id: 1,
            session_id: 7,
            session_handle: "session-reused".into(),
            session_key: Some("reuse".into()),
            attach_existing_session: true,
            profile: AgentProfileSpec {
                id: "unused".into(),
                display_name: "unused".into(),
                runtime: RuntimeKind::Pty,
                command: "unused".into(),
                args: Vec::new(),
                env: Vec::new(),
                permission_args: Vec::new(),
                provider: None,
                icon: None,
                homepage: None,
                hook: None,
            },
            step_title: "reuse".into(),
            prompt: "continue".into(),
            capability_token: "token".into(),
            pipe_name: "pipe".into(),
            mfctl_hint: None,
            workdir: std::env::temp_dir(),
        };
        let (events, _rx) = crossbeam_channel::bounded(8);
        launch_pty(&registry, &spec, events);
        assert_eq!(
            registry.session_of_run("run-new").as_deref(),
            Some("session-reused")
        );

        session.mark_terminated();
        registry.stop_run("run-new").unwrap();
        assert!(!registry.session_alive("session-reused"));
        assert_eq!(registry.binding_count(), 0);
    }

    fn workflow_spec(
        run_id: i64,
        node_key: &str,
        capability_token: &str,
    ) -> mf_agent::runtime::WorkflowLaunchSpec {
        use mf_agent::runtime::WorkflowLaunchSpec;
        WorkflowLaunchSpec {
            project_root: std::env::temp_dir(),
            run_id,
            run_handle: format!("run-{run_id}"),
            step_id: 1,
            task_id: 1,
            session_id: 1,
            session_handle: format!("session-{run_id}"),
            session_key: None,
            attach_existing_session: false,
            node_key: node_key.into(),
            step_title: "t".into(),
            instance: mf_agent::AgentInstanceSnapshot {
                id: "inst".into(),
                name: "inst".into(),
                agent_type: "generic-command".into(),
                version: 1,
                enabled: true,
                run_mode: mf_agent::RunMode::OneShot,
                executable: "x.exe".into(),
                argv: vec![],
                env: vec![],
                config: serde_json::json!({}),
                execution_contract: serde_json::json!({}),
                sealed_secret_ids: vec![],
                external_config: false,
            },
            plugin: None,
            prompt: String::new(),
            capability_token: capability_token.into(),
            pipe_name: "pipe".into(),
            mfctl_hint: None,
            workdir: std::env::temp_dir(),
            run_temp: std::env::temp_dir(),
        }
    }

    #[test]
    fn workflow_secret_token_is_globally_unique_across_projects() {
        // 同一 run_id + node_key 属于不同项目(各自数据库行号):
        // 授权令牌不得相同(跨项目/全局授权碰撞会把 A 项目的 Secret
        // 解封授权泄露给 B 项目);令牌应使用 Store 签发的全局唯一
        // capability token,而不是 step:{run_id}:{node_key}
        let a = workflow_spec(1, "x", "mft_aaaaaaaaaaaaaaaa");
        let b = workflow_spec(1, "x", "mft_bbbbbbbbbbbbbbbb");
        let token_a = workflow_secret_run_token(&a);
        let token_b = workflow_secret_run_token(&b);
        assert_ne!(token_a, token_b, "跨项目同 run/节点必须得到不同授权令牌");
        assert!(
            !token_a.starts_with("step:"),
            "项目内键(step:run:node)不得作为全局授权令牌: {token_a}"
        );
    }

    /// OS 进程表中是否存在该 PID(tasklist;非 Windows 环境返回 false)。
    fn tasklist_has_pid(pid: u32) -> bool {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
            .unwrap_or(false)
    }

    #[test]
    fn stop_run_confirms_real_os_process_termination_by_pid() {
        // stop_run 返回 = 真实 OS 进程终止已确认:
        // 1) 退出事件已上报(reader/waiter 已 child.wait 完成 reap);
        // 2) OS 进程表里该 PID 已消失(kill 后不得立刻谎报 alive=false)。
        if !cfg!(windows) {
            return;
        }
        let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let host = RuntimeHostImpl::new(registry.clone());
        let (events, rx) = crossbeam_channel::bounded(64);
        let workdir = std::env::temp_dir();
        let spec = LaunchSpec {
            project_root: workdir.clone(),
            run_id: 77,
            run_handle: "run-77".into(),
            step_id: 1,
            task_id: 1,
            session_id: 9,
            session_handle: "session-9".into(),
            session_key: None,
            attach_existing_session: false,
            profile: AgentProfileSpec {
                id: "cmd".into(),
                display_name: "cmd".into(),
                runtime: RuntimeKind::Pty,
                command: cmd,
                args: vec!["/K".into()], // 常驻等待停止
                env: vec![],
                permission_args: vec![],
                provider: None,
                icon: None,
                homepage: None,
                hook: None,
            },
            step_title: "t".into(),
            prompt: String::new(),
            capability_token: "tok".into(),
            pipe_name: "pipe".into(),
            mfctl_hint: None,
            workdir: workdir.clone(),
        };
        host.launch(spec, events);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if registry.session_alive("session-9") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        assert!(registry.session_alive("session-9"), "前置:PTY 会话应存活");
        let pid = registry
            .session_pid("session-9")
            .expect("PTY 会话必须可观测 OS PID(终止验证的前提)");
        assert!(tasklist_has_pid(pid), "前置:PID {pid} 应存在于 OS 进程表");

        host.stop_run("run-77")
            .expect("stop_run 必须在真实终止确认后成功返回");
        // 返回即确认:退出事件已送达(不等待、不轮询)
        let mut saw_exited = false;
        while let Ok((_, ev)) = rx.try_recv() {
            if matches!(ev, RuntimeEvent::Exited { .. }) {
                saw_exited = true;
            }
        }
        assert!(
            saw_exited,
            "stop_run 返回时进程生命周期必须已收口(Exited 已上报)"
        );
        assert!(
            !tasklist_has_pid(pid),
            "stop_run 返回后 OS 进程必须真正终止(PID {pid} 仍可见)"
        );
        // 二次停止幂等(无绑定会话 → 视为已停止)
        host.stop_run("run-77").unwrap();
    }

    #[test]
    fn stop_run_terminates_bound_session_process_and_waits() {
        // run 级停止必须真终止 run→session 进程并等待停止确认:
        // 返回时注册表条目已移除、存活探测为假(不能是 no-op)
        let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let host = RuntimeHostImpl::new(registry.clone());
        let (events, _rx) = crossbeam_channel::bounded(16);
        let workdir = std::env::temp_dir();
        let spec = LaunchSpec {
            project_root: workdir.clone(),
            run_id: 42,
            run_handle: "run-42".into(),
            step_id: 1,
            task_id: 1,
            session_id: 7,
            session_handle: "session-7".into(),
            session_key: None,
            attach_existing_session: false,
            profile: AgentProfileSpec {
                id: "cmd".into(),
                display_name: "cmd".into(),
                runtime: RuntimeKind::Pty,
                command: cmd,
                args: vec!["/K".into()], // /K 保持存活等待停止
                env: vec![],
                permission_args: vec![],
                provider: None,
                icon: None,
                homepage: None,
                hook: None,
            },
            step_title: "t".into(),
            prompt: String::new(),
            capability_token: "tok".into(),
            pipe_name: "pipe".into(),
            mfctl_hint: None,
            workdir: workdir.clone(),
        };
        host.launch(spec, events);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if registry.session_alive("session-7") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        assert!(
            registry.session_alive("session-7"),
            "前置:PTY 会话应存活等待停止"
        );

        let started = std::time::Instant::now();
        host.stop_run("run-42").unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "stop_run 应在时限内同步返回"
        );
        assert!(
            !registry.session_alive("session-7"),
            "stop_run 返回后会话进程必须已停止"
        );
        assert!(registry.snapshot("session-7", 7).is_none());
        // 二次停止幂等
        host.stop_run("run-42").unwrap();
    }

    /// C8:stop 超时返回 Err 后**进程树守卫必须仍留在会话里** ——
    /// 终止/等待用 try_clone 的克隆执行,原 guard 只有在停止整体确认后
    /// 才消费;第二次 stop 重试必须仍能整组终止并成功。
    ///(确定性场景:空 Job Object 守卫 + 永不置位的终止确认 →
    /// stop_run 必然走超时 Err 分支。)
    #[test]
    fn stop_run_timeout_keeps_job_guard_for_retry() {
        if !cfg!(windows) {
            return; // Unix 守卫为 pgid 值类型,空构造无意义;语义同源
        }
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let host = RuntimeHostImpl::new(registry.clone());
        let session_handle = "session-55";
        let run_handle = "run-4242";
        let session = Arc::new(PtySession {
            session_id: 55,
            handle: String::new(),
            registry: std::sync::Weak::new(),
            master: Mutex::new(None),
            writer: Mutex::new(None),
            child: Mutex::new(None),
            killer: Mutex::new(None),
            job: Mutex::new(Some(pty::JobGuard::create().unwrap())),
            screen: Mutex::new(Screen::new(TERM_ROWS, TERM_COLS)),
            title: Mutex::new("f8".into()),
            journal: Mutex::new(TerminalJournal::new(16 * 1024 * 1024)),
            flusher: Mutex::new(TranscriptFlusher::new(FlushPolicy::default())),
            exit_gate: Mutex::new(ExitGate::new()),
            alive: AtomicBool::new(true),
            pid: None,
            term: TermSignal::new(),
        });
        registry.register(session_handle, SessionInner::Pty(session.clone()));
        registry.bind_run(run_handle, session_handle);

        // 极短确认超时:终止确认永不置位 → stop_run 必然 Err
        registry.set_stop_confirm_timeout(std::time::Duration::from_millis(1));
        let first = host.stop_run(run_handle);
        assert!(first.is_err(), "前置:确认超时必须返回 Err({first:?})");
        // 守卫必须在会话里保留(重试仍能整组杀);会话/绑定保留
        assert!(
            session.job.lock().is_some(),
            "stop 超时后进程树守卫必须留在会话(第二次重试仍能整组杀)"
        );
        assert!(registry.session_alive(session_handle), "超时后会话保留");

        // 第二次重试:守卫仍在 → 仍可整组终止;确认到达后成功收口
        session.mark_terminated();
        registry.set_stop_confirm_timeout(std::time::Duration::from_secs(10));
        host.stop_run(run_handle)
            .expect("第二次 stop 必须用保留的守卫完成整组终止并确认");
        assert!(
            !registry.session_alive(session_handle),
            "重试后会话必须真正收口"
        );
        assert!(session.job.lock().is_none(), "确认停止后守卫才被消费");
    }

    /// C2:普通 launch_pty 会话的短命 CLI 自然退出 —— 不调用 stop_run,
    /// reader/wait 循环必须自然收口:Exited 携带真实退出码、
    /// 注册表条目移除、session_alive=false。
    #[test]
    fn pty_natural_exit_short_lived_cli_ends_session_without_stop() {
        let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let host = RuntimeHostImpl::new(registry.clone());
        let (events, rx) = crossbeam_channel::bounded(64);
        let workdir = std::env::temp_dir();
        let spec = LaunchSpec {
            project_root: workdir.clone(),
            run_id: 88,
            run_handle: "run-88".into(),
            step_id: 1,
            task_id: 1,
            session_id: 21,
            session_handle: "session-21".into(),
            session_key: None,
            attach_existing_session: false,
            profile: AgentProfileSpec {
                id: "cmd".into(),
                display_name: "cmd".into(),
                runtime: RuntimeKind::Pty,
                command: cmd,
                args: vec!["/C".into(), "echo hi&&exit 7".into()], // 输出后退出
                env: vec![],
                permission_args: vec![],
                provider: None,
                icon: None,
                homepage: None,
                hook: None,
            },
            step_title: "t".into(),
            prompt: String::new(),
            capability_token: "tok".into(),
            pipe_name: "pipe".into(),
            mfctl_hint: None,
            workdir: workdir.clone(),
        };
        host.launch(spec, events);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut exit_code = None;
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok((_, RuntimeEvent::Exited { code })) => {
                    exit_code = code;
                    break;
                }
                Ok(_) => {}
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
        assert_eq!(
            exit_code,
            Some(7),
            "短命 CLI 自然退出必须上报 Exited(码 7),不依赖 stop_run"
        );
        // 注册表条目移除(session_alive 由条目存在性判定)
        let removed = (0..150).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            !registry.session_alive("session-21")
        });
        assert!(
            removed,
            "自然退出后注册表条目必须移除、alive=false(否则资源泄漏)"
        );
    }

    #[test]
    fn http_stop_waits_for_background_loop_real_end() {
        // I6:HTTP stop 不能只设 cancel/alive 就返回 Ok —— 必须持有
        // 终止确认并等待工具循环线程真正结束(事件已上报)。
        // 后台线程 600ms 后才结束:stop_run 必须等满且返回 Ok。
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let session = Arc::new(HttpSession {
            session_id: 11,
            transcript: Mutex::new(Vec::new()),
            alive: AtomicBool::new(true),
            pending: Mutex::new(None),
            cancel: AtomicBool::new(false),
            term: TermSignal::new(),
            join: Mutex::new(None),
        });
        registry.register("session-11", SessionInner::Http(session.clone()));
        registry.bind_run("run-501", "session-11");
        let started = std::time::Instant::now();
        let s2 = session.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(600));
            s2.mark_terminated(); // 循环线程真正结束
        });
        registry
            .stop_run("run-501")
            .expect("循环线程结束后必须确认停止");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(550),
            "stop_run 必须等待循环线程真正结束,实际 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn http_stop_unconfirmed_when_loop_never_ends() {
        // 循环线程永不结束:stop_run 必须返回 Err(调用方不释放租约、
        // 转 Interrupted 人工处理),不得谎报已停止。
        let registry = SessionRegistry::new(mf_agent::Config::default());
        registry.set_stop_confirm_timeout(std::time::Duration::from_millis(250));
        let session = Arc::new(HttpSession {
            session_id: 12,
            transcript: Mutex::new(Vec::new()),
            alive: AtomicBool::new(true),
            pending: Mutex::new(None),
            cancel: AtomicBool::new(false),
            term: TermSignal::new(),
            join: Mutex::new(None),
        });
        registry.register("session-12", SessionInner::Http(session.clone()));
        registry.bind_run("run-502", "session-12");
        let err = registry.stop_run("run-502").expect_err("未确认必须 Err");
        assert!(format!("{err:#}").contains("HTTP 工具循环"), "{err:#}");
        assert!(session.cancel.load(Ordering::SeqCst), "终止信号必须已发出");
    }

    /// I10:HTTP stop 超时不得先删 binding/session —— 第一次超时后
    /// 会话仍可重试停止;确认 terminated 后第二次成功并移除。
    #[test]
    fn http_stop_timeout_keeps_session_and_binding_for_retry() {
        let registry = SessionRegistry::new(mf_agent::Config::default());
        registry.set_stop_confirm_timeout(std::time::Duration::from_millis(200));
        let session = Arc::new(HttpSession {
            session_id: 15,
            transcript: Mutex::new(Vec::new()),
            alive: AtomicBool::new(true),
            pending: Mutex::new(None),
            cancel: AtomicBool::new(false),
            term: TermSignal::new(),
            join: Mutex::new(None),
        });
        registry.register("session-15", SessionInner::Http(session.clone()));
        registry.bind_run("run-504", "session-15");

        // 第一次:工具循环未结束 → Err
        let err = registry.stop_run("run-504").expect_err("未确认必须 Err");
        assert!(format!("{err:#}").contains("HTTP 工具循环"), "{err:#}");
        assert!(
            registry.session_alive("session-15"),
            "超时后 session/binding 必须保留(否则永远无法重试停止)"
        );

        // 循环真正结束(join 句柄已终结)
        let finished = std::thread::spawn(|| {});
        *session.join.lock() = Some(finished);
        session.mark_terminated();

        // 第二次:确认终止 → Ok 且移除
        registry.stop_run("run-504").expect("重试停止必须成功");
        assert!(
            !registry.session_alive("session-15"),
            "确认终止后 session 必须移除"
        );
    }

    #[test]
    fn ask_human_wait_wakes_on_stop() {
        // ask_human 的 6 小时盲等必须可被 stop 唤醒:
        // 通道摘除(Disconnected)+ cancel → 秒级返回。
        let session = Arc::new(HttpSession {
            session_id: 13,
            transcript: Mutex::new(Vec::new()),
            alive: AtomicBool::new(true),
            pending: Mutex::new(None),
            cancel: AtomicBool::new(false),
            term: TermSignal::new(),
            join: Mutex::new(None),
        });
        let (tx, rx) = crossbeam_channel::bounded::<String>(1);
        *session.pending.lock() = Some(PendingQuestion {
            run_handle: "run-wait".into(),
            question_id: Some(1),
            tx,
        });
        let s2 = session.clone();
        let waiter = std::thread::spawn(move || {
            s2.wait_answer(&rx, std::time::Duration::from_secs(6 * 3600))
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        let started = std::time::Instant::now();
        session.cancel.store(true, Ordering::SeqCst);
        session.pending.lock().take(); // stop_run 的唤醒动作
        let answer = waiter.join().expect("等待必须被唤醒并返回");
        assert!(answer.is_none(), "停止后不得返回用户回答");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "唤醒必须秒级,实际 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn http_mock_turn_stop_confirms_after_thread_exit() {
        // 端到端:Mock provider 轮次运行中发起 stop —— cancel 置位后
        // 线程自然收尾(Mock 不感知 cancel),stop_run 必须等到线程
        // **真正退出**(Settled 事件已上报)才返回 Ok。
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let host = RuntimeHostImpl::new(registry.clone());
        let (events, rx) = crossbeam_channel::bounded(64);
        let workdir = std::env::temp_dir();
        let spec = LaunchSpec {
            project_root: workdir.clone(),
            run_id: 601,
            run_handle: "run-601".into(),
            step_id: 1,
            task_id: 1,
            session_id: 21,
            session_handle: "session-601".into(),
            session_key: None,
            attach_existing_session: false,
            profile: AgentProfileSpec {
                id: "mock".into(),
                display_name: "mock".into(),
                runtime: RuntimeKind::Http,
                command: String::new(),
                args: vec![],
                env: vec![],
                permission_args: vec![],
                provider: Some(mf_agent::ProviderConfig {
                    kind: mf_agent::ProviderKind::Mock,
                    base_url: String::new(),
                    api_key: String::new(),
                    model: String::new(),
                }),
                icon: None,
                homepage: None,
                hook: None,
            },
            step_title: "mock 步骤".into(),
            prompt: "MOCK_FAIL".into(), // Mock 睡 300ms 后结算失败
            capability_token: "tok".into(),
            pipe_name: "pipe".into(),
            mfctl_hint: None,
            workdir: workdir.clone(),
        };
        host.launch(spec, events);
        // 等 Mock 轮次线程进入执行(300ms 睡眠窗口内发起 stop)
        std::thread::sleep(std::time::Duration::from_millis(50));
        let started = std::time::Instant::now();
        host.stop_run("run-601")
            .expect("线程真正结束后必须确认停止");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(150),
            "stop 必须等到线程退出(≥ Mock 剩余睡眠),实际 {:?}",
            started.elapsed()
        );
        let mut saw_settled = false;
        while let Ok((_, ev)) = rx.try_recv() {
            if matches!(ev, RuntimeEvent::Settled(_)) {
                saw_settled = true;
            }
        }
        assert!(
            saw_settled,
            "stop 返回时工具循环线程必须已收口(Settled 事件已上报)"
        );
    }

    #[test]
    #[cfg(windows)]
    fn stop_run_terminates_entire_process_tree_including_grandchildren() {
        // C5 回归:cmd /c "ping … > growing.txt" 直接子 cmd.exe 派生孙进程
        // ping.exe 持续写执行目录。停止必须拥有并等待**整棵进程树**:
        // 父+孙进程都消失、文件停止增长,stop_run 才允许返回 Ok
        //(之后才能安全 release/delete 执行租约)。
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("wt");
        std::fs::create_dir_all(&workdir).unwrap();
        let growing = workdir.join("growing.txt");
        let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let host = RuntimeHostImpl::new(registry.clone());
        let (events, _rx) = crossbeam_channel::bounded(64);
        let spec = LaunchSpec {
            project_root: workdir.clone(),
            run_id: 4242,
            run_handle: "run-4242".into(),
            step_id: 1,
            task_id: 1,
            session_id: 77,
            session_handle: "session-77".into(),
            session_key: None,
            attach_existing_session: false,
            profile: AgentProfileSpec {
                id: "cmd".into(),
                display_name: "cmd".into(),
                runtime: RuntimeKind::Pty,
                command: cmd,
                // /c 单命令:cmd.exe 直接 CreateProcess ping.exe(孙进程),
                // 输出重定向持续写 worktree 内文件(相对路径:cwd 即 workdir,
                // 避免引号/空格路径干扰 cmd 的重定向解析)
                args: vec!["/c".into(), "ping -n 600 127.0.0.1 > growing.txt".into()],
                env: vec![],
                permission_args: vec![],
                provider: None,
                icon: None,
                homepage: None,
                hook: None,
            },
            step_title: "t".into(),
            prompt: String::new(),
            capability_token: "tok".into(),
            pipe_name: "pipe".into(),
            mfctl_hint: None,
            workdir: workdir.clone(),
        };
        host.launch(spec, events);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if registry.session_alive("session-77")
                && growing.is_file()
                && std::fs::metadata(&growing)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            growing.is_file() && std::fs::metadata(&growing).unwrap().len() > 0,
            "前置:孙进程(ping)应已在执行目录持续写文件"
        );
        let pid = registry
            .session_pid("session-77")
            .expect("PTY 会话必须可观测 OS PID");
        // 前置:写入正在进行(文件仍在增长 → 孙进程活着)
        let size_a = std::fs::metadata(&growing).unwrap().len();
        // ping 约每秒写一行:窗口放宽到覆盖多次回复
        std::thread::sleep(std::time::Duration::from_millis(2600));
        let size_b = std::fs::metadata(&growing).unwrap().len();
        assert!(
            size_b > size_a,
            "前置:孙进程持续写入(size {size_a}→{size_b})"
        );

        host.stop_run("run-4242")
            .expect("整棵进程树终止并确认后 stop_run 才返回 Ok");

        // 父进程消失
        assert!(
            !tasklist_has_pid(pid),
            "直接子进程(cmd.exe,PID {pid})必须消失"
        );
        // 孙进程消失:进程表无 PING.EXE
        let ping_alive = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq PING.EXE", "/FO", "CSV", "/NH"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .to_uppercase()
                    .contains("PING.EXE")
            })
            .unwrap_or(false);
        assert!(!ping_alive, "孙进程(ping.exe)必须随进程树终止");
        // 文件停止增长(写者已死,即使进程表查询受环境干扰这也是硬证据)
        let size_c = std::fs::metadata(&growing).unwrap().len();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let size_d = std::fs::metadata(&growing).unwrap().len();
        assert_eq!(
            size_c, size_d,
            "孙进程死后执行目录文件必须停止增长(size {size_c}→{size_d})"
        );
    }
}

/// Issue #26 Respond 子任务:question-bound nonce 与幂等投递的契约测试。
/// 全部走 `RuntimeHost` trait 入口,不触碰注册表私有路径。
#[cfg(test)]
mod question_bound_answer_tests {
    use super::*;

    type AnswerRx = crossbeam_channel::Receiver<String>;

    /// 构造注册表 + 一个带待答槽的 HTTP 会话(run↔session 已绑定,
    /// 槽位身份为 `(run_handle, question_id)`),返回投递接收端。
    fn registry_with_pending(
        run_handle: &str,
        question_id: Option<i64>,
    ) -> (Arc<SessionRegistry>, Arc<RuntimeHostImpl>, AnswerRx) {
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let (tx, rx) = crossbeam_channel::bounded::<String>(1);
        let session = Arc::new(HttpSession {
            session_id: 1,
            transcript: Mutex::new(Vec::new()),
            alive: AtomicBool::new(true),
            pending: Mutex::new(Some(PendingQuestion {
                run_handle: run_handle.to_string(),
                question_id,
                tx,
            })),
            cancel: AtomicBool::new(false),
            term: TermSignal::new(),
            join: Mutex::new(None),
        });
        let session_handle = format!("session-{run_handle}");
        registry.register(&session_handle, SessionInner::Http(session));
        registry.bind_run(run_handle, &session_handle);
        let host = RuntimeHostImpl::new(registry.clone());
        (registry, host, rx)
    }

    fn replace_pending(
        registry: &SessionRegistry,
        session_handle: &str,
        run_handle: &str,
        question_id: Option<i64>,
    ) -> AnswerRx {
        let (tx, rx) = crossbeam_channel::bounded::<String>(1);
        let inner = registry
            .sessions
            .lock()
            .get(session_handle)
            .cloned()
            .expect("会话必须存在");
        let SessionInner::Http(h) = inner else {
            panic!("必须是 HTTP 会话");
        };
        *h.pending.lock() = Some(PendingQuestion {
            run_handle: run_handle.to_string(),
            question_id,
            tx,
        });
        rx
    }

    fn assert_empty(rx: &AnswerRx, what: &str) {
        match rx.recv_timeout(std::time::Duration::from_millis(120)) {
            Ok(leaked) => panic!("{what}不应收到输入,却得到 `{leaked}`"),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {}
        }
    }

    #[test]
    fn host_supports_question_bound_answers() {
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let host = RuntimeHostImpl::new(registry);
        assert!(host.supports_question_bound_answers());
    }

    #[test]
    fn same_question_same_answer_replays_without_second_input() {
        // q1 同答案重放一次:第一次真正投递,重放幂等 Ok 且不再注入。
        let (_registry, host, rx) = registry_with_pending("run-q1", Some(11));
        host.answer_question_bound(11, "run-q1", "yes")
            .expect("首次投递必须成功");
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            "yes"
        );
        for _ in 0..2 {
            host.answer_question_bound(11, "run-q1", "yes")
                .expect("同 id 同答案重放必须幂等 Ok");
        }
        assert_empty(&rx, "重放不得产生第二次输入");
    }

    #[test]
    fn same_question_conflicting_answer_is_stably_rejected() {
        let (_registry, host, rx) = registry_with_pending("run-q1", Some(11));
        host.answer_question_bound(11, "run-q1", "yes").unwrap();
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            "yes"
        );
        for _ in 0..2 {
            let err = host
                .answer_question_bound(11, "run-q1", "no")
                .expect_err("同 id 异答案必须稳定拒绝");
            assert!(format!("{err:#}").contains("冲突"), "{err:#}");
        }
        assert_empty(&rx, "冲突答案不得注入");
    }

    #[test]
    fn q1_action_never_lands_on_q2_slot() {
        // q1 从未投递、槽位已被 q2 替换(超时后 runtime 发起下一题):
        // 旧 action 必须被拒绝,q2 通道不得被污染;q2 自己的投递不受影响。
        let (registry, host, _rx1) = registry_with_pending("run-q", Some(1));
        let rx2 = replace_pending(&registry, "session-run-q", "run-q", Some(2));
        let err = host
            .answer_question_bound(1, "run-q", "yes")
            .expect_err("q1 action 不得投给 q2");
        assert!(
            format!("{err:#}").contains("拒绝把旧答案投给新问题"),
            "{err:#}"
        );
        assert_empty(&rx2, "q2 通道不得被 q1 的答案污染");
        host.answer_question_bound(2, "run-q", "second")
            .expect("q2 的正确投递必须成功");
        assert_eq!(
            rx2.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            "second"
        );
    }

    #[test]
    fn q1_replay_after_q2_pending_stays_noop() {
        // q1 已投递成功,runtime 解除阻塞后发起了 q2:
        // q1 同答案重放按账本幂等 Ok,绝不触碰 q2 通道。
        let (registry, host, rx1) = registry_with_pending("run-q", Some(1));
        host.answer_question_bound(1, "run-q", "yes").unwrap();
        assert_eq!(
            rx1.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            "yes"
        );
        let rx2 = replace_pending(&registry, "session-run-q", "run-q", Some(2));
        host.answer_question_bound(1, "run-q", "yes")
            .expect("已投递问题的同答案重放必须幂等 Ok");
        assert_empty(&rx2, "q1 重放不得把输入带给 q2");
    }

    #[test]
    fn unbound_slot_window_rejects_delivery() {
        // Question 事件已发出但 Orchestrator 尚未回填 question 绑定的窗口:
        // 无法证明等待的正是该题 → 拒绝(宁可拒绝,不可误投)。
        let (_registry, host, rx) = registry_with_pending("run-q", None);
        let err = host
            .answer_question_bound(9, "run-q", "yes")
            .expect_err("未绑定窗口必须 fail-closed");
        assert!(
            format!("{err:#}").contains("拒绝把旧答案投给新问题"),
            "{err:#}"
        );
        assert_empty(&rx, "未绑定窗口不得注入");
    }

    #[test]
    fn wrong_run_is_rejected_even_with_live_session() {
        // q1 属于 run A;同一注册表里 run B 有自己的会话与待答槽:
        // 以 B 的名义投 q1 必须被拒绝,两个通道都不得被污染。
        let (_reg_a, host_a, rx_a) = registry_with_pending("run-a", Some(1));
        // host_a 的注册表同时登记 run-b:给它一个绑到 q2 的槽
        let registry = host_a.registry.clone();
        let (tx_b, rx_b) = crossbeam_channel::bounded::<String>(1);
        let session_b = Arc::new(HttpSession {
            session_id: 2,
            transcript: Mutex::new(Vec::new()),
            alive: AtomicBool::new(true),
            pending: Mutex::new(Some(PendingQuestion {
                run_handle: "run-b".into(),
                question_id: Some(2),
                tx: tx_b,
            })),
            cancel: AtomicBool::new(false),
            term: TermSignal::new(),
            join: Mutex::new(None),
        });
        registry.register("session-b", SessionInner::Http(session_b));
        registry.bind_run("run-b", "session-b");

        let err = host_a
            .answer_question_bound(1, "run-b", "yes")
            .expect_err("错误 run 必须被拒绝");
        assert!(
            format!("{err:#}").contains("run `run-b` 正在等待 question Some(2)"),
            "{err:#}"
        );
        assert_empty(&rx_a, "run A 的通道不得被污染");
        assert_empty(&rx_b, "run B 的通道不得被污染");

        // run-b 没有 q1 这个问题;run-a 才是 q1 的归属,仍可正常投递
        host_a
            .answer_question_bound(1, "run-a", "answer-a")
            .expect("正确归属的投递必须成功");
        assert_eq!(
            rx_a.recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            "answer-a"
        );
    }

    #[test]
    fn no_live_sender_fails_closed() {
        // 无 live sender 两种形态:run 从未绑定;核心重启后注册表为空。
        let registry = SessionRegistry::new(mf_agent::Config::default());
        let host = RuntimeHostImpl::new(registry);
        let err = host
            .answer_question_bound(1, "run-gone", "yes")
            .expect_err("无绑定必须拒绝");
        assert!(format!("{err:#}").contains("没有存活会话绑定"), "{err:#}");
        assert!(format!("{err:#}").contains("fail-closed"), "{err:#}");
    }

    #[test]
    fn closed_receiver_channel_fails_closed_without_faking_success() {
        // 槽位在、绑定对,但工具循环线程已消失(rx dropped):
        // 绝不假装投递成功。
        let (registry, host, rx) = registry_with_pending("run-q", Some(1));
        drop(rx); // 接收端先退出
        let err = host
            .answer_question_bound(1, "run-q", "yes")
            .expect_err("通道关闭必须显式失败");
        assert!(format!("{err:#}").contains("不再等待该问题"), "{err:#}");
        assert!(session_pending_cleared(&registry, "session-run-q"));
    }

    fn session_pending_cleared(registry: &SessionRegistry, session_handle: &str) -> bool {
        let inner = registry
            .sessions
            .lock()
            .get(session_handle)
            .cloned()
            .expect("会话必须存在");
        let SessionInner::Http(h) = inner else {
            panic!("必须是 HTTP 会话");
        };
        let cleared = h.pending.lock().is_none();
        cleared
    }

    #[test]
    fn concurrent_duplicate_delivery_produces_single_input() {
        // 并发重复投递(durable outbox 重放 + UI 同时提交):
        // 全部 Ok,但 rx 只收到一次输入。
        let (_registry, host, rx) = registry_with_pending("run-q", Some(1));
        let mut joins = Vec::new();
        for _ in 0..8 {
            let host = host.clone();
            joins.push(std::thread::spawn(move || {
                host.answer_question_bound(1, "run-q", "yes")
                    .expect("并发重放必须全部 Ok(幂等)");
            }));
        }
        for join in joins {
            join.join().expect("线程不得 panic");
        }
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            "yes"
        );
        assert_empty(&rx, "并发重放只允许一次输入");
    }

    #[test]
    fn bind_open_question_correlates_only_the_waiting_run() {
        // Orchestrator 持久化后的回填绑定:只有槽位所属 run 能绑定成功;
        // 错误 run 的回填不得污染槽位,随后投递仍 fail-closed。
        let (_registry, host, rx) = registry_with_pending("run-a", None);
        // run-b 也绑定到同一会话(会话复用场景):以 run-b 名义回填
        // 必须命中"槽位属于 run-a"的守卫,不得污染
        host.registry.bind_run("run-b", "session-run-a");
        host.bind_open_question("run-b", 7);
        assert_empty(&rx, "未绑定成功的窗口不得注入");
        let err = host
            .answer_question_bound(7, "run-a", "yes")
            .expect_err("槽位仍指向未绑定,必须拒绝");
        assert!(
            format!("{err:#}").contains("拒绝把旧答案投给新问题"),
            "{err:#}"
        );
        // 正确 run 的回填后即可投递
        host.bind_open_question("run-a", 7);
        host.answer_question_bound(7, "run-a", "yes")
            .expect("回填后投递必须成功");
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            "yes"
        );
    }
}
