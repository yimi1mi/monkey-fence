fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        // libgit2 在 Windows 上的 Win32 依赖
        println!("cargo:rustc-link-lib=advapi32");
        println!("cargo:rustc-link-lib=user32");
        println!("cargo:rustc-link-lib=ole32");
        println!("cargo:rustc-link-lib=rpcrt4");
    }
}
