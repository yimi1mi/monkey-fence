//! 可控伪 Agent CLI(T0c 行为冻结夹具,Issue #14)。
//!
//! 模拟真实 Agent CLI 的外部可见行为,供 Preview/Node/mfctl 三条链的
//! 契约测试驱动:
//! - 启动:把脱敏 argv/cwd/环境指纹写入 `<record>/launch.json`(原子写);
//!   `MF_RUN_TOKEN` 只记录 SHA-256 指纹与长度,原文绝不落盘;
//! - 原始输入:stdin 收到的全部原始字节(含控制命令)追加到
//!   `<record>/input.hex`(hex 编码);MonkeyFence 是否透传由此证明;
//! - 输出:控制命令写出的 stdout 原始字节追加到 `<record>/output.hex`;
//! - 退出:控制命令以指定码退出(退出语义由调用方驱动)。
//!
//! 控制命令(行协议,行尾 \n 或 \r):
//!   !out <base64>     向 stdout 写 base64 解码后的原始字节
//!   !exit <code>      以指定退出码退出
//!   !sleep <ms>       睡眠指定毫秒
//! 其余行原样记录、不解释 —— 与真实 CLI 对 `/xxx`/Unicode/TUI 的
//! 原生处理同构(解释权在 CLI,不在 MonkeyFence)。

use std::io::{Read, Write};
use std::path::PathBuf;

fn main() {
    enable_raw_console_input();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut record: Option<PathBuf> = None;
    let mut iter = argv.iter().cloned();
    while let Some(arg) = iter.next() {
        if arg == "--record" {
            record = iter.next().map(PathBuf::from);
        }
    }
    let Some(record) = record else {
        eprintln!("fake-agent: 需要 --record <dir>");
        std::process::exit(64);
    };
    let _ = std::fs::create_dir_all(&record);

    // argv 记录(含 --record 及其值),但任意令牌子串先脱敏。
    write_launch_json(&record, &argv);

    let mut hex_in = HexLog::open(&record, "input.hex");
    let mut hex_out = HexLog::open(&record, "output.hex");
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut stdin = std::io::stdin().lock();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                hex_in.append(chunk);
                pending.extend_from_slice(chunk);
                // 以 \n 或 \r 切行(PTY 规程两种行尾都可能出现)
                while let Some(pos) = pending.iter().position(|&b| b == b'\n' || b == b'\r') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    let trimmed = trim_line_ending(&line);
                    if let Some(action) = parse_command(trimmed) {
                        match action {
                            Action::Out(bytes) => {
                                let _ = stdout.write_all(&bytes);
                                let _ = stdout.flush();
                                hex_out.append(&bytes);
                            }
                            Action::Exit(code) => {
                                let _ = stdout.flush();
                                hex_out.flush();
                                hex_in.flush();
                                std::process::exit(code);
                            }
                            Action::Sleep(ms) => {
                                std::thread::sleep(std::time::Duration::from_millis(ms));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// 真实 TUI CLI 会把 ConPTY 输入切换到 VT/raw 模式。测试夹具
/// 使用同样的子进程边界,避免 Windows Console 的 cooked input
/// 在 MonkeyFence 之后吞掉 ESC/NUL,导致错误归因到宿主写入层。
#[cfg(windows)]
fn enable_raw_console_input() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, STD_INPUT_HANDLE,
    };
    unsafe {
        let input = GetStdHandle(STD_INPUT_HANDLE);
        if input.is_null() {
            return;
        }
        let mut mode = 0u32;
        if GetConsoleMode(input, &mut mode) == 0 {
            return;
        }
        mode &= !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT);
        mode |= ENABLE_VIRTUAL_TERMINAL_INPUT;
        let _ = SetConsoleMode(input, mode);
    }
}

#[cfg(not(windows))]
fn enable_raw_console_input() {}

enum Action {
    Out(Vec<u8>),
    Exit(i32),
    Sleep(u64),
}

fn parse_command(line: &[u8]) -> Option<Action> {
    let text = std::str::from_utf8(line).ok()?;
    let (head, rest) = text.split_once(' ')?;
    match head {
        "!out" => b64_decode(rest.trim()).map(Action::Out),
        "!exit" => rest.trim().parse::<i32>().ok().map(Action::Exit),
        "!sleep" => rest.trim().parse::<u64>().ok().map(Action::Sleep),
        _ => None,
    }
}

fn trim_line_ending(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
        end -= 1;
    }
    &line[..end]
}

fn write_launch_json(record: &std::path::Path, argv: &[String]) {
    use serde_json::json;
    let token = std::env::var("MF_RUN_TOKEN").unwrap_or_default();
    // 真实 Node prompt 当前包含结算令牌;夹具只需记录 argv 形状,
    // 不得把令牌带进临时文件、panic 或 CI artifact。
    let recorded_argv: Vec<String> = argv
        .iter()
        .map(|arg| {
            if token.is_empty() {
                arg.clone()
            } else {
                arg.replace(&token, "[MF_RUN_TOKEN]")
            }
        })
        .collect();
    let mut env = serde_json::Map::new();
    env.insert(
        "MF_RUN_TOKEN_SHA256".into(),
        json!(sha256_hex(token.as_bytes())),
    );
    env.insert("MF_RUN_TOKEN_LEN".into(), json!(token.len()));
    env.insert(
        "MF_PIPE".into(),
        json!(std::env::var("MF_PIPE").unwrap_or_default()),
    );
    env.insert(
        "MFCTL_HINT".into(),
        match std::env::var("MFCTL_HINT") {
            Ok(hint) => json!(hint),
            Err(_) => serde_json::Value::Null,
        },
    );
    env.insert(
        "FAKE_AGENT_SESSION".into(),
        json!(std::env::var("FAKE_AGENT_SESSION").unwrap_or_default()),
    );
    let payload = json!({
        "argv": recorded_argv,
        "cwd": std::env::current_dir().unwrap_or_default(),
        "env": env,
        "pid": std::process::id(),
    });
    let tmp = record.join("launch.json.tmp");
    let target = record.join("launch.json");
    // 原子写:先临时文件再 rename,测试侧读到的一定是完整 JSON
    if std::fs::write(&tmp, payload.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &target);
    }
}

/// 追加式 hex 字节日志(每次写入即 flush,供外部轮询观察)。
struct HexLog {
    file: std::fs::File,
}

impl HexLog {
    fn open(record: &std::path::Path, name: &str) -> HexLog {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(record.join(name))
            .unwrap_or_else(|e| panic!("fake-agent 打开 {name} 失败: {e}"));
        HexLog { file }
    }

    fn append(&mut self, bytes: &[u8]) {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        let _ = self.file.write_all(out.as_bytes());
        self.flush();
    }

    fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// 测试侧 `b64` 的对偶解码(标准 base64,含 padding)。
fn b64_decode(text: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let value_of = |c: u8| TABLE.iter().position(|&t| t == c).map(|p| p as u8);
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in text.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = value_of(c)?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}
