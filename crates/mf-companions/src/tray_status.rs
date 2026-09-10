//! 托盘状态与入口逻辑(纯函数层,T6c 落地)。
//!
//! 托盘是观察者:崩溃/退出不影响 Core。本模块只做三件事的可测试部分:
//! - 解析 Core 启动输出的 `WEB_ENTRY=…#nonce=…`(一次性入口);
//! - 读取 `discovery.json` 组装用户可读的服务状态;
//! - 决定「打开工作台」用哪个 URL(未用的一次性入口 > 基础地址——
//!   浏览器已有会话时直接进,无需令牌)。
//!
//! Win32 外壳(图标/菜单/消息循环)在 `bin/tray.rs`,保持薄。

use mf_kernel::singleton::{read_discovery, DiscoveryRecord};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 一次性入口(来自 Core stdout 的 WEB_ENTRY 行)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryUrl {
    /// 完整入口 URL(含 #nonce=…)。
    pub url: String,
    /// 是否仍可使用(一次性;用后置 false)。
    pub fresh: bool,
}

/// 从 Core 输出行提取入口 URL;非 WEB_ENTRY 行返回 None。
pub fn parse_entry_line(line: &str) -> Option<EntryUrl> {
    let value = line.trim().strip_prefix("WEB_ENTRY=")?;
    if value.is_empty() {
        return None;
    }
    Some(EntryUrl {
        url: value.to_string(),
        fresh: true,
    })
}

/// 从 Core 进程的累计输出中找(最后一条)入口 URL。
pub fn parse_entry_from_output(output: &str) -> Option<EntryUrl> {
    output.lines().filter_map(parse_entry_line).last()
}

/// 「打开工作台」目标 URL:优先未使用的一次性入口(stdout 捕获),
/// 其次 entry.url 文件(Core 在每次成功交换后重签,其中 nonce 始终
/// 未消耗——即使 Core 不是托盘拉起的也能直达),最后退回基础地址
/// (同一浏览器持有会话 Cookie 时可直接进入)。
pub fn open_target(entry: Option<&EntryUrl>, entry_file: Option<&str>, base_url: &str) -> String {
    if let Some(e) = entry.filter(|e| e.fresh) {
        return e.url.clone();
    }
    if let Some(url) = entry_file {
        return url.to_string();
    }
    base_url.to_string()
}

/// 引导入口文件路径(discovery.json 同目录的 entry.url)。
pub fn entry_url_path() -> Option<PathBuf> {
    discovery_path().parent().map(|dir| dir.join("entry.url"))
}

/// 读取并校验入口文件内容:仅接受本机回环且带 #nonce= 的 URL,
/// 防止把旧格式/损坏内容误当入口。
pub fn read_entry_url(path: &Path) -> Option<String> {
    let url = std::fs::read_to_string(path).ok()?.trim().to_string();
    (url.starts_with("http://127.0.0.1") && url.contains("#nonce=")).then_some(url)
}

/// 从入口 URL 提取基础地址(去掉 #fragment);非法输入回退原串。
pub fn base_url_of(entry_url: &str) -> String {
    match entry_url.split_once('#') {
        Some((base, _)) => base.to_string(),
        None => entry_url.to_string(),
    }
}

/// 服务后台状态(托盘「服务状态」菜单与 tooltip 的数据)。
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub build: Option<String>,
    pub heartbeat_at: Option<String>,
    /// 心跳距今是否新鲜(owner 心跳节奏内)。
    pub heartbeat_fresh: bool,
    /// 状态来源路径(诊断展示)。
    pub discovery_path: PathBuf,
    /// 托盘是否自己拉起了 Core(可执行「停止服务」)。
    pub owned_by_tray: bool,
}

impl ServiceStatus {
    /// 用户可读的多行状态文本(MessageBox/日志共用)。
    pub fn describe(&self) -> String {
        let mut lines = Vec::new();
        lines.push(if self.running {
            "服务状态:运行中".to_string()
        } else {
            "服务状态:未运行".to_string()
        });
        if let Some(pid) = self.pid {
            lines.push(format!("进程 PID:{pid}"));
        }
        if let Some(port) = self.port {
            lines.push(format!("端口:{port}"));
        } else {
            lines.push("端口:未知(见启动输出)".to_string());
        }
        if let Some(build) = &self.build {
            lines.push(format!("版本:{build}"));
        }
        if let Some(heartbeat) = &self.heartbeat_at {
            lines.push(format!(
                "心跳:{heartbeat}{}",
                if self.heartbeat_fresh {
                    "(新鲜)"
                } else {
                    "(过期)"
                }
            ));
        }
        lines.push(format!("discovery:{}", self.discovery_path.display()));
        lines.push(if self.owned_by_tray {
            "由托盘拉起:是(可从托盘停止)".to_string()
        } else {
            "由托盘拉起:否(托盘不代管外部启动的 Core)".to_string()
        });
        lines.join("\n")
    }

    /// 托盘 tooltip(短)。
    pub fn tooltip(&self) -> String {
        if self.running {
            format!(
                "MonkeyFence 工作台:运行中{}",
                self.port
                    .filter(|port| *port != 0)
                    .map(|port| format!(" · 端口 {port}"))
                    .unwrap_or_default()
            )
        } else {
            "MonkeyFence 工作台:未运行".to_string()
        }
    }
}

/// Windows discovery.json 的用户级路径(与
/// `mf-kernel::singleton` 的 `platform_discovery_path` 同一落点;
/// 该函数私有,这里按 §11.1 契约复刻)。
pub fn discovery_path() -> PathBuf {
    #[cfg(windows)]
    {
        dirs_like::data_local_dir()
            .join("MonkeyFence")
            .join("discovery.json")
    }
    #[cfg(target_os = "linux")]
    {
        dirs_like::state_dir()
            .join("monkeyfence")
            .join("discovery.json")
    }
    #[cfg(target_os = "macos")]
    {
        dirs_like::data_dir()
            .join("MonkeyFence")
            .join("discovery.json")
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        dirs_like::home_dir()
            .join(".monkeyfence")
            .join("discovery.json")
    }
}

// 目录约定与 mf-kernel::singleton 的 platform_discovery_path 逐平台对齐
// (该函数私有,这里按 §11.1 契约复刻;两处必须同步修改)。
mod dirs_like {
    pub fn data_local_dir() -> std::path::PathBuf {
        dirs::data_local_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
    }
    pub fn state_dir() -> std::path::PathBuf {
        dirs::state_dir().unwrap_or_else(data_local_dir)
    }
    pub fn data_dir() -> std::path::PathBuf {
        dirs::data_dir().unwrap_or_else(data_local_dir)
    }
    pub fn home_dir() -> std::path::PathBuf {
        dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
    }
}

/// owner 心跳新鲜窗口(与 Core lifecycle 的节奏对齐;超窗视为可疑,
/// 状态文本标注「过期」但不误判未运行)。
pub const HEARTBEAT_FRESH_WINDOW: Duration = Duration::from_secs(30);

/// 读取服务状态。`owned_by_tray` 标记托盘是否是拉起方。
/// discovery 文件缺失 → 未运行;解析失败按 fail-closed 呈现异常态
/// (running=true 但字段为空并带原始心跳说明,不静默当未运行)。
pub fn read_status(discovery: &Path, owned_by_tray: bool) -> ServiceStatus {
    let mut status = ServiceStatus {
        running: false,
        pid: None,
        port: None,
        build: None,
        heartbeat_at: None,
        heartbeat_fresh: false,
        discovery_path: discovery.to_path_buf(),
        owned_by_tray,
    };
    match read_discovery(discovery) {
        Ok(Some(record)) => {
            status.running = true;
            status.pid = Some(record.pid);
            status.port = zero_means_unknown(record.port);
            status.build = Some(record.build);
            status.heartbeat_at = Some(record.heartbeat_at.clone());
            status.heartbeat_fresh = heartbeat_fresh(&record.heartbeat_at);
        }
        Ok(None) => {
            // 无 discovery 记录:未运行(或从未启动)
        }
        Err(error) => {
            // fail-closed:损坏的 discovery 不当作未运行,展示原始错误态
            status.running = true;
            status.heartbeat_at = Some(format!("读取失败:{error}"));
        }
    }
    status
}

fn zero_means_unknown(port: u16) -> Option<u16> {
    // Core 尚未绑定 gateway 时会写 port=0(见 singleton::platform 文档)
    if port == 0 {
        None
    } else {
        Some(port)
    }
}

/// 心跳是否在新鲜窗口内(RFC3339 解析失败 → 不新鲜)。
pub fn heartbeat_fresh(heartbeat_at: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(heartbeat_at) {
        Ok(at) => SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|now| {
                let age = now.as_secs() as i64 - at.timestamp();
                age >= 0 && age <= HEARTBEAT_FRESH_WINDOW.as_secs() as i64
            })
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// 基础地址(端口未知时用默认 80 → http://127.0.0.1/)。
pub fn base_url(port: Option<u16>) -> String {
    match port {
        Some(port) if port != 0 => format!("http://127.0.0.1:{port}/"),
        _ => "http://127.0.0.1/".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_line_parsing() {
        assert_eq!(
            parse_entry_line("WEB_ENTRY=http://127.0.0.1/#nonce=abc"),
            Some(EntryUrl {
                url: "http://127.0.0.1/#nonce=abc".into(),
                fresh: true
            })
        );
        assert_eq!(parse_entry_line("mf-workbench: serving …"), None);
        assert_eq!(parse_entry_line("WEB_ENTRY="), None);
        // 输出流取最后一条
        let out = "noise\nWEB_ENTRY=http://a/#nonce=1\nmore\nWEB_ENTRY=http://b/#nonce=2\n";
        assert_eq!(
            parse_entry_from_output(out).map(|e| e.url),
            Some("http://b/#nonce=2".into())
        );
    }

    #[test]
    fn open_target_prefers_fresh_entry_then_file_then_base() {
        let fresh = EntryUrl {
            url: "http://127.0.0.1/#nonce=stdout".into(),
            fresh: true,
        };
        // 新入口(stdout)最优先
        assert_eq!(
            open_target(
                Some(&fresh),
                Some("http://127.0.0.1/#nonce=file"),
                "http://b/"
            ),
            "http://127.0.0.1/#nonce=stdout"
        );
        // stdout 入口已消耗 → entry.url 文件(Core 重签,始终可用)
        let mut used = fresh.clone();
        used.fresh = false;
        assert_eq!(
            open_target(
                Some(&used),
                Some("http://127.0.0.1/#nonce=file"),
                "http://b/"
            ),
            "http://127.0.0.1/#nonce=file"
        );
        // 都没有 → 基础地址(依赖既有会话 Cookie)
        assert_eq!(open_target(Some(&used), None, "http://b/"), "http://b/");
        assert_eq!(open_target(None, None, "http://b/"), "http://b/");
        assert_eq!(
            base_url_of("http://127.0.0.1/#nonce=x"),
            "http://127.0.0.1/"
        );
    }

    #[test]
    fn read_entry_url_validates_loopback_nonce() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("entry.url");
        // 合法入口(允许带空白)
        std::fs::write(&path, "  http://127.0.0.1/#nonce=abc \n").unwrap();
        assert_eq!(
            read_entry_url(&path),
            Some("http://127.0.0.1/#nonce=abc".to_string())
        );
        // 无 nonce / 非回环 / 损坏内容 → 拒绝(宁可退回基础地址)
        std::fs::write(&path, "http://127.0.0.1/").unwrap();
        assert_eq!(read_entry_url(&path), None);
        std::fs::write(&path, "http://evil.example/#nonce=abc").unwrap();
        assert_eq!(read_entry_url(&path), None);
        std::fs::write(&path, "{not a url").unwrap();
        assert_eq!(read_entry_url(&path), None);
        // 文件不存在 → None
        assert_eq!(read_entry_url(&tmp.path().join("missing.url")), None);
    }

    #[test]
    fn entry_url_path_is_sibling_of_discovery() {
        let path = entry_url_path().unwrap();
        assert!(path.ends_with("entry.url"));
        assert_eq!(
            path.parent().unwrap().join("discovery.json"),
            discovery_path(),
            "入口文件必须与 discovery.json 同目录(同一状态目录契约)"
        );
    }

    #[test]
    fn status_describe_and_tooltip() {
        let running = ServiceStatus {
            running: true,
            pid: Some(42),
            port: Some(8080),
            build: Some("0.1.0".into()),
            heartbeat_at: None,
            heartbeat_fresh: true,
            discovery_path: PathBuf::from("/tmp/discovery.json"),
            owned_by_tray: true,
        };
        let text = running.describe();
        assert!(text.contains("运行中"));
        assert!(text.contains("PID:42"));
        assert!(text.contains("端口:8080"));
        assert!(text.contains("由托盘拉起:是"));
        assert_eq!(running.tooltip(), "MonkeyFence 工作台:运行中 · 端口 8080");

        let stopped = ServiceStatus {
            running: false,
            ..running
        };
        assert_eq!(stopped.tooltip(), "MonkeyFence 工作台:未运行");
        assert!(stopped.describe().contains("未运行"));
    }

    #[test]
    fn read_status_from_real_file_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("discovery.json");
        // 无文件 → 未运行
        assert!(!read_status(&path, false).running);
        // 正常记录
        std::fs::write(
            &path,
            r#"{"instance_id":"i","port":8080,"pid":7,"build":"0.1.0","heartbeat_at":"2026-09-10T00:00:00Z"}"#,
        )
        .unwrap();
        let status = read_status(&path, true);
        assert!(status.running);
        assert_eq!(status.pid, Some(7));
        assert_eq!(status.port, Some(8080));
        // 该心跳已远超窗口 → 不新鲜(固定过去时间)
        assert!(!status.heartbeat_fresh);
        // port=0 → 未知
        std::fs::write(
            &path,
            r#"{"instance_id":"i","port":0,"pid":7,"build":"0.1.0","heartbeat_at":"2026-09-10T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(read_status(&path, false).port, None);
        // 损坏 → fail-closed(不是未运行)
        std::fs::write(&path, "{not json").unwrap();
        let corrupt = read_status(&path, false);
        assert!(corrupt.running);
        assert!(corrupt.describe().contains("读取失败"));
    }

    #[test]
    fn heartbeat_window() {
        let now = chrono::Utc::now().to_rfc3339();
        assert!(heartbeat_fresh(&now));
        let stale = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        assert!(!heartbeat_fresh(&stale));
        assert!(!heartbeat_fresh("not-a-time"));
    }
}
