use crate::db::Db;
use crate::provider::ToolDef;
use crate::types::{EngineEvent, QuestionView};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// 工具执行上下文:一次任务派发的完整环境
pub struct ToolCtx {
    pub root: PathBuf,
    pub db: Arc<Db>,
    pub run_id: i64,
    pub task_id: i64,
    pub worker: String,
    pub events: crossbeam_channel::Sender<EngineEvent>,
}

/// 终止信号:complete_task / report_failure 由引擎直接处理
pub enum ToolOutcome {
    Result(String),
    Complete(String),
    Fail(String),
}

fn schema(props: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

/// 工作者可用工具
pub fn worker_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "fs_read",
            description: "读取工作区内文件内容(UTF-8,最大 256KB)",
            parameters: schema(
                serde_json::json!({"path": {"type": "string", "description": "相对工作区根的路径"}}),
                &["path"],
            ),
        },
        ToolDef {
            name: "fs_write",
            description: "写入(创建或覆盖)工作区内文件",
            parameters: schema(
                serde_json::json!({
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                }),
                &["path", "content"],
            ),
        },
        ToolDef {
            name: "fs_patch",
            description: "在文件内做一次精确文本替换(find 需唯一)",
            parameters: schema(
                serde_json::json!({
                    "path": {"type": "string"},
                    "find": {"type": "string"},
                    "replace": {"type": "string"}
                }),
                &["path", "find", "replace"],
            ),
        },
        ToolDef {
            name: "fs_list",
            description: "列出目录内容(相对路径;缺省为根)",
            parameters: schema(
                serde_json::json!({"path": {"type": "string"}}),
                &[],
            ),
        },
        ToolDef {
            name: "run_cmd",
            description: "在工作区内运行命令(60 秒超时)",
            parameters: schema(
                serde_json::json!({
                    "cmd": {"type": "string", "description": "可执行文件"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "cwd": {"type": "string"}
                }),
                &["cmd"],
            ),
        },
        ToolDef {
            name: "spawn_subtask",
            description: "创建子任务(可带依赖任务 id 列表),交由其他工作者执行",
            parameters: schema(
                serde_json::json!({
                    "spec": {"type": "string"},
                    "deps": {"type": "array", "items": {"type": "integer"}}
                }),
                &["spec"],
            ),
        },
        ToolDef {
            name: "send_message",
            description: "向协调者或其他工作者发消息",
            parameters: schema(
                serde_json::json!({
                    "to": {"type": "string", "enum": ["coordinator", "user"]},
                    "body": {"type": "string"}
                }),
                &["to", "body"],
            ),
        },
        ToolDef {
            name: "ask_human",
            description: "向用户提问并阻塞等待回答(用于关键决策)",
            parameters: schema(
                serde_json::json!({"question": {"type": "string"}}),
                &["question"],
            ),
        },
        ToolDef {
            name: "complete_task",
            description: "任务成功完成,提交总结",
            parameters: schema(
                serde_json::json!({"summary": {"type": "string"}}),
                &["summary"],
            ),
        },
        ToolDef {
            name: "report_failure",
            description: "任务无法完成,报告原因(累计 3 次将熔断)",
            parameters: schema(
                serde_json::json!({"reason": {"type": "string"}}),
                &["reason"],
            ),
        },
    ]
}

/// 规划者工具(独立于工作者)
pub fn planner_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "create_task",
            description: "创建一个任务;deps 为其他任务 id,全部完成后本任务才会派发",
            parameters: schema(
                serde_json::json!({
                    "spec": {"type": "string", "description": "任务说明,写给执行者看"},
                    "deps": {"type": "array", "items": {"type": "integer"}}
                }),
                &["spec"],
            ),
        },
        ToolDef {
            name: "finalize_plan",
            description: "规划完成:任务已全部创建,调度器开始派发",
            parameters: schema(serde_json::json!({}), &[]),
        },
    ]
}

impl ToolCtx {
    /// 路径沙箱:解析并限制在工作区根内
    pub fn sandbox(&self, rel: &str) -> Result<PathBuf> {
        let rel = rel.trim();
        if rel.is_empty() {
            return Ok(self.root.clone());
        }
        let p = Path::new(rel);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        // 词法规范化,避免不存在路径 canonicalize 失败
        let mut norm = PathBuf::new();
        for comp in joined.components() {
            match comp {
                std::path::Component::ParentDir => {
                    norm.pop();
                }
                std::path::Component::CurDir => {}
                c => norm.push(c),
            }
        }
        if !norm.starts_with(&self.root) {
            anyhow::bail!("路径越界(仅允许工作区内): {}", rel);
        }
        Ok(norm)
    }

    /// 执行工具调用
    pub fn execute(&self, name: &str, arguments: &str) -> ToolOutcome {
        let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        let arg_str = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let result = match name {
            "fs_read" => self.t_fs_read(&arg_str("path")),
            "fs_write" => self.t_fs_write(&arg_str("path"), &arg_str("content")),
            "fs_patch" => self.t_fs_patch(&arg_str("path"), &arg_str("find"), &arg_str("replace")),
            "fs_list" => self.t_fs_list(&arg_str("path")),
            "run_cmd" => self.t_run_cmd(&arg_str("cmd"), &arg_str("cwd"), args.get("args")),
            "spawn_subtask" => self.t_spawn_subtask(&arg_str("spec"), args.get("deps")),
            "send_message" => self.t_send_message(&arg_str("to"), &arg_str("body")),
            "ask_human" => return self.t_ask_human(&arg_str("question")),
            "complete_task" => return ToolOutcome::Complete(arg_str("summary")),
            "report_failure" => return ToolOutcome::Fail(arg_str("reason")),
            _ => Err(anyhow!("未知工具: {}", name)),
        };
        ToolOutcome::Result(match result {
            Ok(text) => text,
            Err(e) => format!("错误: {e}"),
        })
    }

    fn t_fs_read(&self, path: &str) -> Result<String> {
        let p = self.sandbox(path)?;
        let meta = std::fs::metadata(&p).with_context(|| format!("stat {}", p.display()))?;
        if meta.len() > 256 * 1024 {
            anyhow::bail!("文件过大({} 字节)", meta.len());
        }
        Ok(std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?)
    }

    fn t_fs_write(&self, path: &str, content: &str) -> Result<String> {
        let p = self.sandbox(path)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content).with_context(|| format!("write {}", p.display()))?;
        Ok(format!("已写入 {} ({} 字节)", path, content.len()))
    }

    fn t_fs_patch(&self, path: &str, find: &str, replace: &str) -> Result<String> {
        let p = self.sandbox(path)?;
        let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let count = text.matches(find).count();
        if count == 0 {
            anyhow::bail!("未找到匹配文本");
        }
        if count > 1 {
            anyhow::bail!("匹配到 {} 处,需精确唯一", count);
        }
        let new = text.replacen(find, replace, 1);
        std::fs::write(&p, new)?;
        Ok("已替换 1 处".into())
    }

    fn t_fs_list(&self, path: &str) -> Result<String> {
        let p = self.sandbox(path)?;
        let mut out = Vec::new();
        for e in std::fs::read_dir(&p).with_context(|| format!("read_dir {}", p.display()))?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let kind = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                "d"
            } else {
                "f"
            };
            out.push(format!("{kind} {name}"));
        }
        out.sort();
        Ok(out.join("\n"))
    }

    fn t_run_cmd(&self, cmd: &str, cwd: &str, args: Option<&serde_json::Value>) -> Result<String> {
        let argv: Vec<String> = args
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let workdir = if cwd.is_empty() {
            self.root.clone()
        } else {
            self.sandbox(cwd)?
        };
        let mut child = std::process::Command::new(cmd)
            .args(&argv)
            .current_dir(&workdir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {}", cmd))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            match child.try_wait()? {
                Some(status) => {
                    let out = child.stdout.take().map(read_cap).unwrap_or_default();
                    let err = child.stderr.take().map(read_cap).unwrap_or_default();
                    return Ok(format!(
                        "exit={}\nstdout:\n{}\nstderr:\n{}",
                        status.code().unwrap_or(-1),
                        out,
                        err
                    ));
                }
                None => {
                    if std::time::Instant::now() > deadline {
                        let _ = child.kill();
                        anyhow::bail!("命令超时(60s)");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    fn t_spawn_subtask(&self, spec: &str, deps: Option<&serde_json::Value>) -> Result<String> {
        let dep_ids: Vec<i64> = deps
            .and_then(|d| d.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();
        let view = self
            .db
            .create_task(self.run_id, Some(self.task_id), spec, &dep_ids)?;
        let _ = self.events.send(EngineEvent::TaskCreated(view.clone()));
        self.db.push_message(
            self.run_id,
            &self.worker,
            "coordinator",
            "dispatch",
            &format!("spawn_subtask #{}: {}", view.id, spec.chars().take(80).collect::<String>()),
        )?;
        Ok(format!("子任务已创建,id={}", view.id))
    }

    fn t_send_message(&self, to: &str, body: &str) -> Result<String> {
        self.db.push_message(self.run_id, &self.worker, to, "status", body)?;
        let _ = self.events.send(EngineEvent::WorkerLog {
            task_id: self.task_id,
            worker: self.worker.clone(),
            text: format!("→ {}: {}", to, body),
        });
        Ok("已发送".into())
    }

    fn t_ask_human(&self, question: &str) -> ToolOutcome {
        let qid = match self.db.ask_question(self.run_id, Some(self.task_id), question) {
            Ok(id) => id,
            Err(e) => return ToolOutcome::Result(format!("错误: {e}")),
        };
        let _ = self.events.send(EngineEvent::QuestionOpened(QuestionView {
            id: qid,
            run_id: self.run_id,
            task_id: Some(self.task_id),
            question: question.to_string(),
            answer: None,
        }));
        let answer = self.db.wait_answer(qid, Duration::from_secs(6 * 3600));
        match answer {
            Ok(Some(ans)) => {
                if let Ok(Some(v)) = self.db.question_view(qid) {
                    let _ = self.events.send(EngineEvent::QuestionAnswered(v));
                }
                ToolOutcome::Result(format!("用户回答: {}", ans))
            }
            _ => ToolOutcome::Result("用户未在时限内回答".into()),
        }
    }
}

fn read_cap<R: std::io::Read>(mut r: R) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 64 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}
