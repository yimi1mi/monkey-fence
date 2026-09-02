//! launcher bin(T6c):start/open/status/stop。薄壳——不拥有 Core。

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("start") => println!("launcher: start(idempotent via dispatcher)"),
        Some("open") => println!("launcher: open(forwarded to running core)"),
        Some("status") => println!("launcher: status"),
        Some("stop") => println!("launcher: stop(shutdown assessment confirmation)"),
        _ => println!("usage: monkeyfence-launcher start|open|status|stop"),
    }
}
