use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// p4 CLI 封装(-ztag 机器可读输出)
/// 所有命令以 workspace 为当前目录执行,自动继承 P4CONFIG/P4CLIENT
#[derive(Clone, Debug)]
pub struct P4 {
    cwd: PathBuf,
    config: P4CommandConfig,
}

/// 单个 MonkeyFence P4 插件实例的命令环境。`use_p4config=true` 时
/// 继承当前进程环境并可覆盖 P4CONFIG；手动模式会清除继承的 P4 连接变量，
/// 再只注入用户填写的值，避免两种配置来源混用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P4CommandConfig {
    pub executable: String,
    pub use_p4config: bool,
    pub p4config: String,
    pub port: String,
    pub user: String,
    pub client: String,
    pub charset: String,
}

impl Default for P4CommandConfig {
    fn default() -> Self {
        Self {
            executable: "p4".into(),
            use_p4config: true,
            p4config: String::new(),
            port: String::new(),
            user: String::new(),
            client: String::new(),
            charset: String::new(),
        }
    }
}

/// ztag 解析结果:一条记录 = 有序键值对(键可重复)
#[derive(Clone, Debug, Default)]
pub struct ZRecord {
    pairs: Vec<(String, String)>,
}

impl ZRecord {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn get_all(&self, key: &str) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// 解析 -ztag 输出:空行分记录;以空格缩进的行是上一值的续行
pub fn parse_ztag(text: &str) -> Vec<ZRecord> {
    let mut records: Vec<ZRecord> = Vec::new();
    let mut cur = ZRecord::default();
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            if !cur.pairs.is_empty() {
                records.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("... ") {
            let mut parts = rest.splitn(2, ' ');
            let key = parts.next().unwrap_or("").trim().to_string();
            let value = parts.next().unwrap_or("").trim().to_string();
            cur.pairs.push((key, value));
        } else if line.starts_with(' ') || line.starts_with('\t') {
            // 多行值续行
            if let Some(last) = cur.pairs.last_mut() {
                last.1.push('\n');
                last.1.push_str(line.trim_end());
            }
        }
    }
    if !cur.pairs.is_empty() {
        records.push(cur);
    }
    records
}

#[derive(Clone, Debug)]
pub struct P4Info {
    pub user_name: String,
    pub client_name: String,
    pub client_root: String,
    pub client_stream: String,
    pub server_name: String,
}

#[derive(Clone, Debug)]
pub struct OpenedFile {
    pub depot_file: String,
    pub client_file: String,
    /// add | edit | delete | branch | move/add ...
    pub action: String,
    /// "default" 或变更列表号
    pub change: String,
    pub rev: String,
}

impl OpenedFile {
    /// 本地路径(ztag 的 clientFile 已是本地形式)
    pub fn local_path(&self) -> PathBuf {
        PathBuf::from(&self.client_file)
    }

    pub fn file_name(&self) -> String {
        Path::new(&self.depot_file)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.depot_file.clone())
    }
}

#[derive(Clone, Debug)]
pub struct Change {
    pub id: i64,
    pub user: String,
    pub client: String,
    pub status: String,
    pub desc: String,
    pub shelved: bool,
    pub time: i64,
}

impl Change {
    pub fn short_desc(&self) -> String {
        self.desc.lines().next().unwrap_or("").to_string()
    }
}

#[derive(Clone, Debug)]
pub struct FilelogEntry {
    pub change: i64,
    pub rev: String,
    pub action: String,
    pub user: String,
    pub date: String,
    pub desc: String,
}

impl P4 {
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self::with_config(cwd, P4CommandConfig::default())
    }

    pub fn with_config(cwd: impl AsRef<Path>, config: P4CommandConfig) -> Self {
        Self {
            cwd: cwd.as_ref().to_path_buf(),
            config,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.config.executable);
        command.current_dir(&self.cwd);
        if self.config.use_p4config {
            if !self.config.p4config.trim().is_empty() {
                command.env("P4CONFIG", self.config.p4config.trim());
            }
        } else {
            for key in ["P4CONFIG", "P4PORT", "P4USER", "P4CLIENT", "P4CHARSET"] {
                command.env_remove(key);
            }
            for (key, value) in [
                ("P4PORT", &self.config.port),
                ("P4USER", &self.config.user),
                ("P4CLIENT", &self.config.client),
                ("P4CHARSET", &self.config.charset),
            ] {
                if !value.trim().is_empty() {
                    command.env(key, value.trim());
                }
            }
        }
        command
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.command().args(args).output().map_err(|e| {
            anyhow!(
                "无法启动 P4 `{}`: {e}(请在设置 → 版本控制中检查路径)",
                self.config.executable
            )
        })?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            // 有些错误信息在 stdout(ztag 模式)
            anyhow::bail!(
                "p4 {} 失败: {}",
                args.first().unwrap_or(&""),
                if stderr.is_empty() {
                    stdout.trim().to_string()
                } else {
                    stderr.trim().to_string()
                }
            );
        }
        Ok(stdout)
    }

    fn run_ztag(&self, args: &[&str]) -> Result<Vec<ZRecord>> {
        let mut full = vec!["-ztag"];
        full.extend_from_slice(args);
        Ok(parse_ztag(&self.run(&full)?))
    }

    fn run_stdin(&self, args: &[&str], stdin: &str) -> Result<String> {
        use std::io::Write;
        let mut child = self
            .command()
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("无法启动 p4: {e}"))?;
        if let Some(mut si) = child.stdin.take() {
            si.write_all(stdin.as_bytes()).ok();
        }
        let out = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            anyhow::bail!(
                "p4 {} 失败: {}",
                args.first().unwrap_or(&""),
                if stderr.is_empty() {
                    stdout.trim().to_string()
                } else {
                    stderr.trim().to_string()
                }
            );
        }
        Ok(stdout)
    }

    // ---------- 查询 ----------

    pub fn info(&self) -> Result<P4Info> {
        let recs = self.run_ztag(&["info"])?;
        let r = recs.first().ok_or_else(|| anyhow!("p4 info 无输出"))?;
        Ok(P4Info {
            user_name: r.get("userName").unwrap_or_default().to_string(),
            client_name: r.get("clientName").unwrap_or_default().to_string(),
            client_root: r.get("clientRoot").unwrap_or_default().to_string(),
            client_stream: r.get("clientStream").unwrap_or_default().to_string(),
            server_name: r.get("serverName").unwrap_or_default().to_string(),
        })
    }

    /// 当前打开(检出)的文件
    pub fn opened(&self) -> Result<Vec<OpenedFile>> {
        let recs = self.run_ztag(&["opened"])?;
        Ok(recs
            .iter()
            .map(|r| OpenedFile {
                depot_file: r.get("depotFile").unwrap_or_default().to_string(),
                client_file: r.get("clientFile").unwrap_or_default().to_string(),
                action: r.get("action").unwrap_or_default().to_string(),
                change: r.get("change").unwrap_or_default().to_string(),
                rev: r.get("rev").unwrap_or_default().to_string(),
            })
            .collect())
    }

    /// 待提交变更列表(默认列表不在其中)
    pub fn pending_changes(&self, max: u32) -> Result<Vec<Change>> {
        let recs = self.run_ztag(&["changes", "-s", "pending", "-m", &max.to_string()])?;
        Ok(recs
            .iter()
            .map(|r| Change {
                id: r.get("change").unwrap_or_default().parse().unwrap_or(0),
                user: r.get("user").unwrap_or_default().to_string(),
                client: r.get("client").unwrap_or_default().to_string(),
                status: r.get("status").unwrap_or_default().to_string(),
                desc: r.get("desc").unwrap_or_default().to_string(),
                shelved: r.get("shelved").map(|s| !s.is_empty()).unwrap_or(false),
                time: r.get("time").unwrap_or_default().parse().unwrap_or(0),
            })
            .collect())
    }

    /// 已提交历史(限定在 stream 路径下)
    pub fn submitted_history(&self, stream_path: &str, max: u32) -> Result<Vec<Change>> {
        let range = if stream_path.is_empty() {
            "//...".to_string()
        } else {
            format!("{}/...", stream_path.trim_end_matches('/'))
        };
        let recs =
            self.run_ztag(&["changes", "-s", "submitted", "-m", &max.to_string(), &range])?;
        Ok(recs
            .iter()
            .map(|r| Change {
                id: r.get("change").unwrap_or_default().parse().unwrap_or(0),
                user: r.get("user").unwrap_or_default().to_string(),
                client: r.get("client").unwrap_or_default().to_string(),
                status: r.get("status").unwrap_or_default().to_string(),
                desc: r.get("desc").unwrap_or_default().to_string(),
                shelved: false,
                time: r.get("time").unwrap_or_default().parse().unwrap_or(0),
            })
            .collect())
    }

    /// 变更列表详情(含文件清单)
    pub fn describe(&self, cl: i64) -> Result<Vec<OpenedFile>> {
        let recs = self.run_ztag(&["describe", "-s", &cl.to_string()])?;
        let Some(r) = recs.first() else {
            return Ok(vec![]);
        };
        let depots = r.get_all("depotFile");
        let actions = r.get_all("action");
        let revs = r.get_all("rev");
        Ok(depots
            .into_iter()
            .enumerate()
            .map(|(i, d)| OpenedFile {
                depot_file: d.to_string(),
                client_file: String::new(),
                action: actions.get(i).copied().unwrap_or("").to_string(),
                change: cl.to_string(),
                rev: revs.get(i).copied().unwrap_or("").to_string(),
            })
            .collect())
    }

    /// 单文件历史
    pub fn filelog(&self, depot_or_local: &str, max: u32) -> Result<Vec<FilelogEntry>> {
        let recs = self.run_ztag(&["filelog", "-m", &max.to_string(), depot_or_local])?;
        let Some(r) = recs.first() else {
            return Ok(vec![]);
        };
        Ok(r.get_all("rev")
            .into_iter()
            .enumerate()
            .map(|(i, rev)| FilelogEntry {
                change: r
                    .get_all("change")
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                rev: rev.to_string(),
                action: r
                    .get_all("action")
                    .get(i)
                    .copied()
                    .unwrap_or("")
                    .to_string(),
                user: r.get_all("user").get(i).copied().unwrap_or("").to_string(),
                date: r.get_all("date").get(i).copied().unwrap_or("").to_string(),
                desc: r.get_all("desc").get(i).copied().unwrap_or("").to_string(),
            })
            .collect())
    }

    /// 工作区文件 vs have 版本的 unified diff
    pub fn diff_file(&self, local_path: &Path) -> Result<String> {
        self.run(&["diff", "-du", &local_path.to_string_lossy()])
    }

    // ---------- 操作 ----------

    /// 提交文件(用 -d 直接带描述;files 为本地绝对路径)
    pub fn submit(&self, description: &str, files: &[PathBuf]) -> Result<String> {
        if files.is_empty() {
            anyhow::bail!("未选择要提交的文件");
        }
        let mut args: Vec<String> = vec![
            "submit".into(),
            "-d".into(),
            description.replace(['\r', '\n'], " "),
        ];
        for f in files {
            args.push(f.to_string_lossy().into_owned());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.run(&arg_refs)
    }

    /// 还原本地修改(危险:丢失未提交改动)
    pub fn revert(&self, files: &[PathBuf]) -> Result<String> {
        let mut args: Vec<String> = vec!["revert".into()];
        for f in files {
            args.push(f.to_string_lossy().into_owned());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.run(&arg_refs)
    }

    /// 搁置/恢复搁置
    pub fn shelve(&self, cl: i64) -> Result<String> {
        self.run(&["shelve", "-f", "-c", &cl.to_string()])
    }

    pub fn unshelve(&self, from_cl: i64, to_cl: Option<i64>) -> Result<String> {
        match to_cl {
            Some(t) => self.run(&["unshelve", "-s", &from_cl.to_string(), "-c", &t.to_string()]),
            None => self.run(&["unshelve", "-s", &from_cl.to_string()]),
        }
    }

    /// 移动文件到指定变更列表
    pub fn reopen(&self, cl: &str, files: &[PathBuf]) -> Result<String> {
        let mut args: Vec<String> = vec!["reopen".into(), "-c".into(), cl.to_string()];
        for f in files {
            args.push(f.to_string_lossy().into_owned());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.run(&arg_refs)
    }

    /// 同步(获取最新);path 为空则同步整个 client
    pub fn sync(&self, path: Option<&str>) -> Result<String> {
        match path {
            Some(p) => self.run(&["sync", p]),
            None => self.run(&["sync"]),
        }
    }

    /// 创建新的编号变更列表,返回 id
    pub fn new_changelist(&self, description: &str) -> Result<i64> {
        let spec = format!(
            "Change: new\nDescription:\n\t{}\n",
            description.replace('\n', "\n\t")
        );
        let out = self.run_stdin(&["change", "-i"], &spec)?;
        // 输出形如 "Change 123 created."
        let id = out
            .split_whitespace()
            .find(|w| w.chars().all(|c| c.is_ascii_digit()) && !w.is_empty())
            .and_then(|w| w.parse().ok())
            .with_context(|| format!("解析新变更列表 id 失败: {}", out.trim()))?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENED_SAMPLE: &str = "\
... depotFile //DEPOT/main/a.rs
... clientFile //client/main/a.rs
... rev 1
... haveRev 1
... action edit
... change default
... type text
... user u
... client c

... depotFile //DEPOT/main/b.rs
... clientFile //client/main/b.rs
... rev 2
... haveRev 2
... action add
... change 123
... type text
... user u
... client c
";

    const CHANGES_SAMPLE: &str = "\
... change 9715620
... time 1787764116
... user alice
... client alice_ws
... status pending
... shelved 
... desc 第一行
 多行描述继续

... change 9715615
... time 1787764068
... user bob
... client bob_ws
... status submitted
... desc 单行描述
";

    #[test]
    fn parse_opened_records() {
        let recs = parse_ztag(OPENED_SAMPLE);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].get("depotFile"), Some("//DEPOT/main/a.rs"));
        assert_eq!(recs[0].get("action"), Some("edit"));
        assert_eq!(recs[1].get("change"), Some("123"));
    }

    #[test]
    fn parse_changes_with_multiline_desc() {
        let recs = parse_ztag(CHANGES_SAMPLE);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].get("change"), Some("9715620"));
        // 多行 desc 通过缩进续行合并
        assert!(recs[0].get("desc").unwrap().contains("多行描述继续"));
        // "shelved " 空值
        assert_eq!(recs[0].get("shelved"), Some(""));
        assert_eq!(recs[1].get("user"), Some("bob"));
    }

    #[test]
    fn describe_multi_files() {
        let sample = "\
... change 42
... user u
... status pending
... desc 描述
... depotFile //D/f1
... action edit
... rev 1
... depotFile //D/f2
... action delete
... rev 3
";
        let recs = parse_ztag(sample);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].get_all("depotFile"), vec!["//D/f1", "//D/f2"]);
        assert_eq!(recs[0].get_all("action"), vec!["edit", "delete"]);
    }

    #[test]
    fn no_output_is_empty() {
        assert!(parse_ztag("").is_empty());
        assert!(parse_ztag("\n\n").is_empty());
    }
}
