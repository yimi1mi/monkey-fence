//! tray bin(T6c 落地):系统托盘入口——观察者,崩溃/退出不影响 Core。
//!
//! 右键菜单:
//! - 打开工作台:优先托盘拉起 Core 时捕获的一次性入口(#nonce=…),
//!   已用/无则打开基础地址(浏览器持会话时直接进入)。
//! - 服务状态:discovery.json 组装的后台状态(pid/端口/版本/心跳)。
//! - 启动服务 / 停止服务:仅托盘自己拉起的 Core 可停(不代管外部实例);
//!   启动时从 Core stdout 捕获 WEB_ENTRY 供首次打开。
//! - 退出托盘:只退托盘,Core 继续运行。
//!
//! 无头模式(CI/测试):`tray --status` 打印状态文本后退出。

#[cfg(windows)]
mod shell {
    use crate::shell_common::{MenuAction, TrayApp};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
        DispatchMessageW, GetCursorPos, GetMenuItemCount, GetMessageW, LoadCursorW, LoadImageW,
        PostQuitMessage, RegisterClassW, SetForegroundWindow, TrackPopupMenu, TranslateMessage,
        CW_USEDEFAULT, HMENU, IDC_ARROW, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, MENU_ITEM_FLAGS,
        MF_BYPOSITION, MF_DISABLED, MF_ENABLED, MF_SEPARATOR, MF_STRING, MSG, SW_SHOWNORMAL,
        TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
        WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
    };

    const TRAY_CALLBACK: u32 = WM_APP + 1;
    const TRAY_ID: u32 = 1;

    // 菜单命令 ID(TrackPopupMenu 返回值)
    const CMD_OPEN_WEB: usize = 1001;
    const CMD_STATUS: usize = 1002;
    const CMD_START: usize = 1003;
    const CMD_STOP: usize = 1004;
    const CMD_QUIT: usize = 1005;

    // 单实例托盘:窗口过程回调是 C ABI,无法携带上下文——用全局槽
    // (unsafe 静态;run() 期间独占,消息循环单线程)。
    static mut APP: Option<TrayApp> = None;

    #[allow(static_mut_refs)]
    fn app() -> &'static mut TrayApp {
        unsafe { APP.as_mut().expect("tray app initialized") }
    }

    pub fn run(tray_app: TrayApp) -> anyhow::Result<()> {
        unsafe {
            let class_name = to_wide("MonkeyFenceTray");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                lpszClassName: class_name.as_ptr(),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                ..std::mem::zeroed()
            };
            let atom = RegisterClassW(&wc);
            anyhow::ensure!(atom != 0, "RegisterClassW 失败");

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                to_wide("MonkeyFence Tray").as_ptr(),
                WS_OVERLAPPED, // 隐藏窗口(永不 ShowWindow)
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            anyhow::ensure!(!hwnd.is_null(), "CreateWindowExW 失败");

            APP = Some(tray_app);
            if !add_tray_icon(hwnd) {
                anyhow::bail!("托盘图标注册失败(explorer 未就绪或通知区不可用)");
            }
            eprintln!("mf-tray: 托盘已就绪(右键打开菜单;左键直接打开工作台)");

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            remove_tray_icon(hwnd);
            Ok(())
        }
    }

    unsafe fn add_tray_icon(hwnd: HWND) -> bool {
        let mut tip = [0u16; 128];
        copy_wide(&app().status().tooltip(), &mut tip);
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = TRAY_CALLBACK;
        data.hIcon = load_app_icon();
        data.szTip = tip;
        let ok = Shell_NotifyIconW(NIM_ADD, &data) != 0;
        if !ok {
            eprintln!("mf-tray: 托盘图标注册失败(Shell_NotifyIconW NIM_ADD)");
        }
        ok
    }

    unsafe fn remove_tray_icon(hwnd: HWND) {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }

    unsafe fn load_app_icon() -> *mut core::ffi::c_void {
        // IDI_APPLICATION 标准图标(无嵌入资源依赖;后续 ticket 换专属图标)
        // MAKEINTRESOURCEW(32512) = 32512 as usize as *const u16
        LoadImageW(
            std::ptr::null_mut(),
            32512usize as *const u16,
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        )
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            TRAY_CALLBACK => {
                // 旧式托盘协议(NIM_ADD 未设版本):lParam = 鼠标事件,
                // wParam = 图标 id。事件在 lParam 低位。
                let event = (lparam as u32) & 0xFFFF;
                match event {
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    WM_LBUTTONUP => app().on_menu(MenuAction::OpenWeb),
                    _ => {}
                }
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn show_menu(hwnd: HWND) {
        let menu: HMENU = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let status = app().status();
        let open_label = to_wide("打开工作台");
        let status_label = to_wide("服务状态");
        let start_label = to_wide("启动服务");
        let stop_label = to_wide("停止服务(托盘拉起的实例)");
        let quit_label = to_wide("退出托盘(Core 继续运行)");

        let push = |id: usize, flags: MENU_ITEM_FLAGS, text: &Vec<u16>| {
            AppendMenuW(menu, flags | MF_STRING, id, text.as_ptr());
        };
        push(CMD_OPEN_WEB, MF_ENABLED, &open_label);
        push(CMD_STATUS, MF_ENABLED, &status_label);
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        let start_flags = if status.running {
            MF_DISABLED
        } else {
            MF_ENABLED
        };
        push(CMD_START, start_flags, &start_label);
        let stop_flags = if status.running && status.owned_by_tray {
            MF_ENABLED
        } else {
            MF_DISABLED
        };
        push(CMD_STOP, stop_flags, &stop_label);
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        push(CMD_QUIT, MF_ENABLED, &quit_label);
        let _ = GetMenuItemCount(menu);

        // TrackPopupMenu 需要窗口先进入前台,否则菜单不消失(Windows 已知行为)
        SetForegroundWindow(hwnd);
        let mut cursor = std::mem::zeroed();
        _ = GetCursorPos(&mut cursor);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
            cursor.x,
            cursor.y,
            0,
            hwnd,
            std::ptr::null_mut(),
        );
        _ = DestroyMenu(menu);
        let _ = MF_BYPOSITION; // 版本兼容占位

        let action = match cmd as usize {
            CMD_OPEN_WEB => Some(MenuAction::OpenWeb),
            CMD_STATUS => Some(MenuAction::ShowStatus),
            CMD_START => Some(MenuAction::StartService),
            CMD_STOP => Some(MenuAction::StopService),
            CMD_QUIT => Some(MenuAction::Quit),
            _ => None,
        };
        if let Some(action) = action {
            app().on_menu(action);
            if action == MenuAction::Quit {
                PostQuitMessage(0);
            }
        }
    }

    pub fn open_url(url: &str) {
        let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            _ = ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    fn to_wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn copy_wide(text: &str, buffer: &mut [u16]) {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let count = wide.len().min(buffer.len() - 1);
        buffer[..count].copy_from_slice(&wide[..count]);
    }
}

/// 平台无关层:托盘应用状态机 + Core 进程管理(可单测的部分)。
mod shell_common {
    use mf_companions::tray_status::{
        base_url, open_target, parse_entry_from_output, read_status, EntryUrl, ServiceStatus,
    };
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MenuAction {
        OpenWeb,
        ShowStatus,
        StartService,
        StopService,
        Quit,
    }

    /// Core 拉起配置(路径由调用方解析;bin 同目录约定)。
    pub struct TrayApp {
        core_exe: std::path::PathBuf,
        discovery_path: std::path::PathBuf,
        /// entry.url 引导入口文件路径(与 discovery 同目录;测试注入)。
        entry_url_path: std::path::PathBuf,
        entry: Mutex<Option<EntryUrl>>,
        child: Mutex<Option<Child>>,
        // 输出回调(外壳注入:GUI 用 MessageBox;无头模式用 println)
        pub notify: Box<dyn Fn(&str) + Send + Sync>,
        pub open_browser: Box<dyn Fn(&str) + Send + Sync>,
        last_message: Mutex<String>,
    }

    impl TrayApp {
        pub fn new(
            core_exe: std::path::PathBuf,
            discovery_path: std::path::PathBuf,
            notify: Box<dyn Fn(&str) + Send + Sync>,
            open_browser: Box<dyn Fn(&str) + Send + Sync>,
        ) -> Self {
            // entry.url 与 discovery.json 同目录(同一状态目录契约);
            // 从注入的 discovery 路径派生,测试因此与真实用户目录隔离
            let entry_url_path = discovery_path.with_file_name("entry.url");
            TrayApp {
                core_exe,
                discovery_path,
                entry_url_path,
                entry: Mutex::new(None),
                child: Mutex::new(None),
                notify,
                open_browser,
                last_message: Mutex::new(String::new()),
            }
        }

        pub fn status(&self) -> ServiceStatus {
            let owned = self.child.lock().unwrap().is_some();
            read_status(&self.discovery_path, owned)
        }

        /// 最近一次菜单动作产生的用户提示(测试断言用)。
        #[cfg(test)]
        pub fn last_message(&self) -> String {
            self.last_message.lock().unwrap().clone()
        }

        fn say(&self, text: &str) {
            *self.last_message.lock().unwrap() = text.to_string();
            (self.notify)(text);
        }

        pub fn on_menu(&self, action: MenuAction) {
            match action {
                MenuAction::OpenWeb => self.open_web(),
                MenuAction::ShowStatus => {
                    let text = self.status().describe();
                    self.say(&text);
                }
                MenuAction::StartService => self.start_service(),
                MenuAction::StopService => self.stop_service(),
                MenuAction::Quit => {
                    // 只退托盘;托盘拉起的 Core 按观察者语义继续运行,
                    // 用户经「停止服务」显式停
                }
            }
        }

        fn open_web(&self) {
            let status = self.status();
            if !status.running {
                self.say("服务未运行:请先「启动服务」");
                return;
            }
            let target = {
                let entry = self.entry.lock().unwrap();
                // entry.url 由 Core 在每次成功交换后重签(未消耗的一次性
                // 入口)——即使 Core 不是托盘拉起的,这里也能直达工作台
                let entry_file = mf_companions::tray_status::read_entry_url(&self.entry_url_path);
                open_target(
                    entry.as_ref(),
                    entry_file.as_deref(),
                    &base_url(status.port),
                )
            };
            // 一次性入口用后即焚(即使浏览器打开失败也不复用已暴露的 nonce)
            {
                let mut entry = self.entry.lock().unwrap();
                if let Some(e) = entry.as_mut() {
                    e.fresh = false;
                }
            }
            self.say(&format!("正在打开工作台:{target}"));
            (self.open_browser)(&target);
        }

        fn start_service(&self) {
            if self.status().running {
                self.say("服务已在运行");
                return;
            }
            match spawn_core(&self.core_exe) {
                Ok((child, entry)) => {
                    let summary = entry
                        .as_ref()
                        .map(|e| format!("(入口:{})", e.url))
                        .unwrap_or_else(|| "(未捕获到 WEB_ENTRY)".into());
                    *self.child.lock().unwrap() = Some(child);
                    *self.entry.lock().unwrap() = entry;
                    self.say(&format!("服务已启动{summary}"));
                }
                Err(error) => self.say(&format!("启动失败:{error:#}")),
            }
        }

        fn stop_service(&self) {
            let mut child_slot = self.child.lock().unwrap();
            match child_slot.as_mut() {
                Some(child) => {
                    let pid = child.id();
                    let _ = child.kill();
                    let _ = child.wait();
                    *child_slot = None;
                    *self.entry.lock().unwrap() = None;
                    self.say(&format!("服务已停止(pid {pid})"));
                }
                None => {
                    self.say(
                        "托盘未拉起该 Core(外部启动),请从其启动处停止;\n\
                         托盘不代管外部实例(观察者语义)。",
                    );
                }
            }
        }
    }

    /// 拉起 Core:stdout 捕获 WEB_ENTRY(一次性入口)。读端保持打开
    /// (mem::forget)避免 Core 写满管道被阻塞——托盘与 Core 同生命周期。
    fn spawn_core(core_exe: &std::path::Path) -> anyhow::Result<(Child, Option<EntryUrl>)> {
        let mut child = Command::new(core_exe)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| anyhow::anyhow!("无法启动 {}:{error}", core_exe.display()))?;
        let mut buffer = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            let mut chunk = [0u8; 4096];
            // 最多 5s/64KB,拿到入口行即返回
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline && buffer.len() < 64 * 1024 {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        if buffer.contains("WEB_ENTRY=") {
                            break;
                        }
                    }
                }
            }
            std::mem::forget(stdout);
        }
        let entry = parse_entry_from_output(&buffer);
        Ok((child, entry))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn app_with(discovery: &std::path::Path) -> (TrayApp, std::sync::Arc<Mutex<Vec<String>>>) {
            let opened = std::sync::Arc::new(Mutex::new(Vec::new()));
            let opened_for_closure = opened.clone();
            let app = TrayApp::new(
                std::path::PathBuf::from("nowhere-mf-workbench"),
                discovery.to_path_buf(),
                Box::new(|_| {}),
                Box::new(move |url| opened_for_closure.lock().unwrap().push(url.to_string())),
            );
            (app, opened)
        }

        #[test]
        fn open_web_requires_running_service() {
            let tmp = tempfile::tempdir().unwrap();
            let (app, opened) = app_with(&tmp.path().join("discovery.json"));
            app.on_menu(MenuAction::OpenWeb);
            assert!(app.last_message().contains("服务未运行"));
            assert!(opened.lock().unwrap().is_empty());
        }

        #[test]
        fn open_web_uses_base_url_when_no_entry() {
            let tmp = tempfile::tempdir().unwrap();
            let discovery = tmp.path().join("discovery.json");
            std::fs::write(
                &discovery,
                r#"{"instance_id":"i","port":8080,"pid":1,"build":"t","heartbeat_at":"2026-09-10T00:00:00Z"}"#,
            )
            .unwrap();
            let (app, opened) = app_with(&discovery);
            app.on_menu(MenuAction::OpenWeb);
            assert_eq!(
                opened.lock().unwrap().last().map(String::as_str),
                Some("http://127.0.0.1:8080/")
            );
        }

        #[test]
        fn open_web_reads_entry_url_file_for_foreign_core() {
            // 用户路径:Core 由外部启动(托盘无 stdout 入口),但 Core 在
            // discovery 同目录维护 entry.url(交换后重签)→ 直达工作台
            let tmp = tempfile::tempdir().unwrap();
            let discovery = tmp.path().join("discovery.json");
            std::fs::write(
                &discovery,
                r#"{"instance_id":"i","port":0,"pid":1,"build":"t","heartbeat_at":"2026-09-10T00:00:00Z"}"#,
            )
            .unwrap();
            std::fs::write(
                tmp.path().join("entry.url"),
                "http://127.0.0.1/#nonce=refreshed\n",
            )
            .unwrap();
            let (app, opened) = app_with(&discovery);
            app.on_menu(MenuAction::OpenWeb);
            assert_eq!(
                opened.lock().unwrap().last().map(String::as_str),
                Some("http://127.0.0.1/#nonce=refreshed")
            );
        }

        #[test]
        fn stop_service_refuses_foreign_core() {
            let tmp = tempfile::tempdir().unwrap();
            let (app, _opened) = app_with(&tmp.path().join("discovery.json"));
            app.on_menu(MenuAction::StopService);
            assert!(app.last_message().contains("不代管外部实例"));
        }

        #[test]
        fn start_service_reports_missing_binary() {
            let tmp = tempfile::tempdir().unwrap();
            let (app, _opened) = app_with(&tmp.path().join("discovery.json"));
            app.on_menu(MenuAction::StartService);
            assert!(app.last_message().contains("启动失败"));
        }

        #[test]
        fn show_status_reports_text() {
            let tmp = tempfile::tempdir().unwrap();
            let (app, _opened) = app_with(&tmp.path().join("discovery.json"));
            app.on_menu(MenuAction::ShowStatus);
            assert!(app.last_message().contains("服务状态:未运行"));
        }
    }
}

#[cfg(windows)]
fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };
    let title: Vec<u16> = "MonkeyFence 工作台"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let body: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

fn resolve_core_exe() -> std::path::PathBuf {
    // 约定:tray 与 mf-workbench 同目录发布(安装包/构建产物均如此);
    // 环境变量 MF_WORKBENCH_BIN 显式覆盖(开发/测试)
    if let Some(path) = std::env::var_os("MF_WORKBENCH_BIN") {
        return std::path::PathBuf::from(path);
    }
    let exe = if cfg!(windows) {
        "mf-workbench.exe"
    } else {
        "mf-workbench"
    };
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join(exe)))
        .unwrap_or_else(|| std::path::PathBuf::from(exe))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--status") {
        // 无头:读取真实 discovery 并打印状态(CI/巡检可测)
        let app = shell_common::TrayApp::new(
            resolve_core_exe(),
            mf_companions::tray_status::discovery_path(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        );
        println!("{}", app.status().describe());
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("MonkeyFence 托盘(观察者;退出不影响 Core)");
        println!("用法:mf-tray [--status]");
        println!("  (无参数) 启动系统托盘;--status 打印服务状态后退出");
        return;
    }

    #[cfg(windows)]
    {
        let app = shell_common::TrayApp::new(
            resolve_core_exe(),
            mf_companions::tray_status::discovery_path(),
            Box::new(|text| message_box(text)),
            Box::new(|url| shell::open_url(url)),
        );
        if let Err(error) = shell::run(app) {
            eprintln!("mf-tray: {error:#}");
            std::process::exit(1);
        }
    }
    #[cfg(not(windows))]
    {
        println!("mf-tray: 系统托盘仅在 Windows 提供;--status 可用");
    }
}
