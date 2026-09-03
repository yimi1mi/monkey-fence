//! `mf-workbench` bin(T11 验收入口,Issue #64/#65)。
//!
//! 独立启动一个带 Web 面的最小 Core:kernel tracer + SessionRegistry
//! (AppRuntime 装配,transcript sink 就位)+ workbench HTTP 服务(构建
//! 产物 + 一次性 nonce bootstrap + workspace 快照)。启动后打印带
//! nonce fragment 的入口 URL(浏览器直接打开),进程驻留直到终止。
//!
//! 环境变量:`MF_WEB_DIST`(默认 `web/dist`)、`MF_WEB_PORT`(默认 80)。
//! 这是发布验收的入口形态,不改变 production bundle 的装配次序。

use mf_web::workbench_serve::serve_workbench;
use std::sync::Arc;

fn main() {
    let dist = std::env::var("MF_WEB_DIST").unwrap_or_else(|_| "web/dist".into());
    if !std::path::Path::new(&dist).join("index.html").is_file() {
        eprintln!("mf-workbench: 未找到 {dist}/index.html(先构建 web,或设 MF_WEB_DIST)");
        std::process::exit(1);
    }
    // 最小 kernel 装配:AppRuntime(SessionRegistry + transcript sink)+
    // 生产 kernel tracer + terminal host 注入(复用 #65 的装配链)。
    let runtime = match mf_kernel::app_runtime::AppRuntime::assemble(
        mf_agent::Config::load().unwrap_or_default(),
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("mf-workbench assemble: {error}");
            std::process::exit(2);
        }
    };
    let kernel: Arc<dyn mf_kernel::kernel::CoreKernel> =
        match mf_kernel::app_runtime::bootstrap_kernel_with_terminal_host(runtime.registry.clone())
        {
            Ok(kernel_runtime) => kernel_runtime.kernel().clone(),
            Err(error) => {
                eprintln!("mf-workbench kernel: {error}");
                std::process::exit(3);
            }
        };
    let port: u16 = std::env::var("MF_WEB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(80);
    match serve_workbench(kernel, &dist, port) {
        Ok(url) => {
            println!("mf-workbench: serving {dist} on 127.0.0.1:{port}");
            println!("WEB_ENTRY={url}");
        }
        Err(error) => {
            eprintln!("mf-workbench(端口 {port}): {error:#}");
            std::process::exit(4);
        }
    }
    // 驻留(浏览器关闭不停止服务;Ctrl+C / kill 终止)
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
