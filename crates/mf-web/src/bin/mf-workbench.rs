//! `mf-workbench` bin(T11 验收入口,Issue #64/#65)。
//!
//! 独立启动一个带 Web 面的最小 Core:kernel tracer + SessionRegistry
//! (AppRuntime 装配,transcript sink 就位)+ workbench HTTP 服务(构建
//! 产物 + 一次性 nonce bootstrap + workspace 快照)。启动后打印带
//! nonce fragment 的入口 URL(浏览器直接打开),进程驻留直到终止。
//!
//! 环境变量:`MF_WEB_DIST`(默认 `web/dist`)、`MF_WEB_PORT`(默认 80)。
//! 这是发布验收的入口形态,不改变 production bundle 的装配次序。

use mf_web::execution_ports::assemble_project_execution_with;
use mf_web::workbench_serve::{serve_workbench_with_hook, ProjectAttachHook};
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
    let kernel_runtime =
        match mf_kernel::app_runtime::bootstrap_kernel_with_terminal_host(runtime.registry.clone())
        {
            Ok(kernel_runtime) => kernel_runtime,
            Err(error) => {
                eprintln!("mf-workbench kernel: {error}");
                std::process::exit(3);
            }
        };
    // 执行面装配钩(#75):挂载即装配 Orchestrator + ports(数据面
    // 与执行面同生命周期;失败不阻断挂载,响应中提示)。
    let registry = runtime.registry.clone();
    let kernel_runtime_for_hook = kernel_runtime.clone();
    let acceptance_mode = std::env::var("MF_WEB_ACCEPTANCE").ok().as_deref() == Some("1");
    let assembler: ProjectAttachHook = Arc::new(move |project_handle, root| {
        let project = mf_kernel::handles::ProjectStoreHandle::parse(project_handle)
            .map_err(|e| format!("handle 非法:{e}"))?;
        assemble_project_execution_with(
            &kernel_runtime_for_hook,
            &registry,
            &project,
            root,
            acceptance_mode,
        )
    });
    // 验收模式:注册沙箱项目(临时目录,不触碰真实 catalog),并装配
    // 执行面;生产形态项目经 pipe/orchestrator 注册。
    if std::env::var("MF_WEB_ACCEPTANCE").ok().as_deref() == Some("1") {
        let sandbox = std::env::temp_dir().join("mf-workbench-acceptance-project");
        if let Err(error) = std::fs::create_dir_all(&sandbox) {
            eprintln!("mf-workbench: 沙箱目录创建失败:{error}");
        }
        match kernel_runtime.open_project(&sandbox) {
            Ok(project) => {
                println!(
                    "mf-workbench: acceptance sandbox project {}({})",
                    project.handle().as_str(),
                    sandbox.display()
                );
                if let Err(error) = assembler(project.handle().as_str(), &sandbox) {
                    eprintln!("mf-workbench: 沙箱执行面装配失败(仅数据面可用):{error}");
                }
            }
            Err(error) => eprintln!("mf-workbench: 沙箱项目注册失败:{error}"),
        }
    }
    let kernel: Arc<dyn mf_kernel::kernel::CoreKernel> = kernel_runtime.kernel().clone();
    let port: u16 = std::env::var("MF_WEB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(80);
    match serve_workbench_with_hook(kernel, &dist, port, Some(assembler)) {
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
